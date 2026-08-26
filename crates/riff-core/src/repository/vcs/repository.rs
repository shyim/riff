//! VCS Repository - discovers packages from version control systems.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use riff_semver::VersionParser;

use super::bitbucket::BitbucketDriver;
use super::cli::CliVcsDriver;
use super::driver::{normalize_branch, normalize_tag, VcsDriver, VcsDriverError};
use super::git::GitDriver;
use super::github::GitHubDriver;
use super::gitlab::GitLabDriver;
use crate::config::AuthConfig;
use crate::package::{Autoload, AutoloadPath, Dist, Package, Source};
use crate::repository::traits::{ProviderInfo, Repository, SearchMode, SearchResult};

/// Type of VCS driver to use
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcsType {
    /// Auto-detect driver
    Vcs,
    /// Git driver (uses git command line)
    Git,
    /// Mercurial driver
    Hg,
    /// Subversion driver
    Svn,
    /// Fossil driver
    Fossil,
    /// Perforce driver
    Perforce,
    /// GitHub driver (uses GitHub API)
    GitHub,
    /// GitLab driver (uses GitLab API)
    GitLab,
    /// Bitbucket driver
    Bitbucket,
}

impl VcsType {
    /// Parse from string
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "vcs" => Some(VcsType::Vcs),
            "git" => Some(VcsType::Git),
            "hg" | "mercurial" => Some(VcsType::Hg),
            "svn" => Some(VcsType::Svn),
            "fossil" => Some(VcsType::Fossil),
            "perforce" => Some(VcsType::Perforce),
            "github" => Some(VcsType::GitHub),
            "gitlab" => Some(VcsType::GitLab),
            "bitbucket" => Some(VcsType::Bitbucket),
            _ => None,
        }
    }
}

#[cfg(test)]
mod vcs_type_tests {
    use super::VcsType;

    #[test]
    fn composer_vcs_type_names_are_recognized() {
        assert_eq!(VcsType::from_str("hg"), Some(VcsType::Hg));
        assert_eq!(VcsType::from_str("mercurial"), Some(VcsType::Hg));
        assert_eq!(VcsType::from_str("svn"), Some(VcsType::Svn));
        assert_eq!(VcsType::from_str("fossil"), Some(VcsType::Fossil));
        assert_eq!(VcsType::from_str("perforce"), Some(VcsType::Perforce));
    }
}

/// Internal state for VcsRepository (protected by Mutex)
struct VcsRepositoryState {
    /// Discovered packages
    packages: Vec<Arc<Package>>,
    /// Whether packages have been loaded
    loaded: bool,
}

/// VCS repository - discovers packages from version control systems
pub struct VcsRepository {
    /// Repository name
    name: String,
    /// Repository URL
    url: String,
    /// VCS type
    vcs_type: VcsType,
    /// Authentication configuration
    auth: Option<AuthConfig>,
    /// Mutable state
    state: Mutex<VcsRepositoryState>,
}

impl std::fmt::Debug for VcsRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f.debug_struct("VcsRepository")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("vcs_type", &self.vcs_type)
            .field("packages", &state.packages.len())
            .field("loaded", &state.loaded)
            .finish()
    }
}

impl VcsRepository {
    /// Create a new VCS repository
    pub fn new(url: impl Into<String>, vcs_type: VcsType) -> Self {
        let url = url.into();
        let name = format!("vcs ({})", url);

        Self {
            name,
            url,
            vcs_type,
            auth: None,
            state: Mutex::new(VcsRepositoryState {
                packages: Vec::new(),
                loaded: false,
            }),
        }
    }

    /// Set authentication configuration
    pub fn with_auth(mut self, auth: AuthConfig) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Create appropriate driver for the URL and type
    fn create_driver(&self) -> Result<Box<dyn VcsDriver>, VcsDriverError> {
        let vcs_type = if self.vcs_type == VcsType::Vcs {
            self.detect_vcs_type()
        } else {
            self.vcs_type
        };

        match vcs_type {
            VcsType::GitHub => {
                let mut driver = GitHubDriver::new(&self.url)?;
                if let Some(ref auth) = self.auth {
                    driver = driver.with_auth(auth);
                }
                Ok(Box::new(driver))
            }
            VcsType::GitLab => {
                let mut driver = GitLabDriver::new(&self.url)?;
                if let Some(ref auth) = self.auth {
                    driver = driver.with_auth(auth);
                }
                Ok(Box::new(driver))
            }
            VcsType::Bitbucket => {
                let mut driver = BitbucketDriver::new(&self.url)?;
                if let Some(ref auth) = self.auth {
                    driver = driver.with_auth(auth);
                }
                Ok(Box::new(driver))
            }
            VcsType::Git | VcsType::Vcs => Ok(Box::new(GitDriver::new(&self.url))),
            VcsType::Hg | VcsType::Svn | VcsType::Fossil | VcsType::Perforce => {
                Ok(Box::new(CliVcsDriver::new(&self.url, vcs_type)?))
            }
        }
    }

    /// Detect VCS type from URL
    fn detect_vcs_type(&self) -> VcsType {
        let url_lower = self.url.to_lowercase();
        let path = std::path::Path::new(&self.url);

        if path.join(".hg").is_dir() || url_lower.starts_with("hg+") || url_lower.ends_with(".hg") {
            return VcsType::Hg;
        }
        if path.join(".svn").is_dir()
            || url_lower.starts_with("svn+")
            || url_lower.starts_with("svn://")
        {
            return VcsType::Svn;
        }
        if path.join(".fslckout").is_file()
            || path.join("_fossil_").is_file()
            || url_lower.ends_with(".fossil")
        {
            return VcsType::Fossil;
        }
        if url_lower.starts_with("p4://") || url_lower.starts_with("perforce://") {
            return VcsType::Perforce;
        }

        if url_lower.contains("github.com") {
            return VcsType::GitHub;
        }

        if url_lower.contains("gitlab.com") || url_lower.contains("gitlab") {
            return VcsType::GitLab;
        }

        if url_lower.contains("bitbucket.org") {
            return VcsType::Bitbucket;
        }

        VcsType::Git
    }

    /// Load packages from the VCS repository
    fn load_packages(&self) -> Result<(), VcsDriverError> {
        {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.loaded {
                return Ok(());
            }
        }

        let driver = self.create_driver()?;
        let root_identifier = driver.get_root_identifier().ok();
        let mut new_packages = Vec::new();

        if let Ok(tags) = driver.get_tags() {
            for (tag, identifier) in tags {
                if let Some(version) = normalize_tag(&tag) {
                    if let Ok(pkg) =
                        self.create_package_from_ref(&*driver, &tag, &identifier, &version, false)
                    {
                        new_packages.push(Arc::new(pkg));
                    }
                }
            }
        }

        if let Ok(branches) = driver.get_branches() {
            for (branch, identifier) in branches {
                let version = normalize_branch(&branch);
                if let Ok(mut pkg) =
                    self.create_package_from_ref(&*driver, &branch, &identifier, &version, true)
                {
                    if root_identifier.as_deref() == Some(branch.as_str()) {
                        pkg.default_branch = Some(true);
                    }
                    new_packages.push(Arc::new(pkg));
                }
            }
        }

        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.packages.extend(new_packages);
        state.loaded = true;
        Ok(())
    }

    /// Create a package from a VCS reference
    fn create_package_from_ref(
        &self,
        driver: &dyn VcsDriver,
        ref_name: &str,
        identifier: &str,
        version: &str,
        is_dev: bool,
    ) -> Result<Package, VcsDriverError> {
        let info = driver.get_composer_information(identifier)?;

        let json = info
            .manifest
            .ok_or_else(|| VcsDriverError::FileNotFound("composer.json".to_string()))?;

        let name = json.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            VcsDriverError::InvalidFormat("Missing 'name' in composer.json".to_string())
        })?;

        let pretty_version = if !is_dev {
            if let Some(declared_version) = json.get("version").and_then(|v| v.as_str()) {
                let parser = VersionParser::new();
                let declared_normalized = parser.normalize(declared_version).map_err(|error| {
                    VcsDriverError::InvalidFormat(format!(
                        "Invalid declared version '{declared_version}' at {ref_name}: {error}"
                    ))
                })?;
                let reference_normalized = parser.normalize(version).map_err(|error| {
                    VcsDriverError::InvalidFormat(format!(
                        "Invalid VCS version '{version}' for {ref_name}: {error}"
                    ))
                })?;
                if declared_normalized != reference_normalized {
                    return Err(VcsDriverError::InvalidFormat(format!(
                        "The declared version '{declared_version}' does not match VCS tag '{ref_name}'"
                    )));
                }
                declared_version.to_string()
            } else {
                version.to_string()
            }
        } else {
            version.to_string()
        };

        let normalized_version =
            VersionParser::new()
                .normalize(&pretty_version)
                .map_err(|error| {
                    VcsDriverError::InvalidFormat(format!(
                        "Invalid package version '{pretty_version}' at {ref_name}: {error}"
                    ))
                })?;
        let mut pkg = Package::new(name, normalized_version);
        pkg.pretty_version = Some(pretty_version.into());

        pkg.source = Some(Source::new(
            driver.get_vcs_type(),
            driver.get_url(),
            identifier,
        ));

        if self.detect_vcs_type() == VcsType::GitHub {
            if let Some((owner, repo)) = super::driver::parse_github_url(&self.url) {
                let dist_url = format!(
                    "https://api.github.com/repos/{}/{}/zipball/{}",
                    owner, repo, identifier
                );
                pkg.dist = Some(Dist::new("zip", &dist_url).with_reference(identifier));
            }
        }

        if let Some(time_str) = info.time {
            if let Ok(time) = DateTime::parse_from_rfc3339(&time_str) {
                pkg.time = Some(time.with_timezone(&Utc));
            }
        }

        if let Some(desc) = json.get("description").and_then(|v| v.as_str()) {
            pkg.description = Some(desc.to_string());
        }

        if let Some(t) = json.get("type").and_then(|v| v.as_str()) {
            pkg.package_type = t.into();
        }

        if let Some(license) = json.get("license") {
            pkg.license = parse_license(license).into_iter().map(Into::into).collect();
        }

        if let Some(require) = json.get("require").and_then(|v| v.as_object()) {
            pkg.require = require
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("*").to_string()))
                .collect();
        }

        if let Some(require_dev) = json.get("require-dev").and_then(|v| v.as_object()) {
            pkg.require_dev = require_dev
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("*").to_string()))
                .collect();
        }

        if let Some(autoload) = json.get("autoload") {
            pkg.autoload = Some(parse_autoload(autoload));
        }

        if let Some(autoload_dev) = json.get("autoload-dev") {
            pkg.autoload_dev = Some(parse_autoload(autoload_dev));
        }

        if let Some(bin) = json.get("bin").and_then(|v| v.as_array()) {
            pkg.bin = bin
                .iter()
                .filter_map(|v| v.as_str().map(Into::into))
                .collect();
        }

        Ok(pkg)
    }
}

#[async_trait]
impl Repository for VcsRepository {
    fn name(&self) -> &str {
        &self.name
    }

    async fn has_package(&self, name: &str) -> bool {
        !self.find_packages(name).await.is_empty()
    }

    async fn find_packages(&self, name: &str) -> Vec<Arc<Package>> {
        if let Err(error) = self.load_packages() {
            crate::warnln!("Warning: Failed to load {}: {}", self.name, error);
        }

        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .packages
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case(name))
            .cloned()
            .collect()
    }

    async fn find_package(&self, name: &str, version: &str) -> Option<Arc<Package>> {
        let packages = self.find_packages(name).await;
        packages.into_iter().find(|p| p.version == version)
    }

    async fn find_packages_with_constraint(
        &self,
        name: &str,
        _constraint: &str,
    ) -> Vec<Arc<Package>> {
        self.find_packages(name).await
    }

    async fn get_packages(&self) -> Vec<Arc<Package>> {
        if let Err(error) = self.load_packages() {
            crate::warnln!("Warning: Failed to load {}: {}", self.name, error);
        }

        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.packages.clone()
    }

    async fn search(&self, query: &str, _mode: SearchMode) -> Vec<SearchResult> {
        let packages = self.get_packages().await;

        packages
            .iter()
            .filter(|p| {
                p.name.contains(query)
                    || p.description
                        .as_ref()
                        .map(|d| d.contains(query))
                        .unwrap_or(false)
            })
            .map(|p| SearchResult {
                name: p.name.clone(),
                description: p.description.clone(),
                url: Some(self.url.clone()),
                abandoned: None,
                downloads: None,
                favers: None,
            })
            .collect()
    }

    async fn get_providers(&self, _package_name: &str) -> Vec<ProviderInfo> {
        Vec::new()
    }
}

/// Parse license from JSON value
fn parse_license(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(Into::into))
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse autoload from JSON value
fn parse_autoload(value: &serde_json::Value) -> Autoload {
    let mut autoload = Autoload::default();

    if let Some(psr4) = value.get("psr-4").and_then(|v| v.as_object()) {
        for (namespace, paths) in psr4 {
            let path = json_to_autoload_path(paths);
            autoload.psr4.insert(namespace.clone(), path);
        }
    }

    if let Some(psr0) = value.get("psr-0").and_then(|v| v.as_object()) {
        for (namespace, paths) in psr0 {
            let path = json_to_autoload_path(paths);
            autoload.psr0.insert(namespace.clone(), path);
        }
    }

    if let Some(classmap) = value.get("classmap").and_then(|v| v.as_array()) {
        autoload.classmap = classmap
            .iter()
            .filter_map(|v| v.as_str().map(Into::into))
            .collect();
    }

    if let Some(files) = value.get("files").and_then(|v| v.as_array()) {
        autoload.files = files
            .iter()
            .filter_map(|v| v.as_str().map(Into::into))
            .collect();
    }

    autoload
}

/// Convert JSON value to AutoloadPath
fn json_to_autoload_path(value: &serde_json::Value) -> AutoloadPath {
    match value {
        serde_json::Value::String(s) => AutoloadPath::Single(s.as_str().into()),
        serde_json::Value::Array(arr) => {
            let paths: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            if paths.len() == 1 {
                AutoloadPath::Single(paths[0].as_str().into())
            } else {
                AutoloadPath::Multiple(paths.into_iter().map(Into::into).collect())
            }
        }
        _ => AutoloadPath::Single("".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    fn git(repository: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(repository)
            .output()
            .expect("git must be available for VCS repository contract tests");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_manifest(repository: &Path, version: Option<&str>) {
        let mut manifest = serde_json::json!({"name": "a/b"});
        if let Some(version) = version {
            manifest["version"] = serde_json::Value::String(version.to_string());
        }
        fs::write(
            repository.join("composer.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn test_vcs_type_from_str() {
        assert_eq!(VcsType::from_str("vcs"), Some(VcsType::Vcs));
        assert_eq!(VcsType::from_str("git"), Some(VcsType::Git));
        assert_eq!(VcsType::from_str("github"), Some(VcsType::GitHub));
        assert_eq!(VcsType::from_str("gitlab"), Some(VcsType::GitLab));
        assert_eq!(VcsType::from_str("bitbucket"), Some(VcsType::Bitbucket));
        assert_eq!(VcsType::from_str("unknown"), None);
    }

    #[test]
    fn test_detect_vcs_type() {
        let repo = VcsRepository::new("https://github.com/owner/repo", VcsType::Vcs);
        assert_eq!(repo.detect_vcs_type(), VcsType::GitHub);

        let repo = VcsRepository::new("https://gitlab.com/owner/repo", VcsType::Vcs);
        assert_eq!(repo.detect_vcs_type(), VcsType::GitLab);

        let repo = VcsRepository::new("https://bitbucket.org/owner/repo", VcsType::Vcs);
        assert_eq!(repo.detect_vcs_type(), VcsType::Bitbucket);

        let repo = VcsRepository::new("https://example.com/repo.git", VcsType::Vcs);
        assert_eq!(repo.detect_vcs_type(), VcsType::Git);

        let repo = VcsRepository::new("https://example.com/repo.hg", VcsType::Vcs);
        assert_eq!(repo.detect_vcs_type(), VcsType::Hg);

        let repo = VcsRepository::new("svn://example.com/repo", VcsType::Vcs);
        assert_eq!(repo.detect_vcs_type(), VcsType::Svn);

        let repo = VcsRepository::new("https://example.com/repo.fossil", VcsType::Vcs);
        assert_eq!(repo.detect_vcs_type(), VcsType::Fossil);
    }

    #[tokio::test]
    async fn composer_vcs_repository_loads_versions_from_tags_and_branches() {
        let temporary = tempfile::tempdir().unwrap();
        let repository_path = temporary.path();
        git(repository_path, &["init", "-q"]);
        git(
            repository_path,
            &["symbolic-ref", "HEAD", "refs/heads/master"],
        );
        git(
            repository_path,
            &["config", "user.email", "composertest@example.org"],
        );
        git(repository_path, &["config", "user.name", "ComposerTest"]);
        git(repository_path, &["config", "commit.gpgsign", "false"]);

        fs::write(repository_path.join("foo"), "").unwrap();
        git(repository_path, &["add", "foo"]);
        git(repository_path, &["commit", "-q", "-m", "init"]);
        git(repository_path, &["tag", "0.5.0"]);
        git(repository_path, &["branch", "oldbranch"]);

        write_manifest(repository_path, None);
        git(repository_path, &["add", "composer.json"]);
        git(repository_path, &["commit", "-q", "-m", "addcomposer"]);
        git(repository_path, &["tag", "0.6.0"]);

        git(
            repository_path,
            &["checkout", "-q", "-b", "feature/a-1.0-B"],
        );
        fs::write(repository_path.join("foo"), "bar feature").unwrap();
        git(repository_path, &["add", "foo"]);
        git(repository_path, &["commit", "-q", "-m", "change-a"]);
        git(repository_path, &["branch", "foo#bar"]);

        git(repository_path, &["checkout", "-q", "master"]);
        write_manifest(repository_path, Some("1.0.0"));
        git(repository_path, &["add", "composer.json"]);
        git(repository_path, &["commit", "-q", "-m", "addversion"]);
        git(repository_path, &["tag", "0.9.0"]);
        git(repository_path, &["tag", "1.0.0"]);

        git(repository_path, &["checkout", "-q", "-b", "feature-b"]);
        fs::write(repository_path.join("foo"), "baz feature").unwrap();
        git(repository_path, &["add", "foo"]);
        git(repository_path, &["commit", "-q", "-m", "change-b"]);

        git(repository_path, &["checkout", "-q", "master"]);
        git(repository_path, &["branch", "1.0"]);
        git(repository_path, &["branch", "1.1.x"]);
        write_manifest(repository_path, Some("2.0.0"));
        git(repository_path, &["add", "composer.json"]);
        git(repository_path, &["commit", "-q", "-m", "bump-version"]);

        let repository =
            VcsRepository::new(repository_path.to_string_lossy().into_owned(), VcsType::Vcs);
        let packages = repository.get_packages().await;
        let mut actual: BTreeSet<String> = packages
            .iter()
            .map(|package| package.pretty_version().to_string())
            .collect();
        if packages
            .iter()
            .any(|package| package.default_branch == Some(true))
        {
            actual.insert(crate::package::DEFAULT_BRANCH_ALIAS.to_string());
        }

        assert_eq!(
            actual,
            BTreeSet::from([
                "0.6.0".to_string(),
                "1.0.0".to_string(),
                "1.0.x-dev".to_string(),
                "1.1.x-dev".to_string(),
                "9999999-dev".to_string(),
                "dev-feature-b".to_string(),
                "dev-feature/a-1.0-B".to_string(),
                "dev-foo+bar".to_string(),
                "dev-master".to_string(),
            ])
        );
        assert!(packages.iter().any(|package| {
            package.pretty_version() == "dev-master" && package.default_branch == Some(true)
        }));
    }
}
