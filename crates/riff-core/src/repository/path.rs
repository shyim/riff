//! Path repository - loads packages from local filesystem paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use glob::glob;
use sha1::{Digest, Sha1};
use tokio::sync::RwLock;

use super::traits::{ProviderInfo, Repository, SearchMode, SearchResult};
use crate::package::{Autoload, AutoloadPath, Dist, Package, Source};
use crate::util::expand_path;

/// Options for path repository
#[derive(Debug, Clone, Default)]
pub struct PathRepositoryOptions {
    /// Force symlink (true) or mirror (false), or auto (None)
    pub symlink: Option<bool>,
    /// Keep paths as relative
    pub relative: bool,
    /// Reference mode: "none", "config", or "auto"
    pub reference: String,
    /// Override versions for packages
    pub versions: HashMap<String, String>,
}

/// Path repository - loads packages from local filesystem
pub struct PathRepository {
    /// Repository name
    name: String,
    /// Base URL/path pattern (may contain glob patterns)
    url: String,
    /// Resolved absolute path
    resolved_path: PathBuf,
    /// Project directory used to resolve relative repository URLs
    base_dir: PathBuf,
    /// Repository options
    options: PathRepositoryOptions,
    /// Cached packages (with interior mutability for lazy loading)
    packages: RwLock<Option<Vec<Arc<Package>>>>,
}

impl PathRepository {
    /// Create a new path repository
    ///
    /// # Arguments
    /// * `url` - Path to the package(s), can contain glob patterns like `packages/*`
    /// * `options` - Repository options
    pub fn new(url: impl Into<String>, options: PathRepositoryOptions) -> Self {
        let base_dir = std::env::current_dir().unwrap_or_default();
        Self::new_with_base(url, options, base_dir)
    }

    /// Create a path repository resolving relative URLs from a project directory.
    pub fn new_with_base(
        url: impl Into<String>,
        options: PathRepositoryOptions,
        base_dir: impl AsRef<Path>,
    ) -> Self {
        let url = url.into();

        let expanded = expand_path(&url);

        // Resolve to absolute path
        let resolved_path = if Path::new(&expanded).is_absolute() {
            PathBuf::from(&expanded)
        } else {
            base_dir.as_ref().join(&expanded)
        };

        Self {
            name: format!("path repo ({})", url),
            url,
            resolved_path,
            base_dir: base_dir.as_ref().to_path_buf(),
            options,
            packages: RwLock::new(None),
        }
    }

    /// Create a path repository with default options
    pub fn from_path(url: impl Into<String>) -> Self {
        Self::new(
            url,
            PathRepositoryOptions {
                reference: "auto".to_string(),
                ..Default::default()
            },
        )
    }

    /// Get all matching paths (handles glob patterns)
    fn get_url_matches(&self) -> Vec<PathBuf> {
        let path_str = self.resolved_path.to_string_lossy();

        // Check if path contains glob patterns
        if path_str.contains('*') || path_str.contains('?') || path_str.contains('[') {
            match glob(&path_str) {
                Ok(paths) => paths
                    .filter_map(|p| p.ok())
                    .filter(|p| p.is_dir())
                    .collect(),
                Err(_) => Vec::new(),
            }
        } else if self.resolved_path.is_dir() {
            vec![self.resolved_path.clone()]
        } else {
            Vec::new()
        }
    }

    fn validate_url(&self) -> Result<(), String> {
        if !self.get_url_matches().is_empty() {
            return Ok(());
        }

        let has_wildcard = self
            .resolved_path
            .to_string_lossy()
            .chars()
            .any(|character| matches!(character, '*' | '?' | '[' | '{'));
        if has_wildcard {
            let mut parent = self.resolved_path.as_path();
            while parent
                .to_string_lossy()
                .chars()
                .any(|character| matches!(character, '*' | '?' | '[' | '{'))
            {
                let Some(next) = parent.parent() else {
                    break;
                };
                parent = next;
            }
            // Composer permits an empty wildcard repository when the fixed
            // parent path exists, as packages may be added there later.
            if parent.is_dir() {
                return Ok(());
            }
        }

        Err(format!(
            "The `url` supplied for the path ({}) repository does not exist",
            self.url
        ))
    }

    /// Load all packages while reporting an invalid repository path.
    ///
    /// The generic [`Repository`] API predates fallible repository loading and
    /// therefore returns an empty list for load failures. Callers which need to
    /// distinguish an empty repository from a mistyped path should use this
    /// method.
    pub async fn try_get_packages(&self) -> Result<Vec<Arc<Package>>, String> {
        self.validate_url()?;
        Ok(self.ensure_loaded().await)
    }

    /// Load packages from all matching paths
    async fn ensure_loaded(&self) -> Vec<Arc<Package>> {
        // Check if already loaded
        {
            let guard = self.packages.read().await;
            if let Some(ref pkgs) = *guard {
                return pkgs.clone();
            }
        }

        // Load packages
        let matches = self.get_url_matches();
        let mut packages = Vec::new();

        for path in matches {
            if let Some(pkg) = self.load_package_from_path(&path) {
                packages.push(Arc::new(pkg));
            }
        }

        // Store in cache
        {
            let mut guard = self.packages.write().await;
            *guard = Some(packages.clone());
        }

        packages
    }

    /// Load a single package from a directory
    fn load_package_from_path(&self, path: &Path) -> Option<Package> {
        let manifest = path.join("composer.json");

        if !manifest.exists() {
            return None;
        }

        // Read and parse composer.json
        let content = std::fs::read_to_string(&manifest).ok()?;
        let json: serde_json::Value = serde_json::from_str(&content).ok()?;

        let name = json.get("name")?.as_str()?;

        let version = self.determine_version(&json, path, name);

        let mut pkg = Package::new(name, &version);

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

        let reference = self.compute_reference(&content, path);
        let expanded_url = shellexpand::tilde(&self.url);
        let dist_path = if Path::new(expanded_url.as_ref()).is_absolute() {
            path
        } else {
            path.strip_prefix(&self.base_dir).unwrap_or(path)
        };
        let dist_url = dist_path.to_string_lossy();
        let dist_url = if dist_path.is_absolute() {
            dist_url.into_owned()
        } else {
            dist_url.replace('\\', "/")
        };
        let mut dist = Dist::new("path", &dist_url);
        if let Some(ref r) = reference {
            dist = dist.with_reference(r);
        }

        let mut transport_options = std::collections::HashMap::new();
        if let Some(symlink) = self.options.symlink {
            transport_options.insert("symlink".to_string(), serde_json::Value::Bool(symlink));
        }
        transport_options.insert(
            "relative".to_string(),
            serde_json::Value::Bool(self.options.relative),
        );
        pkg.transport_options = Some(serde_json::to_value(&transport_options).ok()?);
        dist = dist.with_transport_options(transport_options);

        pkg.dist = Some(dist);

        if path.join(".git").exists() {
            if let Some(git_ref) = get_git_reference(path) {
                let git_url =
                    get_git_url(path).unwrap_or_else(|| path.to_string_lossy().to_string());
                pkg.source = Some(Source::new("git", &git_url, &git_ref));
            }
        }

        Some(pkg)
    }

    /// Determine the version for a package
    fn determine_version(&self, json: &serde_json::Value, path: &Path, name: &str) -> String {
        // 1. Check for version override in options
        if let Some(version) = self.options.versions.get(name) {
            return version.clone();
        }

        // 2. Check for explicit version in composer.json
        if let Some(version) = json.get("version").and_then(|v| v.as_str()) {
            return version.to_string();
        }

        // 3. Try to guess version from VCS
        if let Some(version) = guess_version_from_vcs(path) {
            return version;
        }

        // 4. Default to dev-main
        "dev-main".to_string()
    }

    /// Compute the reference for the dist
    fn compute_reference(&self, content: &str, path: &Path) -> Option<String> {
        match self.options.reference.as_str() {
            "none" => None,
            "config" => {
                // Hash of composer.json content + options
                let mut hasher = Sha1::new();
                hasher.update(content.as_bytes());
                hasher.update(self.reference_options().as_bytes());
                Some(format!("{:x}", hasher.finalize()))
            }
            _ => {
                // Try git commit hash first
                if let Some(git_ref) = get_git_reference(path) {
                    return Some(git_ref);
                }
                // Fall back to config hash
                let mut hasher = Sha1::new();
                hasher.update(content.as_bytes());
                hasher.update(self.reference_options().as_bytes());
                Some(format!("{:x}", hasher.finalize()))
            }
        }
    }

    fn reference_options(&self) -> String {
        let mut entries = Vec::new();
        if let Some(symlink) = self.options.symlink {
            entries.push(format!("s:7:\"symlink\";b:{};", usize::from(symlink)));
        }
        entries.push(format!(
            "s:8:\"relative\";b:{};",
            usize::from(self.options.relative)
        ));
        if self.options.reference != "auto" {
            entries.push(format!(
                "s:9:\"reference\";s:{}:\"{}\";",
                self.options.reference.len(),
                self.options.reference
            ));
        }
        if !self.options.versions.is_empty() {
            let versions: std::collections::BTreeMap<_, _> = self.options.versions.iter().collect();
            let values = versions
                .into_iter()
                .map(|(name, version)| {
                    format!(
                        "s:{}:\"{}\";s:{}:\"{}\";",
                        name.len(),
                        name,
                        version.len(),
                        version
                    )
                })
                .collect::<String>();
            entries.push(format!(
                "s:8:\"versions\";a:{}:{{{values}}}",
                self.options.versions.len()
            ));
        }
        format!("a:{}:{{{}}}", entries.len(), entries.concat())
    }

    /// Get the URL pattern
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Get the options
    pub fn options(&self) -> &PathRepositoryOptions {
        &self.options
    }
}

#[async_trait]
impl Repository for PathRepository {
    fn name(&self) -> &str {
        &self.name
    }

    async fn has_package(&self, name: &str) -> bool {
        !self.find_packages(name).await.is_empty()
    }

    async fn find_packages(&self, name: &str) -> Vec<Arc<Package>> {
        let packages = self.ensure_loaded().await;

        packages
            .into_iter()
            .filter(|p| p.name.eq_ignore_ascii_case(name))
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
        // For path repositories, we typically have only one version
        // Return all matching packages
        self.find_packages(name).await
    }

    async fn get_packages(&self) -> Vec<Arc<Package>> {
        self.ensure_loaded().await
    }

    async fn search(&self, query: &str, _mode: SearchMode) -> Vec<SearchResult> {
        let packages = self.ensure_loaded().await;

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
                url: None,
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

/// Get the current git commit hash
fn get_git_reference(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(path)
        .output()
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Get the git remote URL
fn get_git_url(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(path)
        .output()
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Guess version from VCS (git)
fn guess_version_from_vcs(path: &Path) -> Option<String> {
    // Check if it's a git repository
    if !path.join(".git").exists() {
        return None;
    }

    // Try to get the current branch
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Check for tags
    let tag_output = Command::new("git")
        .args(["describe", "--tags", "--exact-match", "HEAD"])
        .current_dir(path)
        .output()
        .ok();

    if let Some(tag_output) = tag_output {
        if tag_output.status.success() {
            let tag = String::from_utf8_lossy(&tag_output.stdout)
                .trim()
                .to_string();
            // Strip 'v' prefix if present
            let version = tag.strip_prefix('v').unwrap_or(&tag);
            return Some(version.to_string());
        }
    }

    // Return branch as dev version
    Some(format!("dev-{}", branch))
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
    use tempfile::TempDir;

    fn create_test_package(dir: &Path, name: &str, version: Option<&str>) {
        std::fs::create_dir_all(dir).unwrap();

        let mut json = serde_json::json!({
            "name": name,
            "description": "Test package"
        });

        if let Some(v) = version {
            json["version"] = serde_json::Value::String(v.to_string());
        }

        std::fs::write(
            dir.join("composer.json"),
            serde_json::to_string_pretty(&json).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn composer_path_repository_loads_package_with_explicit_version() {
        let temp = TempDir::new().unwrap();
        let pkg_dir = temp.path().join("my-package");
        create_test_package(&pkg_dir, "test/path-versioned", Some("0.0.2"));

        let repo = PathRepository::from_path(pkg_dir.to_string_lossy().to_string());

        let packages = repo.get_packages().await;
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "test/path-versioned");
        assert_eq!(packages[0].version, "0.0.2");
    }

    #[tokio::test]
    async fn composer_path_repository_loads_package_without_explicit_version() {
        let temp = TempDir::new().unwrap();
        let pkg_dir = temp.path().join("without-version");
        create_test_package(&pkg_dir, "test/path-unversioned", None);

        let repo = PathRepository::from_path(pkg_dir.to_string_lossy().to_string());
        let packages = repo.get_packages().await;

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "test/path-unversioned");
        assert!(!packages[0].version.is_empty());
    }

    #[tokio::test]
    async fn composer_path_repository_reports_an_incorrect_filesystem_path() {
        let temp = TempDir::new().unwrap();
        let missing = temp.path().join("missing");
        let repo = PathRepository::from_path(missing.to_string_lossy().to_string());

        let error = repo.try_get_packages().await.unwrap_err();

        assert!(error.contains("supplied for the path"));
        assert!(error.contains(missing.to_string_lossy().as_ref()));
        assert!(error.ends_with("repository does not exist"));
    }

    #[tokio::test]
    async fn composer_path_repository_loads_wildcard_packages() {
        let temp = TempDir::new().unwrap();

        create_test_package(
            &temp.path().join("packages/with-version"),
            "test/path-versioned",
            Some("0.0.2"),
        );
        create_test_package(
            &temp.path().join("packages/without-version"),
            "test/path-unversioned",
            None,
        );

        let pattern = temp.path().join("packages/*").to_string_lossy().to_string();
        let repo = PathRepository::from_path(pattern);

        let packages = repo.get_packages().await;
        assert_eq!(packages.len(), 2);
        let mut names = packages
            .into_iter()
            .map(|package| package.name.clone())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["test/path-unversioned", "test/path-versioned"]);
    }

    #[tokio::test]
    async fn composer_path_repository_keeps_relative_dist_url() {
        let temp = TempDir::new().unwrap();
        create_test_package(
            &temp.path().join("packages/pkg-a"),
            "vendor/pkg-a",
            Some("1.0.0"),
        );

        let repo = PathRepository::new_with_base(
            "packages/*",
            PathRepositoryOptions::default(),
            temp.path(),
        );

        let packages = repo.get_packages().await;
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "vendor/pkg-a");
        assert_eq!(packages[0].dist.as_ref().unwrap().url, "packages/pkg-a");
        assert_eq!(
            packages[0]
                .dist
                .as_ref()
                .unwrap()
                .transport_options
                .as_ref()
                .unwrap()["relative"],
            serde_json::Value::Bool(false)
        );
    }

    #[tokio::test]
    async fn composer_path_repository_applies_explicit_version_overrides() {
        let temp = TempDir::new().unwrap();
        create_test_package(
            &temp.path().join("packages/with-version"),
            "test/path-versioned",
            Some("0.0.2"),
        );
        create_test_package(
            &temp.path().join("packages/without-version"),
            "test/path-unversioned",
            None,
        );

        let mut versions = HashMap::new();
        versions.insert("test/path-unversioned".to_string(), "4.3.2.1".to_string());
        versions.insert("test/path-versioned".to_string(), "3.2.1.0".to_string());

        let options = PathRepositoryOptions {
            versions,
            ..Default::default()
        };

        let pattern = temp.path().join("packages/*").to_string_lossy().to_string();
        let repo = PathRepository::new(pattern, options);

        let packages = repo.get_packages().await;
        let versions = packages
            .into_iter()
            .map(|package| (package.name.clone(), package.version.to_string()))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            versions,
            std::collections::BTreeMap::from([
                ("test/path-unversioned".to_string(), "4.3.2.1".to_string()),
                ("test/path-versioned".to_string(), "3.2.1.0".to_string()),
            ])
        );
    }

    #[tokio::test]
    async fn composer_path_repository_reference_none() {
        let temp = TempDir::new().unwrap();
        create_test_package(
            &temp.path().join("packages/with-version"),
            "test/path-versioned",
            Some("0.0.2"),
        );
        create_test_package(
            &temp.path().join("packages/without-version"),
            "test/path-unversioned",
            None,
        );

        let pattern = temp.path().join("packages/*").to_string_lossy().to_string();
        let repo = PathRepository::new(
            pattern,
            PathRepositoryOptions {
                reference: "none".to_string(),
                ..Default::default()
            },
        );

        let packages = repo.get_packages().await;
        assert_eq!(packages.len(), 2);
        assert!(packages
            .iter()
            .all(|package| package.dist.as_ref().unwrap().reference.is_none()));
    }

    #[tokio::test]
    async fn composer_path_repository_reference_config() {
        let temp = TempDir::new().unwrap();
        create_test_package(
            &temp.path().join("packages/with-version"),
            "test/path-versioned",
            Some("0.0.2"),
        );
        create_test_package(
            &temp.path().join("packages/without-version"),
            "test/path-unversioned",
            None,
        );

        let pattern = temp.path().join("packages/*").to_string_lossy().to_string();
        let repo = PathRepository::new(
            pattern,
            PathRepositoryOptions {
                reference: "config".to_string(),
                relative: true,
                ..Default::default()
            },
        );

        let packages = repo.get_packages().await;
        assert_eq!(packages.len(), 2);
        let serialized_options = r#"a:2:{s:8:"relative";b:1;s:9:"reference";s:6:"config";}"#;
        for package in packages {
            let package_dir = temp.path().join(&package.dist.as_ref().unwrap().url);
            let content = std::fs::read(package_dir.join("composer.json")).unwrap();
            let mut hasher = Sha1::new();
            hasher.update(content);
            hasher.update(serialized_options.as_bytes());
            assert_eq!(
                package.dist.as_ref().unwrap().reference.as_deref(),
                Some(format!("{:x}", hasher.finalize()).as_str())
            );
        }
    }

    #[tokio::test]
    async fn test_path_repository_find_package() {
        let temp = TempDir::new().unwrap();
        let pkg_dir = temp.path().join("my-package");
        create_test_package(&pkg_dir, "vendor/my-package", Some("1.0.0"));

        let repo = PathRepository::from_path(pkg_dir.to_string_lossy().to_string());

        let found = repo.find_packages("vendor/my-package").await;
        assert_eq!(found.len(), 1);

        let not_found = repo.find_packages("vendor/other").await;
        assert!(not_found.is_empty());
    }
}
