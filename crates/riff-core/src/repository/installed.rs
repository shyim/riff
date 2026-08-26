use async_trait::async_trait;
use compact_str::CompactString;
use indexmap::IndexMap;
use riff_semver::VersionParser;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

use super::traits::{ProviderInfo, Repository, SearchMode, SearchResult, WritableRepository};
use crate::package::{Dist, Funding, Package, Source, Support};

/// Repository for installed packages (vendor/composer/installed.json)
pub struct InstalledRepository {
    /// Path to the vendor directory
    vendor_dir: PathBuf,
    /// Installed packages
    packages: RwLock<HashMap<String, Arc<Package>>>,
    /// Canonical names of installed development packages.
    dev_package_names: RwLock<HashSet<String>>,
    /// Whether development requirements were installed.
    dev_mode: AtomicBool,
    /// Installation paths supplied by the installation manager.
    install_paths: RwLock<HashMap<String, PathBuf>>,
    /// Whether the repository has been modified
    dirty: AtomicBool,
}

impl InstalledRepository {
    /// Create a new installed repository
    pub fn new(vendor_dir: impl Into<PathBuf>) -> Self {
        Self {
            vendor_dir: vendor_dir.into(),
            packages: RwLock::new(HashMap::new()),
            dev_package_names: RwLock::new(HashSet::new()),
            dev_mode: AtomicBool::new(true),
            install_paths: RwLock::new(HashMap::new()),
            dirty: AtomicBool::new(false),
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

        let value: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse installed.json: {}", e))?;
        let data = if value.is_array() {
            InstalledJson {
                packages: serde_json::from_value(value)
                    .map_err(|e| format!("Failed to parse installed.json: {}", e))?,
                dev: false,
                dev_package_names: Vec::new(),
            }
        } else {
            serde_json::from_value(value)
                .map_err(|e| format!("Failed to parse installed.json: {}", e))?
        };

        let InstalledJson {
            packages: installed_packages,
            dev,
            dev_package_names,
        } = data;
        let mut packages = self.packages.write().await;
        packages.clear();

        for pkg_data in installed_packages {
            let package = Package::from_installed_json(&pkg_data);
            packages.insert(package.name.clone(), Arc::new(package));
        }
        let mut installed_dev_packages = self.dev_package_names.write().await;
        *installed_dev_packages = dev_package_names
            .into_iter()
            .map(|name| name.to_lowercase())
            .collect();
        self.dev_mode.store(dev, Ordering::Release);
        self.dirty.store(false, Ordering::Release);

        Ok(())
    }

    /// Load only the fields required to plan and execute package operations.
    pub fn load_transaction_packages(&self) -> Result<Vec<Arc<Package>>, String> {
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

    /// Set the installed development packages. Names that are not present in
    /// the repository are omitted when the repository is written.
    pub async fn set_dev_package_names(&self, names: impl IntoIterator<Item = String>) {
        let mut dev_package_names = self.dev_package_names.write().await;
        *dev_package_names = names.into_iter().map(|name| name.to_lowercase()).collect();
        self.dirty.store(true, Ordering::Release);
    }

    /// Set whether development requirements were installed.
    pub fn set_dev_mode(&self, dev_mode: bool) {
        self.dev_mode.store(dev_mode, Ordering::Release);
        self.dirty.store(true, Ordering::Release);
    }

    /// Record the installation path used for an installed package.
    pub async fn set_install_path(&self, package_name: &str, path: impl Into<PathBuf>) {
        self.install_paths
            .write()
            .await
            .insert(package_name.to_lowercase(), path.into());
        self.dirty.store(true, Ordering::Release);
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
        self.dirty.store(true, Ordering::Release);
    }

    async fn remove_package(&mut self, package: &Package) {
        let mut packages = self.packages.write().await;
        packages.remove(&package.name.to_lowercase());
        self.dirty.store(true, Ordering::Release);
    }

    fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    async fn write(&self) -> std::io::Result<()> {
        let packages = self.packages.read().await;
        let dev_package_names = self.dev_package_names.read().await;
        let install_paths = self.install_paths.read().await;
        let composer_dir = self
            .installed_json_path()
            .parent()
            .expect("installed.json always has a composer parent")
            .to_path_buf();

        let mut serialized_packages: Vec<_> = packages
            .values()
            .map(|package| {
                let mut installed = package.to_installed_json();
                if package.package_type != "metapackage" {
                    let install_path = install_paths
                        .get(&package.name)
                        .cloned()
                        .unwrap_or_else(|| self.vendor_dir.join(&package.name));
                    installed.install_path = pathdiff::diff_paths(install_path, &composer_dir)
                        .map(|path| path.to_string_lossy().replace('\\', "/"));
                }
                installed
            })
            .collect();
        serialized_packages.sort_by(|left, right| left.name.cmp(&right.name));
        let mut installed_dev_package_names = packages
            .keys()
            .filter(|name| dev_package_names.contains(*name))
            .cloned()
            .collect::<Vec<_>>();
        installed_dev_package_names.sort();
        let installed = InstalledJson {
            packages: serialized_packages,
            dev: self.dev_mode.load(Ordering::Acquire),
            dev_package_names: installed_dev_package_names,
        };

        let content = serde_json::to_string_pretty(&installed).map_err(std::io::Error::other)?;

        let path = self.installed_json_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(path, content)?;
        self.dirty.store(false, Ordering::Release);

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
    pub suggest: IndexMap<String, String>,
    #[serde(default)]
    pub autoload: serde_json::Value,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default, deserialize_with = "deserialize_installed_support")]
    pub support: IndexMap<String, String>,
    #[serde(default)]
    pub funding: Vec<Funding>,
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

fn deserialize_installed_support<'de, D>(
    deserializer: D,
) -> Result<IndexMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Null => Ok(IndexMap::new()),
        serde_json::Value::Array(values) if values.is_empty() => Ok(IndexMap::new()),
        serde_json::Value::Object(values) => serde_json::from_value(values.into())
            .map_err(|error| D::Error::custom(error.to_string())),
        _ => Err(D::Error::custom(
            "installed package support must be an object or empty array",
        )),
    }
}

fn support_from_installed(values: &IndexMap<String, String>) -> Option<Support> {
    if values.is_empty() {
        return None;
    }
    Some(Support {
        issues: values.get("issues").cloned(),
        forum: values.get("forum").cloned(),
        wiki: values.get("wiki").cloned(),
        source: values.get("source").cloned(),
        email: values.get("email").cloned(),
        irc: values.get("irc").cloned(),
        docs: values.get("docs").cloned(),
        rss: values.get("rss").cloned(),
        chat: values.get("chat").cloned(),
        security: values.get("security").cloned(),
    })
}

fn support_to_installed(support: Option<&Support>) -> IndexMap<String, String> {
    let Some(support) = support else {
        return IndexMap::new();
    };
    [
        ("issues", support.issues.as_ref()),
        ("forum", support.forum.as_ref()),
        ("wiki", support.wiki.as_ref()),
        ("source", support.source.as_ref()),
        ("email", support.email.as_ref()),
        ("irc", support.irc.as_ref()),
        ("docs", support.docs.as_ref()),
        ("rss", support.rss.as_ref()),
        ("chat", support.chat.as_ref()),
        ("security", support.security.as_ref()),
    ]
    .into_iter()
    .filter_map(|(key, value)| value.map(|value| (key.to_string(), value.clone())))
    .collect()
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

        let normalized_version = if data.version_normalized.is_empty() {
            VersionParser::new()
                .normalize(&data.version)
                .unwrap_or_else(|_| data.version.clone())
        } else {
            data.version_normalized.clone()
        };
        let mut pkg = Package::new(&data.name, normalized_version);
        pkg.pretty_version = Some(data.version.clone().into());
        pkg.package_type = data.package_type.clone().into();
        pkg.source = source;
        pkg.dist = dist;
        pkg.require = data.require.clone().into();
        pkg.require_dev = data.require_dev.clone().into();
        pkg.conflict = data.conflict.clone().into();
        pkg.replace = data.replace.clone().into();
        pkg.provide = data.provide.clone().into();
        pkg.suggest = data.suggest.clone().into();
        pkg.description = data.description.clone();
        pkg.homepage = data.homepage.clone();
        pkg.support = support_from_installed(&data.support);
        pkg.funding.clone_from(&data.funding);
        pkg.license = parse_license_value(&data.license)
            .into_iter()
            .map(Into::into)
            .collect();
        pkg.time = data.time.as_deref().and_then(|time| {
            chrono::DateTime::parse_from_rfc3339(time)
                .ok()
                .map(|time| time.with_timezone(&chrono::Utc))
        });

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
            suggest: self
                .suggest
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            autoload: serde_json::Value::Null,
            description: self.description.clone(),
            homepage: self.homepage.clone(),
            support: support_to_installed(self.support.as_ref()),
            funding: self.funding.clone(),
            license: serde_json::Value::Null,
            time: self.time.map(|t| t.to_rfc3339()),
            install_path: None,
        }
    }
}

/// Parse a Composer `installed.php` file without executing PHP code.
///
/// Composer may inspect this file during startup before a project's dependencies
/// are trusted. The accepted grammar is deliberately limited to the values
/// emitted by Composer and Riff: arrays, scalar literals, string
/// concatenation, and `__DIR__`-relative strings.
pub fn safely_load_installed_versions(path: impl AsRef<Path>) -> Option<serde_json::Value> {
    let path = path.as_ref();
    let contents = std::fs::read(path).ok()?;
    let directory = path.parent().unwrap_or_else(|| Path::new(""));
    InstalledPhpParser::new(&contents, directory).parse()
}

struct InstalledPhpParser<'a> {
    input: &'a [u8],
    position: usize,
    directory: &'a Path,
}

impl<'a> InstalledPhpParser<'a> {
    const MAX_DEPTH: usize = 128;

    fn new(input: &'a [u8], directory: &'a Path) -> Self {
        Self {
            input,
            position: 0,
            directory,
        }
    }

    fn parse(mut self) -> Option<serde_json::Value> {
        self.skip_whitespace();
        self.consume(b"<?php")?;
        self.require_whitespace()?;
        self.consume(b"return")?;
        self.require_whitespace()?;
        let value = self.parse_value(0)?;
        self.skip_whitespace();
        self.consume(b";")?;
        self.skip_whitespace();
        (self.position == self.input.len()).then_some(value)
    }

    fn parse_value(&mut self, depth: usize) -> Option<serde_json::Value> {
        if depth > Self::MAX_DEPTH {
            return None;
        }
        self.skip_whitespace();
        if self.starts_with(b"array") {
            return self.parse_array(depth);
        }
        if self.starts_with(b"__DIR__") || matches!(self.peek(), Some(b'\'' | b'"')) {
            return self
                .parse_string_expression(true)
                .map(serde_json::Value::String);
        }
        if self.consume_keyword(b"true") {
            return Some(serde_json::Value::Bool(true));
        }
        if self.consume_keyword(b"false") {
            return Some(serde_json::Value::Bool(false));
        }
        if self.consume_keyword(b"null") {
            return Some(serde_json::Value::Null);
        }
        self.parse_number().map(serde_json::Value::Number)
    }

    fn parse_array(&mut self, depth: usize) -> Option<serde_json::Value> {
        self.consume(b"array")?;
        self.skip_whitespace();
        self.consume(b"(")?;
        self.skip_whitespace();

        let mut entries = Vec::new();
        let mut next_index = 0usize;
        while !self.starts_with(b")") {
            let checkpoint = self.position;
            let possible_key = self.parse_key();
            self.skip_whitespace();
            let key = if possible_key.is_some() && self.starts_with(b"=>") {
                self.consume(b"=>")?;
                let key = possible_key?;
                if let Ok(index) = key.parse::<usize>() {
                    next_index = next_index.max(index.saturating_add(1));
                }
                key
            } else {
                self.position = checkpoint;
                let key = next_index.to_string();
                next_index += 1;
                key
            };
            let value = self.parse_value(depth + 1)?;
            self.skip_whitespace();
            if self.starts_with(b",") {
                self.consume(b",")?;
            } else if !self.starts_with(b")") {
                return None;
            }
            self.skip_whitespace();
            entries.push((key, value));
        }
        self.consume(b")")?;

        if entries
            .iter()
            .enumerate()
            .all(|(index, (key, _))| key == &index.to_string())
        {
            return Some(serde_json::Value::Array(
                entries.into_iter().map(|(_, value)| value).collect(),
            ));
        }

        let mut object = serde_json::Map::new();
        for (key, value) in entries {
            object.insert(key, value);
        }
        Some(serde_json::Value::Object(object))
    }

    fn parse_key(&mut self) -> Option<String> {
        self.skip_whitespace();
        if matches!(self.peek(), Some(b'\'' | b'"')) {
            return self.parse_string_expression(false);
        }

        let start = self.position;
        if self.peek() == Some(b'-') {
            self.position += 1;
            self.skip_whitespace();
        }
        let digits = self.position;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.position += 1;
        }
        (self.position > digits).then(|| {
            String::from_utf8_lossy(&self.input[start..self.position])
                .replace(char::is_whitespace, "")
        })
    }

    fn parse_number(&mut self) -> Option<serde_json::Number> {
        let negative = if self.peek() == Some(b'-') {
            self.position += 1;
            self.skip_whitespace();
            true
        } else {
            false
        };
        let start = self.position;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.position += 1;
        }
        if self.position == start {
            return None;
        }
        let integer_end = self.position;
        if self.peek() == Some(b'.') {
            self.position += 1;
            let fraction_start = self.position;
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.position += 1;
            }
            if self.position == fraction_start {
                return None;
            }
            let mut literal = String::new();
            if negative {
                literal.push('-');
            }
            literal.push_str(std::str::from_utf8(&self.input[start..self.position]).ok()?);
            return literal
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64);
        }

        let digits = std::str::from_utf8(&self.input[start..integer_end]).ok()?;
        if negative {
            digits
                .parse::<i64>()
                .ok()
                .map(|number| serde_json::Number::from(-number))
        } else {
            digits.parse::<u64>().ok().map(serde_json::Number::from)
        }
    }

    fn parse_string_expression(&mut self, allow_directory: bool) -> Option<String> {
        self.skip_whitespace();
        let directory = if allow_directory && self.starts_with(b"__DIR__") {
            self.consume(b"__DIR__")?;
            self.skip_whitespace();
            self.consume(b".")?;
            self.skip_whitespace();
            Some(self.directory.to_string_lossy().into_owned())
        } else {
            None
        };

        let mut value = directory.unwrap_or_default();
        value.push_str(&self.parse_string_literal()?);
        loop {
            let checkpoint = self.position;
            self.skip_whitespace();
            if !self.starts_with(b".") {
                self.position = checkpoint;
                break;
            }
            self.consume(b".")?;
            self.skip_whitespace();
            if !matches!(self.peek(), Some(b'\'' | b'"')) {
                return None;
            }
            value.push_str(&self.parse_string_literal()?);
        }
        Some(value)
    }

    fn parse_string_literal(&mut self) -> Option<String> {
        let quote = self.peek()?;
        if quote != b'\'' && quote != b'"' {
            return None;
        }
        self.position += 1;
        let mut value = Vec::new();
        loop {
            let byte = self.peek()?;
            self.position += 1;
            if byte == quote {
                return String::from_utf8(value).ok();
            }
            if byte == b'\\' {
                let escaped = self.peek()?;
                self.position += 1;
                match (quote, escaped) {
                    (b'\'', b'\'' | b'\\') | (b'"', b'"' | b'\\') => value.push(escaped),
                    (b'"', b'0') => value.push(0),
                    _ => return None,
                }
            } else {
                if quote == b'"' && byte == b'$' {
                    return None;
                }
                value.push(byte);
            }
        }
    }

    fn consume_keyword(&mut self, keyword: &[u8]) -> bool {
        let end = self.position + keyword.len();
        if self
            .input
            .get(self.position..end)
            .is_none_or(|candidate| !candidate.eq_ignore_ascii_case(keyword))
        {
            return false;
        }
        if self
            .input
            .get(end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return false;
        }
        self.position = end;
        true
    }

    fn require_whitespace(&mut self) -> Option<()> {
        let start = self.position;
        self.skip_whitespace();
        (self.position > start).then_some(())
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.position += 1;
        }
    }

    fn starts_with(&self, value: &[u8]) -> bool {
        self.input[self.position..].starts_with(value)
    }

    fn consume(&mut self, value: &[u8]) -> Option<()> {
        self.starts_with(value)
            .then(|| self.position += value.len())
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
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
    fn installed_package_round_trips_discovery_metadata() {
        let installed: InstalledPackage = serde_json::from_value(serde_json::json!({
            "name": "vendor/package",
            "version": "1.2.3",
            "version_normalized": "1.2.3.0",
            "homepage": "https://example.org/project",
            "support": {"source": "https://example.org/source", "issues": "https://example.org/issues"},
            "funding": [{"type": "github", "url": "https://github.com/example"}]
        }))
        .unwrap();

        let package = Package::from_installed_json(&installed);
        let round_trip = package.to_installed_json();

        assert_eq!(
            package.homepage.as_deref(),
            Some("https://example.org/project")
        );
        assert_eq!(
            package
                .support
                .as_ref()
                .and_then(|support| support.source.as_deref()),
            Some("https://example.org/source")
        );
        assert_eq!(package.funding[0].funding_type.as_deref(), Some("github"));
        assert_eq!(round_trip.homepage, installed.homepage);
        assert_eq!(round_trip.support, installed.support);
        assert_eq!(round_trip.funding, installed.funding);

        let empty_array_support: InstalledPackage = serde_json::from_value(serde_json::json!({
            "name": "vendor/legacy",
            "version": "1.0.0",
            "support": []
        }))
        .unwrap();
        assert!(empty_array_support.support.is_empty());
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

    #[tokio::test]
    async fn composer_array_repository_adds_package() {
        let temp = tempfile::tempdir().unwrap();
        let mut repository = InstalledRepository::new(temp.path());

        repository
            .add_package(Package::new("foo/package", "1.0.0.0"))
            .await;

        assert_eq!(repository.count().await, 1);
        assert!(repository.is_dirty());
    }

    #[tokio::test]
    async fn composer_array_repository_removes_package() {
        let temp = tempfile::tempdir().unwrap();
        let mut repository = InstalledRepository::new(temp.path());
        let foo = Package::new("foo/package", "1.0.0.0");
        let bar = Package::new("bar/package", "2.0.0.0");
        repository.add_package(foo.clone()).await;
        repository.add_package(bar).await;
        assert_eq!(repository.count().await, 2);

        repository.remove_package(&foo).await;

        assert_eq!(repository.count().await, 1);
        let packages = repository.get_packages().await;
        assert_eq!(packages[0].name, "bar/package");
    }

    #[tokio::test]
    async fn composer_filesystem_repository_reads_legacy_package_array() {
        let temp = tempfile::tempdir().unwrap();
        let composer_dir = temp.path().join("composer");
        std::fs::create_dir_all(&composer_dir).unwrap();
        std::fs::write(
            composer_dir.join("installed.json"),
            r#"[{
                "name": "package1",
                "version": "1.0.0-beta",
                "type": "vendor"
            }]"#,
        )
        .unwrap();
        let repository = InstalledRepository::new(temp.path());

        repository.load().await.unwrap();
        let packages = repository.get_packages().await;

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "package1");
        assert_eq!(packages[0].version, "1.0.0.0-beta");
        assert_eq!(packages[0].package_type, "vendor");
        assert!(!repository.is_dirty());
    }

    #[tokio::test]
    async fn composer_filesystem_repository_rejects_corrupted_file() {
        let temp = tempfile::tempdir().unwrap();
        let composer_dir = temp.path().join("composer");
        std::fs::create_dir_all(&composer_dir).unwrap();
        std::fs::write(composer_dir.join("installed.json"), r#""foo""#).unwrap();
        let repository = InstalledRepository::new(temp.path());

        let error = repository.load().await.unwrap_err();

        assert!(error.starts_with("Failed to parse installed.json:"));
        assert!(repository.get_packages().await.is_empty());
    }

    #[tokio::test]
    async fn composer_filesystem_repository_missing_file_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        let repository = InstalledRepository::new(temp.path());

        repository.load().await.unwrap();

        assert!(repository.get_packages().await.is_empty());
        assert_eq!(repository.count().await, 0);
    }

    #[tokio::test]
    async fn composer_filesystem_repository_writes_packages_paths_and_dev_names() {
        let temp = tempfile::tempdir().unwrap();
        let mut repository = InstalledRepository::new(temp.path());
        let mut first = Package::new("mypkg", "0.1.10.0");
        first.pretty_version = Some("0.1.10".into());
        let mut second = Package::new("mypkg2", "1.2.3.0");
        second.pretty_version = Some("1.2.3".into());
        repository.add_package(second).await;
        repository.add_package(first).await;
        repository
            .set_dev_package_names(["mypkg2".to_string(), "missing/package".to_string()])
            .await;
        repository.set_dev_mode(true);
        let install_path = temp.path().join("woop/woop");
        repository
            .set_install_path("mypkg", install_path.clone())
            .await;
        repository.set_install_path("mypkg2", install_path).await;

        repository.write().await.unwrap();

        let installed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(repository.installed_json_path()).unwrap())
                .unwrap();
        assert_eq!(installed["dev"], true);
        assert_eq!(
            installed["dev-package-names"],
            serde_json::json!(["mypkg2"])
        );
        assert_eq!(installed["packages"][0]["name"], "mypkg");
        assert_eq!(installed["packages"][0]["version"], "0.1.10");
        assert_eq!(installed["packages"][0]["version_normalized"], "0.1.10.0");
        assert_eq!(installed["packages"][0]["install-path"], "../woop/woop");
        assert_eq!(installed["packages"][1]["name"], "mypkg2");
        assert_eq!(installed["packages"][1]["install-path"], "../woop/woop");
        assert!(!repository.is_dirty());
    }

    #[test]
    fn composer_filesystem_repository_safely_loads_installed_versions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("installed.php");
        std::fs::write(
            &path,
            r#"<?php return array(
    'root' => array(
        'install_path' => __DIR__ . '/./',
        'aliases' => array(0 => '1.10.x-dev', 1 => '2.10.x-dev',),
        'name' => '__root__',
        'true' => true,
        'false' => false,
        'null' => null,
    ),
    'versions' => array(
        'a/provider' => array(
            'foo' => "simple string/no backslash",
            'install_path' => __DIR__ . '/vendor/{${passthru(\'bash -i\')}}',
            'empty array' => array(),
        ),
        'c/c' => array(
            'install_path' => '/foo/bar/ven/do{}r/c/c${}',
            'aliases' => array(),
            'reference' => '{${passthru(\'bash -i\')}} Foo\\Bar' . "\0" . '',
        ),
    ),
);"#,
        )
        .unwrap();

        let data = safely_load_installed_versions(&path).expect("fixture must be safe");

        assert_eq!(
            data["root"]["install_path"],
            format!("{}/./", temp.path().display())
        );
        assert_eq!(
            data["root"]["aliases"],
            serde_json::json!(["1.10.x-dev", "2.10.x-dev"])
        );
        assert_eq!(data["root"]["true"], true);
        assert_eq!(data["root"]["false"], false);
        assert!(data["root"]["null"].is_null());
        assert_eq!(
            data["versions"]["a/provider"]["install_path"],
            format!(
                "{}/vendor/{{${{passthru('bash -i')}}}}",
                temp.path().display()
            )
        );
        assert_eq!(
            data["versions"]["c/c"]["reference"],
            "{${passthru('bash -i')}} Foo\\Bar\0"
        );
    }

    #[test]
    fn installed_versions_safe_loader_rejects_executable_php() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("installed.php");
        std::fs::write(
            &path,
            "<?php passthru('touch /tmp/unsafe'); return array();",
        )
        .unwrap();

        assert!(safely_load_installed_versions(path).is_none());
    }

    #[tokio::test]
    async fn installed_repository_write_clears_dirty_and_sorts_packages() {
        let temp = tempfile::tempdir().unwrap();
        let mut repository = InstalledRepository::new(temp.path());
        repository
            .add_package(Package::new("z/package", "1.0.0.0"))
            .await;
        repository
            .add_package(Package::new("a/package", "1.0.0.0"))
            .await;
        assert!(repository.is_dirty());

        repository.write().await.unwrap();

        assert!(!repository.is_dirty());
        let installed: InstalledJson =
            serde_json::from_slice(&std::fs::read(repository.installed_json_path()).unwrap())
                .unwrap();
        assert_eq!(installed.packages[0].name, "a/package");
        assert_eq!(installed.packages[1].name, "z/package");
    }

    #[test]
    fn installed_package_preserves_release_time() {
        let installed: InstalledPackage = serde_json::from_value(serde_json::json!({
            "name": "vendor/package",
            "version": "1.0.0",
            "version_normalized": "1.0.0.0",
            "time": "2024-08-25T00:00:00+00:00"
        }))
        .unwrap();
        let package = Package::from_installed_json(&installed);
        assert_eq!(
            package.time.unwrap().to_rfc3339(),
            "2024-08-25T00:00:00+00:00"
        );
    }
}
