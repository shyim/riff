use super::{Autoload, Dist, Link, Source};
use chrono::{DateTime, Utc};
use compact_str::CompactString;
use indexmap::IndexMap;
use serde::de::{Error as _, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::hash::BuildHasher;

/// Package stability levels
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Stability {
    /// Development version
    Dev,
    /// Alpha release
    Alpha,
    /// Beta release
    Beta,
    /// Release candidate
    #[serde(rename = "RC")]
    RC,
    /// Stable release
    #[default]
    Stable,
}

impl Stability {
    /// Returns the stability priority (lower is more stable)
    pub fn priority(&self) -> u8 {
        match self {
            Stability::Stable => 0,
            Stability::RC => 5,
            Stability::Beta => 10,
            Stability::Alpha => 15,
            Stability::Dev => 20,
        }
    }

    /// Parses stability from a version string
    pub fn from_version(version: &str) -> Self {
        let lower = version.to_lowercase();
        if lower.contains("dev") {
            Stability::Dev
        } else if lower.contains("alpha") {
            Stability::Alpha
        } else if lower.contains("beta") {
            Stability::Beta
        } else if lower.contains("rc") {
            Stability::RC
        } else {
            Stability::Stable
        }
    }

    /// Parse stability from a string (e.g., from composer.json minimum-stability)
    fn parse_stability(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "dev" => Stability::Dev,
            "alpha" => Stability::Alpha,
            "beta" => Stability::Beta,
            "rc" => Stability::RC,
            "stable" | "" => Stability::Stable,
            _ => Stability::Stable, // Default to stable for unknown values
        }
    }
}

impl std::str::FromStr for Stability {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Stability::parse_stability(s))
    }
}

impl std::fmt::Display for Stability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stability::Dev => write!(f, "dev"),
            Stability::Alpha => write!(f, "alpha"),
            Stability::Beta => write!(f, "beta"),
            Stability::RC => write!(f, "RC"),
            Stability::Stable => write!(f, "stable"),
        }
    }
}

/// Information about abandoned packages
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Abandoned {
    /// Package is abandoned with no replacement
    Yes,
    /// Package is abandoned with a suggested replacement
    Replacement(String),
}

impl Abandoned {
    /// Returns true if the package is abandoned
    pub fn is_abandoned(&self) -> bool {
        true
    }

    /// Returns the replacement package name if any
    pub fn replacement(&self) -> Option<&str> {
        match self {
            Abandoned::Yes => None,
            Abandoned::Replacement(pkg) => Some(pkg.as_str()),
        }
    }
}

/// Author information
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Author {
    /// Author name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<CompactString>,
    /// Author email
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<CompactString>,
    /// Author homepage
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<CompactString>,
    /// Author role (e.g., "Developer", "Maintainer")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<CompactString>,
}

/// Support information (links to resources)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Support {
    /// Issues/bug tracker URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issues: Option<String>,
    /// Forum URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forum: Option<String>,
    /// Wiki URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wiki: Option<String>,
    /// Source code URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Email address for support
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// IRC channel
    #[serde(skip_serializing_if = "Option::is_none")]
    pub irc: Option<String>,
    /// Documentation URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
    /// RSS feed URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss: Option<String>,
    /// Chat URL (e.g., Slack, Discord)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat: Option<String>,
    /// Security policy URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<String>,
}

/// Funding information
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Funding {
    /// Type of funding (e.g., "github", "patreon", "opencollective")
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub funding_type: Option<CompactString>,
    /// URL to the funding page
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<CompactString>,
}

/// Scripts configuration (composer event handlers)
pub type Scripts = IndexMap<String, ScriptHandler>;

/// Insertion-ordered dependency links optimized for Composer's small maps.
///
/// Resolver operations overwhelmingly iterate these maps, while point lookups
/// are rare and the typical map contains only a handful of entries. Keeping a
/// contiguous list avoids allocating and hashing an index for every dependency
/// section of every candidate package.
#[derive(Debug, Clone, Default)]
pub struct DependencyMap(Vec<(CompactString, CompactString)>);

impl PartialEq for DependencyMap {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self
                .iter()
                .all(|(key, value)| other.get(key) == Some(value))
    }
}

impl Eq for DependencyMap {}

impl<S> From<IndexMap<String, String, S>> for DependencyMap
where
    S: BuildHasher,
{
    fn from(map: IndexMap<String, String, S>) -> Self {
        Self::from_iter(map)
    }
}

impl<S> From<&IndexMap<String, String, S>> for DependencyMap
where
    S: BuildHasher,
{
    fn from(map: &IndexMap<String, String, S>) -> Self {
        Self::from_iter(
            map.iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
    }
}

impl<K, V> FromIterator<(K, V)> for DependencyMap
where
    K: Into<CompactString>,
    V: Into<CompactString>,
{
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        let iter = iter.into_iter();
        let mut map = Self(Vec::with_capacity(iter.size_hint().0));
        for (key, value) in iter {
            map.insert_compact(key.into(), value.into());
        }
        map
    }
}

impl IntoIterator for DependencyMap {
    type Item = (CompactString, CompactString);
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a DependencyMap {
    type Item = (&'a CompactString, &'a CompactString);
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, (CompactString, CompactString)>,
        fn(&(CompactString, CompactString)) -> (&CompactString, &CompactString),
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().map(pair_refs)
    }
}

impl<'a> IntoIterator for &'a mut DependencyMap {
    type Item = (&'a CompactString, &'a mut CompactString);
    type IntoIter = std::iter::Map<
        std::slice::IterMut<'a, (CompactString, CompactString)>,
        fn(&mut (CompactString, CompactString)) -> (&CompactString, &mut CompactString),
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter_mut().map(pair_mut)
    }
}

impl DependencyMap {
    pub(crate) fn from_ordered_iter<K, V, I>(iter: I) -> Self
    where
        K: Into<CompactString>,
        V: Into<CompactString>,
        I: IntoIterator<Item = (K, V)>,
    {
        let iter = iter.into_iter();
        let mut entries = Vec::with_capacity(iter.size_hint().0);
        entries.extend(iter.map(|(key, value)| (key.into(), value.into())));
        Self(entries)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&CompactString, &CompactString)> {
        self.0.iter().map(pair_refs)
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&CompactString, &mut CompactString)> {
        self.0.iter_mut().map(pair_mut)
    }

    pub fn keys(&self) -> impl Iterator<Item = &CompactString> {
        self.0.iter().map(|(key, _)| key)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut CompactString> {
        self.0.iter_mut().map(|(_, value)| value)
    }

    pub fn retain(&mut self, mut predicate: impl FnMut(&str, &str) -> bool) {
        self.0
            .retain(|(key, value)| predicate(key.as_str(), value.as_str()));
    }

    pub fn get(&self, key: &str) -> Option<&CompactString> {
        self.0
            .iter()
            .find_map(|(candidate, value)| (candidate == key).then_some(value))
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.0.iter().any(|(candidate, _)| candidate == key)
    }

    pub fn insert(&mut self, key: String, value: String) -> Option<CompactString> {
        self.insert_compact(key.into(), value.into())
    }

    fn insert_compact(
        &mut self,
        key: CompactString,
        value: CompactString,
    ) -> Option<CompactString> {
        if let Some((_, current)) = self.0.iter_mut().find(|(candidate, _)| candidate == key) {
            return Some(std::mem::replace(current, value));
        }
        self.0.push((key, value));
        None
    }
}

fn pair_refs(pair: &(CompactString, CompactString)) -> (&CompactString, &CompactString) {
    (&pair.0, &pair.1)
}

fn pair_mut(pair: &mut (CompactString, CompactString)) -> (&CompactString, &mut CompactString) {
    (&pair.0, &mut pair.1)
}

impl Serialize for DependencyMap {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for DependencyMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let deduplicate = deserializer.is_human_readable();
        deserializer.deserialize_any(DependencyMapVisitor { deduplicate })
    }
}

struct DependencyMapVisitor {
    deduplicate: bool,
}

impl<'de> Visitor<'de> for DependencyMapVisitor {
    type Value = DependencyMap;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a dependency map")
    }

    fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut map = DependencyMap(Vec::with_capacity(access.size_hint().unwrap_or(0)));
        while let Some(key) = access.next_key::<CompactString>()? {
            let value = access.next_value::<serde_json::Value>()?;
            let Some(value) = value.as_str() else {
                continue;
            };
            let value = CompactString::new(value);
            if self.deduplicate {
                map.insert_compact(key, value);
            } else {
                map.0.push((key, value));
            }
        }
        Ok(map)
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(DependencyMap::default())
    }

    fn visit_string<E>(self, _value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(DependencyMap::default())
    }

    fn visit_seq<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while access.next_element::<IgnoredAny>()?.is_some() {}
        Ok(DependencyMap::default())
    }
}

/// Script handler which can be a single command or multiple commands
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ScriptHandler {
    /// Single script command
    Single(String),
    /// Multiple script commands
    Multiple(Vec<String>),
}

/// Complete package definition
///
/// Represents a Composer package with all metadata, dependencies, and configuration.
/// This combines both the base Package and CompletePackage from PHP Composer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Package {
    /// Package name (lowercase, vendor/package format)
    pub name: String,

    /// Pretty name (original case)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pretty_name: Option<String>,

    /// Normalized version
    pub version: CompactString,

    /// Pretty version (human-readable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pretty_version: Option<CompactString>,

    /// Package type (library, project, metapackage, composer-plugin, etc.)
    #[serde(rename = "type", default = "default_package_type")]
    pub package_type: CompactString,

    /// Package stability
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stability: Option<Stability>,

    /// Source repository information
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,

    /// Distribution archive information
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dist: Option<Dist>,

    /// Required dependencies
    #[serde(skip_serializing_if = "DependencyMap::is_empty", default)]
    pub require: DependencyMap,

    /// Development dependencies
    #[serde(
        rename = "require-dev",
        skip_serializing_if = "DependencyMap::is_empty",
        default
    )]
    pub require_dev: DependencyMap,

    /// Conflicting packages
    #[serde(skip_serializing_if = "DependencyMap::is_empty", default)]
    pub conflict: DependencyMap,

    /// Provided virtual packages
    #[serde(skip_serializing_if = "DependencyMap::is_empty", default)]
    pub provide: DependencyMap,

    /// Replaced packages
    #[serde(skip_serializing_if = "DependencyMap::is_empty", default)]
    pub replace: DependencyMap,

    /// Suggested packages
    #[serde(skip_serializing_if = "DependencyMap::is_empty", default)]
    pub suggest: DependencyMap,

    /// Autoload configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autoload: Option<Autoload>,

    /// Development autoload configuration
    #[serde(rename = "autoload-dev", skip_serializing_if = "Option::is_none")]
    pub autoload_dev: Option<Autoload>,

    /// Include paths
    #[serde(
        rename = "include-path",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub include_path: Vec<String>,

    /// Target directory (deprecated)
    #[serde(rename = "target-dir", skip_serializing_if = "Option::is_none")]
    pub target_dir: Option<String>,

    /// Binary files
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub bin: Vec<CompactString>,

    /// Extra metadata (free-form JSON)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,

    /// Notification URL for package statistics
    #[serde(rename = "notification-url", skip_serializing_if = "Option::is_none")]
    pub notification_url: Option<String>,

    /// Installation source (source or dist)
    #[serde(
        rename = "installation-source",
        skip_serializing_if = "Option::is_none"
    )]
    pub installation_source: Option<String>,

    /// Release date
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<DateTime<Utc>>,

    // CompletePackage fields
    /// Package description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Homepage URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,

    /// License identifiers (SPDX format)
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub license: Vec<CompactString>,

    /// Keywords
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub keywords: Vec<CompactString>,

    /// Authors
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub authors: Vec<Author>,

    /// Support information
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_support"
    )]
    pub support: Option<Support>,

    /// Funding information
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub funding: Vec<Funding>,

    /// Scripts (composer event handlers)
    #[serde(skip_serializing_if = "IndexMap::is_empty", default)]
    pub scripts: Scripts,

    /// Whether the package is abandoned
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abandoned: Option<Abandoned>,

    /// Archive name pattern
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive: Option<ArchiveConfig>,

    /// Whether this is the default branch
    #[serde(rename = "default-branch", skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<bool>,

    /// Transport options for downloading
    #[serde(rename = "transport-options", skip_serializing_if = "Option::is_none")]
    pub transport_options: Option<serde_json::Value>,
}

/// Archive configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ArchiveConfig {
    /// Archive name pattern
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Files/directories to exclude from archive
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub exclude: Vec<String>,
}

fn default_package_type() -> CompactString {
    "library".into()
}

fn deserialize_optional_support<'de, D>(deserializer: D) -> Result<Option<Support>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if !value.is_object() {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(D::Error::custom)
}

/// Package type constants
pub mod package_type {
    /// Standard library package (default)
    pub const LIBRARY: &str = "library";
    /// Project package (not meant to be a dependency)
    pub const PROJECT: &str = "project";
    /// Metapackage - no code, only dependencies
    pub const METAPACKAGE: &str = "metapackage";
    /// Composer plugin
    pub const COMPOSER_PLUGIN: &str = "composer-plugin";
}

impl Package {
    /// Creates a new package with minimal required fields
    pub fn new(name: impl Into<String>, version: impl Into<CompactString>) -> Self {
        let name = name.into();
        let version = version.into();
        let stability = Stability::from_version(&version);

        Self {
            name: name.to_lowercase(),
            pretty_name: Some(name),
            version: version.clone(),
            pretty_version: Some(version.clone()),
            package_type: default_package_type(),
            stability: Some(stability),
            source: None,
            dist: None,
            require: DependencyMap::default(),
            require_dev: DependencyMap::default(),
            conflict: DependencyMap::default(),
            provide: DependencyMap::default(),
            replace: DependencyMap::default(),
            suggest: DependencyMap::default(),
            autoload: None,
            autoload_dev: None,
            include_path: Vec::new(),
            target_dir: None,
            bin: Vec::new(),
            extra: None,
            notification_url: None,
            installation_source: None,
            time: None,
            description: None,
            homepage: None,
            license: Vec::new(),
            keywords: Vec::new(),
            authors: Vec::new(),
            support: None,
            funding: Vec::new(),
            scripts: IndexMap::new(),
            abandoned: None,
            archive: None,
            default_branch: None,
            transport_options: None,
        }
    }

    /// Replace `self.version` constraints with the actual package version.
    ///
    /// In Riff, packages can use `self.version` as a constraint in replace,
    /// provide, conflict, require, and require-dev to reference their own version.
    /// This method replaces all occurrences with `=<version>`.
    pub fn replace_self_version(&mut self) {
        let version_constraint = format!("={}", self.version);

        Self::replace_self_version_in_map(&mut self.replace, &version_constraint);
        Self::replace_self_version_in_map(&mut self.provide, &version_constraint);
        Self::replace_self_version_in_map(&mut self.conflict, &version_constraint);
        Self::replace_self_version_in_map(&mut self.require, &version_constraint);
        Self::replace_self_version_in_map(&mut self.require_dev, &version_constraint);
    }

    /// Helper to replace self.version in a constraint map
    fn replace_self_version_in_map(map: &mut DependencyMap, version_constraint: &str) {
        for constraint in map.values_mut() {
            if constraint == "self.version" {
                *constraint = version_constraint.into();
            }
        }
    }

    /// Returns the package name (lowercase)
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the pretty package name (original case)
    pub fn pretty_name(&self) -> &str {
        self.pretty_name.as_deref().unwrap_or(&self.name)
    }

    /// Returns the normalized version
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Returns the pretty version
    pub fn pretty_version(&self) -> &str {
        self.pretty_version.as_deref().unwrap_or(&self.version)
    }

    /// Returns the display version, including a VCS reference for development packages.
    pub fn full_pretty_version(&self, truncate_reference: bool) -> String {
        let Some(source) = self
            .source
            .as_ref()
            .filter(|source| matches!(source.source_type.as_str(), "git" | "hg"))
            .filter(|_| self.is_dev())
        else {
            return self.pretty_version().to_string();
        };
        if source.reference.is_empty() {
            return self.pretty_version().to_string();
        }

        let reference =
            if truncate_reference && source.source_type != "svn" && source.reference.len() == 40 {
                &source.reference[..7]
            } else {
                &source.reference
            };
        format!("{} {reference}", self.pretty_version())
    }

    /// Returns Composer's normalized legacy target directory.
    pub fn normalized_target_dir(&self) -> Option<String> {
        self.target_dir.as_ref().map(|target_dir| {
            let trailing_separator = target_dir.ends_with('/')
                || target_dir.ends_with('\\')
                || target_dir
                    .rsplit(['/', '\\'])
                    .next()
                    .is_some_and(|part| matches!(part, "." | ".."));
            let mut normalized = target_dir
                .split(['/', '\\'])
                .filter(|part| !part.is_empty() && !matches!(*part, "." | ".."))
                .collect::<Vec<_>>()
                .join("/");
            if trailing_separator && !normalized.is_empty() {
                normalized.push('/');
            }
            normalized
        })
    }

    /// Returns the package type
    pub fn package_type(&self) -> &str {
        &self.package_type
    }

    /// Returns true if this is a metapackage (no files, only dependencies)
    pub fn is_metapackage(&self) -> bool {
        self.package_type == package_type::METAPACKAGE
    }

    /// Returns true if this is a composer plugin
    pub fn is_composer_plugin(&self) -> bool {
        self.package_type == package_type::COMPOSER_PLUGIN
    }

    /// Returns true if this is a platform package (php, ext-*, lib-*)
    pub fn is_platform_package(&self) -> bool {
        self.name == "php"
            || self.name.starts_with("ext-")
            || self.name.starts_with("lib-")
            || self.name == "composer"
            || self.name == "composer-runtime-api"
            || self.name == "composer-plugin-api"
    }

    /// Returns the stability
    pub fn stability(&self) -> Stability {
        self.stability.unwrap_or_default()
    }

    /// Returns true if this is a development version
    pub fn is_dev(&self) -> bool {
        self.stability() == Stability::Dev
    }

    /// Returns true if the package is abandoned
    pub fn is_abandoned(&self) -> bool {
        self.abandoned.is_some()
    }

    /// Returns the unique name (name-version)
    pub fn unique_name(&self) -> String {
        format!("{}-{}", self.name, self.version)
    }

    /// Returns a pretty string representation
    pub fn pretty_string(&self) -> String {
        format!("{} {}", self.pretty_name(), self.pretty_version())
    }

    /// Converts require/require-dev/etc maps to Link structs
    pub fn get_links(&self) -> Vec<Link> {
        use super::LinkType;

        let mut links = Vec::new();

        for (target, constraint) in &self.require {
            links.push(Link::new(&self.name, target, constraint, LinkType::Require));
        }

        for (target, constraint) in &self.require_dev {
            links.push(Link::new(
                &self.name,
                target,
                constraint,
                LinkType::DevRequire,
            ));
        }

        for (target, constraint) in &self.conflict {
            links.push(Link::new(
                &self.name,
                target,
                constraint,
                LinkType::Conflict,
            ));
        }

        for (target, constraint) in &self.provide {
            links.push(Link::new(&self.name, target, constraint, LinkType::Provide));
        }

        for (target, constraint) in &self.replace {
            links.push(Link::new(&self.name, target, constraint, LinkType::Replace));
        }

        links
    }

    /// Returns all names this package "owns" - its name plus all provides and replaces
    ///
    /// When `include_provides` is true, includes both provides and replaces.
    /// When false, only includes the package name and replaces (replaces are stronger).
    ///
    /// This is used for:
    /// - Pool indexing (finding packages by any of their names)
    /// - Same-name conflict detection (packages providing same name conflict)
    pub fn get_names(&self, include_provides: bool) -> Vec<String> {
        let mut names = vec![self.name.to_lowercase()];

        // Replaces are always included (stronger relationship) - sort for deterministic order
        let mut replace_keys: Vec<_> = self.replace.keys().collect();
        replace_keys.sort();
        for replaced_name in replace_keys {
            let name = replaced_name.as_str().to_lowercase();
            if !names.contains(&name) {
                names.push(name);
            }
        }

        // Provides are only included when requested - sort for deterministic order
        if include_provides {
            let mut provide_keys: Vec<_> = self.provide.keys().collect();
            provide_keys.sort();
            for provided_name in provide_keys {
                let name = provided_name.as_str().to_lowercase();
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }

        names
    }

    /// Updates both source and dist references (for version control)
    pub fn set_references(&mut self, reference: impl Into<String>) {
        let reference = reference.into();

        if let Some(source) = &mut self.source {
            source.reference = reference.clone();
        }

        if let Some(dist) = &mut self.dist {
            let url = dist.url.to_lowercase();
            let generated_dist_url = url.starts_with("http://") || url.starts_with("https://");
            let generated_dist_url = generated_dist_url
                && (url.contains("github.com/")
                    || url.contains("gitlab.com/")
                    || url.contains("bitbucket.org/"));
            if generated_dist_url {
                if let Some(previous) = dist.reference.as_deref() {
                    if previous.len() == 40 && previous.bytes().all(|byte| byte.is_ascii_hexdigit())
                    {
                        dist.url = dist.url.replace(previous, &reference);
                    }
                }
                dist.reference = Some(reference.clone());
            } else if dist.reference.is_some() {
                dist.reference = Some(reference.clone());
            }
        }
    }
}

impl Default for Package {
    fn default() -> Self {
        Self::new("vendor/package", "1.0.0")
    }
}

impl std::fmt::Display for Package {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.unique_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composer_complete_package(
        name: &str,
        normalized_version: &str,
        pretty_version: &str,
    ) -> Package {
        let mut package = Package::new(name, normalized_version);
        package.pretty_version = Some(pretty_version.into());
        package
    }

    fn composer_versioning_schemes() -> [(&'static str, &'static str, &'static str); 5] {
        [
            ("foo", "1.0.0.0-beta", "1-beta"),
            ("node", "0.5.6.0", "0.5.6"),
            ("li3", "0.10.0.0", "0.10"),
            ("mongodb_odm", "1.0.0.0-beta3", "1.0.0BETA3"),
            ("DoctrineCommon", "2.2.0.0-dev", "2.2.0-DEV"),
        ]
    }

    #[test]
    fn test_package_creation() {
        let package = Package::new("vendor/package", "1.0.0");

        assert_eq!(package.name(), "vendor/package");
        assert_eq!(package.version(), "1.0.0");
        assert_eq!(package.stability(), Stability::Stable);
    }

    #[test]
    fn test_package_dev_version() {
        let package = Package::new("vendor/package", "dev-main");

        assert!(package.is_dev());
        assert_eq!(package.stability(), Stability::Dev);
    }

    #[test]
    fn test_stability_priority() {
        assert!(Stability::Stable.priority() < Stability::RC.priority());
        assert!(Stability::RC.priority() < Stability::Beta.priority());
        assert!(Stability::Beta.priority() < Stability::Alpha.priority());
        assert!(Stability::Alpha.priority() < Stability::Dev.priority());
    }

    #[test]
    fn test_stability_from_str() {
        use std::str::FromStr;

        assert_eq!(Stability::from_str("dev").unwrap(), Stability::Dev);
        assert_eq!(Stability::from_str("alpha").unwrap(), Stability::Alpha);
        assert_eq!(Stability::from_str("beta").unwrap(), Stability::Beta);
        assert_eq!(Stability::from_str("rc").unwrap(), Stability::RC);
        assert_eq!(Stability::from_str("RC").unwrap(), Stability::RC);
        assert_eq!(Stability::from_str("stable").unwrap(), Stability::Stable);
        assert_eq!(Stability::from_str("STABLE").unwrap(), Stability::Stable);
        assert_eq!(Stability::from_str("").unwrap(), Stability::Stable);
        assert_eq!(Stability::from_str("unknown").unwrap(), Stability::Stable);
    }

    #[test]
    fn test_abandoned_package() {
        let mut package = Package::new("vendor/old-package", "1.0.0");
        package.abandoned = Some(Abandoned::Replacement("vendor/new-package".to_string()));

        assert!(package.is_abandoned());
        assert_eq!(
            package.abandoned.as_ref().unwrap().replacement(),
            Some("vendor/new-package")
        );
    }

    #[test]
    fn test_package_serialization() {
        let package = Package::new("vendor/package", "1.0.0");
        let json = serde_json::to_string(&package).unwrap();
        let deserialized: Package = serde_json::from_str(&json).unwrap();

        assert_eq!(package.name(), deserialized.name());
        assert_eq!(package.version(), deserialized.version());
    }

    #[test]
    fn dependency_map_preserves_order_and_updates_in_place() {
        let mut map = DependencyMap::default();
        assert_eq!(map.insert("a/package".into(), "^1".into()), None);
        assert_eq!(map.insert("b/package".into(), "^2".into()), None);
        assert_eq!(
            map.insert("a/package".into(), "^3".into()).as_deref(),
            Some("^1")
        );

        assert_eq!(
            map.iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect::<Vec<_>>(),
            [("a/package", "^3"), ("b/package", "^2")]
        );
    }

    #[test]
    fn dependency_map_json_duplicates_use_last_value() {
        let map: DependencyMap =
            serde_json::from_str(r#"{"vendor/package":"^1","vendor/package":"^2"}"#).unwrap();

        assert_eq!(map.len(), 1);
        assert_eq!(
            map.get("vendor/package").map(CompactString::as_str),
            Some("^2")
        );
    }

    #[test]
    fn test_pretty_version_defaults_to_version() {
        let package = Package::new("vendor/package", "1.0.0.0");
        assert_eq!(package.pretty_version(), "1.0.0.0");
    }

    #[test]
    fn test_pretty_version_with_explicit_value() {
        let mut package = Package::new("vendor/package", "1.0.0.0");
        package.pretty_version = Some("v1.0.0".into());
        assert_eq!(package.pretty_version(), "v1.0.0");
        assert_eq!(package.version(), "1.0.0.0");
    }

    #[test]
    fn test_pretty_version_formats() {
        let test_cases = [
            ("1.0.0.0", "1.0.0"),
            ("1.0.0.0", "v1.0.0"),
            ("2.3.4.0", "2.3.4"),
            ("1.0.0.0", "1.0"),
            ("9999999.0.0.0-dev", "dev-main"),
        ];

        for (normalized, pretty) in test_cases {
            let mut package = Package::new("vendor/package", normalized);
            package.pretty_version = Some(pretty.into());
            assert_eq!(package.pretty_version(), pretty);
        }
    }

    // Ported from Composer\Test\Package\BasePackageTest::testFormatVersionForDevPackage.
    #[test]
    fn composer_dev_package_full_pretty_version_includes_source_reference() {
        for (reference, truncate, expected) in [
            ("v2.1.0-RC2", true, "PrettyVersion v2.1.0-RC2"),
            (
                "bbf527a27356414bfa9bf520f018c5cb7af67c77",
                true,
                "PrettyVersion bbf527a",
            ),
            ("v1.0.0", false, "PrettyVersion v1.0.0"),
            (
                "bbf527a27356414bfa9bf520f018c5cb7af67c77",
                false,
                "PrettyVersion bbf527a27356414bfa9bf520f018c5cb7af67c77",
            ),
        ] {
            let mut package = Package::new("vendor/package", "dev-main");
            package.pretty_version = Some("PrettyVersion".into());
            package.stability = Some(Stability::Dev);
            package.source = Some(Source::git("https://example.org/repo.git", reference));

            assert_eq!(package.full_pretty_version(truncate), expected);
        }
    }

    // Ported from Composer\Test\Package\CompletePackageTest::testGetTargetDir.
    #[test]
    fn composer_complete_package_normalizes_legacy_target_directory() {
        let mut package = Package::new("a", "1.0.0.0");
        assert_eq!(package.normalized_target_dir(), None);

        for (target_dir, expected) in [
            ("./../foo/", "foo/"),
            ("foo/../../../bar/", "foo/bar/"),
            ("../..", ""),
            ("..", ""),
            ("/..", ""),
            ("/foo/..", "foo/"),
            ("/foo/..//bar", "foo/bar"),
        ] {
            package.target_dir = Some(target_dir.to_string());
            assert_eq!(package.normalized_target_dir().as_deref(), Some(expected));
        }
    }

    #[test]
    fn composer_complete_package_has_expected_naming_semantics() {
        for (name, normalized, pretty) in composer_versioning_schemes() {
            let package = composer_complete_package(name, normalized, pretty);
            assert_eq!(package.name(), name.to_lowercase());
            assert_eq!(package.pretty_name(), name);
        }
    }

    #[test]
    fn composer_complete_package_has_expected_versioning_semantics() {
        for (name, normalized, pretty) in composer_versioning_schemes() {
            let package = composer_complete_package(name, normalized, pretty);
            assert_eq!(package.version(), normalized);
            assert_eq!(package.pretty_version(), pretty);
        }
    }

    #[test]
    fn composer_complete_package_has_expected_marshalling_semantics() {
        for (name, normalized, pretty) in composer_versioning_schemes() {
            let package = composer_complete_package(name, normalized, pretty);
            assert_eq!(
                package.to_string(),
                format!("{}-{normalized}", name.to_lowercase())
            );
        }
    }

    #[test]
    fn composer_array_loader_self_version() {
        let mut package = Package::new("A", "1.2.3.4");
        package
            .replace
            .insert("foo".to_string(), "self.version".to_string());

        package.replace_self_version();

        assert_eq!(
            package.replace.get("foo").map(CompactString::as_str),
            Some("=1.2.3.4")
        );
    }

    #[test]
    fn composer_array_loader_type_default() {
        let default_package: Package =
            serde_json::from_value(serde_json::json!({"name": "A", "version": "1.0"})).unwrap();
        assert_eq!(default_package.package_type(), "library");

        let custom_package: Package = serde_json::from_value(
            serde_json::json!({"name": "A", "version": "1.0", "type": "foo"}),
        )
        .unwrap();
        assert_eq!(custom_package.package_type(), "foo");
    }

    #[test]
    fn composer_array_loader_abandoned() {
        let package: Package = serde_json::from_value(serde_json::json!({
            "name": "A",
            "version": "1.2.3.4",
            "abandoned": "foo/bar"
        }))
        .unwrap();

        assert!(package.is_abandoned());
        assert_eq!(
            package.abandoned.as_ref().and_then(Abandoned::replacement),
            Some("foo/bar")
        );
    }

    #[test]
    fn composer_array_loader_not_abandoned() {
        let package: Package =
            serde_json::from_value(serde_json::json!({"name": "A", "version": "1.2.3.4"})).unwrap();

        assert!(!package.is_abandoned());
        assert!(package.abandoned.is_none());
    }

    #[test]
    fn composer_array_loader_package_links_require() {
        let package: Package = serde_json::from_value(serde_json::json!({
            "name": "acme/package",
            "version": "dev-1",
            "require": {"foo/bar": "1.0"}
        }))
        .unwrap();

        assert_eq!(
            package.require.get("foo/bar").map(CompactString::as_str),
            Some("1.0")
        );
    }

    #[test]
    fn composer_array_loader_package_links_replace() {
        let mut package: Package = serde_json::from_value(serde_json::json!({
            "name": "acme/package",
            "version": "dev-1",
            "replace": {"coyote/package": "self.version"}
        }))
        .unwrap();

        package.replace_self_version();

        assert_eq!(
            package
                .replace
                .get("coyote/package")
                .map(CompactString::as_str),
            Some("=dev-1")
        );
    }

    #[test]
    fn composer_array_loader_keeps_plugin_api_constraints_as_declared() {
        for constraint in [
            "1.0",
            "1.0.0",
            "1.0.0.0",
            "1",
            "=1.0.0",
            "==1.0",
            "~1.0.0",
            "*",
            "3.0.*",
            "@stable",
            "1.0.0@stable",
            "^5.1",
            ">=1.0.0 <2.5",
            "x",
            "1.0.0-dev",
        ] {
            let package: Package = serde_json::from_value(serde_json::json!({
                "name": "plugin/package",
                "version": "9.9.9",
                "require": {"composer-plugin-api": constraint}
            }))
            .unwrap();
            assert_eq!(
                package
                    .require
                    .get("composer-plugin-api")
                    .map(CompactString::as_str),
                Some(constraint)
            );
        }
    }

    #[test]
    fn composer_array_loader_plugin_api_supports_self_version() {
        let mut package: Package = serde_json::from_value(serde_json::json!({
            "name": "plugin/package",
            "version": "6.6.6",
            "require": {"composer-plugin-api": "self.version"}
        }))
        .unwrap();

        package.replace_self_version();

        assert_eq!(
            package
                .require
                .get("composer-plugin-api")
                .map(CompactString::as_str),
            Some("=6.6.6")
        );
    }

    #[test]
    fn composer_array_loader_accepts_integer_shaped_link_targets() {
        let package: Package = serde_json::from_value(serde_json::json!({
            "name": "plugin/package",
            "version": "9.9.9",
            "require": {"1": "dev-main"}
        }))
        .unwrap();

        assert_eq!(
            package.require.get("1").map(CompactString::as_str),
            Some("dev-main")
        );
    }

    #[test]
    fn composer_array_loader_ignores_non_string_require_constraints() {
        let package: Package = serde_json::from_value(serde_json::json!({
            "name": "acme/package",
            "version": "dev-1",
            "require": {"foo/bar": {"random-string": "1.0"}}
        }))
        .unwrap();

        assert!(package.require.is_empty());
    }

    #[test]
    fn dependency_map_preserves_suggest_descriptions() {
        let package: Package = serde_json::from_value(serde_json::json!({
            "name": "acme/package",
            "version": "dev-1",
            "suggest": {"foo/bar": "Adds the optional integration"}
        }))
        .unwrap();

        assert_eq!(
            package.suggest.get("foo/bar").map(CompactString::as_str),
            Some("Adds the optional integration")
        );
    }

    #[test]
    fn composer_array_loader_ignores_non_map_replace_metadata() {
        let package: Package = serde_json::from_value(serde_json::json!({
            "name": "acme/package",
            "version": "dev-1",
            "replace": "coyote/package"
        }))
        .unwrap();

        assert!(package.replace.is_empty());
    }

    #[test]
    fn composer_array_loader_ignores_string_support_metadata() {
        let package: Package = serde_json::from_value(serde_json::json!({
            "name": "acme/package",
            "version": "dev-1",
            "support": "https://example.org"
        }))
        .unwrap();

        assert!(package.support.is_none());
    }

    #[test]
    fn forced_reference_rewrites_generated_dist_url() {
        let old = "1111111111111111111111111111111111111111";
        let new = "2222222222222222222222222222222222222222";
        let mut package = Package::new("acme/package", "dev-main");
        package.source = Some(Source::git("https://github.com/acme/package", old));
        package.dist = Some(
            Dist::zip(format!(
                "https://api.github.com/repos/acme/package/zipball/{old}"
            ))
            .with_reference(old),
        );

        package.set_references(new);

        assert_eq!(package.source.as_ref().unwrap().reference, new);
        assert_eq!(
            package.dist.as_ref().unwrap().reference.as_deref(),
            Some(new)
        );
        assert!(package.dist.as_ref().unwrap().url.ends_with(new));
    }
}
