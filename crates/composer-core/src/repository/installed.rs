use async_trait::async_trait;
use compact_str::CompactString;
use indexmap::IndexMap;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use super::traits::{ProviderInfo, Repository, SearchMode, SearchResult, WritableRepository};
use crate::package::{Dist, Package, Source};

/// Repository for installed packages (vendor/composer/installed.json)
pub struct InstalledRepository {
    /// Path to the vendor directory
    vendor_dir: PathBuf,
    /// Installed packages
    packages: RwLock<HashMap<String, Arc<Package>>>,
    /// Whether the repository has been modified
    dirty: RwLock<bool>,
}

impl InstalledRepository {
    /// Create a new installed repository
    pub fn new(vendor_dir: impl Into<PathBuf>) -> Self {
        Self {
            vendor_dir: vendor_dir.into(),
            packages: RwLock::new(HashMap::new()),
            dirty: RwLock::new(false),
        }
    }

    /// Get the path to installed.json
    pub fn installed_json_path(&self) -> PathBuf {
        self.vendor_dir.join("composer").join("installed.json")
    }

    /// Load packages from installed.json
    pub async fn load(&self) -> Result<(), String> {
        let path = self.installed_json_path();
        if !path.exists() {
            return Ok(());
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read installed.json: {}", e))?;

        let data: InstalledJson = serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse installed.json: {}", e))?;

        let mut packages = self.packages.write().await;
        packages.clear();

        for pkg_data in data.packages {
            let package = Package::from_installed_json(&pkg_data);
            packages.insert(package.name.clone(), Arc::new(package));
        }

        Ok(())
    }

    /// Load only the fields required to plan and execute package operations.
    pub(crate) fn load_transaction_packages(&self) -> Result<Vec<Arc<Package>>, String> {
        let path = self.installed_json_path();
        if !path.exists() {
            return Ok(Vec::new());
        }

        let content =
            std::fs::read(&path).map_err(|e| format!("Failed to read installed.json: {}", e))?;
        let data: TransactionInstalledJson<'_> = serde_json::from_slice(&content)
            .map_err(|e| format!("Failed to parse installed.json: {}", e))?;

        Ok(data
            .packages
            .into_iter()
            .map(|package| Arc::new(package.into_package()))
            .collect())
    }

    /// Get the vendor directory path
    pub fn vendor_dir(&self) -> &Path {
        &self.vendor_dir
    }
}

#[async_trait]
impl Repository for InstalledRepository {
    fn name(&self) -> &str {
        "installed"
    }

    async fn has_package(&self, name: &str) -> bool {
        let packages = self.packages.read().await;
        packages.contains_key(&name.to_lowercase())
    }

    async fn find_packages(&self, name: &str) -> Vec<Arc<Package>> {
        let packages = self.packages.read().await;
        packages
            .get(&name.to_lowercase())
            .map(|p| vec![p.clone()])
            .unwrap_or_default()
    }

    async fn find_package(&self, name: &str, version: &str) -> Option<Arc<Package>> {
        let packages = self.packages.read().await;
        packages.get(&name.to_lowercase()).and_then(|p| {
            if p.version == version || p.pretty_version.as_deref() == Some(version) {
                Some(p.clone())
            } else {
                None
            }
        })
    }

    async fn find_packages_with_constraint(
        &self,
        name: &str,
        _constraint: &str,
    ) -> Vec<Arc<Package>> {
        // Installed repository only has one version per package
        self.find_packages(name).await
    }

    async fn get_packages(&self) -> Vec<Arc<Package>> {
        let packages = self.packages.read().await;
        packages.values().cloned().collect()
    }

    async fn search(&self, query: &str, _mode: SearchMode) -> Vec<SearchResult> {
        let query_lower = query.to_lowercase();
        let packages = self.packages.read().await;

        packages
            .values()
            .filter(|p| p.name.to_lowercase().contains(&query_lower))
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

    async fn get_providers(&self, package_name: &str) -> Vec<ProviderInfo> {
        let packages = self.packages.read().await;

        packages
            .values()
            .filter(|p| p.provide.contains_key(package_name))
            .map(|p| ProviderInfo {
                name: p.name.clone(),
                description: p.description.clone(),
                package_type: p.package_type.to_string(),
            })
            .collect()
    }

    async fn count(&self) -> usize {
        let packages = self.packages.read().await;
        packages.len()
    }
}

#[async_trait]
impl WritableRepository for InstalledRepository {
    async fn add_package(&mut self, package: Package) {
        let mut packages = self.packages.write().await;
        packages.insert(package.name.to_lowercase(), Arc::new(package));
        *self.dirty.write().await = true;
    }

    async fn remove_package(&mut self, package: &Package) {
        let mut packages = self.packages.write().await;
        packages.remove(&package.name.to_lowercase());
        *self.dirty.write().await = true;
    }

    fn is_dirty(&self) -> bool {
        // Can't await in a non-async fn, so return false
        // Real implementation would need to restructure this
        false
    }

    async fn write(&self) -> std::io::Result<()> {
        let packages = self.packages.read().await;

        let installed = InstalledJson {
            packages: packages.values().map(|p| p.to_installed_json()).collect(),
            dev: true,
            dev_package_names: vec![],
        };

        let content = serde_json::to_string_pretty(&installed)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        let path = self.installed_json_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(path, content)?;
        *self.dirty.write().await = false;

        Ok(())
    }
}

/// Structure of installed.json
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct InstalledJson {
    packages: Vec<InstalledPackage>,
    #[serde(default)]
    dev: bool,
    #[serde(default, rename = "dev-package-names")]
    dev_package_names: Vec<String>,
}

/// Borrowed projection used by transaction planning. Serde skips every other
/// installed metadata field without allocating it.
#[derive(Debug, serde::Deserialize)]
struct TransactionInstalledJson<'a> {
    #[serde(borrow)]
    packages: Vec<TransactionInstalledPackage<'a>>,
}

#[derive(Debug, serde::Deserialize)]
struct TransactionInstalledPackage<'a> {
    #[serde(borrow)]
    name: Cow<'a, str>,
    #[serde(borrow)]
    version: Cow<'a, str>,
    #[serde(default, borrow)]
    version_normalized: Cow<'a, str>,
    #[serde(rename = "type", default, borrow)]
    package_type: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    source: Option<TransactionInstalledSource<'a>>,
    #[serde(default, borrow)]
    dist: Option<TransactionInstalledDist<'a>>,
}

#[derive(Debug, serde::Deserialize)]
struct TransactionInstalledSource<'a> {
    #[serde(rename = "type", borrow)]
    source_type: Cow<'a, str>,
    #[serde(borrow)]
    url: Cow<'a, str>,
    #[serde(borrow)]
    reference: Cow<'a, str>,
}

#[derive(Debug, serde::Deserialize)]
struct TransactionInstalledDist<'a> {
    #[serde(rename = "type", borrow)]
    dist_type: Cow<'a, str>,
    #[serde(borrow)]
    url: Cow<'a, str>,
    #[serde(default, borrow)]
    reference: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    shasum: Option<Cow<'a, str>>,
}

impl TransactionInstalledPackage<'_> {
    fn into_package(self) -> Package {
        let mut package = Package::new(
            self.name.into_owned(),
            CompactString::new(self.version_normalized.as_ref()),
        );
        package.pretty_version = Some(CompactString::new(self.version.as_ref()));
        package.package_type =
            CompactString::new(self.package_type.as_deref().unwrap_or("library"));
        package.source = self.source.map(|source| Source {
            source_type: CompactString::new(source.source_type.as_ref()),
            url: source.url.into_owned(),
            reference: source.reference.into_owned(),
            mirrors: None,
        });
        package.dist = self.dist.map(|dist| Dist {
            dist_type: CompactString::new(dist.dist_type.as_ref()),
            url: dist.url.into_owned(),
            reference: dist.reference.map(Cow::into_owned),
            shasum: dist.shasum.map(Cow::into_owned),
            sha256: None,
            mirrors: None,
            transport_options: None,
        });
        package
    }
}

/// Package entry in installed.json
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub version_normalized: String,
    #[serde(rename = "type", default = "default_type")]
    pub package_type: String,
    #[serde(default)]
    pub source: Option<InstalledSource>,
    #[serde(default)]
    pub dist: Option<InstalledDist>,
    #[serde(default)]
    pub require: IndexMap<String, String>,
    #[serde(default, rename = "require-dev")]
    pub require_dev: IndexMap<String, String>,
    #[serde(default)]
    pub conflict: IndexMap<String, String>,
    #[serde(default)]
    pub replace: IndexMap<String, String>,
    #[serde(default)]
    pub provide: IndexMap<String, String>,
    #[serde(default)]
    pub autoload: serde_json::Value,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub license: serde_json::Value,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default, rename = "install-path")]
    pub install_path: Option<String>,
}

fn default_type() -> String {
    "library".to_string()
}

fn parse_license_value(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        serde_json::Value::String(s) => vec![s.clone()],
        _ => vec![],
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstalledSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub url: String,
    pub reference: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstalledDist {
    #[serde(rename = "type")]
    pub dist_type: String,
    pub url: String,
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub shasum: Option<String>,
}

impl Package {
    /// Create a Package from installed.json format
    pub fn from_installed_json(data: &InstalledPackage) -> Self {
        let source = data.source.as_ref().map(|s| Source {
            source_type: s.source_type.clone().into(),
            url: s.url.clone(),
            reference: s.reference.clone(),
            mirrors: None,
        });

        let dist = data.dist.as_ref().map(|d| Dist {
            dist_type: d.dist_type.clone().into(),
            url: d.url.clone(),
            reference: d.reference.clone(),
            shasum: d.shasum.clone(),
            sha256: None,
            mirrors: None,
            transport_options: None,
        });

        let mut pkg = Package::new(&data.name, &data.version_normalized);
        pkg.pretty_version = Some(data.version.clone().into());
        pkg.package_type = data.package_type.clone().into();
        pkg.source = source;
        pkg.dist = dist;
        pkg.require = data.require.clone().into();
        pkg.require_dev = data.require_dev.clone().into();
        pkg.conflict = data.conflict.clone().into();
        pkg.replace = data.replace.clone().into();
        pkg.provide = data.provide.clone().into();
        pkg.description = data.description.clone();
        pkg.license = parse_license_value(&data.license)
            .into_iter()
            .map(Into::into)
            .collect();

        pkg.replace_self_version();

        pkg
    }

    /// Convert to installed.json format
    pub fn to_installed_json(&self) -> InstalledPackage {
        let source = self.source.as_ref().map(|s| InstalledSource {
            source_type: s.source_type.to_string(),
            url: s.url.clone(),
            reference: s.reference.clone(),
        });

        let dist = self.dist.as_ref().map(|d| InstalledDist {
            dist_type: d.dist_type.to_string(),
            url: d.url.clone(),
            reference: d.reference.clone(),
            shasum: d.shasum.clone(),
        });

        InstalledPackage {
            name: self.name.clone(),
            version: self
                .pretty_version
                .as_deref()
                .unwrap_or(&self.version)
                .to_string(),
            version_normalized: self.version.to_string(),
            package_type: self.package_type.to_string(),
            source,
            dist,
            require: self
                .require
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            require_dev: self
                .require_dev
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            conflict: self
                .conflict
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            replace: self
                .replace
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            provide: self
                .provide
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            autoload: serde_json::Value::Null,
            description: self.description.clone(),
            license: serde_json::Value::Null,
            time: self.time.map(|t| t.to_rfc3339()),
            install_path: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_json_uses_composer_field_names() {
        let installed: InstalledJson = serde_json::from_str(
            r#"{
                "packages": [{
                    "name": "vendor/package",
                    "version": "1.0.0",
                    "version_normalized": "1.0.0.0",
                    "install-path": "../vendor/package"
                }],
                "dev": true,
                "dev-package-names": ["vendor/package"]
            }"#,
        )
        .unwrap();

        assert_eq!(installed.dev_package_names, ["vendor/package"]);
        assert_eq!(
            installed.packages[0].install_path.as_deref(),
            Some("../vendor/package")
        );

        let serialized = serde_json::to_string(&installed).unwrap();
        assert!(serialized.contains("\"dev-package-names\""));
        assert!(serialized.contains("\"install-path\""));
    }

    #[test]
    fn transaction_projection_keeps_operation_fields_only() {
        let temp = tempfile::tempdir().unwrap();
        let composer_dir = temp.path().join("composer");
        std::fs::create_dir_all(&composer_dir).unwrap();
        std::fs::write(
            composer_dir.join("installed.json"),
            r#"{
                "packages": [{
                    "name": "Vendor/Package",
                    "version": "v1.2.3",
                    "version_normalized": "1.2.3.0",
                    "type": "composer-plugin",
                    "source": {
                        "type": "git",
                        "url": "https://example.test/source.git",
                        "reference": "source-ref"
                    },
                    "dist": {
                        "type": "zip",
                        "url": "https://example.test/archive.zip",
                        "reference": "dist-ref",
                        "shasum": "abc"
                    },
                    "require": {"unused/package": "^1.0"},
                    "autoload": {"psr-4": {"Unused\\\\": "src/"}},
                    "description": "ignored cold metadata",
                    "license": ["MIT"]
                }],
                "dev": true,
                "dev-package-names": ["vendor/package"]
            }"#,
        )
        .unwrap();

        let packages = InstalledRepository::new(temp.path())
            .load_transaction_packages()
            .unwrap();
        let package = packages.first().unwrap();

        assert_eq!(package.name, "vendor/package");
        assert_eq!(package.version, "1.2.3.0");
        assert_eq!(package.pretty_version.as_deref(), Some("v1.2.3"));
        assert_eq!(package.package_type, "composer-plugin");
        assert_eq!(
            package
                .source
                .as_ref()
                .map(|source| source.reference.as_str()),
            Some("source-ref")
        );
        assert_eq!(
            package
                .dist
                .as_ref()
                .and_then(|dist| dist.reference.as_deref()),
            Some("dist-ref")
        );
        assert!(package.require.is_empty());
        assert!(package.description.is_none());
    }
}
