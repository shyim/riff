use std::time::Duration;

use crate::advisory::AuditBehavior;
use crate::filter_list::{
    FilterEntriesByList, FilterListApiRequest, FilterListEntry, FilterListEntryBuilder,
    PackageVersions,
};
use crate::json::{Repository, RiffManifest, SecurityAdvisory};
use crate::package::Package;
#[cfg(test)]
use crate::policy_config::PolicyEnvironment;
use crate::policy_config::{PackagePolicyConfig, PolicyBlockScope, PolicyOperation, PolicyScope};
use crate::riff::Riff;
use crate::util::{canonical_package_name, is_platform_package};
use riff_semver::Semver;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyPhase {
    Update,
    Install,
}

#[derive(Debug, Clone)]
pub enum PolicyViolation {
    Advisory(SecurityAdvisory),
    Filter(FilterListEntry),
    Abandoned,
}

impl PolicyViolation {
    pub fn diagnostic(&self, package: &Package) -> String {
        match self {
            Self::Advisory(advisory) => format!(
                "Package {} {} is affected by security advisory \"{}\".",
                package.name,
                package.pretty_version(),
                advisory.advisory_id
            ),
            Self::Filter(entry) => {
                let mut detail = if entry.list_name == "malware" {
                    format!(
                        "Package {} {} was flagged as malware",
                        package.name,
                        package.pretty_version()
                    )
                } else {
                    format!(
                        "Package {} {} was filtered by {}",
                        package.name,
                        package.pretty_version(),
                        entry.list_name
                    )
                };
                if let Some(source) = &entry.source {
                    detail.push_str(&format!(" reported by {source}"));
                }
                if let Some(url) = &entry.url {
                    detail.push_str(&format!(" (see {url})"));
                }
                if let Some(reason) = &entry.reason {
                    detail.push_str(&format!(" reason: {reason}"));
                }
                if let Some(id) = &entry.id {
                    detail.push_str(&format!(" [{id}]"));
                }
                detail.push('.');
                detail
            }
            Self::Abandoned => format!(
                "Package {} {} is abandoned and package policy blocks abandoned packages.",
                package.name,
                package.pretty_version()
            ),
        }
    }
}

/// Normalized dependency policy plus repository data fetched for one operation.
#[derive(Debug, Clone)]
pub struct PackagePolicy {
    pub config: PackagePolicyConfig,
    advisories: Vec<SecurityAdvisory>,
    filters: Vec<FilterListEntry>,
    unreachable_repositories: Vec<String>,
}

impl PackagePolicy {
    pub async fn load(
        riff: &Riff,
        packages: &[&Package],
        scope: PolicyScope,
        blocking_disabled: bool,
    ) -> anyhow::Result<Self> {
        Self::load_inner(riff, packages, scope, blocking_disabled, false).await
    }

    pub(crate) async fn load_for_update(
        riff: &Riff,
        packages: &[&Package],
        blocking_disabled: bool,
        update_mirrors: bool,
    ) -> anyhow::Result<Self> {
        Self::load_inner(
            riff,
            packages,
            PolicyScope::Update,
            blocking_disabled,
            update_mirrors,
        )
        .await
    }

    async fn load_inner(
        riff: &Riff,
        packages: &[&Package],
        scope: PolicyScope,
        blocking_disabled: bool,
        disable_security_filter: bool,
    ) -> anyhow::Result<Self> {
        let mut config = if blocking_disabled {
            riff.package_policy.with_blocking_disabled()
        } else {
            riff.package_policy.clone()
        };
        // Composer does not run its security/abandoned pool filter for
        // `update --lock`/mirror-only updates, while filter-list policy still
        // checks the locked package set.
        if disable_security_filter {
            config.advisories.block = false;
            config.abandoned.block = false;
        }
        let mut advisories = Vec::new();
        let mut filters = Vec::new();
        collect_inline_metadata(&riff.manifest, &mut advisories, &mut filters);
        if !config.enabled {
            return Ok(Self {
                config,
                advisories,
                filters,
                unreachable_repositories: Vec::new(),
            });
        }

        let package_versions = package_versions(packages);
        let configured_lists = configured_filter_lists(&config, scope);
        let advisory_ignore_unreachable = match scope {
            PolicyScope::Audit => config.ignore_unreachable.audit,
            PolicyScope::Install => config.ignore_unreachable.install,
            PolicyScope::Update => config.ignore_unreachable.update,
        };
        let uses_install_scope_lists =
            scope == PolicyScope::Update && config.malware.should_block(PolicyBlockScope::Install);
        let filter_ignore_unreachable = match scope {
            PolicyScope::Audit => config.ignore_unreachable.audit,
            PolicyScope::Install => config.ignore_unreachable.install,
            PolicyScope::Update => {
                config.ignore_unreachable.update
                    && (!uses_install_scope_lists || config.ignore_unreachable.install)
            }
        };
        let mut unreachable_repositories = Vec::new();

        if should_fetch_advisories(&config, scope) {
            let allow_partial = scope == PolicyScope::Update
                && config.advisories.ignore.is_empty()
                && config.advisories.ignore_severity.is_empty()
                && config
                    .advisories
                    .ignore_id
                    .keys()
                    .all(|identifier| identifier.starts_with("PKSA-"));
            if !allow_partial {
                for advisory in &advisories {
                    if advisory.title.is_none()
                        || advisory.reported_at.is_none()
                        || advisory.sources.is_none()
                    {
                        anyhow::bail!(
                            "Advisory for {} could not be loaded as a full advisory",
                            advisory.package_name
                        );
                    }
                }
            }
            for repository in riff.repository_manager.repositories() {
                match repository
                    .get_security_advisories(&package_versions, allow_partial)
                    .await
                {
                    Ok(entries) => advisories.extend(entries),
                    Err(error) if advisory_ignore_unreachable => {
                        unreachable_repositories.push(error)
                    }
                    Err(error) => anyhow::bail!(error),
                }
            }
        }
        if !configured_lists.is_empty() {
            for repository in riff.repository_manager.repositories() {
                match repository
                    .get_filter_entries(&package_versions, &configured_lists)
                    .await
                {
                    Ok(entries) => extend_filter_entries(&mut filters, entries),
                    Err(error) if filter_ignore_unreachable => unreachable_repositories.push(error),
                    Err(error) => anyhow::bail!(error),
                }
            }
            collect_custom_source_entries(
                &config,
                &package_versions,
                &configured_lists,
                filter_ignore_unreachable,
                &mut filters,
                &mut unreachable_repositories,
            )
            .await?;
        }

        advisories.sort_by(|left, right| {
            left.package_name
                .cmp(&right.package_name)
                .then_with(|| left.advisory_id.cmp(&right.advisory_id))
        });
        advisories.dedup_by(|left, right| {
            left.package_name.eq_ignore_ascii_case(&right.package_name)
                && left.advisory_id.eq_ignore_ascii_case(&right.advisory_id)
        });
        filters.sort_by(|left, right| {
            left.package_name
                .cmp(&right.package_name)
                .then_with(|| left.list_name.cmp(&right.list_name))
                .then_with(|| left.id.cmp(&right.id))
        });
        filters.dedup();

        Ok(Self {
            config,
            advisories,
            filters,
            unreachable_repositories,
        })
    }

    #[cfg(test)]
    fn from_manifest(manifest: &RiffManifest) -> Self {
        let audit = serde_json::to_value(&manifest.config.audit).unwrap_or_default();
        let config = PackagePolicyConfig::from_raw(
            &manifest.config.policy,
            &audit,
            &PolicyEnvironment::new(),
        )
        .expect("test dependency policy must be valid");
        let mut advisories = Vec::new();
        let mut filters = Vec::new();
        collect_inline_metadata(manifest, &mut advisories, &mut filters);
        Self {
            config,
            advisories,
            filters,
            unreachable_repositories: Vec::new(),
        }
    }

    pub fn violations(
        &self,
        package: &Package,
        phase: PolicyPhase,
        apply_advisories: bool,
        also_apply_install_scope: bool,
    ) -> Vec<PolicyViolation> {
        if !self.config.enabled || is_platform_package(&package.name) {
            return Vec::new();
        }
        let mut violations = Vec::new();

        if phase == PolicyPhase::Update
            && apply_advisories
            && !package.is_dev()
            && self.config.advisories.block
        {
            for advisory in &self.advisories {
                if package_names(package)
                    .any(|name| advisory.package_name.eq_ignore_ascii_case(name))
                    && Semver::satisfies(&package.version, &advisory.affected_versions)
                    && !self.advisory_is_ignored(advisory, package, PolicyOperation::Block)
                {
                    violations.push(PolicyViolation::Advisory(advisory.clone()));
                }
            }
        }

        for filter in &self.filters {
            let blocks_phase = if also_apply_install_scope {
                self.filter_blocks(&filter.list_name, PolicyPhase::Install)
            } else {
                self.filter_blocks(&filter.list_name, phase)
            };
            if !blocks_phase
                || !package_names(package)
                    .any(|name| package_pattern_matches(&filter.package_name, name))
                || !Semver::satisfies(&package.version, &filter.constraint)
                || self.filter_is_ignored(filter, package, PolicyOperation::Block)
            {
                continue;
            }
            violations.push(PolicyViolation::Filter(filter.clone()));
        }

        if phase == PolicyPhase::Update
            && package.abandoned.is_some()
            && self.config.advisories.block
            && self.config.abandoned.block
            && !package_names(package).any(|name| {
                self.config.abandoned.package_is_ignored(
                    name,
                    &package.version,
                    PolicyOperation::Block,
                )
            })
        {
            violations.push(PolicyViolation::Abandoned);
        }

        violations
    }

    pub fn audit_advisories(&self) -> &[SecurityAdvisory] {
        &self.advisories
    }

    pub fn audit_filters(&self, packages: &[&Package]) -> Vec<FilterListEntry> {
        let mut entries = packages
            .iter()
            .flat_map(|package| {
                self.filters.iter().filter_map(move |entry| {
                    let active = if entry.list_name.eq_ignore_ascii_case("malware") {
                        self.config.malware.audit != AuditBehavior::Ignore
                    } else {
                        self.config
                            .custom_lists
                            .get(&entry.list_name)
                            .is_some_and(|policy| policy.audit != AuditBehavior::Ignore)
                    };
                    (active
                        && package_names(package)
                            .any(|name| package_pattern_matches(&entry.package_name, name))
                        && Semver::satisfies(&package.version, &entry.constraint)
                        && !self.filter_is_ignored(entry, package, PolicyOperation::Audit))
                    .then(|| {
                        let mut entry = entry.clone();
                        entry.package_name.clone_from(&package.name);
                        entry
                    })
                })
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.package_name
                .cmp(&right.package_name)
                .then_with(|| left.list_name.cmp(&right.list_name))
                .then_with(|| left.id.cmp(&right.id))
        });
        entries.dedup();
        entries
    }

    pub fn unreachable_repositories(&self) -> &[String] {
        &self.unreachable_repositories
    }

    pub fn advisory_is_ignored_for_audit(
        &self,
        advisory: &SecurityAdvisory,
        package: &Package,
    ) -> bool {
        self.advisory_is_ignored(advisory, package, PolicyOperation::Audit)
    }

    fn filter_blocks(&self, name: &str, phase: PolicyPhase) -> bool {
        let scope = match phase {
            PolicyPhase::Install => PolicyBlockScope::Install,
            PolicyPhase::Update => PolicyBlockScope::Update,
        };
        if name.eq_ignore_ascii_case("malware") {
            self.config.malware.should_block(scope)
        } else {
            self.config
                .custom_lists
                .get(name)
                .is_some_and(|policy| policy.should_block(scope))
        }
    }

    fn advisory_is_ignored(
        &self,
        advisory: &SecurityAdvisory,
        package: &Package,
        operation: PolicyOperation,
    ) -> bool {
        package_names(package).any(|name| {
            self.config
                .advisories
                .package_is_ignored(name, &package.version, operation)
        }) || self
            .config
            .advisories
            .identifier_is_ignored(&advisory.advisory_id, operation)
            || advisory.cve.as_deref().is_some_and(|identifier| {
                self.config
                    .advisories
                    .identifier_is_ignored(identifier, operation)
            })
            || advisory
                .sources
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|source| {
                    self.config
                        .advisories
                        .identifier_is_ignored(&source.remote_id, operation)
                })
            || advisory.severity.as_deref().is_some_and(|severity| {
                self.config
                    .advisories
                    .severity_is_ignored(severity, operation)
            })
    }

    fn filter_is_ignored(
        &self,
        filter: &FilterListEntry,
        package: &Package,
        operation: PolicyOperation,
    ) -> bool {
        if filter.list_name.eq_ignore_ascii_case("malware") {
            return package_names(package).any(|name| {
                self.config
                    .malware
                    .package_is_ignored(name, &package.version, operation)
            }) || filter.source.as_deref().is_some_and(|source| {
                self.config
                    .malware
                    .ignore_source
                    .iter()
                    .any(|ignored| ignored == source)
            });
        }
        self.config
            .custom_lists
            .get(&filter.list_name)
            .is_some_and(|policy| {
                package_names(package)
                    .any(|name| policy.package_is_ignored(name, &package.version, operation))
            })
    }
}

fn package_names(package: &Package) -> impl Iterator<Item = &str> {
    std::iter::once(package.name.as_str())
        .chain(package.provide.keys().map(|name| name.as_str()))
        .chain(package.replace.keys().map(|name| name.as_str()))
}

fn package_versions(packages: &[&Package]) -> PackageVersions {
    let mut versions = PackageVersions::new();
    for package in packages {
        if is_platform_package(&package.name) {
            continue;
        }
        for name in package_names(package) {
            versions
                .entry(canonical_package_name(name).into_owned())
                .or_default()
                .insert(package.version.to_string());
        }
    }
    versions
}

fn should_fetch_advisories(config: &PackagePolicyConfig, scope: PolicyScope) -> bool {
    match scope {
        PolicyScope::Audit => config.advisories.audit != AuditBehavior::Ignore,
        PolicyScope::Update => config.advisories.block,
        PolicyScope::Install => false,
    }
}

fn configured_filter_lists(config: &PackagePolicyConfig, scope: PolicyScope) -> Vec<String> {
    let malware_active = match scope {
        PolicyScope::Audit => config.malware.audit != AuditBehavior::Ignore,
        PolicyScope::Install => config.malware.should_block(PolicyBlockScope::Install),
        PolicyScope::Update => {
            config.malware.should_block(PolicyBlockScope::Update)
                || config.malware.should_block(PolicyBlockScope::Install)
        }
    };
    let mut lists = malware_active
        .then(|| "malware".to_string())
        .into_iter()
        .collect::<Vec<_>>();
    lists.extend(config.custom_lists.iter().filter_map(|(name, policy)| {
        let active = match scope {
            PolicyScope::Audit => policy.audit != AuditBehavior::Ignore,
            PolicyScope::Update => policy.should_block(PolicyBlockScope::Update),
            PolicyScope::Install => false,
        };
        active.then(|| name.clone())
    }));
    lists.sort();
    lists
}

fn extend_filter_entries(target: &mut Vec<FilterListEntry>, entries: FilterEntriesByList) {
    target.extend(entries.into_values().flatten());
}

async fn collect_custom_source_entries(
    config: &PackagePolicyConfig,
    package_versions: &PackageVersions,
    configured_lists: &[String],
    ignore_unreachable: bool,
    filters: &mut Vec<FilterListEntry>,
    unreachable: &mut Vec<String>,
) -> anyhow::Result<()> {
    let package_names = package_versions.keys().cloned().collect::<Vec<_>>();
    let client = reqwest::Client::new();
    for (name, policy) in &config.custom_lists {
        if !configured_lists.contains(name) {
            continue;
        }
        for source in &policy.sources {
            let request = FilterListApiRequest::post_purls(
                &source.url,
                &package_names,
                std::slice::from_ref(name),
            )?;
            let response = client
                .post(&request.url)
                .header(reqwest::header::CONTENT_TYPE, request.content_type)
                .timeout(Duration::from_secs(request.timeout_seconds))
                .body(request.body)
                .send()
                .await;
            let response = match response {
                Ok(response) if response.status().is_success() => response,
                Ok(response) => {
                    let error = format!(
                        "Dependency policy source {} returned HTTP {}",
                        source.url,
                        response.status()
                    );
                    if ignore_unreachable {
                        unreachable.push(error);
                        continue;
                    }
                    anyhow::bail!(error);
                }
                Err(error) => {
                    let error = format!(
                        "Failed to fetch dependency policy source {}: {error}",
                        source.url
                    );
                    if ignore_unreachable {
                        unreachable.push(error);
                        continue;
                    }
                    anyhow::bail!(error);
                }
            };
            let document: serde_json::Value = response.json().await?;
            let raw = document.get("filter").cloned().unwrap_or_default();
            let wrapped = if raw.is_array() {
                serde_json::json!({(name): raw})
            } else {
                raw
            };
            let entries = FilterListEntryBuilder.build(&wrapped, package_versions, None);
            extend_filter_entries(filters, entries);
        }
    }
    Ok(())
}

#[derive(Clone)]
struct RepositorySelection {
    only: Vec<String>,
    exclude: Vec<String>,
}

impl RepositorySelection {
    fn allows(&self, package: &str) -> bool {
        (self.only.is_empty()
            || self
                .only
                .iter()
                .any(|pattern| package_pattern_matches(pattern, package)))
            && !self
                .exclude
                .iter()
                .any(|pattern| package_pattern_matches(pattern, package))
    }
}

fn collect_inline_metadata(
    manifest: &RiffManifest,
    advisories: &mut Vec<SecurityAdvisory>,
    filters: &mut Vec<FilterListEntry>,
) {
    for repository in manifest.repositories.as_vec() {
        collect_repository_metadata(&repository, &[], advisories, filters);
    }
}

fn collect_repository_metadata(
    repository: &Repository,
    selections: &[RepositorySelection],
    advisories: &mut Vec<SecurityAdvisory>,
    filters: &mut Vec<FilterListEntry>,
) {
    match repository {
        Repository::Filtered {
            repository,
            only,
            exclude,
            ..
        } => {
            let mut nested = selections.to_vec();
            nested.push(RepositorySelection {
                only: only.clone(),
                exclude: exclude.clone(),
            });
            collect_repository_metadata(repository, &nested, advisories, filters);
        }
        Repository::Package {
            security_advisories,
            filter,
            ..
        } => {
            advisories.extend(
                security_advisories
                    .values()
                    .flatten()
                    .filter(|advisory| {
                        selections
                            .iter()
                            .all(|selection| selection.allows(&advisory.package_name))
                    })
                    .cloned(),
            );
            for (list, entries) in filter {
                filters.extend(
                    entries
                        .iter()
                        .filter(|entry| {
                            selections
                                .iter()
                                .all(|selection| selection.allows(&entry.package))
                        })
                        .map(|entry| FilterListEntry {
                            package_name: entry.package.clone(),
                            list_name: list.clone(),
                            constraint: entry.constraint.clone(),
                            url: entry.url.clone(),
                            reason: entry.reason.clone(),
                            id: entry.id.clone(),
                            source: entry.source.clone(),
                        }),
                );
            }
        }
        _ => {}
    }
}

fn package_pattern_matches(pattern: &str, package: &str) -> bool {
    crate::package::package_name_matches(pattern, package)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::Abandoned;

    fn policy(value: serde_json::Value) -> PackagePolicy {
        let manifest: RiffManifest = serde_json::from_value(value).unwrap();
        PackagePolicy::from_manifest(&manifest)
    }

    fn filter_policy(config: serde_json::Value, list: &str, constraint: &str) -> PackagePolicy {
        policy(serde_json::json!({
            "config": {"policy": {(list): config}},
            "repositories": [{
                "type": "package",
                "package": {"name": "acme/package", "version": "1.0", "type": "metapackage"},
                "filter": {(list): [{"package": "acme/package", "constraint": constraint}]}
            }]
        }))
    }

    fn advisory_policy(config: serde_json::Value) -> PackagePolicy {
        policy(serde_json::json!({
            "config": {"policy": {"advisories": config}},
            "repositories": [{
                "type": "package",
                "package": {"name": "acme/package", "version": "1.0", "type": "metapackage"},
                "security-advisories": {"acme/package": [
                    {"advisoryId": "PKSA-one", "packageName": "acme/package", "affectedVersions": ">=1.0,<1.1"},
                    {"advisoryId": "PKSA-two", "packageName": "acme/package", "affectedVersions": ">=1.0,<1.1"}
                ]}
            }]
        }))
    }

    #[test]
    fn composer_filter_list_removes_only_matching_package_versions() {
        let policy = filter_policy(serde_json::json!(true), "test-list", "1.0");
        assert_eq!(
            policy
                .violations(
                    &Package::new("acme/package", "1.0"),
                    PolicyPhase::Update,
                    true,
                    false,
                )
                .len(),
            1
        );
        assert!(policy
            .violations(
                &Package::new("acme/package", "2.0"),
                PolicyPhase::Update,
                true,
                false,
            )
            .is_empty());
    }

    #[test]
    fn composer_filter_list_package_ignores_are_version_scoped() {
        for config in [
            serde_json::json!({"ignore": ["acme/package"]}),
            serde_json::json!({"ignore": {"acme/package": {"constraint": "*"}}}),
        ] {
            let policy = filter_policy(config, "test-list", "*");
            assert!(policy
                .violations(
                    &Package::new("acme/package", "1.0"),
                    PolicyPhase::Update,
                    true,
                    false,
                )
                .is_empty());
        }

        let scoped_policy = filter_policy(
            serde_json::json!({"ignore": {"acme/package": {"constraint": "<=1.0"}}}),
            "test-list",
            ">=1.0",
        );
        assert!(scoped_policy
            .violations(
                &Package::new("acme/package", "1.0"),
                PolicyPhase::Update,
                true,
                false,
            )
            .is_empty());
        assert_eq!(
            scoped_policy
                .violations(
                    &Package::new("acme/package", "1.1"),
                    PolicyPhase::Update,
                    true,
                    false,
                )
                .len(),
            1
        );

        let rules_policy = policy(serde_json::json!({
            "config": {"policy": {"test-list": {"ignore": {
                "vendor/foo": [
                    {"constraint": "^1.0", "reason": "old version"},
                    {"constraint": "^3.0", "on-block": false}
                ],
                "vendor/bar": {"constraint": "^2.0", "on-block": false},
                "vendor/baz": null,
                "vendor/qux": {"on-audit": false}
            }}}}
        }));
        let list = &rules_policy.config.custom_lists["test-list"];
        assert!(list.package_is_ignored("vendor/foo", "1.2.0", PolicyOperation::Block));
        assert!(!list.package_is_ignored("vendor/foo", "2.0.0", PolicyOperation::Block));
        assert!(!list.package_is_ignored("vendor/foo", "3.1.0", PolicyOperation::Block));
        assert!(!list.package_is_ignored("vendor/bar", "2.1.0", PolicyOperation::Block));
        assert!(list.package_is_ignored("vendor/baz", "1.0.0", PolicyOperation::Block));
        assert!(list.package_is_ignored("vendor/qux", "1.0.0", PolicyOperation::Block));

        let empty = policy(serde_json::json!({
            "config": {"policy": {"test-list": {"ignore": []}}}
        }));
        assert!(!empty.config.custom_lists["test-list"].package_is_ignored(
            "vendor/foo",
            "1.0.0",
            PolicyOperation::Block,
        ));
    }

    #[test]
    fn composer_locked_update_candidates_use_only_install_scope_filters() {
        let package = Package::new("acme/package", "1.0");
        let custom = filter_policy(serde_json::json!(true), "company-policy", "*");
        assert_eq!(
            custom
                .violations(&package, PolicyPhase::Update, true, false)
                .len(),
            1
        );
        assert!(custom
            .violations(&package, PolicyPhase::Update, true, true)
            .is_empty());
        assert!(custom
            .violations(&package, PolicyPhase::Install, false, false)
            .is_empty());

        let default_malware = filter_policy(serde_json::json!(true), "malware", "*");
        assert_eq!(
            default_malware
                .violations(&package, PolicyPhase::Install, true, false)
                .len(),
            1
        );
        assert_eq!(
            default_malware
                .violations(&package, PolicyPhase::Update, true, true)
                .len(),
            1
        );

        let malware = filter_policy(
            serde_json::json!({"block-scope": "install"}),
            "malware",
            "*",
        );
        assert_eq!(
            malware
                .violations(&package, PolicyPhase::Update, true, true)
                .len(),
            1
        );
        assert!(malware
            .violations(&package, PolicyPhase::Update, true, false)
            .is_empty());
    }

    #[test]
    fn composer_security_advisory_filter_reports_and_ignores_identifiers() {
        let package = Package::new("acme/package", "1.0");
        let policy = advisory_policy(serde_json::json!(true));
        let identifiers = policy
            .violations(&package, PolicyPhase::Update, true, false)
            .into_iter()
            .filter_map(|violation| match violation {
                PolicyViolation::Advisory(advisory) => Some(advisory.advisory_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(identifiers, ["PKSA-one", "PKSA-two"]);
        assert!(policy
            .violations(
                &Package::new("acme/package", "2.0"),
                PolicyPhase::Update,
                true,
                false,
            )
            .is_empty());
        assert!(policy
            .violations(&package, PolicyPhase::Install, false, false)
            .is_empty());

        let ignored = advisory_policy(serde_json::json!({
            "ignore-id": ["PKSA-one", "PKSA-two"]
        }));
        assert!(ignored
            .violations(&package, PolicyPhase::Update, true, false)
            .is_empty());

        let disabled = advisory_policy(serde_json::json!({"block": false}));
        assert!(disabled
            .violations(&package, PolicyPhase::Update, true, false)
            .is_empty());
        assert!(disabled
            .violations(&package, PolicyPhase::Install, false, false)
            .is_empty());

        for config in [serde_json::json!({}), serde_json::json!(true)] {
            let defaults = advisory_policy(config);
            assert_eq!(
                defaults
                    .violations(&package, PolicyPhase::Update, true, false)
                    .len(),
                2
            );
            assert!(defaults
                .violations(&package, PolicyPhase::Install, false, false)
                .is_empty());
        }
    }

    #[test]
    fn composer_malware_policy_honors_the_block_scope_matrix() {
        let package = Package::new("acme/package", "1.0");
        for config in [serde_json::json!(true), serde_json::json!({})] {
            let policy = filter_policy(config, "malware", "*");
            assert_eq!(
                policy
                    .violations(&package, PolicyPhase::Update, true, false)
                    .len(),
                1
            );
            assert_eq!(
                policy
                    .violations(&package, PolicyPhase::Install, false, false)
                    .len(),
                1
            );
        }

        let disabled = filter_policy(serde_json::json!({"block": false}), "malware", "*");
        assert!(disabled
            .violations(&package, PolicyPhase::Update, true, false)
            .is_empty());
        assert!(disabled
            .violations(&package, PolicyPhase::Install, false, false)
            .is_empty());

        let configured = filter_policy(
            serde_json::json!({
                "block": false,
                "block-scope": "update",
                "ignore": {"acme/malware2": {"constraint": "1.0"}},
                "ignore-source": ["untrusted"]
            }),
            "malware",
            "*",
        );
        assert!(!configured.config.malware.block);
        assert_eq!(configured.config.malware.block_scope, "update");
        assert_eq!(configured.config.malware.ignore_source, ["untrusted"]);
        assert!(configured
            .config
            .malware
            .ignore
            .contains_key("acme/malware2"));

        for (scope, update, install) in [
            ("all", true, true),
            ("update", true, false),
            ("install", false, true),
        ] {
            let policy = filter_policy(serde_json::json!({"block-scope": scope}), "malware", "*");
            assert_eq!(
                !policy
                    .violations(&package, PolicyPhase::Update, true, false)
                    .is_empty(),
                update
            );
            assert_eq!(
                !policy
                    .violations(&package, PolicyPhase::Install, false, false)
                    .is_empty(),
                install
            );
        }
    }

    #[test]
    fn inline_advisories_honor_structured_ignores_and_severity() {
        let policy = policy(serde_json::json!({
            "config": {"policy": {"advisories": {"ignore-severity": ["low"]}}},
            "repositories": [{
                "type": "package",
                "package": {"name": "acme/package", "version": "1.0", "type": "metapackage"},
                "security-advisories": {"acme/package": [{
                    "advisoryId": "PKSA-test", "packageName": "acme/package",
                    "affectedVersions": "*", "severity": "low"
                }]}
            }]
        }));
        assert!(policy
            .violations(
                &Package::new("acme/package", "1.0"),
                PolicyPhase::Update,
                true,
                false
            )
            .is_empty());
    }

    #[test]
    fn malware_defaults_to_all_scopes_and_honors_source_ignores() {
        let policy = policy(serde_json::json!({
            "config": {"policy": {"malware": {"ignore-source": ["trusted"]}}},
            "repositories": [{
                "type": "package",
                "package": {"name": "acme/package", "version": "1.0", "type": "metapackage"},
                "filter": {"malware": [{"package": "acme/package", "constraint": "*", "source": "trusted"}]}
            }]
        }));
        let package = Package::new("acme/package", "1.0");
        assert!(policy
            .violations(&package, PolicyPhase::Update, true, false)
            .is_empty());
        assert!(policy
            .violations(&package, PolicyPhase::Install, false, false)
            .is_empty());
    }

    #[test]
    fn abandoned_blocking_uses_versioned_package_ignores() {
        let mut package = Package::new("acme/package", "1.2.0");
        package.abandoned = Some(Abandoned::Yes);
        let defaults = policy(serde_json::json!({}));
        assert!(defaults
            .violations(&package, PolicyPhase::Update, true, false)
            .is_empty());

        let policy = policy(serde_json::json!({
            "config": {"policy": {"abandoned": {
                "block": true,
                "ignore": {"acme/package": {"constraint": "^1.0"}}
            }}}
        }));
        assert!(policy
            .violations(&package, PolicyPhase::Update, true, false)
            .is_empty());

        let mut blocked = Package::new("acme/other", "1.0.0");
        blocked.abandoned = Some(Abandoned::Yes);
        assert_eq!(
            policy
                .violations(&blocked, PolicyPhase::Update, true, false)
                .len(),
            1
        );
        assert!(policy.config.abandoned.block);
        assert!(policy.config.abandoned.ignore.contains_key("acme/package"));
    }

    #[test]
    fn master_policy_false_disables_every_blocker() {
        let policy = policy(serde_json::json!({
            "config": {"policy": false},
            "repositories": [{
                "type": "package",
                "package": {"name": "acme/package", "version": "1.0", "type": "metapackage"},
                "filter": {"malware": [{"package": "acme/package", "constraint": "*"}]}
            }]
        }));
        assert!(policy
            .violations(
                &Package::new("acme/package", "1.0"),
                PolicyPhase::Update,
                true,
                false
            )
            .is_empty());
    }
}
