//! Package repository - inline package definitions in composer.json.

use std::sync::Arc;

use async_trait::async_trait;
use riff_semver::VersionParser;

use super::traits::{ProviderInfo, Repository, SearchMode, SearchResult};
use crate::package::{
    validate_package_metadata, Abandoned, Autoload, AutoloadPath, Dist, Funding, Package, Source,
};

/// Package repository - provides packages from inline definitions
///
/// This repository type allows defining packages directly in composer.json:
///
/// ```json
/// {
///     "repositories": [
///         {
///             "type": "package",
///             "package": {
///                 "name": "vendor/package",
///                 "version": "1.0.0",
///                 "dist": {
///                     "url": "https://example.com/package.zip",
///                     "type": "zip"
///                 }
///             }
///         }
///     ]
/// }
/// ```
///
/// Multiple versions can be defined using an array:
///
/// ```json
/// {
///     "repositories": [
///         {
///             "type": "package",
///             "package": [
///                 { "name": "vendor/package", "version": "1.0.0", ... },
///                 { "name": "vendor/package", "version": "2.0.0", ... }
///             ]
///         }
///     ]
/// }
/// ```
#[derive(Debug)]
pub struct PackageRepository {
    /// Repository name
    name: String,
    /// Loaded packages
    packages: Vec<Arc<Package>>,
}

impl PackageRepository {
    /// Create a new package repository from inline package definition(s)
    ///
    /// # Arguments
    /// * `package_config` - Either a single package object or an array of package objects
    pub fn new(package_config: &serde_json::Value) -> Result<Self, String> {
        let mut packages = Vec::new();

        // Handle both single package and array of packages
        let package_array = if package_config.is_array() {
            package_config.as_array().unwrap().clone()
        } else if package_config.is_object() {
            vec![package_config.clone()]
        } else {
            return Err("Package config must be an object or array".to_string());
        };

        for (index, pkg_json) in package_array.iter().enumerate() {
            let pkg = Self::load_package(pkg_json)
                .map_err(|e| format!("Invalid package at index {}: {}", index, e))?;
            packages.push(Arc::new(pkg));
        }

        let name = if packages.len() == 1 {
            format!("package {}", packages[0].name)
        } else {
            format!("package repo ({} packages)", packages.len())
        };

        Ok(Self { name, packages })
    }

    /// Load a single package from JSON
    fn load_package(json: &serde_json::Value) -> Result<Package, String> {
        // Required fields
        let name = json
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or("Package must have a 'name' field")?;

        let version = match json.get("version") {
            Some(serde_json::Value::String(version)) => version.clone(),
            Some(serde_json::Value::Number(version)) => version.to_string(),
            Some(_) => return Err("Package 'version' must be a string or number".to_string()),
            None => return Err("Package must have a 'version' field".to_string()),
        };
        let normalized_version = if let Some(normalized) = json
            .get("version_normalized")
            .and_then(serde_json::Value::as_str)
        {
            normalized.to_string()
        } else {
            VersionParser::new().normalize(&version).map_err(|error| {
                format!("Failed to normalize version for package \"{name}\": {error}")
            })?
        };

        let mut pkg = Package::new(name, normalized_version);
        pkg.pretty_version = Some(version.into());

        // Parse dist
        if let Some(dist_json) = json.get("dist") {
            pkg.dist = Some(Self::parse_dist(dist_json)?);
        }

        // Parse source
        if let Some(source_json) = json.get("source") {
            pkg.source = Some(Self::parse_source(source_json)?);
        }

        // Optional fields
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
        if let Some(conflict) = json.get("conflict").and_then(|v| v.as_object()) {
            pkg.conflict = dependency_map(conflict);
        }
        if let Some(provide) = json.get("provide").and_then(|v| v.as_object()) {
            pkg.provide = dependency_map(provide);
        }
        if let Some(replace) = json.get("replace").and_then(|v| v.as_object()) {
            pkg.replace = dependency_map(replace);
        }
        if let Some(suggest) = json.get("suggest").and_then(|v| v.as_object()) {
            pkg.suggest = dependency_map(suggest);
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

        // Extra metadata
        if let Some(homepage) = json.get("homepage").and_then(|v| v.as_str()) {
            pkg.homepage = Some(homepage.to_string());
        }

        if let Some(keywords) = json.get("keywords").and_then(|v| v.as_array()) {
            pkg.keywords = keywords
                .iter()
                .filter_map(|v| v.as_str().map(Into::into))
                .collect();
        }
        pkg.extra = json.get("extra").cloned();
        pkg.default_branch = json.get("default-branch").and_then(|value| value.as_bool());
        pkg.abandoned = match json.get("abandoned") {
            Some(serde_json::Value::Bool(true)) => Some(Abandoned::Yes),
            Some(serde_json::Value::String(replacement)) => {
                Some(Abandoned::Replacement(replacement.clone()))
            }
            _ => None,
        };
        pkg.funding = json
            .get("funding")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|funding| {
                let funding_type = funding
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(Into::into);
                let url = funding
                    .get("url")
                    .and_then(serde_json::Value::as_str)
                    .map(Into::into);
                (funding_type.is_some() || url.is_some()).then_some(Funding { funding_type, url })
            })
            .collect();

        validate_package_metadata(&pkg).map_err(|error| error.to_string())?;
        Ok(pkg)
    }

    /// Parse dist configuration
    fn parse_dist(json: &serde_json::Value) -> Result<Dist, String> {
        let dist_type = json
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or("dist must have a 'type' field")?;

        let url = json
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or("dist must have a 'url' field")?;

        let mut dist = Dist::new(dist_type, url);

        if let Some(reference) = json.get("reference").and_then(|v| v.as_str()) {
            dist = dist.with_reference(reference);
        }

        if let Some(shasum) = json.get("shasum").and_then(|v| v.as_str()) {
            dist = dist.with_shasum(shasum);
        }

        if let Some(mirrors) = json.get("mirrors") {
            dist.mirrors = Some(
                serde_json::from_value(mirrors.clone())
                    .map_err(|error| format!("invalid dist mirrors: {error}"))?,
            );
        }

        Ok(dist)
    }

    /// Parse source configuration
    fn parse_source(json: &serde_json::Value) -> Result<Source, String> {
        let source_type = json
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or("source must have a 'type' field")?;

        let url = json
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or("source must have a 'url' field")?;

        let reference = json
            .get("reference")
            .and_then(|v| v.as_str())
            .ok_or("source must have a 'reference' field")?;

        let mut source = Source::new(source_type, url, reference);
        if let Some(mirrors) = json.get("mirrors") {
            source.mirrors = Some(
                serde_json::from_value(mirrors.clone())
                    .map_err(|error| format!("invalid source mirrors: {error}"))?,
            );
        }

        Ok(source)
    }

    /// Search inline packages while applying Composer's optional package-type filter.
    pub fn search_with_type(
        &self,
        query: &str,
        mode: SearchMode,
        package_type: Option<&str>,
    ) -> Vec<SearchResult> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|term| term.to_lowercase())
            .collect();
        let mut seen = std::collections::HashSet::new();

        self.packages
            .iter()
            .filter(|package| {
                package_type.is_none_or(|expected| package.package_type() == expected)
            })
            .filter_map(|package| {
                let name = match mode {
                    SearchMode::Vendor => package.name.split('/').next().unwrap_or(&package.name),
                    SearchMode::Fulltext | SearchMode::Name => &package.name,
                };
                let name_lower = name.to_lowercase();
                let fulltext = (mode == SearchMode::Fulltext).then(|| {
                    let mut text = package.description.clone().unwrap_or_default();
                    for keyword in &package.keywords {
                        text.push(' ');
                        text.push_str(keyword);
                    }
                    text.to_lowercase()
                });
                let matches = terms.iter().any(|term| {
                    name_lower.contains(term)
                        || fulltext
                            .as_ref()
                            .is_some_and(|metadata| metadata.contains(term))
                });

                if !matches || !seen.insert(name.to_string()) {
                    return None;
                }

                let (description, abandoned) = if mode == SearchMode::Vendor {
                    (None, None)
                } else {
                    let abandoned = package.abandoned.as_ref().map(|abandoned| match abandoned {
                        Abandoned::Yes => String::new(),
                        Abandoned::Replacement(replacement) => replacement.clone(),
                    });
                    (package.description.clone(), abandoned)
                };

                Some(SearchResult {
                    name: name.to_string(),
                    description,
                    url: None,
                    abandoned,
                    downloads: None,
                    favers: None,
                })
            })
            .collect()
    }
}

#[async_trait]
impl Repository for PackageRepository {
    fn name(&self) -> &str {
        &self.name
    }

    async fn has_package(&self, name: &str) -> bool {
        !self.find_packages(name).await.is_empty()
    }

    async fn find_packages(&self, name: &str) -> Vec<Arc<Package>> {
        self.packages
            .iter()
            .filter(|package| {
                package.name.eq_ignore_ascii_case(name)
                    || package
                        .provide
                        .keys()
                        .chain(package.replace.keys())
                        .any(|provided| provided.eq_ignore_ascii_case(name))
            })
            .cloned()
            .collect()
    }

    async fn find_package(&self, name: &str, version: &str) -> Option<Arc<Package>> {
        self.packages
            .iter()
            .find(|package| {
                package.name.eq_ignore_ascii_case(name)
                    && (package.version == version || package.pretty_version() == version)
            })
            .cloned()
    }

    async fn find_packages_with_constraint(
        &self,
        name: &str,
        _constraint: &str,
    ) -> Vec<Arc<Package>> {
        // For inline packages, return all versions matching the name
        // The solver will filter by constraint
        self.find_packages(name).await
    }

    async fn get_packages(&self) -> Vec<Arc<Package>> {
        self.packages.clone()
    }

    async fn search(&self, query: &str, mode: SearchMode) -> Vec<SearchResult> {
        self.search_with_type(query, mode, None)
    }

    async fn search_with_type(
        &self,
        query: &str,
        mode: SearchMode,
        package_type: Option<&str>,
    ) -> Vec<SearchResult> {
        PackageRepository::search_with_type(self, query, mode, package_type)
    }

    async fn get_providers(&self, package_name: &str) -> Vec<ProviderInfo> {
        self.packages
            .iter()
            .filter(|package| {
                package
                    .provide
                    .keys()
                    .chain(package.replace.keys())
                    .any(|provided| provided.eq_ignore_ascii_case(package_name))
            })
            .map(|package| ProviderInfo {
                name: package.name.clone(),
                description: package.description.clone(),
                package_type: package.package_type.to_string(),
            })
            .collect()
    }
}

fn dependency_map(
    values: &serde_json::Map<String, serde_json::Value>,
) -> crate::package::DependencyMap {
    values
        .iter()
        .map(|(name, constraint)| (name.clone(), constraint.as_str().unwrap_or("*").to_string()))
        .collect()
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

    #[tokio::test]
    async fn test_single_package() {
        let config = serde_json::json!({
            "name": "vendor/package",
            "version": "1.0.0",
            "dist": {
                "url": "https://example.com/package.zip",
                "type": "zip"
            }
        });

        let repo = PackageRepository::new(&config).unwrap();
        let packages = repo.get_packages().await;

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "vendor/package");
        assert_eq!(packages[0].version, "1.0.0.0");
        assert_eq!(packages[0].pretty_version(), "1.0.0");
        assert!(packages[0].dist.is_some());
    }

    // Ported from Composer\Test\Package\Loader\ArrayLoaderTest::testInvalidVersion.
    #[test]
    fn composer_array_loader_rejects_invalid_versions() {
        let error = PackageRepository::new(&serde_json::json!({
            "name": "acme/package",
            "version": "AA",
            "dist": {"url": "https://example.org/package.zip", "type": "zip"}
        }))
        .unwrap_err();

        assert!(error.contains("Failed to normalize version for package \"acme/package\""));
        assert!(error.contains("Invalid version string \"AA\""));
    }

    // Ported from Composer\Test\Package\Loader\ArrayLoaderTest::testNoneStringVersion.
    #[tokio::test]
    async fn composer_array_loader_accepts_numeric_versions_as_pretty_strings() {
        let repository = PackageRepository::new(&serde_json::json!({
            "name": "acme/package",
            "version": 1,
            "dist": {"url": "https://example.org/package.zip", "type": "zip"}
        }))
        .unwrap();
        let packages = repository.get_packages().await;

        assert_eq!(packages[0].pretty_version(), "1");
        assert_eq!(packages[0].version(), "1.0.0.0");
    }

    // Ported from Composer\Test\Package\Loader\ArrayLoaderTest::
    // testNormalizedVersionOptimization.
    #[tokio::test]
    async fn composer_array_loader_honors_precomputed_normalized_versions() {
        let repository = PackageRepository::new(&serde_json::json!([
            {
                "name": "acme/package",
                "version": "1.2.3",
                "dist": {"url": "https://example.org/one.zip", "type": "zip"}
            },
            {
                "name": "acme/optimized",
                "version": "1.2.3",
                "version_normalized": "1.2.3.4",
                "dist": {"url": "https://example.org/two.zip", "type": "zip"}
            }
        ]))
        .unwrap();
        let packages = repository.get_packages().await;

        assert_eq!(packages[0].version(), "1.2.3.0");
        assert_eq!(packages[1].version(), "1.2.3.4");
    }

    #[tokio::test]
    async fn test_multiple_packages() {
        let config = serde_json::json!([
            {
                "name": "vendor/package",
                "version": "1.0.0",
                "dist": {
                    "url": "https://example.com/package-1.0.0.zip",
                    "type": "zip"
                }
            },
            {
                "name": "vendor/package",
                "version": "2.0.0",
                "dist": {
                    "url": "https://example.com/package-2.0.0.zip",
                    "type": "zip"
                }
            }
        ]);

        let repo = PackageRepository::new(&config).unwrap();
        let packages = repo.get_packages().await;

        assert_eq!(packages.len(), 2);
    }

    #[tokio::test]
    async fn test_package_with_source() {
        let config = serde_json::json!({
            "name": "vendor/package",
            "version": "1.0.0",
            "source": {
                "url": "https://github.com/vendor/package.git",
                "type": "git",
                "reference": "abc123"
            }
        });

        let repo = PackageRepository::new(&config).unwrap();
        let packages = repo.get_packages().await;

        assert_eq!(packages.len(), 1);
        assert!(packages[0].source.is_some());
        assert_eq!(packages[0].source.as_ref().unwrap().reference, "abc123");
    }

    #[tokio::test]
    async fn test_package_with_both_dist_and_source() {
        let config = serde_json::json!({
            "name": "vendor/package",
            "version": "1.0.0",
            "dist": {
                "url": "https://example.com/package.zip",
                "type": "zip"
            },
            "source": {
                "url": "https://github.com/vendor/package.git",
                "type": "git",
                "reference": "abc123"
            }
        });

        let repo = PackageRepository::new(&config).unwrap();
        let packages = repo.get_packages().await;

        assert_eq!(packages.len(), 1);
        assert!(packages[0].dist.is_some());
        assert!(packages[0].source.is_some());
    }

    #[tokio::test]
    async fn test_package_with_metadata() {
        let config = serde_json::json!({
            "name": "vendor/package",
            "version": "1.0.0",
            "description": "A test package",
            "type": "library",
            "license": "MIT",
            "require": {
                "php": ">=8.0"
            },
            "conflict": { "vendor/conflict": "^1" },
            "provide": { "virtual/api": "1.0" },
            "replace": { "vendor/old": "self.version" },
            "suggest": { "vendor/optional": "Adds optional support" },
            "default-branch": true,
            "abandoned": "vendor/replacement",
            "funding": [{"type": "github", "url": "https://github.com/sponsors/vendor"}],
            "extra": { "branch-alias": { "dev-main": "1.x-dev" } },
            "autoload": {
                "psr-4": {
                    "Vendor\\Package\\": "src/"
                }
            },
            "dist": {
                "url": "https://example.com/package.zip",
                "type": "zip"
            }
        });

        let repo = PackageRepository::new(&config).unwrap();
        let packages = repo.get_packages().await;

        assert_eq!(packages[0].description, Some("A test package".to_string()));
        assert_eq!(packages[0].package_type, "library");
        assert_eq!(packages[0].license, vec!["MIT".to_string()]);
        assert!(packages[0].require.contains_key("php"));
        assert_eq!(
            packages[0]
                .conflict
                .get("vendor/conflict")
                .map(|value| value.as_str()),
            Some("^1")
        );
        assert_eq!(
            packages[0]
                .provide
                .get("virtual/api")
                .map(|value| value.as_str()),
            Some("1.0")
        );
        assert_eq!(
            packages[0]
                .replace
                .get("vendor/old")
                .map(|value| value.as_str()),
            Some("self.version")
        );
        assert!(packages[0].suggest.contains_key("vendor/optional"));
        assert_eq!(packages[0].default_branch, Some(true));
        assert_eq!(
            packages[0].abandoned,
            Some(Abandoned::Replacement("vendor/replacement".to_string()))
        );
        assert!(packages[0].extra.is_some());
        assert_eq!(packages[0].funding.len(), 1);
        assert!(packages[0].autoload.is_some());
    }

    #[tokio::test]
    async fn test_find_package() {
        let config = serde_json::json!([
            {
                "name": "vendor/package",
                "version": "1.0.0",
                "dist": { "url": "https://example.com/1.zip", "type": "zip" }
            },
            {
                "name": "vendor/package",
                "version": "2.0.0",
                "dist": { "url": "https://example.com/2.zip", "type": "zip" }
            }
        ]);

        let repo = PackageRepository::new(&config).unwrap();

        let found = repo.find_package("vendor/package", "1.0.0").await;
        assert!(found.is_some());
        assert_eq!(found.unwrap().version, "1.0.0.0");

        let found = repo.find_package("vendor/package", "2.0.0").await;
        assert!(found.is_some());
        assert_eq!(found.unwrap().version, "2.0.0.0");

        let not_found = repo.find_package("vendor/package", "3.0.0").await;
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn test_find_virtual_providers_and_replacers() {
        let config = serde_json::json!([
            {
                "name": "vendor/provider",
                "version": "1.0.0",
                "provide": {"virtual/api": "1.0.0"},
                "dist": {"url": "https://example.com/provider.zip", "type": "zip"}
            },
            {
                "name": "vendor/replacer",
                "version": "1.0.0",
                "replace": {"virtual/api": "1.0.0"},
                "dist": {"url": "https://example.com/replacer.zip", "type": "zip"}
            }
        ]);
        let repo = PackageRepository::new(&config).unwrap();

        let packages = repo.find_packages("VIRTUAL/API").await;
        assert_eq!(packages.len(), 2);
        let providers = repo.get_providers("virtual/api").await;
        assert_eq!(providers.len(), 2);
    }

    #[test]
    fn test_missing_name() {
        let config = serde_json::json!({
            "version": "1.0.0",
            "dist": { "url": "https://example.com/package.zip", "type": "zip" }
        });

        let result = PackageRepository::new(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("name"));
    }

    #[test]
    fn test_missing_version() {
        let config = serde_json::json!({
            "name": "vendor/package",
            "dist": { "url": "https://example.com/package.zip", "type": "zip" }
        });

        let result = PackageRepository::new(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("version"));
    }

    #[test]
    fn metadata_only_package_is_accepted() {
        let config = serde_json::json!({
            "name": "vendor/package",
            "version": "1.0.0"
        });

        let result = PackageRepository::new(&config);
        let repository = result.expect("solver and search metadata need no download transport");
        assert_eq!(repository.packages.len(), 1);
        assert!(repository.packages[0].dist.is_none());
        assert!(repository.packages[0].source.is_none());
    }

    #[test]
    fn test_dist_missing_type() {
        let config = serde_json::json!({
            "name": "vendor/package",
            "version": "1.0.0",
            "dist": {
                "url": "https://example.com/package.zip"
            }
        });

        let result = PackageRepository::new(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("type"));
    }

    #[test]
    fn test_source_missing_reference() {
        let config = serde_json::json!({
            "name": "vendor/package",
            "version": "1.0.0",
            "source": {
                "url": "https://github.com/vendor/package.git",
                "type": "git"
            }
        });

        let result = PackageRepository::new(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("reference"));
    }

    #[tokio::test]
    async fn test_search() {
        let config = serde_json::json!([
            {
                "name": "vendor/foo-package",
                "version": "1.0.0",
                "description": "A foo package",
                "dist": { "url": "https://example.com/foo.zip", "type": "zip" }
            },
            {
                "name": "vendor/bar-package",
                "version": "1.0.0",
                "description": "A bar package",
                "dist": { "url": "https://example.com/bar.zip", "type": "zip" }
            }
        ]);

        let repo = PackageRepository::new(&config).unwrap();

        let results = repo.search("foo", SearchMode::Name).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "vendor/foo-package");
    }

    #[tokio::test]
    async fn composer_array_repository_has_package_version_identity() {
        let repo = PackageRepository::new(&serde_json::json!([
            {
                "name": "foo/package",
                "version": "1",
                "dist": {"url": "https://example.test/foo.zip", "type": "zip"}
            },
            {
                "name": "bar/package",
                "version": "2",
                "dist": {"url": "https://example.test/bar.zip", "type": "zip"}
            }
        ]))
        .unwrap();

        assert!(repo.find_package("foo/package", "1").await.is_some());
        assert!(repo.find_package("bar/package", "1").await.is_none());
    }

    #[tokio::test]
    async fn composer_array_repository_finds_all_versions_by_name() {
        let repo = PackageRepository::new(&serde_json::json!([
            {
                "name": "foo/package",
                "version": "1",
                "dist": {"url": "https://example.test/foo.zip", "type": "zip"}
            },
            {
                "name": "bar/package",
                "version": "2",
                "dist": {"url": "https://example.test/bar-2.zip", "type": "zip"}
            },
            {
                "name": "bar/package",
                "version": "3",
                "dist": {"url": "https://example.test/bar-3.zip", "type": "zip"}
            }
        ]))
        .unwrap();

        let foo = repo.find_packages("FOO/PACKAGE").await;
        assert_eq!(foo.len(), 1);
        assert_eq!(foo[0].name, "foo/package");

        let bar = repo.find_packages("bar/package").await;
        assert_eq!(bar.len(), 2);
        assert!(bar.iter().any(|package| package.pretty_version() == "2"));
        assert!(bar.iter().any(|package| package.pretty_version() == "3"));
    }

    #[tokio::test]
    async fn composer_array_repository_searches_names_and_fulltext() {
        let repo = PackageRepository::new(&serde_json::json!([
            {
                "name": "vendor/foo",
                "version": "1",
                "description": "A quiet library",
                "dist": {"url": "https://example.test/foo.zip", "type": "zip"}
            },
            {
                "name": "vendor/bar",
                "version": "1",
                "description": "Contains the FoObAr feature",
                "dist": {"url": "https://example.test/bar.zip", "type": "zip"}
            }
        ]))
        .unwrap();

        let by_name = repo.search("FOO", SearchMode::Name).await;
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].name, "vendor/foo");

        let fulltext = repo.search("foobar", SearchMode::Fulltext).await;
        assert_eq!(fulltext.len(), 1);
        assert_eq!(fulltext[0].name, "vendor/bar");
        assert!(repo.search("missing", SearchMode::Name).await.is_empty());
    }

    // Ported from Composer\Test\Repository\ArrayRepositoryTest::testSearchWithPackageType.
    #[test]
    fn composer_array_repository_filters_search_by_package_type() {
        let repo = PackageRepository::new(&serde_json::json!([
            {
                "name": "vendor/foo",
                "version": "1",
                "dist": {"url": "https://example.test/foo.zip", "type": "zip"}
            },
            {
                "name": "vendor/bar",
                "version": "1",
                "dist": {"url": "https://example.test/bar.zip", "type": "zip"}
            },
            {
                "name": "vendor/foobar",
                "version": "1",
                "type": "composer-plugin",
                "dist": {"url": "https://example.test/foobar.zip", "type": "zip"}
            }
        ]))
        .unwrap();

        let libraries = repo.search_with_type("foo", SearchMode::Fulltext, Some("library"));
        assert_eq!(libraries.len(), 1);
        assert_eq!(libraries[0].name, "vendor/foo");
        assert!(repo
            .search_with_type("bar", SearchMode::Fulltext, Some("package"))
            .is_empty());
        let plugins = repo.search_with_type("foo", SearchMode::Name, Some("composer-plugin"));
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "vendor/foobar");
    }

    #[tokio::test]
    async fn composer_array_repository_reports_abandoned_search_results() {
        let repo = PackageRepository::new(&serde_json::json!([
            {
                "name": "vendor/foo1",
                "version": "1",
                "abandoned": true,
                "dist": {"url": "https://example.test/foo1.zip", "type": "zip"}
            },
            {
                "name": "vendor/foo2",
                "version": "1",
                "abandoned": "vendor/bar",
                "dist": {"url": "https://example.test/foo2.zip", "type": "zip"}
            }
        ]))
        .unwrap();

        let mut results = repo.search("foo", SearchMode::Name).await;
        results.sort_by(|left, right| left.name.cmp(&right.name));

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].abandoned.as_deref(), Some(""));
        assert_eq!(results[1].abandoned.as_deref(), Some("vendor/bar"));
    }
}
