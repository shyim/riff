//! Root package version detection.
//!
//! This module handles detecting the version of the root package (the project itself)
//! based on git branch, branch-alias configuration, and environment variables.
//!
//! The priority order is:
//! 1. Explicit version in composer.json
//! 2. COMPOSER_ROOT_VERSION environment variable
//! 3. VCS version, including an exact tag on a detached HEAD
//! 4. Branch alias matching the detected branch version

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use regex::Regex;
use riff_semver::VersionParser;

pub const DEFAULT_ROOT_VERSION: &str = "1.0.0.0";
pub const DEFAULT_ROOT_PRETTY_VERSION: &str = "1.0.0+no-version-set";

/// Result of root version detection
#[derive(Debug, Clone)]
pub struct RootVersion {
    /// The normalized version string (e.g., "6.7.x-dev", "dev-trunk")
    pub version: String,
    /// The pretty version string for display
    pub pretty_version: String,
    /// How the version was determined
    pub source: RootVersionSource,
}

/// How the root version was determined
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootVersionSource {
    /// From COMPOSER_ROOT_VERSION environment variable
    Environment,
    /// From explicit version field in composer.json
    RiffManifest,
    /// From branch-alias matching the current git branch
    BranchAlias,
    /// From git branch name (converted to dev-* version)
    GitBranch,
    /// Default fallback when nothing else works
    Default,
}

/// Detailed VCS-derived package version information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionGuess {
    pub version: String,
    pub pretty_version: String,
    pub commit: Option<String>,
    pub feature_version: Option<String>,
    pub feature_pretty_version: Option<String>,
}

/// Controls feature-branch inheritance during VCS version guessing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionGuessOptions {
    pub infer_feature_version: bool,
    pub non_feature_branches: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionGuessCommandOutput {
    pub success: bool,
    pub stdout: String,
}

/// Injectable command boundary used by the version guesser.
pub trait VersionGuessProcess: Send + Sync {
    fn run(
        &self,
        working_dir: &Path,
        program: &str,
        arguments: &[&str],
    ) -> VersionGuessCommandOutput;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemVersionGuessProcess;

impl VersionGuessProcess for SystemVersionGuessProcess {
    fn run(
        &self,
        working_dir: &Path,
        program: &str,
        arguments: &[&str],
    ) -> VersionGuessCommandOutput {
        match Command::new(program)
            .args(arguments)
            .current_dir(working_dir)
            .output()
        {
            Ok(output) => VersionGuessCommandOutput {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            },
            Err(_) => VersionGuessCommandOutput {
                success: false,
                stdout: String::new(),
            },
        }
    }
}

/// Composer-compatible Git and Mercurial version guesser.
#[derive(Debug, Clone)]
pub struct VersionGuesser<P = SystemVersionGuessProcess> {
    process: P,
}

impl Default for VersionGuesser<SystemVersionGuessProcess> {
    fn default() -> Self {
        Self {
            process: SystemVersionGuessProcess,
        }
    }
}

impl VersionGuesser<SystemVersionGuessProcess> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<P: VersionGuessProcess> VersionGuesser<P> {
    pub fn with_process(process: P) -> Self {
        Self { process }
    }

    pub fn guess(&self, working_dir: &Path, options: &VersionGuessOptions) -> Option<VersionGuess> {
        self.guess_git(working_dir, options)
            .or_else(|| self.guess_hg(working_dir))
    }

    fn guess_git(&self, working_dir: &Path, options: &VersionGuessOptions) -> Option<VersionGuess> {
        let output = self.process.run(
            working_dir,
            "git",
            &["branch", "-a", "--no-color", "--no-abbrev", "-v"],
        );
        if !output.success {
            return None;
        }
        let branches = parse_git_branches(&output.stdout);
        let current = branches.iter().find(|branch| branch.current)?;

        if current.detached {
            let tag =
                self.process
                    .run(working_dir, "git", &["describe", "--exact-match", "--tags"]);
            if tag.success {
                let pretty = tag.stdout.trim();
                if !pretty.is_empty() {
                    if let Ok(version) = VersionParser::new().normalize(pretty) {
                        return Some(VersionGuess {
                            version,
                            pretty_version: pretty.to_owned(),
                            commit: Some(current.commit.clone()),
                            feature_version: None,
                            feature_pretty_version: None,
                        });
                    }
                }
            }
            let version = format!("dev-{}", current.commit);
            return Some(VersionGuess {
                pretty_version: version.clone(),
                version,
                commit: Some(current.commit.clone()),
                feature_version: None,
                feature_pretty_version: None,
            });
        }

        let version_branch = if options.infer_feature_version
            && is_feature_branch(&current.name, &options.non_feature_branches)
        {
            let candidates = branches
                .iter()
                .filter(|branch| {
                    !branch.current
                        && !branch.detached
                        && !is_feature_branch(&branch.name, &options.non_feature_branches)
                })
                .filter_map(|branch| {
                    let range = format!("{}..{}", branch.reference, current.reference);
                    let distance = self.process.run(working_dir, "git", &["rev-list", &range]);
                    distance.success.then(|| {
                        (
                            branch.name.clone(),
                            distance
                                .stdout
                                .lines()
                                .filter(|line| !line.trim().is_empty())
                                .count(),
                        )
                    })
                })
                .collect();
            nearest_version_branch(&current.name, candidates, &options.non_feature_branches)
        } else {
            current.name.clone()
        };
        let pretty_version = normalize_branch_to_dev(&version_branch);
        let (version, _) = normalize_version(&pretty_version);
        let (feature_version, feature_pretty_version) = if version_branch != current.name {
            let feature_pretty = normalize_branch_to_dev(&current.name);
            let (feature, _) = normalize_version(&feature_pretty);
            (Some(feature), Some(feature_pretty))
        } else {
            (None, None)
        };
        Some(VersionGuess {
            version,
            pretty_version,
            commit: Some(current.commit.clone()),
            feature_version,
            feature_pretty_version,
        })
    }

    fn guess_hg(&self, working_dir: &Path) -> Option<VersionGuess> {
        let output = self.process.run(working_dir, "hg", &["branch"]);
        let branch = output.success.then(|| output.stdout.trim().to_owned())?;
        if branch.is_empty() {
            return None;
        }
        let pretty_version = normalize_branch_to_dev(&branch);
        let (version, _) = normalize_version(&pretty_version);
        Some(VersionGuess {
            version,
            pretty_version,
            commit: None,
            feature_version: None,
            feature_pretty_version: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitBranchGuess {
    reference: String,
    name: String,
    commit: String,
    current: bool,
    detached: bool,
}

fn parse_git_branches(output: &str) -> Vec<GitBranchGuess> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (current, line) = line
                .strip_prefix("* ")
                .map_or((false, line), |line| (true, line));
            let (reference, remainder, detached) = if line.starts_with('(') {
                let end = line.find(") ")?;
                (&line[..=end], &line[end + 2..], true)
            } else {
                let (reference, remainder) = line.split_once(char::is_whitespace)?;
                (reference, remainder.trim_start(), false)
            };
            let commit = remainder.split_whitespace().next()?;
            if commit.is_empty() {
                return None;
            }
            let name = reference
                .strip_prefix("remotes/origin/")
                .or_else(|| reference.strip_prefix("remotes/upstream/"))
                .unwrap_or(reference)
                .to_owned();
            Some(GitBranchGuess {
                reference: reference.to_owned(),
                name,
                commit: commit.to_owned(),
                current,
                detached,
            })
        })
        .collect()
}

impl std::fmt::Display for RootVersionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RootVersionSource::Environment => write!(f, "COMPOSER_ROOT_VERSION env"),
            RootVersionSource::RiffManifest => write!(f, "composer.json version field"),
            RootVersionSource::BranchAlias => write!(f, "branch-alias"),
            RootVersionSource::GitBranch => write!(f, "git branch"),
            RootVersionSource::Default => write!(f, "default"),
        }
    }
}

/// Detects the root package version.
///
/// Priority order:
/// 1. COMPOSER_ROOT_VERSION environment variable
/// 2. Explicit version in composer.json
/// 3. Branch alias matching the current git branch
/// 4. Git branch name converted to a dev version
/// 5. Default "dev-main"
///
/// # Arguments
/// * `working_dir` - The project root directory (where composer.json is)
/// * `composer_version` - The version field from composer.json (if any)
/// * `branch_aliases` - Branch aliases from extra.branch-alias
pub fn detect_root_version(
    working_dir: &Path,
    composer_version: Option<&str>,
    branch_aliases: &HashMap<String, (String, String)>,
) -> RootVersion {
    detect_root_version_with_non_feature_branches(
        working_dir,
        composer_version,
        branch_aliases,
        &[],
    )
}

/// Detect the root version while honoring Composer's configurable list of
/// branches that should be treated as release lines rather than feature work.
pub fn detect_root_version_with_non_feature_branches(
    working_dir: &Path,
    composer_version: Option<&str>,
    branch_aliases: &HashMap<String, (String, String)>,
    non_feature_branches: &[String],
) -> RootVersion {
    detect_root_version_with_process(
        working_dir,
        composer_version,
        branch_aliases,
        non_feature_branches,
        SystemVersionGuessProcess,
    )
}

fn detect_root_version_with_process<P: VersionGuessProcess>(
    working_dir: &Path,
    composer_version: Option<&str>,
    branch_aliases: &HashMap<String, (String, String)>,
    non_feature_branches: &[String],
    process: P,
) -> RootVersion {
    // Composer only consults COMPOSER_ROOT_VERSION when composer.json does not
    // declare a version, so an explicit package version takes precedence.
    if let Some(explicit_version) = composer_version {
        let explicit_version = explicit_version.trim();
        if !explicit_version.is_empty() {
            let (version, pretty_version) = normalize_version(explicit_version);
            log::debug!(
                "Root version from composer.json: {} (normalized: {})",
                explicit_version,
                version
            );
            return RootVersion {
                version,
                pretty_version,
                source: RootVersionSource::RiffManifest,
            };
        }
    }

    if let Ok(env_version) = std::env::var("COMPOSER_ROOT_VERSION") {
        let env_version = env_version.trim();
        if !env_version.is_empty() {
            let (version, _) = normalize_version(env_version);
            let pretty_version = normalize_root_env_pretty_version(env_version);
            log::debug!(
                "Root version from COMPOSER_ROOT_VERSION: {} (normalized: {})",
                env_version,
                version
            );
            return RootVersion {
                version,
                pretty_version,
                source: RootVersionSource::Environment,
            };
        }
    }

    let guess_options = VersionGuessOptions {
        infer_feature_version: true,
        non_feature_branches: non_feature_branches.to_vec(),
    };
    if let Some(guess) = VersionGuesser::with_process(process).guess(working_dir, &guess_options) {
        if let Some((alias_normalized, alias_pretty)) = branch_aliases.get(&guess.pretty_version) {
            let (version, pretty_version) = normalize_version(alias_normalized);
            log::debug!(
                "Root version from branch-alias: {} -> {} (normalized: {}, pretty: {})",
                guess.pretty_version,
                alias_normalized,
                version,
                pretty_version
            );
            return RootVersion {
                version,
                pretty_version: alias_pretty.clone(),
                source: RootVersionSource::BranchAlias,
            };
        }

        log::debug!(
            "Root version from VCS: {} (normalized: {})",
            guess.pretty_version,
            guess.version
        );
        return RootVersion {
            version: guess.version,
            pretty_version: guess.pretty_version,
            source: RootVersionSource::GitBranch,
        };
    }

    // Default fallback. This intentionally remains visibly synthetic so it
    // is never mistaken for a declared or VCS-derived release.
    log::debug!("Root version defaulting to {DEFAULT_ROOT_PRETTY_VERSION}");
    RootVersion {
        version: DEFAULT_ROOT_VERSION.to_string(),
        pretty_version: DEFAULT_ROOT_PRETTY_VERSION.to_string(),
        source: RootVersionSource::Default,
    }
}

fn nearest_version_branch(
    current: &str,
    candidates: Vec<(String, usize)>,
    non_feature_branches: &[String],
) -> String {
    if !is_feature_branch(current, non_feature_branches) {
        return current.to_string();
    }

    candidates
        .into_iter()
        .filter(|(branch, _)| branch != current && !is_feature_branch(branch, non_feature_branches))
        .min_by(
            |(left_branch, left_distance), (right_branch, right_distance)| {
                left_distance
                    .cmp(right_distance)
                    .then_with(|| right_branch.cmp(left_branch))
            },
        )
        .map_or_else(|| current.to_string(), |(branch, _)| branch)
}

fn is_feature_branch(branch: &str, non_feature_branches: &[String]) -> bool {
    let built_in = matches!(
        branch,
        "master"
            | "main"
            | "latest"
            | "next"
            | "current"
            | "support"
            | "tip"
            | "trunk"
            | "default"
            | "develop"
    ) || branch.split_once('.').is_some_and(|(major, rest)| {
        !rest.is_empty() && major.bytes().all(|byte| byte.is_ascii_digit())
    });
    if built_in {
        return false;
    }
    !non_feature_branches.iter().any(|pattern| {
        Regex::new(&format!("^(?:{pattern})$")).is_ok_and(|pattern| pattern.is_match(branch))
    })
}

/// Gets the current git branch name.
///
/// Returns None if:
/// - Not in a git repository
/// - In detached HEAD state
/// - Unable to read git files
pub fn get_git_branch(path: &Path) -> Option<String> {
    let git_dir = path.join(".git");
    if !git_dir.exists() {
        return None;
    }

    let head_path = git_dir.join("HEAD");
    if !head_path.exists() {
        return None;
    }

    let head_content = std::fs::read_to_string(head_path).ok()?;
    let head = head_content.trim();

    // Check if it's a symbolic reference (normal branch)
    if let Some(stripped) = head.strip_prefix("ref: refs/heads/") {
        return Some(stripped.to_string());
    }

    // Detached HEAD - no branch name available
    // We could try to find a tag or use the commit hash, but for now return None
    None
}

/// Normalizes a branch name to a dev version string.
///
/// Examples:
/// - "main" -> "dev-main"
/// - "master" -> "dev-master"
/// - "trunk" -> "dev-trunk"
/// - "1.0" -> "1.0.x-dev"
/// - "feature/foo" -> "dev-feature/foo"
fn normalize_branch_to_dev(branch: &str) -> String {
    let branch = branch.trim();

    // Check if it already has dev- prefix
    if branch.starts_with("dev-") {
        return branch.to_string();
    }

    // Check if it looks like a version number
    if branch
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        // Numeric branch like "1.0" or "1.x" -> "1.0.x-dev" or "1.x-dev"
        let cleaned = branch.trim_end_matches(".x");
        return format!("{}.x-dev", cleaned);
    }

    // Regular branch name
    format!("dev-{}", branch)
}

/// Normalizes a version string using the semver parser.
///
/// Returns (normalized_version, pretty_version)
fn normalize_version(version: &str) -> (String, String) {
    let parser = VersionParser::new();

    match parser.normalize(version) {
        Ok(normalized) => (normalized, version.to_string()),
        Err(_) => {
            // If normalization fails, use the original
            (version.to_string(), version.to_string())
        }
    }
}

fn normalize_root_env_pretty_version(version: &str) -> String {
    let Some(branch) = version.strip_suffix("-dev") else {
        return version.to_string();
    };
    if branch.is_empty()
        || branch
            .bytes()
            .any(|byte| !byte.is_ascii_digit() && byte != b'.')
    {
        return version.to_string();
    }

    format!("{branch}.x-dev")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{environment_lock, EnvironmentGuard};
    use std::collections::BTreeMap;

    #[test]
    fn test_normalize_branch_to_dev() {
        assert_eq!(normalize_branch_to_dev("main"), "dev-main");
        assert_eq!(normalize_branch_to_dev("master"), "dev-master");
        assert_eq!(normalize_branch_to_dev("trunk"), "dev-trunk");
        assert_eq!(normalize_branch_to_dev("dev-main"), "dev-main");
        assert_eq!(normalize_branch_to_dev("1.0"), "1.0.x-dev");
        assert_eq!(normalize_branch_to_dev("1.x"), "1.x-dev");
        assert_eq!(normalize_branch_to_dev("2.0.x"), "2.0.x-dev");
        assert_eq!(normalize_branch_to_dev("feature/foo"), "dev-feature/foo");
    }

    #[test]
    fn test_detect_root_version_from_env() {
        let _guard = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_ROOT_VERSION", None);
        std::env::set_var("COMPOSER_ROOT_VERSION", "1.2.3");
        let result = detect_root_version(Path::new("/nonexistent"), None, &HashMap::new());

        assert_eq!(result.source, RootVersionSource::Environment);
        assert_eq!(result.pretty_version, "1.2.3");
    }

    #[test]
    fn composer_root_package_loader_prefers_an_explicit_version_over_the_environment() {
        let _guard = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_ROOT_VERSION", Some("9.9.9"));

        let result = detect_root_version(Path::new("/nonexistent"), Some("2.0.0"), &HashMap::new());

        assert_eq!(result.source, RootVersionSource::RiffManifest);
        assert_eq!(result.pretty_version, "2.0.0");
    }

    #[test]
    fn composer_version_guesser_normalizes_root_version_from_environment() {
        let _guard = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_ROOT_VERSION", None);
        for (environment, expected) in [
            ("1.0-dev", "1.0.x-dev"),
            ("1.0.x-dev", "1.0.x-dev"),
            ("1-dev", "1.x-dev"),
            ("1.x-dev", "1.x-dev"),
            ("1.0.0", "1.0.0"),
        ] {
            std::env::set_var("COMPOSER_ROOT_VERSION", environment);
            let root = detect_root_version(Path::new("/nonexistent"), None, &HashMap::new());

            assert_eq!(root.source, RootVersionSource::Environment);
            assert_eq!(root.pretty_version, expected);
        }
    }

    #[test]
    fn test_detect_root_version_from_manifest() {
        let _guard = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_ROOT_VERSION", None);
        let result = detect_root_version(Path::new("/nonexistent"), Some("2.0.0"), &HashMap::new());

        assert_eq!(result.source, RootVersionSource::RiffManifest);
        assert_eq!(result.pretty_version, "2.0.0");
    }

    #[test]
    fn composer_root_package_loader_preserves_version_branch_pretty_version() {
        let _guard = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_ROOT_VERSION", None);
        let result =
            detect_root_version(Path::new("/nonexistent"), Some("3.0-dev"), &HashMap::new());

        assert_eq!(result.source, RootVersionSource::RiffManifest);
        assert_eq!(result.pretty_version, "3.0-dev");
    }

    #[test]
    fn test_detect_root_version_default() {
        let _guard = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_ROOT_VERSION", None);
        let result = detect_root_version(Path::new("/nonexistent"), None, &HashMap::new());

        assert_eq!(result.source, RootVersionSource::Default);
        assert_eq!(result.version, DEFAULT_ROOT_VERSION);
        assert_eq!(result.pretty_version, DEFAULT_ROOT_PRETTY_VERSION);
    }

    #[test]
    fn test_root_version_source_display() {
        assert_eq!(
            RootVersionSource::Environment.to_string(),
            "COMPOSER_ROOT_VERSION env"
        );
        assert_eq!(
            RootVersionSource::RiffManifest.to_string(),
            "composer.json version field"
        );
        assert_eq!(RootVersionSource::BranchAlias.to_string(), "branch-alias");
        assert_eq!(RootVersionSource::GitBranch.to_string(), "git branch");
        assert_eq!(RootVersionSource::Default.to_string(), "default");
    }

    #[test]
    fn test_branch_alias_matching() {
        // Simulate Shopware's branch-alias setup:
        // "dev-master": "6.7.x-dev", "dev-trunk": "6.7.x-dev"
        let mut branch_aliases = HashMap::new();
        branch_aliases.insert(
            "dev-master".to_string(),
            ("6.7.x-dev".to_string(), "6.7.x-dev".to_string()),
        );
        branch_aliases.insert(
            "dev-trunk".to_string(),
            ("6.7.x-dev".to_string(), "6.7.x-dev".to_string()),
        );

        // Simulate being on "trunk" branch - should match "dev-trunk" alias
        // Since we can't create a real git repo in test, we test the normalization
        let normalized = normalize_branch_to_dev("trunk");
        assert_eq!(normalized, "dev-trunk");

        // Verify the alias lookup would work
        let alias = branch_aliases.get(&normalized);
        assert!(alias.is_some());
        let (version, pretty) = alias.unwrap();
        assert_eq!(version, "6.7.x-dev");
        assert_eq!(pretty, "6.7.x-dev");
    }

    #[test]
    fn test_numeric_branch_normalization() {
        // Test version-like branches (common in release branches)
        assert_eq!(normalize_branch_to_dev("6.7"), "6.7.x-dev");
        assert_eq!(normalize_branch_to_dev("1.0"), "1.0.x-dev");
        assert_eq!(normalize_branch_to_dev("2.x"), "2.x-dev");
    }

    #[test]
    fn composer_version_guesser_numeric_branches_show_nicely() {
        let _guard = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_ROOT_VERSION", None);
        let process = MockVersionGuessProcess::default().with_output(
            "git branch -a --no-color --no-abbrev -v",
            true,
            &format!("* 1.5 {FIRST_COMMIT} Commit message\n"),
        );

        let root = detect_root_version_with_process(
            Path::new("dummy/path"),
            None,
            &HashMap::new(),
            &[],
            process,
        );

        assert_eq!(root.source, RootVersionSource::GitBranch);
        assert_eq!(root.pretty_version, "1.5.x-dev");
        assert_eq!(root.version, "1.5.9999999.9999999-dev");
    }

    #[test]
    fn composer_version_guesser_invalid_tag_becomes_branch_version() {
        let _guard = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_ROOT_VERSION", None);
        let process = MockVersionGuessProcess::default().with_output(
            "git branch -a --no-color --no-abbrev -v",
            true,
            &format!("* foo {FIRST_COMMIT} Commit message\n"),
        );

        let root = detect_root_version_with_process(
            Path::new("dummy/path"),
            None,
            &HashMap::new(),
            &[],
            process,
        );

        assert_eq!(root.source, RootVersionSource::GitBranch);
        assert_eq!(root.version, "dev-foo");
        assert_eq!(root.pretty_version, "dev-foo");
    }

    // Ported from Composer\Test\Package\Loader\RootPackageLoaderTest::
    // testNoVersionIsVisibleInPrettyVersion.
    #[test]
    fn composer_root_package_loader_marks_missing_version_as_synthetic() {
        let _guard = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_ROOT_VERSION", None);

        let root = detect_root_version(Path::new("/nonexistent"), None, &HashMap::new());

        assert_eq!(root.version, "1.0.0.0");
        assert_eq!(root.pretty_version, "1.0.0+no-version-set");
    }

    // Ported from Composer\Test\Package\Loader\RootPackageLoaderTest::
    // testFeatureBranchPrettyVersion.
    #[test]
    fn composer_root_package_loader_uses_nearest_release_line_for_feature_branch() {
        let branch =
            nearest_version_branch("latest-production", vec![("master".to_string(), 0)], &[]);
        assert_eq!(normalize_branch_to_dev(&branch), "dev-master");
    }

    // Ported from Composer\Test\Package\Loader\RootPackageLoaderTest::
    // testNonFeatureBranchPrettyVersion.
    #[test]
    fn composer_root_package_loader_keeps_configured_non_feature_branch() {
        let branch = nearest_version_branch(
            "latest-production",
            vec![("master".to_string(), 0)],
            &["latest-.*".to_string()],
        );
        assert_eq!(normalize_branch_to_dev(&branch), "dev-latest-production");
    }

    #[derive(Default)]
    struct MockVersionGuessProcess {
        outputs: BTreeMap<String, VersionGuessCommandOutput>,
    }

    impl MockVersionGuessProcess {
        fn with_output(mut self, command: &str, success: bool, stdout: &str) -> Self {
            self.outputs.insert(
                command.to_owned(),
                VersionGuessCommandOutput {
                    success,
                    stdout: stdout.to_owned(),
                },
            );
            self
        }
    }

    impl VersionGuessProcess for MockVersionGuessProcess {
        fn run(
            &self,
            _working_dir: &Path,
            program: &str,
            arguments: &[&str],
        ) -> VersionGuessCommandOutput {
            let command = std::iter::once(program)
                .chain(arguments.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.outputs
                .get(&command)
                .cloned()
                .unwrap_or(VersionGuessCommandOutput {
                    success: false,
                    stdout: String::new(),
                })
        }
    }

    fn feature_options(patterns: &[&str]) -> VersionGuessOptions {
        VersionGuessOptions {
            infer_feature_version: true,
            non_feature_branches: patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
        }
    }

    const FIRST_COMMIT: &str = "03a15d220da53c52eddd5f32ffca64a7b3801bea";
    const SECOND_COMMIT: &str = "13a15d220da53c52eddd5f32ffca64a7b3801bea";

    // Ported from VersionGuesserTest::testHgGuessVersionReturnsData.
    #[test]
    fn composer_version_guesser_falls_back_to_the_mercurial_branch() {
        let process =
            MockVersionGuessProcess::default().with_output("hg branch", true, "default\n");
        let guess = VersionGuesser::with_process(process)
            .guess(Path::new("dummy/path"), &VersionGuessOptions::default())
            .unwrap();
        assert_eq!(guess.version, "dev-default");
        assert_eq!(guess.pretty_version, "dev-default");
        assert_eq!(guess.commit, None);
    }

    // Ported from VersionGuesserTest::testGuessVersionReturnsData.
    #[test]
    fn composer_version_guesser_returns_the_current_git_branch_and_commit() {
        let branches = format!(
            "* master {FIRST_COMMIT} Commit message\n  (no branch) {FIRST_COMMIT} Commit message\n"
        );
        let process = MockVersionGuessProcess::default().with_output(
            "git branch -a --no-color --no-abbrev -v",
            true,
            &branches,
        );
        let guess = VersionGuesser::with_process(process)
            .guess(Path::new("dummy/path"), &VersionGuessOptions::default())
            .unwrap();
        assert_eq!(guess.version, "dev-master");
        assert_eq!(guess.pretty_version, "dev-master");
        assert_eq!(guess.commit.as_deref(), Some(FIRST_COMMIT));
        assert_eq!(guess.feature_version, None);
        assert_eq!(guess.feature_pretty_version, None);
    }

    // Ported from VersionGuesserTest's custom-default-branch method.
    #[test]
    fn composer_version_guesser_does_not_infer_an_unconfigured_default_branch() {
        let branches = format!(
            "  arbitrary {FIRST_COMMIT} Commit message\n* current {SECOND_COMMIT} Another message\n"
        );
        let process = MockVersionGuessProcess::default().with_output(
            "git branch -a --no-color --no-abbrev -v",
            true,
            &branches,
        );
        let guess = VersionGuesser::with_process(process)
            .guess(Path::new("dummy/path"), &feature_options(&[]))
            .unwrap();
        assert_eq!(guess.version, "dev-current");
        assert_eq!(guess.commit.as_deref(), Some(SECOND_COMMIT));
        assert_eq!(guess.feature_version, None);
    }

    // Ported from VersionGuesserTest's literal non-feature-branch method.
    #[test]
    fn composer_version_guesser_inherits_a_configured_non_feature_branch() {
        let branches = format!(
            "  arbitrary {FIRST_COMMIT} Commit message\n* feature {SECOND_COMMIT} Another message\n"
        );
        let process = MockVersionGuessProcess::default()
            .with_output("git branch -a --no-color --no-abbrev -v", true, &branches)
            .with_output(
                "git rev-list arbitrary..feature",
                true,
                &format!("{SECOND_COMMIT}\n"),
            );
        let guess = VersionGuesser::with_process(process)
            .guess(Path::new("dummy/path"), &feature_options(&["arbitrary"]))
            .unwrap();
        assert_eq!(guess.version, "dev-arbitrary");
        assert_eq!(guess.pretty_version, "dev-arbitrary");
        assert_eq!(guess.commit.as_deref(), Some(SECOND_COMMIT));
        assert_eq!(guess.feature_version.as_deref(), Some("dev-feature"));
        assert_eq!(guess.feature_pretty_version.as_deref(), Some("dev-feature"));
    }

    // Ported from VersionGuesserTest's regex non-feature-branch method.
    #[test]
    fn composer_version_guesser_matches_configured_non_feature_branch_regexes() {
        let branches = format!(
            "  latest-testing {FIRST_COMMIT} Commit message\n* feature {SECOND_COMMIT} Another message\n"
        );
        let process = MockVersionGuessProcess::default()
            .with_output("git branch -a --no-color --no-abbrev -v", true, &branches)
            .with_output(
                "git rev-list latest-testing..feature",
                true,
                &format!("{SECOND_COMMIT}\n"),
            );
        let guess = VersionGuesser::with_process(process)
            .guess(Path::new("dummy/path"), &feature_options(&["latest-.*"]))
            .unwrap();
        assert_eq!(guess.version, "dev-latest-testing");
        assert_eq!(guess.feature_version.as_deref(), Some("dev-feature"));
        assert_eq!(guess.commit.as_deref(), Some(SECOND_COMMIT));
    }

    // Ported from VersionGuesserTest's current non-feature-branch method.
    #[test]
    fn composer_version_guesser_keeps_the_current_configured_non_feature_branch() {
        let branches = format!(
            "* latest-testing {FIRST_COMMIT} Commit message\n  current {SECOND_COMMIT} Another message\n  master {SECOND_COMMIT} Another message\n"
        );
        let process = MockVersionGuessProcess::default().with_output(
            "git branch -a --no-color --no-abbrev -v",
            true,
            &branches,
        );
        let guess = VersionGuesser::with_process(process)
            .guess(Path::new("dummy/path"), &feature_options(&["latest-.*"]))
            .unwrap();
        assert_eq!(guess.version, "dev-latest-testing");
        assert_eq!(guess.commit.as_deref(), Some(FIRST_COMMIT));
        assert_eq!(guess.feature_version, None);
        assert_eq!(guess.feature_pretty_version, None);
    }

    // Ported from VersionGuesserTest's three detached-HEAD methods.
    #[test]
    fn composer_version_guesser_uses_the_hash_for_detached_heads() {
        for description in [
            "(no branch)",
            "(HEAD detached at FETCH_HEAD)",
            "(HEAD detached at 03a15d220)",
        ] {
            let process = MockVersionGuessProcess::default().with_output(
                "git branch -a --no-color --no-abbrev -v",
                true,
                &format!("* {description} {FIRST_COMMIT} Commit message\n"),
            );
            let guess = VersionGuesser::with_process(process)
                .guess(Path::new("dummy/path"), &VersionGuessOptions::default())
                .unwrap();
            assert_eq!(guess.version, format!("dev-{FIRST_COMMIT}"));
            assert_eq!(guess.commit.as_deref(), Some(FIRST_COMMIT));
        }
    }

    // Ported from VersionGuesserTest's normalized/pretty tag methods.
    #[test]
    fn composer_version_guesser_normalizes_tags_and_preserves_the_pretty_tag() {
        for (tag, expected) in [("v2.0.5-alpha2", "2.0.5.0-alpha2"), ("1.0.0", "1.0.0.0")] {
            let process = MockVersionGuessProcess::default()
                .with_output(
                    "git branch -a --no-color --no-abbrev -v",
                    true,
                    &format!("* (HEAD detached at {tag}) {FIRST_COMMIT} Commit message\n"),
                )
                .with_output("git describe --exact-match --tags", true, tag);
            let guess = VersionGuesser::with_process(process)
                .guess(Path::new("dummy/path"), &VersionGuessOptions::default())
                .unwrap();
            assert_eq!(guess.version, expected);
            assert_eq!(guess.pretty_version, tag);
        }
    }

    #[test]
    fn composer_root_package_loader_uses_an_exact_tag_on_detached_head() {
        let _guard = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_ROOT_VERSION", None);
        let tag = "v6.7.13.1";
        let process = MockVersionGuessProcess::default()
            .with_output(
                "git branch -a --no-color --no-abbrev -v",
                true,
                &format!("* (HEAD detached at {tag}) {FIRST_COMMIT} Commit message\n"),
            )
            .with_output("git describe --exact-match --tags", true, tag);

        let root = detect_root_version_with_process(
            Path::new("dummy/path"),
            None,
            &HashMap::new(),
            &[],
            process,
        );

        assert_eq!(root.version, "6.7.13.1");
        assert_eq!(root.pretty_version, tag);
        assert_eq!(root.source, RootVersionSource::GitBranch);
    }

    // Ported from VersionGuesserTest::testRemoteBranchesAreSelected.
    #[test]
    fn composer_version_guesser_can_inherit_a_remote_numeric_branch() {
        let branches = format!(
            "* feature-branch {FIRST_COMMIT} Commit message\n  remotes/origin/1.5 {FIRST_COMMIT} Commit message\n"
        );
        let process = MockVersionGuessProcess::default()
            .with_output("git branch -a --no-color --no-abbrev -v", true, &branches)
            .with_output(
                "git rev-list remotes/origin/1.5..feature-branch",
                true,
                "\n",
            );
        let guess = VersionGuesser::with_process(process)
            .guess(Path::new("dummy/path"), &feature_options(&[]))
            .unwrap();
        assert_eq!(guess.pretty_version, "1.5.x-dev");
        assert_eq!(guess.version, "1.5.9999999.9999999-dev");
        assert_eq!(
            guess.feature_pretty_version.as_deref(),
            Some("dev-feature-branch")
        );
    }
}
