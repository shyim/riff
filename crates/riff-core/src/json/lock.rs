use indexmap::IndexMap;
use md5::{Digest, Md5};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;

/// Deserializes a HashMap that might be represented as an empty array in JSON.
/// Composer outputs `[]` for empty maps like stability-flags, platform-dev, etc.
fn deserialize_map_or_empty_array<'de, D, K, V>(deserializer: D) -> Result<HashMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Deserialize<'de> + std::hash::Hash + Eq,
    V: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    #[serde(bound(
        deserialize = "K: Deserialize<'de> + std::hash::Hash + Eq, V: Deserialize<'de>"
    ))]
    enum MapOrArray<K, V> {
        Map(HashMap<K, V>),
        #[allow(dead_code)]
        Array(Vec<serde_json::Value>),
    }

    match MapOrArray::deserialize(deserializer)? {
        MapOrArray::Map(map) => Ok(map),
        MapOrArray::Array(_) => Ok(HashMap::new()),
    }
}

/// Deserializes an IndexMap that might be represented as an empty array in JSON.
/// Composer outputs `[]` for empty maps like stability-flags, platform-dev, etc.
fn deserialize_indexmap_or_empty_array<'de, D, K, V>(
    deserializer: D,
) -> Result<IndexMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Deserialize<'de> + std::hash::Hash + Eq,
    V: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    #[serde(bound(
        deserialize = "K: Deserialize<'de> + std::hash::Hash + Eq, V: Deserialize<'de>"
    ))]
    enum MapOrArray<K, V> {
        Map(IndexMap<K, V>),
        #[allow(dead_code)]
        Array(Vec<serde_json::Value>),
    }

    match MapOrArray::deserialize(deserializer)? {
        MapOrArray::Map(map) => Ok(map),
        MapOrArray::Array(_) => Ok(IndexMap::new()),
    }
}

// Old implementation (kept for HashMap fields)
fn _old_deserialize_map_or_empty_array<'de, D, K, V>(
    deserializer: D,
) -> Result<HashMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Deserialize<'de> + std::hash::Hash + Eq,
    V: Deserialize<'de>,
{
    use serde::de::{self, MapAccess, SeqAccess, Visitor};
    use std::marker::PhantomData;

    struct MapOrEmptyArrayVisitor<K, V> {
        marker: PhantomData<HashMap<K, V>>,
    }

    impl<'de, K, V> Visitor<'de> for MapOrEmptyArrayVisitor<K, V>
    where
        K: Deserialize<'de> + std::hash::Hash + Eq,
        V: Deserialize<'de>,
    {
        type Value = HashMap<K, V>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a map or an empty array")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            // Only accept empty arrays
            if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(de::Error::custom(
                    "expected empty array or map, got non-empty array",
                ));
            }
            Ok(HashMap::new())
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut result = HashMap::new();
            while let Some((key, value)) = map.next_entry()? {
                result.insert(key, value);
            }
            Ok(result)
        }
    }

    deserializer.deserialize_any(MapOrEmptyArrayVisitor {
        marker: PhantomData,
    })
}

/// Represents a composer.lock file
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RiffLockfile {
    /// Readme note
    #[serde(
        default = "default_readme",
        rename = "_readme",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub readme: Vec<String>,

    /// Content hash for detecting changes
    #[serde(default)]
    pub content_hash: String,

    /// Legacy Composer 1 whole-file hash, accepted for freshness checks but
    /// never emitted when Riff rewrites the lock file.
    #[serde(default, rename = "hash", skip_serializing)]
    pub legacy_hash: String,

    /// Production packages
    #[serde(default)]
    pub packages: Vec<LockedPackage>,

    /// Development packages
    #[serde(default, rename = "packages-dev")]
    pub packages_dev: Vec<LockedPackage>,

    /// Package aliases
    #[serde(default)]
    pub aliases: Vec<LockAlias>,

    /// Minimum stability
    #[serde(default)]
    pub minimum_stability: String,

    /// Per-package stability flags
    #[serde(default, deserialize_with = "deserialize_map_or_empty_array")]
    pub stability_flags: HashMap<String, u8>,

    /// Whether to prefer stable versions
    #[serde(default)]
    pub prefer_stable: bool,

    /// Whether to prefer lowest versions
    #[serde(default)]
    pub prefer_lowest: bool,

    /// Platform requirements
    #[serde(default, deserialize_with = "deserialize_indexmap_or_empty_array")]
    pub platform: IndexMap<String, String>,

    /// Platform dev requirements
    #[serde(default, deserialize_with = "deserialize_indexmap_or_empty_array")]
    pub platform_dev: IndexMap<String, String>,

    /// Platform overrides from config
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "deserialize_indexmap_or_empty_array"
    )]
    pub platform_overrides: IndexMap<String, String>,

    /// Plugin API version used to generate this lock file
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plugin_api_version: String,
}

fn default_readme() -> Vec<String> {
    vec![
        "This file locks the dependencies of your project to a known state".to_string(),
        "Read more about it at https://getcomposer.org/doc/01-basic-usage.md#installing-dependencies".to_string(),
        "This file is @generated automatically".to_string(),
    ]
}

impl Default for RiffLockfile {
    fn default() -> Self {
        Self {
            readme: default_readme(),
            content_hash: String::new(),
            legacy_hash: String::new(),
            packages: Vec::new(),
            packages_dev: Vec::new(),
            aliases: Vec::new(),
            minimum_stability: String::new(),
            stability_flags: HashMap::new(),
            prefer_stable: false,
            prefer_lowest: false,
            platform: IndexMap::new(),
            platform_dev: IndexMap::new(),
            platform_overrides: IndexMap::new(),
            plugin_api_version: String::new(),
        }
    }
}

/// A locked package entry
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct LockedPackage {
    /// Package name (vendor/package)
    pub name: String,

    /// Version string
    pub version: String,

    /// Source information (VCS)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<LockSource>,

    /// Distribution information (archive)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dist: Option<LockDist>,

    /// Required packages
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "deserialize_indexmap_or_empty_array"
    )]
    pub require: IndexMap<String, String>,

    /// Conflicts
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "deserialize_indexmap_or_empty_array"
    )]
    pub conflict: IndexMap<String, String>,

    /// Provided packages
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "deserialize_indexmap_or_empty_array"
    )]
    pub provide: IndexMap<String, String>,

    /// Replaced packages
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "deserialize_indexmap_or_empty_array"
    )]
    pub replace: IndexMap<String, String>,

    /// Development requirements
    #[serde(
        default,
        rename = "require-dev",
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "deserialize_indexmap_or_empty_array"
    )]
    pub require_dev: IndexMap<String, String>,

    /// Suggested packages
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "deserialize_indexmap_or_empty_array"
    )]
    pub suggest: IndexMap<String, String>,

    /// Binary executables
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bin: Vec<String>,

    /// Package type
    #[serde(rename = "type", default = "default_type")]
    pub package_type: String,

    /// Extra metadata
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,

    /// Autoload configuration
    #[serde(default, skip_serializing_if = "LockAutoload::is_empty")]
    pub autoload: LockAutoload,

    /// Dev autoload configuration
    #[serde(
        default,
        rename = "autoload-dev",
        skip_serializing_if = "LockAutoload::is_empty"
    )]
    pub autoload_dev: LockAutoload,

    /// Packagist notification URL
    #[serde(
        default,
        rename = "notification-url",
        skip_serializing_if = "Option::is_none"
    )]
    pub notification_url: Option<String>,

    /// License(s)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub license: Vec<String>,

    /// Authors
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<LockAuthor>,

    /// Package description
    #[serde(default, skip_serializing_if = "is_none_or_empty")]
    pub description: Option<String>,

    /// Homepage URL
    #[serde(default, skip_serializing_if = "is_none_or_empty")]
    pub homepage: Option<String>,

    /// Keywords for search
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,

    /// Support information
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "deserialize_indexmap_or_empty_array"
    )]
    pub support: IndexMap<String, String>,

    /// Funding information
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub funding: Vec<LockFunding>,

    /// Release time
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,

    /// Whether this is an abandoned package
    #[serde(default, skip_serializing_if = "is_null_or_false")]
    pub abandoned: serde_json::Value,

    /// Archive exclusions
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<LockArchive>,

    /// Installation source preference
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_source: Option<String>,

    /// Default branch flag
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<bool>,

    /// Downloader-specific options, used by path repositories among others.
    #[serde(
        default,
        rename = "transport-options",
        skip_serializing_if = "Option::is_none"
    )]
    pub transport_options: Option<serde_json::Value>,
}

fn is_null_or_false(v: &serde_json::Value) -> bool {
    v.is_null() || v == &serde_json::Value::Bool(false)
}

fn is_none_or_empty(v: &Option<String>) -> bool {
    match v {
        None => true,
        Some(s) => s.is_empty(),
    }
}

fn default_type() -> String {
    "library".to_string()
}

/// Source information for VCS-based packages
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockSource {
    /// Source type (git, hg, svn)
    #[serde(rename = "type")]
    pub source_type: String,

    /// Repository URL
    pub url: String,

    /// Commit/tag/branch reference
    pub reference: String,

    /// Alternative source repository URLs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirrors: Option<Vec<crate::package::Mirror>>,
}

/// Distribution information for archive-based packages
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockDist {
    /// Distribution type (zip, tar, etc.)
    #[serde(rename = "type")]
    pub dist_type: String,

    /// Download URL
    pub url: String,

    /// Reference (optional for dist)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,

    /// SHA sum for verification (empty string when not available)
    #[serde(default, serialize_with = "serialize_shasum")]
    pub shasum: Option<String>,

    /// Alternative distribution URLs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirrors: Option<Vec<crate::package::Mirror>>,
}

fn serialize_shasum<S>(shasum: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    // Composer always outputs shasum field, empty string when not available
    serializer.serialize_str(shasum.as_deref().unwrap_or(""))
}

/// Autoload configuration in lock file
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct LockAutoload {
    /// PSR-4 autoloading
    #[serde(
        default,
        rename = "psr-4",
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "deserialize_indexmap_or_empty_array"
    )]
    pub psr4: IndexMap<String, serde_json::Value>,

    /// PSR-0 autoloading
    #[serde(
        default,
        rename = "psr-0",
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "deserialize_indexmap_or_empty_array"
    )]
    pub psr0: IndexMap<String, serde_json::Value>,

    /// Classmap files/directories
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classmap: Vec<String>,

    /// Files to always include
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,

    /// Paths to exclude from classmap
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_from_classmap: Vec<String>,
}

impl LockAutoload {
    pub fn is_empty(&self) -> bool {
        self.psr4.is_empty()
            && self.psr0.is_empty()
            && self.classmap.is_empty()
            && self.files.is_empty()
    }
}

/// Author information
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockAuthor {
    /// Author name
    pub name: String,

    /// Author email
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,

    /// Author homepage
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,

    /// Author role
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// Funding information
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockFunding {
    /// Funding URL
    pub url: String,

    /// Funding type (github, patreon, custom, etc.)
    #[serde(rename = "type")]
    pub funding_type: String,
}

/// Archive configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockArchive {
    /// Name for the archive
    #[serde(default)]
    pub name: Option<String>,

    /// Paths to exclude from archives
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Package alias
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockAlias {
    /// Package name
    pub package: String,

    /// Original version
    pub version: String,

    /// Aliased version
    pub alias: String,

    /// Normalized aliased version
    pub alias_normalized: String,
}

impl RiffLockfile {
    /// Compare the JSON representation without allocating intermediate values.
    pub fn equivalent_for_write(&self, other: &Self) -> bool {
        self.readme == other.readme
            && self.content_hash == other.content_hash
            && self.packages.len() == other.packages.len()
            && self
                .packages
                .iter()
                .zip(&other.packages)
                .all(|(left, right)| left.equivalent_for_write(right))
            && self.packages_dev.len() == other.packages_dev.len()
            && self
                .packages_dev
                .iter()
                .zip(&other.packages_dev)
                .all(|(left, right)| left.equivalent_for_write(right))
            && self.aliases == other.aliases
            && self.minimum_stability == other.minimum_stability
            && self.stability_flags == other.stability_flags
            && self.prefer_stable == other.prefer_stable
            && self.prefer_lowest == other.prefer_lowest
            && self.platform == other.platform
            && self.platform_dev == other.platform_dev
            && self.platform_overrides == other.platform_overrides
            && self.plugin_api_version == other.plugin_api_version
    }

    /// Parse a composer.lock from JSON string
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(content: &str) -> Result<Self, serde_json::Error> {
        match serde_json::from_str(content) {
            Ok(lock) => Ok(lock),
            Err(error) => recover_content_hash_conflict(content)
                .and_then(|recovered| serde_json::from_str(&recovered).ok())
                .ok_or(error),
        }
    }

    /// Parse a composer.lock from a file path
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self, LockLoadError> {
        let content = std::fs::read_to_string(path.as_ref()).map_err(LockLoadError::Io)?;
        Self::from_str(&content).map_err(LockLoadError::Parse)
    }

    /// Serialize to JSON string
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Get all packages (both prod and dev)
    pub fn all_packages(&self) -> impl Iterator<Item = &LockedPackage> {
        self.packages.iter().chain(self.packages_dev.iter())
    }

    /// Find a package by name
    pub fn find_package(&self, name: &str) -> Option<&LockedPackage> {
        let name_lower = name.to_lowercase();
        self.all_packages()
            .find(|p| p.name.to_lowercase() == name_lower)
    }

    /// Check if a package is a dev dependency
    pub fn is_dev_package(&self, name: &str) -> bool {
        let name_lower = name.to_lowercase();
        self.packages_dev
            .iter()
            .any(|p| p.name.to_lowercase() == name_lower)
    }

    /// Get the total number of locked packages
    pub fn package_count(&self) -> usize {
        self.packages.len() + self.packages_dev.len()
    }

    /// Check whether this lock file was generated from the supplied
    /// composer.json content.
    pub fn is_fresh(&self, manifest_content: &str) -> bool {
        if !self.content_hash.is_empty() {
            return self.content_hash == crate::util::compute_content_hash(manifest_content);
        }
        if self.legacy_hash.is_empty() {
            return false;
        }

        let mut hasher = Md5::new();
        hasher.update(manifest_content.as_bytes());
        self.legacy_hash == format!("{:x}", hasher.finalize())
    }
}

fn recover_content_hash_conflict(content: &str) -> Option<String> {
    const CONFLICT_HASH: &str = "VCS merge conflict detected. Please run `composer update --lock`.";

    let lines = content.lines().collect::<Vec<_>>();
    let starts = marker_lines(&lines, "<<<<<<<");
    let separators = marker_lines(&lines, "=======");
    let ends = marker_lines(&lines, ">>>>>>>");
    let bases = marker_lines(&lines, "|||||||");
    if starts.len() != 1 || separators.len() != 1 || ends.len() != 1 || bases.len() > 1 {
        return None;
    }

    let (start, separator, end) = (starts[0], separators[0], ends[0]);
    if !(start < separator && separator < end)
        || bases
            .first()
            .is_some_and(|&base| !(start < base && base < separator))
    {
        return None;
    }

    let conflict_lines = &lines[start + 1..end];
    let content_lines = conflict_lines
        .iter()
        .filter(|line| !is_conflict_marker(line))
        .copied()
        .collect::<Vec<_>>();
    if content_lines.len() < 2 || !content_lines.iter().all(|line| is_content_hash_line(line)) {
        return None;
    }
    if !lines[start + 1..separator]
        .iter()
        .any(|line| is_content_hash_line(line))
        || !lines[separator + 1..end]
            .iter()
            .any(|line| is_content_hash_line(line))
    {
        return None;
    }

    let indent = content_lines[0]
        .chars()
        .take_while(|character| character.is_whitespace())
        .collect::<String>();
    let conflict_hash = serde_json::to_string(CONFLICT_HASH).ok()?;
    let mut recovered = Vec::with_capacity(lines.len() - (end - start));
    recovered.extend(lines[..start].iter().map(|line| (*line).to_string()));
    recovered.push(format!("{indent}\"content-hash\": {conflict_hash},"));
    recovered.extend(lines[end + 1..].iter().map(|line| (*line).to_string()));
    Some(recovered.join("\n"))
}

fn marker_lines(lines: &[&str], marker: &str) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.trim_start().starts_with(marker).then_some(index))
        .collect()
}

fn is_conflict_marker(line: &str) -> bool {
    ["<<<<<<<", "|||||||", "=======", ">>>>>>>"]
        .iter()
        .any(|marker| line.trim_start().starts_with(marker))
}

fn is_content_hash_line(line: &str) -> bool {
    let line = line.trim().strip_suffix(',').unwrap_or(line.trim());
    let Some((key, value)) = line.split_once(':') else {
        return false;
    };
    key.trim() == "\"content-hash\"" && serde_json::from_str::<String>(value.trim()).is_ok()
}

impl LockedPackage {
    fn equivalent_for_write(&self, other: &Self) -> bool {
        self.name == other.name
            && self.version == other.version
            && self.source == other.source
            && dist_equivalent_for_write(&self.dist, &other.dist)
            && self.require == other.require
            && self.conflict == other.conflict
            && self.provide == other.provide
            && self.replace == other.replace
            && self.require_dev == other.require_dev
            && self.suggest == other.suggest
            && self.bin == other.bin
            && self.package_type == other.package_type
            && self.extra == other.extra
            && autoload_equivalent_for_write(&self.autoload, &other.autoload)
            && autoload_equivalent_for_write(&self.autoload_dev, &other.autoload_dev)
            && self.notification_url == other.notification_url
            && self.license == other.license
            && self.authors == other.authors
            && optional_nonempty_equivalent(&self.description, &other.description)
            && optional_nonempty_equivalent(&self.homepage, &other.homepage)
            && self.keywords == other.keywords
            && self.support == other.support
            && self.funding == other.funding
            && self.time == other.time
            && abandoned_equivalent_for_write(&self.abandoned, &other.abandoned)
            && self.archive == other.archive
            && self.installation_source == other.installation_source
            && self.default_branch == other.default_branch
            && self.transport_options == other.transport_options
    }

    /// Get the best download URL (prefer dist over source)
    pub fn download_url(&self) -> Option<&str> {
        self.dist
            .as_ref()
            .map(|d| d.url.as_str())
            .or_else(|| self.source.as_ref().map(|s| s.url.as_str()))
    }

    /// Get the reference (commit hash, tag, etc.)
    pub fn reference(&self) -> Option<&str> {
        self.dist
            .as_ref()
            .and_then(|d| d.reference.as_deref())
            .or_else(|| self.source.as_ref().map(|s| s.reference.as_str()))
    }

    /// Check if this is an abandoned package
    pub fn is_abandoned(&self) -> bool {
        match &self.abandoned {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::String(_) => true,
            _ => false,
        }
    }

    /// Get the replacement package name if abandoned
    pub fn abandoned_replacement(&self) -> Option<&str> {
        match &self.abandoned {
            serde_json::Value::String(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        }
    }
}

fn dist_equivalent_for_write(left: &Option<LockDist>, right: &Option<LockDist>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            left.dist_type == right.dist_type
                && left.url == right.url
                && left.reference == right.reference
                && left.shasum.as_deref().unwrap_or_default()
                    == right.shasum.as_deref().unwrap_or_default()
        }
        (None, None) => true,
        _ => false,
    }
}

fn autoload_equivalent_for_write(left: &LockAutoload, right: &LockAutoload) -> bool {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => true,
        (false, false) => left == right,
        _ => false,
    }
}

fn optional_nonempty_equivalent(left: &Option<String>, right: &Option<String>) -> bool {
    left.as_deref().filter(|value| !value.is_empty())
        == right.as_deref().filter(|value| !value.is_empty())
}

fn abandoned_equivalent_for_write(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    (is_null_or_false(left) && is_null_or_false(right)) || left == right
}

/// Errors that can occur when loading a lock file
#[derive(Debug)]
pub enum LockLoadError {
    Io(std::io::Error),
    Parse(serde_json::Error),
}

impl std::fmt::Display for LockLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockLoadError::Io(e) => write!(f, "Failed to read lock file: {}", e),
            LockLoadError::Parse(e) => write!(f, "Failed to parse lock file: {}", e),
        }
    }
}

impl std::error::Error for LockLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LockLoadError::Io(e) => Some(e),
            LockLoadError::Parse(e) => Some(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const COMPOSER_MANIFEST: &str = r#"{"minimum-stability":"beta","name":"test"}"#;

    fn assert_equivalence_matches_json(left: &RiffLockfile, right: &RiffLockfile) {
        assert_eq!(
            left.equivalent_for_write(right),
            serde_json::to_value(left).unwrap() == serde_json::to_value(right).unwrap()
        );
    }

    fn locked_package_strategy() -> impl Strategy<Value = LockedPackage> {
        (
            "[a-z][a-z0-9-]{0,7}/[a-z][a-z0-9-]{0,11}",
            (0u16..20, 0u16..20, 0u16..20),
            proptest::collection::vec(
                (
                    "[a-z][a-z0-9-]{0,7}/[a-z][a-z0-9-]{0,11}",
                    (0u16..20, 0u16..20),
                ),
                0..8,
            ),
            prop_oneof![Just("library"), Just("composer-plugin"), Just("project")],
            proptest::option::of("[a-zA-Z0-9 ._-]{1,40}"),
        )
            .prop_map(|(name, version, requirements, package_type, description)| {
                let require = requirements
                    .into_iter()
                    .map(|(name, (major, minor))| (name, format!("^{major}.{minor}")))
                    .collect();
                LockedPackage {
                    name,
                    version: format!("{}.{}.{}", version.0, version.1, version.2),
                    require,
                    package_type: package_type.to_string(),
                    description,
                    ..Default::default()
                }
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn composer_lock_roundtrips_generated_package_graphs(
            packages in proptest::collection::vec(locked_package_strategy(), 0..20),
            packages_dev in proptest::collection::vec(locked_package_strategy(), 0..12),
            prefer_stable in any::<bool>(),
            prefer_lowest in any::<bool>(),
        ) {
            let lock = RiffLockfile {
                content_hash: "generated-content-hash".to_string(),
                packages,
                packages_dev,
                minimum_stability: "dev".to_string(),
                prefer_stable,
                prefer_lowest,
                ..Default::default()
            };

            let serialized = lock.to_json().expect("serialize generated lock file");
            let reparsed = RiffLockfile::from_str(&serialized)
                .expect("parse generated lock file");

            prop_assert!(lock.equivalent_for_write(&reparsed));
        }
    }

    #[test]
    fn test_parse_minimal_lock() {
        let json = r#"{
            "content-hash": "abc123",
            "packages": [],
            "packages-dev": []
        }"#;

        let lock = RiffLockfile::from_str(json).unwrap();
        assert_eq!(lock.content_hash, "abc123");
        assert!(lock.packages.is_empty());
        assert!(lock.packages_dev.is_empty());
    }

    #[test]
    fn test_parse_package() {
        let json = r#"{
            "content-hash": "abc123",
            "packages": [{
                "name": "vendor/package",
                "version": "1.0.0",
                "source": {
                    "type": "git",
                    "url": "https://github.com/vendor/package.git",
                    "reference": "abc123def"
                },
                "dist": {
                    "type": "zip",
                    "url": "https://example.com/package.zip",
                    "shasum": "sha256hash"
                },
                "require": {
                    "php": ">=8.0"
                },
                "type": "library",
                "description": "A test package"
            }],
            "packages-dev": []
        }"#;

        let lock = RiffLockfile::from_str(json).unwrap();
        assert_eq!(lock.packages.len(), 1);

        let pkg = &lock.packages[0];
        assert_eq!(pkg.name, "vendor/package");
        assert_eq!(pkg.version, "1.0.0");
        assert_eq!(pkg.package_type, "library");

        let source = pkg.source.as_ref().unwrap();
        assert_eq!(source.source_type, "git");
        assert_eq!(source.reference, "abc123def");

        let dist = pkg.dist.as_ref().unwrap();
        assert_eq!(dist.dist_type, "zip");
    }

    #[test]
    fn test_find_package() {
        let json = r#"{
            "content-hash": "abc",
            "packages": [{"name": "vendor/prod", "version": "1.0.0"}],
            "packages-dev": [{"name": "vendor/dev", "version": "2.0.0"}]
        }"#;

        let lock = RiffLockfile::from_str(json).unwrap();

        assert!(lock.find_package("vendor/prod").is_some());
        assert!(lock.find_package("vendor/dev").is_some());
        assert!(lock.find_package("VENDOR/PROD").is_some()); // case-insensitive
        assert!(lock.find_package("nonexistent").is_none());

        assert!(!lock.is_dev_package("vendor/prod"));
        assert!(lock.is_dev_package("vendor/dev"));
    }

    #[test]
    fn composer_locker_missing_lock_cannot_load_locked_packages() {
        let temp = tempfile::tempdir().unwrap();
        let result = RiffLockfile::from_file(temp.path().join("composer.lock"));

        assert!(matches!(
            result,
            Err(LockLoadError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn composer_locker_loads_locked_packages() {
        let lock = RiffLockfile::from_str(
            r#"{
                "packages": [
                    {"name": "pkg1", "version": "1.0.0-beta"},
                    {"name": "pkg2", "version": "0.1.10"}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(
            lock.find_package("pkg1")
                .map(|package| package.version.as_str()),
            Some("1.0.0-beta")
        );
        assert_eq!(
            lock.find_package("pkg2")
                .map(|package| package.version.as_str()),
            Some("0.1.10")
        );
    }

    #[test]
    fn composer_locker_existing_lock_is_locked() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("composer.lock");
        std::fs::write(&path, r#"{"packages":[]}"#).unwrap();

        let lock = RiffLockfile::from_file(path).unwrap();

        assert_eq!(lock.package_count(), 0);
    }

    #[test]
    fn composer_locker_serializes_complete_package_state() {
        let manifest = format!("{COMPOSER_MANIFEST}  ");
        let lock = RiffLockfile {
            content_hash: crate::util::compute_content_hash(&manifest),
            packages: vec![
                LockedPackage {
                    name: "pkg1".into(),
                    version: "1.0.0-beta".into(),
                    package_type: "library".into(),
                    ..Default::default()
                },
                LockedPackage {
                    name: "pkg2".into(),
                    version: "0.1.10".into(),
                    package_type: "library".into(),
                    ..Default::default()
                },
            ],
            minimum_stability: "dev".into(),
            platform_overrides: IndexMap::from([("foo/bar".into(), "1.0".into())]),
            plugin_api_version: "2.9.0".into(),
            ..Default::default()
        };

        assert_eq!(
            serde_json::to_value(lock).unwrap(),
            serde_json::json!({
                "_readme": [
                    "This file locks the dependencies of your project to a known state",
                    "Read more about it at https://getcomposer.org/doc/01-basic-usage.md#installing-dependencies",
                    "This file is @generated automatically"
                ],
                "content-hash": crate::util::compute_content_hash(&manifest),
                "packages": [
                    {"name": "pkg1", "version": "1.0.0-beta", "type": "library"},
                    {"name": "pkg2", "version": "0.1.10", "type": "library"}
                ],
                "packages-dev": [],
                "aliases": [],
                "minimum-stability": "dev",
                "stability-flags": {},
                "prefer-stable": false,
                "prefer-lowest": false,
                "platform": {},
                "platform-dev": {},
                "platform-overrides": {"foo/bar": "1.0"},
                "plugin-api-version": "2.9.0"
            })
        );
    }

    #[test]
    fn composer_locker_is_fresh_with_legacy_hash() {
        let mut hasher = Md5::new();
        hasher.update(COMPOSER_MANIFEST.as_bytes());
        let lock = RiffLockfile::from_str(
            &serde_json::json!({
                "hash": format!("{:x}", hasher.finalize()),
                "packages": []
            })
            .to_string(),
        )
        .unwrap();

        assert!(lock.content_hash.is_empty());
        assert!(lock.is_fresh(COMPOSER_MANIFEST));
        assert!(serde_json::to_value(lock).unwrap().get("hash").is_none());
    }

    #[test]
    fn composer_locker_is_not_fresh_with_different_legacy_hash() {
        let lock =
            RiffLockfile::from_str(r#"{"hash":"00000000000000000000000000000000","packages":[]}"#)
                .unwrap();

        assert!(!lock.is_fresh(COMPOSER_MANIFEST));
    }

    #[test]
    fn composer_locker_is_fresh_with_content_hash() {
        let lock = RiffLockfile::from_str(
            &serde_json::json!({
                "hash": "stale-legacy-hash",
                "content-hash": crate::util::compute_content_hash(COMPOSER_MANIFEST),
                "packages": []
            })
            .to_string(),
        )
        .unwrap();

        assert!(lock.is_fresh(COMPOSER_MANIFEST));
    }

    #[test]
    fn composer_locker_is_fresh_with_content_hash_and_no_legacy_hash() {
        let lock = RiffLockfile {
            content_hash: crate::util::compute_content_hash(COMPOSER_MANIFEST),
            ..Default::default()
        };

        assert!(lock.is_fresh(COMPOSER_MANIFEST));
    }

    #[test]
    fn composer_locker_is_not_fresh_with_different_content_hash() {
        let lock = RiffLockfile {
            content_hash: crate::util::compute_content_hash(
                r#"{"minimum-stability":"beta","name":"test2"}"#,
            ),
            ..Default::default()
        };

        assert!(!lock.is_fresh(COMPOSER_MANIFEST));
    }

    #[test]
    fn test_abandoned_package() {
        let json = r#"{
            "content-hash": "abc",
            "packages": [
                {"name": "pkg1", "version": "1.0", "abandoned": false},
                {"name": "pkg2", "version": "1.0", "abandoned": true},
                {"name": "pkg3", "version": "1.0", "abandoned": "new/package"}
            ],
            "packages-dev": []
        }"#;

        let lock = RiffLockfile::from_str(json).unwrap();

        assert!(!lock.packages[0].is_abandoned());
        assert!(lock.packages[1].is_abandoned());
        assert!(lock.packages[2].is_abandoned());

        assert!(lock.packages[0].abandoned_replacement().is_none());
        assert!(lock.packages[1].abandoned_replacement().is_none());
        assert_eq!(
            lock.packages[2].abandoned_replacement(),
            Some("new/package")
        );
    }

    #[test]
    fn test_parse_empty_arrays_as_maps() {
        // Composer outputs empty arrays [] instead of empty objects {} for some fields
        let json = r#"{
            "content-hash": "abc123",
            "packages": [],
            "packages-dev": [],
            "aliases": [],
            "minimum-stability": "stable",
            "stability-flags": [],
            "prefer-stable": true,
            "prefer-lowest": false,
            "platform": {
                "php": ">=8.2"
            },
            "platform-dev": [],
            "plugin-api-version": "2.9.0"
        }"#;

        let lock = RiffLockfile::from_str(json).unwrap();
        assert!(lock.stability_flags.is_empty());
        assert!(lock.platform_dev.is_empty());
        assert_eq!(lock.platform.get("php"), Some(&">=8.2".to_string()));
    }

    #[test]
    fn test_parse_package_with_empty_arrays() {
        let json = r#"{
            "content-hash": "abc123",
            "packages": [{
                "name": "vendor/package",
                "version": "1.0.0",
                "require": [],
                "require-dev": [],
                "conflict": [],
                "provide": [],
                "replace": [],
                "suggest": [],
                "type": "library"
            }],
            "packages-dev": []
        }"#;

        let lock = RiffLockfile::from_str(json).unwrap();
        assert_eq!(lock.packages.len(), 1);

        let pkg = &lock.packages[0];
        assert!(pkg.require.is_empty());
        assert!(pkg.require_dev.is_empty());
        assert!(pkg.conflict.is_empty());
        assert!(pkg.provide.is_empty());
        assert!(pkg.replace.is_empty());
        assert!(pkg.suggest.is_empty());
    }

    // Ported from Composer\Test\Json\JsonFileTest's complex lock-file merge
    // conflict contracts. Riff deliberately rejects ambiguous conflicts.
    #[test]
    fn composer_lock_parser_rejects_complex_merge_conflicts_with_any_line_endings() {
        let conflict = concat!(
            "{\n",
            "<<<<<<< HEAD\n",
            "  \"content-hash\": \"ours\",\n",
            "=======\n",
            "  \"content-hash\": \"theirs\",\n",
            ">>>>>>> branch\n",
            "<<<<<<< HEAD\n",
            "  \"packages\": [],\n",
            "=======\n",
            "  \"packages\": [{\"name\":\"pkg\",\"version\":\"1.0.0\"}],\n",
            ">>>>>>> branch\n",
            "  \"packages-dev\": []\n",
            "}\n",
        );

        for json in [conflict.to_string(), conflict.replace('\n', "\r\n")] {
            assert!(RiffLockfile::from_str(&json).is_err());
        }
    }

    fn simple_content_hash_conflict() -> String {
        concat!(
            "{\n",
            "  \"_readme\": [],\n",
            "<<<<<<< HEAD\n",
            "  \"content-hash\": \"ours\",\n",
            "=======\n",
            "  \"content-hash\": \"theirs\",\n",
            ">>>>>>> branch\n",
            "  \"packages\": [],\n",
            "  \"packages-dev\": []\n",
            "}\n",
        )
        .to_string()
    }

    #[test]
    fn composer_lock_parser_recovers_simple_content_hash_conflicts() {
        let warning = "VCS merge conflict detected. Please run `composer update --lock`.";
        for content in [
            simple_content_hash_conflict(),
            simple_content_hash_conflict().replace('\n', "\r\n"),
        ] {
            let lock = RiffLockfile::from_str(&content).unwrap();
            assert_eq!(lock.content_hash, warning);
            assert!(lock.packages.is_empty());
            assert!(lock.packages_dev.is_empty());
        }
    }

    #[test]
    fn composer_lock_parser_recovers_extended_content_hash_conflict() {
        let content = simple_content_hash_conflict().replace(
            "=======\n",
            concat!(
                "||||||| parent of commit\n",
                "  \"content-hash\": \"base\",\n",
                "=======\n",
            ),
        );

        let lock = RiffLockfile::from_str(&content).unwrap();

        assert_eq!(
            lock.content_hash,
            "VCS merge conflict detected. Please run `composer update --lock`."
        );
    }

    #[test]
    fn lock_write_equivalence_short_circuits_regular_changes() {
        let mut left = RiffLockfile {
            content_hash: "content".into(),
            minimum_stability: "stable".into(),
            plugin_api_version: "2.9.0".into(),
            ..Default::default()
        };
        let mut package = LockedPackage {
            name: "vendor/package".into(),
            version: "1.0.0".into(),
            package_type: "library".into(),
            ..Default::default()
        };
        package.require.insert("php".into(), "^8.2".into());
        left.packages.push(package);

        let mut right = left.clone();
        assert_equivalence_matches_json(&left, &right);
        assert!(left.equivalent_for_write(&right));

        right.packages[0].version = "2.0.0".into();
        assert_equivalence_matches_json(&left, &right);
        assert!(!left.equivalent_for_write(&right));

        right = left.clone();
        right.prefer_lowest = true;
        assert_equivalence_matches_json(&left, &right);
        assert!(!left.equivalent_for_write(&right));
    }

    #[test]
    fn lock_write_equivalence_matches_skipped_field_semantics() {
        let mut left = RiffLockfile::default();
        left.packages.push(LockedPackage {
            name: "vendor/package".into(),
            version: "1.0.0".into(),
            package_type: "library".into(),
            dist: Some(LockDist {
                dist_type: "zip".into(),
                url: "https://example.test/package.zip".into(),
                reference: Some("reference".into()),
                shasum: None,
                mirrors: None,
            }),
            ..Default::default()
        });

        let mut right = left.clone();
        right.packages[0].dist.as_mut().unwrap().shasum = Some(String::new());
        right.packages[0].description = Some(String::new());
        right.packages[0].homepage = Some(String::new());
        right.packages[0].abandoned = serde_json::Value::Bool(false);
        right.packages[0]
            .autoload
            .exclude_from_classmap
            .push("ignored-while-autoload-is-empty".into());
        assert_equivalence_matches_json(&left, &right);
        assert!(left.equivalent_for_write(&right));

        right.packages[0]
            .autoload
            .psr4
            .insert("Vendor\\Package\\".into(), serde_json::json!("src"));
        assert_equivalence_matches_json(&left, &right);
        assert!(!left.equivalent_for_write(&right));
    }
}
