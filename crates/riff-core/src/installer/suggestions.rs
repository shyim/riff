use std::collections::{BTreeMap, HashMap, HashSet};

use crate::package::Package;
use crate::util::canonical_package_name;

pub const MODE_LIST: u8 = 1;
pub const MODE_BY_PACKAGE: u8 = 2;
pub const MODE_BY_SUGGESTION: u8 = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuggestedPackage {
    pub source: String,
    pub target: String,
    pub reason: String,
}

/// Collects package suggestions independently of terminal I/O so commands can
/// render only the mode they need and tests can assert stable output.
#[derive(Clone, Debug, Default)]
pub struct SuggestedPackagesReporter {
    packages: Vec<SuggestedPackage>,
}

impl SuggestedPackagesReporter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn packages(&self) -> &[SuggestedPackage] {
        &self.packages
    }

    pub fn add_package(
        &mut self,
        source: impl Into<String>,
        target: impl Into<String>,
        reason: impl Into<String>,
    ) -> &mut Self {
        self.packages.push(SuggestedPackage {
            source: source.into(),
            target: target.into(),
            reason: reason.into(),
        });
        self
    }

    pub fn add_suggestions_from_package(&mut self, package: &Package) -> &mut Self {
        let source = package.pretty_name().to_string();
        for (target, reason) in &package.suggest {
            self.add_package(source.clone(), target.as_str(), reason.as_str());
        }
        self
    }

    /// Filter out suggestions fulfilled by another installed package. A
    /// polyfill that provides and suggests the same virtual package keeps its
    /// own suggestion, matching Composer's native-extension hint behavior.
    pub fn filtered<'a>(&'a self, installed: Option<&[Package]>) -> Vec<&'a SuggestedPackage> {
        if self.packages.is_empty() {
            return Vec::new();
        }

        let installed_names = installed.map(installed_name_providers).unwrap_or_default();
        self.packages
            .iter()
            .filter(|suggestion| {
                installed_names
                    .get(canonical_package_name(&suggestion.target).as_ref())
                    .is_none_or(|providers| {
                        providers.iter().all(|provider| {
                            provider == canonical_package_name(&suggestion.source).as_ref()
                        })
                    })
            })
            .collect()
    }

    /// Lazily acquire installed packages only when there are suggestions to
    /// filter. Repository-backed callers avoid an unnecessary query otherwise.
    pub fn render_with<F>(&self, mode: u8, installed: F) -> Vec<String>
    where
        F: FnOnce() -> Vec<Package>,
    {
        if self.packages.is_empty() {
            return Vec::new();
        }
        let installed = installed();
        self.render(mode, Some(&installed))
    }

    pub fn render(&self, mode: u8, installed: Option<&[Package]>) -> Vec<String> {
        let filtered = self.filtered(installed);
        let mut suggesters: BTreeMap<&str, BTreeMap<&str, &str>> = BTreeMap::new();
        let mut suggested: BTreeMap<&str, BTreeMap<&str, &str>> = BTreeMap::new();
        for suggestion in filtered {
            suggesters
                .entry(&suggestion.source)
                .or_default()
                .insert(&suggestion.target, &suggestion.reason);
            suggested
                .entry(&suggestion.target)
                .or_default()
                .insert(&suggestion.source, &suggestion.reason);
        }

        if mode & MODE_LIST != 0 {
            return suggested
                .keys()
                .map(|target| (*target).to_string())
                .collect();
        }

        let mut output = Vec::new();
        if mode & MODE_BY_PACKAGE != 0 {
            for (source, suggestions) in &suggesters {
                output.push(format!("{source} suggests:"));
                for (target, reason) in suggestions {
                    output.push(format_suggestion(target, reason));
                }
                output.push(String::new());
            }
        }

        if mode & MODE_BY_SUGGESTION != 0 {
            if mode & MODE_BY_PACKAGE != 0 {
                output.push("-".repeat(78));
            }
            for (target, sources) in &suggested {
                output.push(format!("{target} is suggested by:"));
                for (source, reason) in sources {
                    output.push(format_suggestion(source, reason));
                }
                output.push(String::new());
            }
        }
        output
    }
}

fn installed_name_providers(packages: &[Package]) -> HashMap<String, HashSet<String>> {
    packages
        .iter()
        .flat_map(|package| {
            package.get_names(true).into_iter().map(move |name| {
                (
                    canonical_package_name(&name).into_owned(),
                    canonical_package_name(&package.name).into_owned(),
                )
            })
        })
        .fold(HashMap::new(), |mut names, (provided, provider)| {
            names.entry(provided).or_default().insert(provider);
            names
        })
}

fn format_suggestion(name: &str, reason: &str) -> String {
    if reason.is_empty() {
        format!(" - {name}")
    } else {
        format!(" - {name}: {}", escape_reason(reason))
    }
}

fn escape_reason(reason: &str) -> String {
    reason
        .replace('\n', " ")
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suggestion() -> SuggestedPackage {
        SuggestedPackage {
            source: "a".to_string(),
            target: "b".to_string(),
            reason: "c".to_string(),
        }
    }

    // Ported from Composer\Test\Installer\SuggestedPackagesReporterTest::testConstructor.
    #[test]
    fn composer_suggested_packages_reporter_constructor_is_ready_to_render() {
        let mut reporter = SuggestedPackagesReporter::new();
        reporter.add_package("a", "b", "c");
        assert_eq!(reporter.render(MODE_LIST, None), ["b"]);
    }

    // Ported from Composer\Test\Installer\SuggestedPackagesReporterTest::
    // testGetPackagesEmptyByDefault.
    #[test]
    fn composer_suggested_packages_reporter_is_empty_by_default() {
        assert!(SuggestedPackagesReporter::new().packages().is_empty());
    }

    // Ported from Composer\Test\Installer\SuggestedPackagesReporterTest::testGetPackages.
    #[test]
    fn composer_suggested_packages_reporter_returns_added_packages() {
        let mut reporter = SuggestedPackagesReporter::new();
        reporter.add_package("a", "b", "c");
        assert_eq!(reporter.packages(), [suggestion()]);
    }

    // Ported from Composer\Test\Installer\SuggestedPackagesReporterTest::testAddPackageAppends.
    #[test]
    fn composer_suggested_packages_reporter_appends_duplicate_targets() {
        let mut reporter = SuggestedPackagesReporter::new();
        reporter.add_package("a", "b", "c").add_package(
            "different source",
            "b",
            "different reason",
        );
        assert_eq!(reporter.packages().len(), 2);
        assert_eq!(reporter.packages()[0], suggestion());
        assert_eq!(reporter.packages()[1].target, "b");
    }

    // Ported from Composer\Test\Installer\SuggestedPackagesReporterTest::
    // testAddSuggestionsFromPackage.
    #[test]
    fn composer_suggested_packages_reporter_adds_package_suggestions() {
        let mut package = Package::new("vendor/package", "1.0.0.0");
        package.pretty_name = Some("package-pretty-name".to_string());
        package.suggest = [("target-a", "reason-a"), ("target-b", "reason-b")]
            .into_iter()
            .collect();
        let mut reporter = SuggestedPackagesReporter::new();
        reporter.add_suggestions_from_package(&package);

        assert_eq!(reporter.packages().len(), 2);
        assert_eq!(reporter.packages()[0].source, "package-pretty-name");
        assert_eq!(reporter.packages()[1].target, "target-b");
    }

    // Ported from Composer\Test\Installer\SuggestedPackagesReporterTest::testOutput.
    #[test]
    fn composer_suggested_packages_reporter_outputs_by_package() {
        let mut reporter = SuggestedPackagesReporter::new();
        reporter.add_package("a", "b", "c");
        assert_eq!(
            reporter.render(MODE_BY_PACKAGE, None),
            ["a suggests:", " - b: c", ""]
        );
    }

    // Ported from Composer\Test\Installer\SuggestedPackagesReporterTest::
    // testOutputWithNoSuggestionReason.
    #[test]
    fn composer_suggested_packages_reporter_omits_empty_reason_separator() {
        let mut reporter = SuggestedPackagesReporter::new();
        reporter.add_package("a", "b", "");
        assert_eq!(
            reporter.render(MODE_BY_PACKAGE, None),
            ["a suggests:", " - b", ""]
        );
    }

    // Ported from Composer\Test\Installer\SuggestedPackagesReporterTest::
    // testOutputIgnoresFormatting.
    #[test]
    fn composer_suggested_packages_reporter_strips_control_characters() {
        let mut reporter = SuggestedPackagesReporter::new();
        reporter.add_package(
            "source",
            "target1",
            "\x1b[1;37;42m Like us\r\non Facebook \x1b[0m",
        );
        reporter.add_package("source", "target2", "<bg=green>Like us on Facebook</>");
        assert_eq!(
            reporter.render(MODE_BY_PACKAGE, None),
            [
                "source suggests:",
                " - target1: [1;37;42m Like us on Facebook [0m",
                " - target2: <bg=green>Like us on Facebook</>",
                "",
            ]
        );
    }

    // Ported from Composer\Test\Installer\SuggestedPackagesReporterTest::
    // testOutputMultiplePackages.
    #[test]
    fn composer_suggested_packages_reporter_outputs_multiple_sources() {
        let mut reporter = SuggestedPackagesReporter::new();
        reporter.add_package("a", "b", "c").add_package(
            "source package",
            "target",
            "because reasons",
        );
        assert_eq!(
            reporter.render(MODE_BY_PACKAGE, None),
            [
                "a suggests:",
                " - b: c",
                "",
                "source package suggests:",
                " - target: because reasons",
                "",
            ]
        );
    }

    // Ported from Composer\Test\Installer\SuggestedPackagesReporterTest::
    // testOutputSkipInstalledPackages.
    #[test]
    fn composer_suggested_packages_reporter_skips_installed_targets() {
        let mut provider = Package::new("vendor/package2", "1.0.0.0");
        provider.provide.insert("b".to_string(), "*".to_string());
        let mut reporter = SuggestedPackagesReporter::new();
        reporter.add_package("a", "b", "c").add_package(
            "source package",
            "target",
            "because reasons",
        );

        assert_eq!(
            reporter.render(MODE_BY_PACKAGE, Some(&[provider])),
            ["source package suggests:", " - target: because reasons", ""]
        );
    }

    // Ported from Composer\Test\Installer\SuggestedPackagesReporterTest::
    // testOutputShowsSuggestionProvidedBySuggestingPackageItself.
    #[test]
    fn composer_suggested_packages_reporter_keeps_self_provided_target() {
        let mut polyfill = Package::new("acme/polyfill-foo", "1.0.0.0");
        polyfill
            .provide
            .insert("ext-foo".to_string(), "*".to_string());
        let mut reporter = SuggestedPackagesReporter::new();
        reporter.add_package(
            "acme/polyfill-foo",
            "ext-foo",
            "install the native extension for better performance",
        );

        assert_eq!(
            reporter.render(MODE_BY_PACKAGE, Some(&[polyfill])),
            [
                "acme/polyfill-foo suggests:",
                " - ext-foo: install the native extension for better performance",
                "",
            ]
        );
    }

    // Ported from Composer\Test\Installer\SuggestedPackagesReporterTest::
    // testOutputNotGettingInstalledPackagesWhenNoSuggestions.
    #[test]
    fn composer_suggested_packages_reporter_does_not_load_installed_when_empty() {
        let reporter = SuggestedPackagesReporter::new();
        let output = reporter.render_with(MODE_BY_PACKAGE, || {
            panic!("installed repository must not be loaded")
        });
        assert!(output.is_empty());
    }
}
