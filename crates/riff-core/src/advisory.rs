use riff_semver::Semver;
use serde::{Deserialize, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};

/// Complete security advisory metadata used by Riff's audit command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditAdvisory {
    pub advisory_id: String,
    pub package_name: String,
    pub title: String,
    #[serde(default)]
    pub cve: Option<String>,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
    pub affected_versions: String,
    pub reported_at: String,
    #[serde(default)]
    pub sources: Vec<AuditAdvisorySource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditAdvisorySource {
    pub name: String,
    pub remote_id: String,
}

/// An advisory retained in the report because a configured rule ignored it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IgnoredAuditAdvisory {
    #[serde(flatten)]
    pub advisory: AuditAdvisory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore_reason: Option<String>,
}

/// A named repository or organization policy entry checked during an audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditFilterEntry {
    pub package_name: String,
    pub list_name: String,
    pub constraint: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AuditBehavior {
    #[default]
    Ignore,
    Report,
    Fail,
}

/// Evaluated audit findings. Ignored findings remain visible but never fail an audit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AuditReport {
    #[serde(serialize_with = "serialize_map_or_empty_array")]
    pub advisories: BTreeMap<String, Vec<AuditAdvisory>>,
    #[serde(
        rename = "ignored-advisories",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub ignored_advisories: BTreeMap<String, Vec<IgnoredAuditAdvisory>>,
    #[serde(serialize_with = "serialize_map_or_empty_array")]
    pub filter: BTreeMap<String, Vec<AuditFilterEntry>>,
    #[serde(
        rename = "unreachable-repositories",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub unreachable_repositories: Vec<String>,
    #[serde(skip)]
    failing_filter_lists: BTreeSet<String>,
    #[serde(skip)]
    failing_advisories: bool,
}

impl AuditReport {
    pub fn has_failing_findings(&self) -> bool {
        self.failing_advisories
            || self.filter.values().flatten().any(|entry| {
                self.failing_filter_lists
                    .iter()
                    .any(|list| list.eq_ignore_ascii_case(&entry.list_name))
            })
    }

    pub fn advisory_count(&self) -> usize {
        self.advisories.values().map(Vec::len).sum()
    }

    pub fn ignored_advisory_count(&self) -> usize {
        self.ignored_advisories.values().map(Vec::len).sum()
    }

    pub fn filter_summary(&self, summary_only: bool) -> Option<String> {
        if self.filter.is_empty() {
            return None;
        }
        Some(format!(
            "Found {} package{} matching filters{}",
            self.filter.len(),
            if self.filter.len() == 1 { "" } else { "s" },
            if summary_only { "." } else { ":" }
        ))
    }

    pub fn filter_diagnostics(&self) -> Vec<String> {
        self.filter
            .values()
            .flatten()
            .map(|entry| {
                let mut parts = vec![format!(
                    "{} matched dependency policy \"{}\"",
                    entry.package_name, entry.list_name
                )];
                if let Some(reason) = &entry.reason {
                    parts.push(format!("Reason: {reason}"));
                }
                if let Some(url) = &entry.url {
                    parts.push(format!("URL: {url}"));
                }
                if let Some(source) = &entry.source {
                    parts.push(format!("Source: {source}"));
                }
                format!("{}.", parts.join(". "))
            })
            .collect()
    }
}

fn serialize_map_or_empty_array<S, T>(
    value: &BTreeMap<String, T>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Serialize,
{
    if value.is_empty() {
        Vec::<serde_json::Value>::new().serialize(serializer)
    } else {
        value.serialize(serializer)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("audit repositories were unreachable: {repositories:?}")]
pub struct UnreachableAuditRepositories {
    repositories: Vec<String>,
}

impl UnreachableAuditRepositories {
    pub fn repositories(&self) -> &[String] {
        &self.repositories
    }
}

/// Reusable rules for turning raw advisory and filter metadata into an audit report.
#[derive(Debug, Clone)]
pub struct AdvisoryPolicy {
    ignored_advisories: BTreeMap<String, Option<String>>,
    ignored_severities: BTreeMap<String, Option<String>>,
    filter_behaviors: BTreeMap<String, AuditBehavior>,
    ignore_unreachable: bool,
    advisory_behavior: AuditBehavior,
}

impl Default for AdvisoryPolicy {
    fn default() -> Self {
        Self {
            ignored_advisories: BTreeMap::new(),
            ignored_severities: BTreeMap::new(),
            filter_behaviors: BTreeMap::new(),
            ignore_unreachable: false,
            advisory_behavior: AuditBehavior::Fail,
        }
    }
}

impl AdvisoryPolicy {
    pub fn ignore_advisory(
        mut self,
        identifier_or_package: impl Into<String>,
        reason: Option<String>,
    ) -> Self {
        self.ignored_advisories
            .insert(identifier_or_package.into(), reason);
        self
    }

    pub fn ignore_severity(mut self, severity: impl Into<String>, reason: Option<String>) -> Self {
        self.ignored_severities.insert(severity.into(), reason);
        self
    }

    pub fn filter_behavior(
        mut self,
        list_name: impl Into<String>,
        behavior: AuditBehavior,
    ) -> Self {
        self.filter_behaviors.insert(list_name.into(), behavior);
        self
    }

    pub fn advisory_behavior(mut self, behavior: AuditBehavior) -> Self {
        self.advisory_behavior = behavior;
        self
    }

    pub fn ignore_unreachable(mut self, ignore: bool) -> Self {
        self.ignore_unreachable = ignore;
        self
    }

    pub fn evaluate(
        &self,
        installed_versions: &BTreeMap<String, String>,
        advisories: impl IntoIterator<Item = AuditAdvisory>,
        filter_entries: impl IntoIterator<Item = AuditFilterEntry>,
        unreachable_repositories: Vec<String>,
    ) -> Result<AuditReport, UnreachableAuditRepositories> {
        if !self.ignore_unreachable && !unreachable_repositories.is_empty() {
            return Err(UnreachableAuditRepositories {
                repositories: unreachable_repositories,
            });
        }

        let mut report = AuditReport {
            unreachable_repositories,
            ..AuditReport::default()
        };

        for advisory in advisories {
            if self.advisory_behavior == AuditBehavior::Ignore {
                continue;
            }
            let Some(version) = installed_version(installed_versions, &advisory.package_name)
            else {
                continue;
            };
            if !Semver::satisfies(version, &advisory.affected_versions) {
                continue;
            }

            if let Some(reason) = self.ignore_reason(&advisory) {
                report
                    .ignored_advisories
                    .entry(advisory.package_name.clone())
                    .or_default()
                    .push(IgnoredAuditAdvisory {
                        advisory,
                        ignore_reason: reason,
                    });
            } else {
                if self.advisory_behavior == AuditBehavior::Fail {
                    report.failing_advisories = true;
                }
                report
                    .advisories
                    .entry(advisory.package_name.clone())
                    .or_default()
                    .push(advisory);
            }
        }

        for entry in filter_entries {
            let behavior = self
                .filter_behaviors
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&entry.list_name))
                .map(|(_, behavior)| *behavior)
                .unwrap_or(AuditBehavior::Ignore);
            if behavior == AuditBehavior::Ignore {
                continue;
            }

            for (package, version) in installed_versions {
                if !package_pattern_matches(&entry.package_name, package)
                    || !Semver::satisfies(version, &entry.constraint)
                {
                    continue;
                }

                let mut matched = entry.clone();
                matched.package_name.clone_from(package);
                report
                    .filter
                    .entry(package.clone())
                    .or_default()
                    .push(matched);
                if behavior == AuditBehavior::Fail {
                    report.failing_filter_lists.insert(entry.list_name.clone());
                }
            }
        }

        Ok(report)
    }

    /// Returns `Some(reason)` for an ignored advisory and `None` for an active one.
    /// The nested option preserves an ignore rule without an explanatory reason.
    fn ignore_reason(&self, advisory: &AuditAdvisory) -> Option<Option<String>> {
        let mut matched = None;

        for (pattern, reason) in &self.ignored_advisories {
            if (pattern.contains('/') || pattern.contains('*'))
                && package_pattern_matches(pattern, &advisory.package_name)
            {
                matched = Some(reason.clone());
            }
        }
        if let Some(reason) = find_case_insensitive(&self.ignored_advisories, &advisory.advisory_id)
        {
            matched = Some(reason.clone());
        }
        if let Some(severity) = &advisory.severity {
            if let Some(reason) = find_case_insensitive(&self.ignored_severities, severity) {
                matched =
                    Some(Some(reason.clone().unwrap_or_else(|| {
                        format!("{severity} severity is ignored")
                    })));
            }
        }
        if let Some(cve) = &advisory.cve {
            if let Some(reason) = find_case_insensitive(&self.ignored_advisories, cve) {
                matched = Some(reason.clone());
            }
        }
        for source in &advisory.sources {
            if let Some(reason) = find_case_insensitive(&self.ignored_advisories, &source.remote_id)
            {
                matched = Some(reason.clone());
                break;
            }
        }

        matched
    }
}

fn installed_version<'a>(
    installed_versions: &'a BTreeMap<String, String>,
    package_name: &str,
) -> Option<&'a str> {
    installed_versions
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(package_name))
        .map(|(_, version)| version.as_str())
}

fn find_case_insensitive<'a, T>(values: &'a BTreeMap<String, T>, needle: &str) -> Option<&'a T> {
    values
        .iter()
        .find(|(value, _)| value.eq_ignore_ascii_case(needle))
        .map(|(_, item)| item)
}

fn package_pattern_matches(pattern: &str, package: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let package = package.to_ascii_lowercase();
    let (mut pattern_index, mut package_index) = (0usize, 0usize);
    let (mut wildcard, mut retry) = (None, 0usize);
    let pattern = pattern.as_bytes();
    let package = package.as_bytes();

    while package_index < package.len() {
        if pattern.get(pattern_index) == package.get(package_index) {
            pattern_index += 1;
            package_index += 1;
        } else if pattern.get(pattern_index) == Some(&b'*') {
            wildcard = Some(pattern_index);
            pattern_index += 1;
            retry = package_index;
        } else if let Some(index) = wildcard {
            pattern_index = index + 1;
            retry += 1;
            package_index = retry;
        } else {
            return false;
        }
    }

    pattern[pattern_index..].iter().all(|byte| *byte == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advisory(id: &str, package: &str, severity: &str) -> AuditAdvisory {
        AuditAdvisory {
            advisory_id: id.into(),
            package_name: package.into(),
            title: format!("advisory {id}"),
            cve: Some(format!("CVE-{id}")),
            link: Some(format!("https://example.com/{id}")),
            severity: Some(severity.into()),
            affected_versions: ">=1,<4".into(),
            reported_at: "2022-05-25T13:21:00+00:00".into(),
            sources: vec![AuditAdvisorySource {
                name: "test".into(),
                remote_id: format!("REMOTE-{id}"),
            }],
        }
    }

    fn installed(packages: &[(&str, &str)]) -> BTreeMap<String, String> {
        packages
            .iter()
            .map(|(name, version)| ((*name).into(), (*version).into()))
            .collect()
    }

    #[test]
    fn composer_auditor_applies_id_package_cve_and_remote_ignore_rules() {
        for (rule, reason) in [
            ("ID1", None),
            ("CVE-ID1", Some("known safe".to_string())),
            ("REMOTE-ID1", None),
            ("vendor/package", Some("safe usage".to_string())),
            ("vendor/*", Some("trusted vendor".to_string())),
        ] {
            let report = AdvisoryPolicy::default()
                .ignore_advisory(rule, reason.clone())
                .evaluate(
                    &installed(&[("vendor/package", "3.0.0")]),
                    [advisory("ID1", "vendor/package", "medium")],
                    [],
                    vec![],
                )
                .unwrap();

            assert!(report.advisories.is_empty(), "rule={rule}");
            assert_eq!(report.ignored_advisory_count(), 1, "rule={rule}");
            assert_eq!(
                report.ignored_advisories["vendor/package"][0].ignore_reason, reason,
                "rule={rule}"
            );
            assert!(!report.has_failing_findings(), "rule={rule}");
            let json = serde_json::to_value(&report).unwrap();
            assert_eq!(
                json["ignored-advisories"]["vendor/package"]
                    .as_array()
                    .unwrap()
                    .len(),
                1,
                "rule={rule}"
            );
            if let Some(reason) = reason {
                assert_eq!(
                    json["ignored-advisories"]["vendor/package"][0]["ignoreReason"], reason,
                    "rule={rule}"
                );
            } else {
                assert!(json["ignored-advisories"]["vendor/package"][0]
                    .get("ignoreReason")
                    .is_none());
            }
        }

        let report = AdvisoryPolicy::default()
            .ignore_advisory("ID1", None)
            .evaluate(
                &installed(&[("vendor/package", "3.0.0")]),
                [
                    advisory("ID1", "vendor/package", "medium"),
                    advisory("ID2", "vendor/package", "medium"),
                ],
                [],
                vec![],
            )
            .unwrap();
        assert_eq!(report.advisory_count(), 1);
        assert_eq!(report.ignored_advisory_count(), 1);
        assert!(report.has_failing_findings());
    }

    #[test]
    fn composer_auditor_ignores_configured_severities_without_hiding_findings() {
        let findings = || {
            [
                advisory("ID1", "vendor/package", "medium"),
                advisory("ID2", "vendor/package", "medium"),
                advisory("ID3", "vendor/package", "high"),
            ]
        };
        for (severities, active, ignored) in [
            (vec!["medium"], 1, 2),
            (vec!["high"], 2, 1),
            (vec!["high", "medium"], 0, 3),
        ] {
            let policy = severities
                .iter()
                .fold(AdvisoryPolicy::default(), |policy, severity| {
                    policy.ignore_severity(*severity, None)
                });
            let report = policy
                .evaluate(
                    &installed(&[("vendor/package", "2.0.0")]),
                    findings(),
                    [],
                    vec![],
                )
                .unwrap();

            assert_eq!(report.advisory_count(), active);
            assert_eq!(report.ignored_advisory_count(), ignored);
            assert_eq!(report.has_failing_findings(), active > 0);
            for finding in report.ignored_advisories.values().flatten() {
                assert!(finding
                    .ignore_reason
                    .as_deref()
                    .is_some_and(|reason| { reason.ends_with(" severity is ignored") }));
            }
        }
    }

    #[test]
    fn composer_auditor_preserves_reachable_findings_and_unreachable_errors() {
        let error = "HTTP/1.1 404 Not Found".to_string();
        let packages = installed(&[("vendor/package", "3.0.0")]);
        let findings = || {
            [
                advisory("ID1", "vendor/package", "medium"),
                advisory("ID2", "vendor/package", "high"),
            ]
        };

        let failure = AdvisoryPolicy::default()
            .evaluate(&packages, findings(), [], vec![error.clone()])
            .unwrap_err();
        assert_eq!(failure.repositories(), &[error]);

        let report = AdvisoryPolicy::default()
            .ignore_unreachable(true)
            .evaluate(
                &packages,
                findings(),
                [],
                vec!["HTTP/1.1 404 Not Found".into()],
            )
            .unwrap();
        assert_eq!(report.advisory_count(), 2);
        assert!(report.has_failing_findings());

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(
            json["unreachable-repositories"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            json["advisories"]["vendor/package"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn advisory_audit_behavior_controls_visibility_and_exit_status() {
        let versions = installed(&[("vendor/package", "1.0.0")]);
        let report = AdvisoryPolicy::default()
            .advisory_behavior(AuditBehavior::Report)
            .evaluate(
                &versions,
                [advisory("REPORT", "vendor/package", "high")],
                [],
                Vec::new(),
            )
            .unwrap();
        assert_eq!(report.advisory_count(), 1);
        assert!(!report.has_failing_findings());

        let report = AdvisoryPolicy::default()
            .advisory_behavior(AuditBehavior::Ignore)
            .evaluate(
                &versions,
                [advisory("IGNORE", "vendor/package", "high")],
                [],
                Vec::new(),
            )
            .unwrap();
        assert_eq!(report.advisory_count(), 0);
        assert!(!report.has_failing_findings());
    }

    fn filter(constraint: &str) -> AuditFilterEntry {
        AuditFilterEntry {
            package_name: "vendor/package".into(),
            list_name: "test-list".into(),
            constraint: constraint.into(),
            url: Some("https://example.com/filtered".into()),
            reason: Some("internal".into()),
            id: Some("ID-test-1".into()),
            source: Some("aikido".into()),
        }
    }

    #[test]
    fn composer_auditor_applies_versioned_filter_behaviors() {
        for (behavior, constraint, expected_count, fails) in [
            (AuditBehavior::Ignore, ">=8", 0, false),
            (AuditBehavior::Fail, ">=10", 0, false),
            (AuditBehavior::Fail, ">=8", 1, true),
            (AuditBehavior::Report, ">=8", 1, false),
        ] {
            let report = AdvisoryPolicy::default()
                .filter_behavior("test-list", behavior)
                .evaluate(
                    &installed(&[("vendor/package", "9.0.0")]),
                    [],
                    [filter(constraint)],
                    vec![],
                )
                .unwrap();
            assert_eq!(
                report.filter.values().map(Vec::len).sum::<usize>(),
                expected_count
            );
            assert_eq!(report.has_failing_findings(), fails);
            if expected_count == 1 {
                assert_eq!(
                    report.filter_summary(false).as_deref(),
                    Some("Found 1 package matching filters:")
                );
                assert_eq!(
                    report.filter_summary(true).as_deref(),
                    Some("Found 1 package matching filters.")
                );
                assert_eq!(
                    report.filter_diagnostics(),
                    vec!["vendor/package matched dependency policy \"test-list\". Reason: internal. URL: https://example.com/filtered. Source: aikido."]
                );
            }
        }

        let report = AdvisoryPolicy::default()
            .filter_behavior("test-list", AuditBehavior::Fail)
            .evaluate(
                &installed(&[("vendor/package", "9.0.0"), ("vendor/other", "1.0.0")]),
                [],
                [
                    filter(">=8"),
                    AuditFilterEntry {
                        package_name: "vendor/other".into(),
                        constraint: ">=1".into(),
                        ..filter(">=8")
                    },
                ],
                vec![],
            )
            .unwrap();
        assert_eq!(report.filter.len(), 2);
        assert!(report.has_failing_findings());
        assert_eq!(
            report.filter_summary(false).as_deref(),
            Some("Found 2 packages matching filters:")
        );
    }

    #[test]
    fn composer_auditor_serializes_filter_findings_with_composer_field_names() {
        let report = AdvisoryPolicy::default()
            .filter_behavior("test-list", AuditBehavior::Fail)
            .evaluate(
                &installed(&[("vendor/package", "9.0.0")]),
                [],
                [filter(">=8")],
                vec![],
            )
            .unwrap();
        let json = serde_json::to_value(&report).unwrap();
        let finding = &json["filter"]["vendor/package"][0];

        assert_eq!(json["advisories"], serde_json::json!([]));
        assert_eq!(finding["packageName"], "vendor/package");
        assert_eq!(finding["listName"], "test-list");
        assert_eq!(finding["constraint"], ">=8");
        assert_eq!(finding["url"], "https://example.com/filtered");
        assert_eq!(finding["reason"], "internal");
        assert_eq!(finding["source"], "aikido");
    }

    #[test]
    fn composer_auditor_reports_filters_and_vulnerabilities_together() {
        let report = AdvisoryPolicy::default()
            .filter_behavior("test-list", AuditBehavior::Fail)
            .evaluate(
                &installed(&[("vendor/vulnerable", "2.0.0"), ("vendor/package", "9.0.0")]),
                [advisory("ID1", "vendor/vulnerable", "high")],
                [filter(">=8")],
                vec![],
            )
            .unwrap();

        assert_eq!(report.advisory_count(), 1);
        assert_eq!(report.filter.len(), 1);
        assert!(report.has_failing_findings());
    }

    #[test]
    fn affected_versions_are_checked_before_ignore_rules() {
        let mut unaffected = advisory("ID1", "vendor/package", "high");
        unaffected.affected_versions = ">=4".into();
        let report = AdvisoryPolicy::default()
            .ignore_advisory("ID1", None)
            .evaluate(
                &installed(&[("vendor/package", "3.0.0")]),
                [unaffected],
                [],
                vec![],
            )
            .unwrap();

        assert_eq!(report.advisory_count(), 0);
        assert_eq!(report.ignored_advisory_count(), 0);
    }
}
