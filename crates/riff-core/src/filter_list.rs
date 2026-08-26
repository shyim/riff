use crate::package::package_name_matches;
use riff_semver::Semver;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const RESERVED_LIST_NAMES: &[&str] = &["advisories", "abandoned"];
const FUTURE_RESERVED_LIST_NAMES: &[&str] = &[
    "package",
    "packages",
    "license",
    "licence",
    "licenses",
    "licences",
    "support",
    "maintenance",
    "security",
    "minimum-release-age",
];
const FUTURE_RESERVED_LIST_PREFIXES: &[&str] = &["ignore"];

/// Filter-list capabilities advertised by a Composer repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposerRepositoryFilterInformation {
    pub metadata: bool,
    pub lists: Vec<String>,
    pub summary_url: Option<String>,
    pub api_url: Option<String>,
}

impl ComposerRepositoryFilterInformation {
    pub fn from_data(data: &Value) -> Self {
        Self::from_data_with(data, str::to_owned)
    }

    pub fn from_data_with(data: &Value, canonicalize_url: impl Fn(&str) -> String) -> Self {
        let object = data.as_object();
        let lists = object
            .and_then(|object| object.get("lists"))
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|lists| lists.iter())
            .filter_map(|(name, config)| {
                let enabled = config
                    .as_object()
                    .and_then(|config| config.get("enabled"))
                    .is_some_and(php_truthy);
                (enabled && !is_reserved_list_name(name)).then(|| name.clone())
            })
            .collect();

        let read_url = |key: &str| {
            object
                .and_then(|object| object.get(key))
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
                .map(&canonicalize_url)
        };

        Self {
            metadata: object
                .and_then(|object| object.get("metadata"))
                .is_some_and(php_truthy),
            lists,
            summary_url: read_url("summary-url"),
            api_url: read_url("api-url"),
        }
    }
}

fn php_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !matches!(value.as_str(), "" | "0"),
        Value::Array(value) => !value.is_empty(),
        Value::Object(_) => true,
    }
}

fn is_reserved_list_name(name: &str) -> bool {
    RESERVED_LIST_NAMES.contains(&name)
        || FUTURE_RESERVED_LIST_NAMES.contains(&name)
        || FUTURE_RESERVED_LIST_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// One package match from a repository-provided dependency filter list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterListEntry {
    pub package_name: String,
    pub list_name: String,
    pub constraint: String,
    pub url: Option<String>,
    pub reason: Option<String>,
    pub id: Option<String>,
    pub source: Option<String>,
}

pub type FilterEntriesByList = BTreeMap<String, Vec<FilterListEntry>>;
pub type FilterListMap = BTreeMap<String, FilterEntriesByList>;
pub type PackageVersions = BTreeMap<String, BTreeSet<String>>;

/// Transport-independent request produced for a remote filter-list endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterListApiRequest {
    pub url: String,
    pub method: &'static str,
    pub content_type: &'static str,
    pub timeout_seconds: u64,
    pub body: String,
}

impl FilterListApiRequest {
    pub fn post_purls(
        url: impl Into<String>,
        package_names: &[String],
        configured_lists: &[String],
    ) -> serde_json::Result<Self> {
        let packages = package_names
            .iter()
            .map(|package| format!("pkg://composer/{package}"))
            .collect::<Vec<_>>();
        let body = serde_json::to_string(&serde_json::json!({
            "packages": packages,
            "lists": configured_lists,
        }))?;
        Ok(Self {
            url: url.into(),
            method: "POST",
            content_type: "application/json",
            timeout_seconds: 10,
            body,
        })
    }
}

/// Converts repository JSON metadata into typed filter-list entries.
#[derive(Debug, Clone, Copy, Default)]
pub struct FilterListEntryBuilder;

impl FilterListEntryBuilder {
    pub fn build(
        &self,
        raw_by_list: &Value,
        package_versions: &PackageVersions,
        default_package: Option<&str>,
    ) -> FilterEntriesByList {
        let Some(raw_by_list) = raw_by_list.as_object() else {
            return BTreeMap::new();
        };
        let mut result = BTreeMap::new();

        for (list_name, raw_entries) in raw_by_list {
            let Some(raw_entries) = raw_entries.as_array() else {
                continue;
            };
            for raw_entry in raw_entries {
                let Some(data) = raw_entry.as_object() else {
                    continue;
                };
                let Some(constraint) = data.get("constraint").and_then(Value::as_str) else {
                    continue;
                };
                let package_name = data
                    .get("package")
                    .and_then(Value::as_str)
                    .or(default_package);
                let Some(package_name) = package_name else {
                    continue;
                };
                if !package_has_matching_version(package_versions, package_name, constraint) {
                    continue;
                }

                result
                    .entry(list_name.clone())
                    .or_insert_with(Vec::new)
                    .push(FilterListEntry {
                        package_name: package_name.to_owned(),
                        list_name: list_name.clone(),
                        constraint: constraint.to_owned(),
                        url: optional_string(data.get("url")),
                        reason: optional_string(data.get("reason")),
                        id: optional_string(data.get("id")),
                        source: optional_string(data.get("source")),
                    });
            }
        }

        result
    }
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

fn package_has_matching_version(
    package_versions: &PackageVersions,
    package_name: &str,
    constraint: &str,
) -> bool {
    package_versions
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(package_name))
        .is_some_and(|(_, versions)| {
            versions
                .iter()
                .any(|version| Semver::satisfies(version, constraint))
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterListPackage {
    pub name: String,
    pub version: String,
    pub additional_names: Vec<String>,
    pub is_root: bool,
    pub is_root_alias: bool,
}

impl FilterListPackage {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            additional_names: Vec::new(),
            is_root: false,
            is_root_alias: false,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("{message}")]
pub struct FilterListProviderError {
    message: String,
}

impl FilterListProviderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

pub trait FilterListProvider: Send + Sync {
    fn has_filter(&self) -> Result<bool, FilterListProviderError> {
        Ok(true)
    }

    fn filter_lists(&self) -> Vec<String>;

    fn filter(
        &self,
        package_versions: &PackageVersions,
        configured_lists: &[String],
    ) -> Result<FilterEntriesByList, FilterListProviderError>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatchingFilterLists {
    pub filter: FilterEntriesByList,
    pub unreachable_repositories: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CollectedFilterLists {
    pub filter: FilterListMap,
    pub unreachable_repositories: Vec<String>,
}

/// Aggregates repository and explicit filter-list sources.
pub struct FilterListProviderSet {
    providers: Vec<Box<dyn FilterListProvider>>,
    unreachable_repositories: Vec<FilterListProviderError>,
}

impl FilterListProviderSet {
    pub fn new(
        repositories: Vec<Box<dyn FilterListProvider>>,
        mut sources: Vec<Box<dyn FilterListProvider>>,
    ) -> Self {
        let mut providers = Vec::new();
        let mut unreachable_repositories = Vec::new();
        for repository in repositories {
            match repository.has_filter() {
                Ok(true) => providers.push(repository),
                Ok(false) => {}
                Err(error) => unreachable_repositories.push(error),
            }
        }
        providers.append(&mut sources);
        Self {
            providers,
            unreachable_repositories,
        }
    }

    pub fn get_matching_filter_lists(
        &self,
        packages: &[FilterListPackage],
        configured_lists: &[String],
        ignore_unreachable: bool,
    ) -> Result<MatchingFilterLists, FilterListProviderError> {
        if !ignore_unreachable {
            if let Some(error) = self.unreachable_repositories.first() {
                return Err(error.clone());
            }
        }

        let mut package_versions = PackageVersions::new();
        for package in packages.iter().filter(|package| !package.is_root_alias) {
            package_versions
                .entry(package.name.clone())
                .or_default()
                .insert(package.version.clone());
        }

        let mut unreachable_repositories = if ignore_unreachable {
            self.unreachable_repositories
                .iter()
                .map(|error| error.message.clone())
                .collect()
        } else {
            Vec::new()
        };
        let mut filter = FilterEntriesByList::new();

        for provider in &self.providers {
            let provider_lists = provider.filter_lists();
            let relevant_lists = configured_lists
                .iter()
                .filter(|configured| provider_lists.contains(configured))
                .cloned()
                .collect::<Vec<_>>();
            if relevant_lists.is_empty() {
                continue;
            }

            let provided = match provider.filter(&package_versions, &relevant_lists) {
                Ok(provided) => provided,
                Err(error) if ignore_unreachable => {
                    unreachable_repositories.push(error.message);
                    continue;
                }
                Err(error) => return Err(error),
            };
            for (list_name, entries) in provided {
                if !configured_lists.contains(&list_name) || !provider_lists.contains(&list_name) {
                    continue;
                }
                filter
                    .entry(list_name)
                    .or_default()
                    .extend(entries.into_iter().filter(|entry| {
                        package_has_matching_version(
                            &package_versions,
                            &entry.package_name,
                            &entry.constraint,
                        )
                    }));
            }
        }

        Ok(MatchingFilterLists {
            filter,
            unreachable_repositories,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOperation {
    Audit,
    Block(FilterBlockScope),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FilterBlockScope {
    Install,
    Update,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterListIgnoreRule {
    pub package_pattern: String,
    pub constraint: String,
    pub on_audit: bool,
    pub on_block: bool,
}

impl FilterListIgnoreRule {
    pub fn new(package_pattern: impl Into<String>) -> Self {
        Self {
            package_pattern: package_pattern.into(),
            constraint: "*".to_owned(),
            on_audit: true,
            on_block: true,
        }
    }

    pub fn with_constraint(mut self, constraint: impl Into<String>) -> Self {
        self.constraint = constraint.into();
        self
    }

    pub fn on_audit(mut self, enabled: bool) -> Self {
        self.on_audit = enabled;
        self
    }

    pub fn on_block(mut self, enabled: bool) -> Self {
        self.on_block = enabled;
        self
    }

    fn applies_to(&self, operation: FilterOperation) -> bool {
        match operation {
            FilterOperation::Audit => self.on_audit,
            FilterOperation::Block(_) => self.on_block,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterListPolicy {
    pub audit: bool,
    pub block_scopes: BTreeSet<FilterBlockScope>,
    pub ignore: Vec<FilterListIgnoreRule>,
    pub ignore_sources: Vec<String>,
}

impl FilterListPolicy {
    pub fn audit(mut self, enabled: bool) -> Self {
        self.audit = enabled;
        self
    }

    pub fn block_for(mut self, scope: FilterBlockScope) -> Self {
        self.block_scopes.insert(scope);
        self
    }

    pub fn ignore(mut self, rule: FilterListIgnoreRule) -> Self {
        self.ignore.push(rule);
        self
    }

    pub fn ignore_source(mut self, source: impl Into<String>) -> Self {
        self.ignore_sources.push(source.into());
        self
    }

    fn is_active(&self, operation: FilterOperation) -> bool {
        match operation {
            FilterOperation::Audit => self.audit,
            FilterOperation::Block(scope) => {
                self.block_scopes.contains(&FilterBlockScope::All)
                    || self.block_scopes.contains(&scope)
            }
        }
    }
}

/// Applies configured filter-list policy to package entries.
#[derive(Debug, Clone, Copy, Default)]
pub struct FilterListAuditor;

impl FilterListAuditor {
    pub fn collect_filter_lists(result: MatchingFilterLists) -> CollectedFilterLists {
        let mut map = FilterListMap::new();
        for entries in result.filter.into_values() {
            for entry in entries {
                map.entry(entry.package_name.clone())
                    .or_default()
                    .entry(entry.list_name.clone())
                    .or_default()
                    .push(entry);
            }
        }
        CollectedFilterLists {
            filter: map,
            unreachable_repositories: result.unreachable_repositories,
        }
    }

    pub fn get_matching_entries(
        &self,
        package: &FilterListPackage,
        filter_list_map: &FilterListMap,
        policies: &BTreeMap<String, FilterListPolicy>,
        operation: FilterOperation,
    ) -> Vec<FilterListEntry> {
        if package.is_root || filter_list_map.is_empty() {
            return Vec::new();
        }

        let mut matching = Vec::new();
        for package_name in std::iter::once(&package.name).chain(&package.additional_names) {
            let Some(entries_by_list) = filter_list_map
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(package_name))
                .map(|(_, entries)| entries)
            else {
                continue;
            };

            for (list_name, entries) in entries_by_list {
                let Some(policy) = policies
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(list_name))
                    .map(|(_, policy)| policy)
                    .filter(|policy| policy.is_active(operation))
                else {
                    continue;
                };
                if policy.ignore.iter().any(|rule| {
                    rule.applies_to(operation)
                        && package_name_matches(&rule.package_pattern, package_name)
                        && Semver::satisfies(&package.version, &rule.constraint)
                }) {
                    continue;
                }

                matching.extend(
                    entries
                        .iter()
                        .filter(|entry| {
                            Semver::satisfies(&package.version, &entry.constraint)
                                && !(list_name.eq_ignore_ascii_case("malware")
                                    && entry.source.as_ref().is_some_and(|source| {
                                        policy
                                            .ignore_sources
                                            .iter()
                                            .any(|ignored| ignored == source)
                                    }))
                        })
                        .cloned(),
                );
            }
        }
        matching
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn versions(packages: &[(&str, &[&str])]) -> PackageVersions {
        packages
            .iter()
            .map(|(name, versions)| {
                (
                    (*name).to_owned(),
                    versions
                        .iter()
                        .map(|version| (*version).to_owned())
                        .collect(),
                )
            })
            .collect()
    }

    fn entry(list: &str, package: &str, constraint: &str) -> FilterListEntry {
        FilterListEntry {
            package_name: package.to_owned(),
            list_name: list.to_owned(),
            constraint: constraint.to_owned(),
            url: None,
            reason: None,
            id: None,
            source: None,
        }
    }

    // Ported from ComposerRepositoryFilterInformationTest's list-shape methods.
    #[test]
    fn composer_repository_filter_information_selects_enabled_custom_lists() {
        let info = ComposerRepositoryFilterInformation::from_data(&json!({
            "metadata": true,
            "lists": {
                "company-policy": {"enabled": true},
                "aikido": {"enabled": true},
                "malware": {"enabled": true},
                "disabled": {"enabled": false},
                "missing": {},
                "scalar": true
            }
        }));
        assert!(info.metadata);
        assert_eq!(info.lists, ["company-policy", "aikido", "malware"]);
    }

    // Ported from ComposerRepositoryFilterInformationTest's reserved-name methods.
    #[test]
    fn composer_repository_filter_information_drops_reserved_names_and_prefixes() {
        let info = ComposerRepositoryFilterInformation::from_data(&json!({
            "lists": {
                "advisories": {"enabled": true},
                "abandoned": {"enabled": true},
                "security": {"enabled": true},
                "ignore-foo": {"enabled": true},
                "ignoremalware": {"enabled": true},
                "company-policy": {"enabled": true}
            }
        }));
        assert_eq!(info.lists, ["company-policy"]);
    }

    // Ported from ComposerRepositoryFilterInformationTest's URL methods.
    #[test]
    fn composer_repository_filter_information_canonicalizes_only_string_urls() {
        let missing = ComposerRepositoryFilterInformation::from_data(&json!({
            "lists": {"malware": {"enabled": true}}
        }));
        assert_eq!(missing.summary_url, None);
        assert_eq!(missing.api_url, None);

        let defaults = ComposerRepositoryFilterInformation::from_data(&json!({
            "lists": {"malware": {"enabled": true}},
            "summary-url": ["oops"]
        }));
        assert_eq!(defaults.summary_url, None);
        assert_eq!(defaults.api_url, None);

        let canonicalized = ComposerRepositoryFilterInformation::from_data_with(
            &json!({
                "summary-url": "/p2/filter-summary.json",
                "api-url": "/api/filter"
            }),
            |url| format!("https://example.org{url}"),
        );
        assert_eq!(
            canonicalized.summary_url.as_deref(),
            Some("https://example.org/p2/filter-summary.json")
        );
        assert_eq!(
            canonicalized.api_url.as_deref(),
            Some("https://example.org/api/filter")
        );
    }

    // Ported from FilterListEntryBuilderTest's matching/default-package methods.
    #[test]
    fn composer_filter_list_entry_builder_matches_constraints_and_package_precedence() {
        let builder = FilterListEntryBuilder;
        let package_versions = versions(&[("vendor/foo", &["1.2.3"])]);
        let result = builder.build(
            &json!({
                "malware": [
                    {"package": "vendor/foo", "constraint": "^1.0", "id": "A"},
                    {"package": "vendor/foo", "constraint": "^9.0", "id": "B"},
                    {"package": "vendor/unknown", "constraint": "*", "id": "C"}
                ]
            }),
            &package_versions,
            Some("vendor/bar"),
        );
        assert_eq!(result["malware"].len(), 1);
        assert_eq!(result["malware"][0].package_name, "vendor/foo");
        assert_eq!(result["malware"][0].id.as_deref(), Some("A"));

        let defaulted = builder.build(
            &json!({"malware": [{"constraint": "^1.0", "id": "PKFE-001"}]}),
            &package_versions,
            Some("vendor/foo"),
        );
        assert_eq!(defaulted["malware"][0].package_name, "vendor/foo");
    }

    // Ported from FilterListEntryBuilderTest's malformed/empty-input methods.
    #[test]
    fn composer_filter_list_entry_builder_ignores_malformed_shapes_and_empty_input() {
        let builder = FilterListEntryBuilder;
        let package_versions = versions(&[("vendor/foo", &["1.0.0"])]);
        let result = builder.build(
            &json!({
                "malware": "not-an-array",
                "typosquatting": [
                    "not-an-entry",
                    {"package": "vendor/foo", "constraint": "*"}
                ]
            }),
            &package_versions,
            None,
        );
        assert_eq!(result.keys().collect::<Vec<_>>(), ["typosquatting"]);
        assert_eq!(result["typosquatting"].len(), 1);
        assert!(builder
            .build(&json!({}), &package_versions, None)
            .is_empty());
    }

    // Ported from FilterListAuditorTest::testGetMatchingEntriesUnfilteredPackages.
    #[test]
    fn composer_filter_list_auditor_honors_package_ignore_rules_by_operation() {
        let package = FilterListPackage::new("acme/package", "1.0.0");
        let filter_map = BTreeMap::from([
            (
                "acme/package".to_owned(),
                BTreeMap::from([("list".to_owned(), vec![entry("list", "acme/package", "*")])]),
            ),
            (
                "acme/other".to_owned(),
                BTreeMap::from([("list".to_owned(), vec![entry("list", "acme/other", "*")])]),
            ),
        ]);
        let cases = [
            (FilterListIgnoreRule::new("acme/other"), 1),
            (FilterListIgnoreRule::new("acme/package"), 0),
            (FilterListIgnoreRule::new("acme/package").on_block(false), 1),
            (FilterListIgnoreRule::new("acme/package").on_audit(false), 0),
            (FilterListIgnoreRule::new("acme/*"), 0),
            (
                FilterListIgnoreRule::new("acme/package").with_constraint("1.0"),
                0,
            ),
            (
                FilterListIgnoreRule::new("acme/*").with_constraint("1.0"),
                0,
            ),
            (
                FilterListIgnoreRule::new("acme/package").with_constraint("1.1"),
                1,
            ),
            (
                FilterListIgnoreRule::new("acme/*").with_constraint("1.1"),
                1,
            ),
        ];
        for (rule, expected) in cases {
            let policies = BTreeMap::from([(
                "list".to_owned(),
                FilterListPolicy::default()
                    .block_for(FilterBlockScope::Update)
                    .ignore(rule),
            )]);
            assert_eq!(
                FilterListAuditor
                    .get_matching_entries(
                        &package,
                        &filter_map,
                        &policies,
                        FilterOperation::Block(FilterBlockScope::Update),
                    )
                    .len(),
                expected
            );
        }

        let multiple = BTreeMap::from([(
            "list".to_owned(),
            FilterListPolicy::default()
                .block_for(FilterBlockScope::Update)
                .ignore(FilterListIgnoreRule::new("acme/package").with_constraint("1.1"))
                .ignore(FilterListIgnoreRule::new("acme/package").with_constraint("1.0")),
        )]);
        assert!(FilterListAuditor
            .get_matching_entries(
                &package,
                &filter_map,
                &multiple,
                FilterOperation::Block(FilterBlockScope::Update),
            )
            .is_empty());

        let first_matches = BTreeMap::from([(
            "list".to_owned(),
            FilterListPolicy::default()
                .block_for(FilterBlockScope::Update)
                .ignore(FilterListIgnoreRule::new("acme/package").with_constraint("1.0"))
                .ignore(FilterListIgnoreRule::new("acme/package").with_constraint("1.1")),
        )]);
        assert!(FilterListAuditor
            .get_matching_entries(
                &package,
                &filter_map,
                &first_matches,
                FilterOperation::Block(FilterBlockScope::Update),
            )
            .is_empty());

        let none_matches = BTreeMap::from([(
            "list".to_owned(),
            FilterListPolicy::default()
                .block_for(FilterBlockScope::Update)
                .ignore(FilterListIgnoreRule::new("acme/package").with_constraint("1.1"))
                .ignore(FilterListIgnoreRule::new("acme/package").with_constraint("1.2")),
        )]);
        assert_eq!(
            FilterListAuditor
                .get_matching_entries(
                    &package,
                    &filter_map,
                    &none_matches,
                    FilterOperation::Block(FilterBlockScope::Update),
                )
                .len(),
            1
        );
    }

    // Ported from FilterListAuditorTest's ignore-source methods.
    #[test]
    fn composer_filter_list_auditor_ignores_only_configured_malware_sources() {
        let package = FilterListPackage::new("acme/package", "1.0.0");
        let mut untrusted = entry("malware", "acme/package", "*");
        untrusted.source = Some("untrusted".to_owned());
        let mut trusted = entry("malware", "acme/package", "*");
        trusted.source = Some("trusted".to_owned());
        let no_source = entry("malware", "acme/package", "*");
        let filter_map = BTreeMap::from([(
            "acme/package".to_owned(),
            BTreeMap::from([("malware".to_owned(), vec![untrusted, trusted, no_source])]),
        )]);
        let policies = BTreeMap::from([(
            "malware".to_owned(),
            FilterListPolicy::default()
                .audit(true)
                .block_for(FilterBlockScope::Update)
                .ignore_source("untrusted"),
        )]);

        for operation in [
            FilterOperation::Audit,
            FilterOperation::Block(FilterBlockScope::Update),
        ] {
            let matched =
                FilterListAuditor.get_matching_entries(&package, &filter_map, &policies, operation);
            assert_eq!(matched.len(), 2);
            assert!(matched
                .iter()
                .all(|entry| entry.source.as_deref() != Some("untrusted")));
        }

        let without_ignore = BTreeMap::from([(
            "malware".to_owned(),
            FilterListPolicy::default()
                .audit(true)
                .block_for(FilterBlockScope::Update),
        )]);
        assert_eq!(
            FilterListAuditor
                .get_matching_entries(
                    &package,
                    &filter_map,
                    &without_ignore,
                    FilterOperation::Audit,
                )
                .len(),
            3
        );
    }

    // Ported from FilterListAuditorTest's unconfigured-list methods.
    #[test]
    fn composer_filter_list_auditor_returns_only_entries_from_active_lists() {
        let package = FilterListPackage::new("acme/package", "1.0.0");
        let filter_map = BTreeMap::from([(
            "acme/package".to_owned(),
            BTreeMap::from([
                (
                    "configured".to_owned(),
                    vec![entry("configured", "acme/package", "*")],
                ),
                (
                    "unconfigured".to_owned(),
                    vec![entry("unconfigured", "acme/package", "*")],
                ),
            ]),
        )]);
        let policies = BTreeMap::from([(
            "configured".to_owned(),
            FilterListPolicy::default().block_for(FilterBlockScope::Update),
        )]);
        let matched = FilterListAuditor.get_matching_entries(
            &package,
            &filter_map,
            &policies,
            FilterOperation::Block(FilterBlockScope::Update),
        );
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].list_name, "configured");

        let only_unconfigured = BTreeMap::from([(
            "acme/package".to_owned(),
            BTreeMap::from([(
                "unconfigured".to_owned(),
                vec![entry("unconfigured", "acme/package", "*")],
            )]),
        )]);
        assert!(FilterListAuditor
            .get_matching_entries(
                &package,
                &only_unconfigured,
                &policies,
                FilterOperation::Block(FilterBlockScope::Update),
            )
            .is_empty());
    }

    struct StaticProvider {
        lists: Vec<String>,
        entries: FilterEntriesByList,
        construction_error: Option<String>,
        filter_error: Option<String>,
    }

    impl FilterListProvider for StaticProvider {
        fn has_filter(&self) -> Result<bool, FilterListProviderError> {
            if let Some(error) = &self.construction_error {
                Err(FilterListProviderError::new(error))
            } else {
                Ok(true)
            }
        }

        fn filter_lists(&self) -> Vec<String> {
            self.lists.clone()
        }

        fn filter(
            &self,
            _package_versions: &PackageVersions,
            _configured_lists: &[String],
        ) -> Result<FilterEntriesByList, FilterListProviderError> {
            if let Some(error) = &self.filter_error {
                Err(FilterListProviderError::new(error))
            } else {
                Ok(self.entries.clone())
            }
        }
    }

    // Ported from FilterListProviderSetTest::testGetMatchingFilterListsOnlyReturnsConfiguredLists.
    #[test]
    fn composer_filter_list_provider_set_returns_only_configured_matching_lists() {
        let mut malware = entry("malware", "acme/package", "1.0");
        malware.reason = Some("malware".to_owned());
        let mut typosquatting = entry("typosquatting", "acme/package", "1.0");
        typosquatting.reason = Some("typosquatting".to_owned());
        let provider = StaticProvider {
            lists: vec!["malware".to_owned(), "typosquatting".to_owned()],
            entries: BTreeMap::from([
                ("malware".to_owned(), vec![malware]),
                ("typosquatting".to_owned(), vec![typosquatting]),
            ]),
            construction_error: None,
            filter_error: None,
        };
        let set = FilterListProviderSet::new(vec![Box::new(provider)], vec![]);
        let result = set
            .get_matching_filter_lists(
                &[FilterListPackage::new("acme/package", "1.0.0")],
                &["malware".to_owned()],
                false,
            )
            .unwrap();
        assert_eq!(result.filter.keys().collect::<Vec<_>>(), ["malware"]);
        assert_eq!(
            result.filter["malware"][0].reason.as_deref(),
            Some("malware")
        );
    }

    fn unreachable_set() -> FilterListProviderSet {
        FilterListProviderSet::new(
            vec![Box::new(StaticProvider {
                lists: vec!["malware".to_owned()],
                entries: BTreeMap::new(),
                construction_error: Some("repo.example.com could not be reached".to_owned()),
                filter_error: None,
            })],
            vec![],
        )
    }

    // Ported from FilterListProviderSetTest's ignored-unreachable method.
    #[test]
    fn composer_filter_list_provider_set_reports_ignored_construction_failures() {
        let result = unreachable_set()
            .get_matching_filter_lists(
                &[FilterListPackage::new("acme/package", "1.0.0")],
                &["malware".to_owned()],
                true,
            )
            .unwrap();
        assert!(result.filter.is_empty());
        assert_eq!(
            result.unreachable_repositories,
            ["repo.example.com could not be reached"]
        );
    }

    // Ported from FilterListProviderSetTest's fatal-unreachable method.
    #[test]
    fn composer_filter_list_provider_set_propagates_construction_failures() {
        let error = unreachable_set()
            .get_matching_filter_lists(
                &[FilterListPackage::new("acme/package", "1.0.0")],
                &["malware".to_owned()],
                false,
            )
            .unwrap_err();
        assert_eq!(error.message(), "repo.example.com could not be reached");
    }

    #[test]
    fn composer_filter_list_pool_filter_collects_explicit_additional_sources() {
        let source = StaticProvider {
            lists: vec!["test-list".to_owned()],
            entries: BTreeMap::from([(
                "test-list".to_owned(),
                vec![entry("test-list", "acme/package", "3.0")],
            )]),
            construction_error: None,
            filter_error: None,
        };
        let set = FilterListProviderSet::new(vec![], vec![Box::new(source)]);
        let result = set
            .get_matching_filter_lists(
                &[
                    FilterListPackage::new("acme/package", "3.0.0"),
                    FilterListPackage::new("acme/package", "2.0.0"),
                    FilterListPackage::new("acme/other", "1.0.0"),
                ],
                &["test-list".to_owned()],
                false,
            )
            .unwrap();

        assert_eq!(result.filter["test-list"].len(), 1);
        assert_eq!(result.filter["test-list"][0].package_name, "acme/package");
        assert_eq!(result.filter["test-list"][0].constraint, "3.0");
    }

    fn runtime_unreachable_set() -> FilterListProviderSet {
        FilterListProviderSet::new(
            vec![Box::new(StaticProvider {
                lists: vec!["test-list".to_owned()],
                entries: BTreeMap::new(),
                construction_error: None,
                filter_error: Some("HTTP/1.1 500 Internal Server Error".to_owned()),
            })],
            vec![],
        )
    }

    #[test]
    fn composer_filter_list_pool_filter_reports_ignored_runtime_failures() {
        let result = runtime_unreachable_set()
            .get_matching_filter_lists(
                &[FilterListPackage::new("acme/package", "1.0.0")],
                &["test-list".to_owned()],
                true,
            )
            .unwrap();

        assert!(result.filter.is_empty());
        assert_eq!(
            result.unreachable_repositories,
            ["HTTP/1.1 500 Internal Server Error"]
        );
    }

    #[test]
    fn composer_filter_list_pool_filter_propagates_runtime_failures() {
        let error = runtime_unreachable_set()
            .get_matching_filter_lists(
                &[FilterListPackage::new("acme/package", "1.0.0")],
                &["test-list".to_owned()],
                false,
            )
            .unwrap_err();

        assert_eq!(error.message(), "HTTP/1.1 500 Internal Server Error");
    }

    // Ported from FilterListApiClientTest::testPostPurlsSendsPackagesAndListsAsBody.
    #[test]
    fn composer_filter_list_api_request_posts_ordered_purls_and_lists_as_json() {
        let request = FilterListApiRequest::post_purls(
            "https://example.org/api/filter",
            &["vendor/foo".to_owned(), "vendor/bar".to_owned()],
            &["malware".to_owned(), "typosquatting".to_owned()],
        )
        .unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.content_type, "application/json");
        assert_eq!(request.timeout_seconds, 10);
        assert_eq!(
            request.body,
            r#"{"packages":["pkg://composer/vendor/foo","pkg://composer/vendor/bar"],"lists":["malware","typosquatting"]}"#
        );
    }
}
