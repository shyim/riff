use async_trait::async_trait;
use indexmap::IndexMap;
use regex::Regex;
use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;
use tokio::sync::RwLock;

use super::package_cache::{ArchivedCachedPackage, CachedPackage};
use super::traits::{ProviderInfo, Repository, SearchMode, SearchResult};
use crate::cache::{CacheMetadata, RepoCache};
use crate::config::AuthConfig;
use crate::filter_list::{
    ComposerRepositoryFilterInformation, FilterEntriesByList, FilterListEntryBuilder,
    PackageVersions,
};
use crate::json::SecurityAdvisory;
use crate::package::{
    parse_branch_aliases, Autoload, AutoloadPath, Dist, Package, Source, Stability,
};
use riff_semver::{Constraint, Operator, Semver, VersionParser};

/// Default TTL for cached metadata (10 minutes, matching Riff)
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(600);
// Bump when Package conversion semantics or this cache envelope changes.
const PARSED_PACKAGE_CACHE_VERSION: u8 = 2;
const FILTERED_PACKAGE_CACHE_VERSION: u8 = 4;

/// Result from conditional HTTP request
enum FetchResult {
    /// 304 Not Modified - cached data is still valid
    NotModified,
    /// New data received with metadata
    Modified(String, CacheMetadata),
}

/// Mirror configuration for source repositories
#[derive(Debug, Clone)]
pub struct SourceMirror {
    /// Mirror URL pattern
    pub url: String,
    /// Whether this mirror is preferred
    pub preferred: bool,
}

/// Mirror configuration for dist (archives)
#[derive(Debug, Clone)]
pub struct DistMirror {
    /// Mirror URL pattern
    pub url: String,
    /// Whether this mirror is preferred
    pub preferred: bool,
}

/// Stability filter configuration
#[derive(Debug, Clone, Default)]
pub struct StabilityConfig {
    /// Acceptable stabilities (keys are stability names, values are priority)
    pub acceptable: HashMap<Stability, u8>,
    /// Per-package stability flags (package name -> stability)
    pub flags: HashMap<String, Stability>,
}

/// HTTP request metadata for Composer repository POST APIs, independent of a
/// particular HTTP client implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerRepositoryApiRequest {
    pub url: String,
    pub method: &'static str,
    pub content_type: &'static str,
    pub timeout_seconds: u64,
    pub body: String,
    pub transport_options: Value,
}

/// The repository source selected for filter-list metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerRepositoryFilterSource {
    Api,
    Summary,
    PackageMetadata,
}

#[derive(Debug, Clone, Default)]
struct SecurityAdvisoryInformation {
    metadata: bool,
    api_url: Option<String>,
}

/// A conditional repository response which can retain a previously cached
/// representation after a 304 response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConditionalRepositoryResponse<T> {
    Modified(T),
    NotModified,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConditionalRepositoryDocument<T> {
    cached: Option<T>,
}

impl<T> ConditionalRepositoryDocument<T> {
    pub fn resolve(&mut self, response: ConditionalRepositoryResponse<T>) -> Result<&T, String> {
        if let ConditionalRepositoryResponse::Modified(document) = response {
            self.cached = Some(document);
        }
        self.cached
            .as_ref()
            .ok_or_else(|| "repository returned 304 without a cached document".to_owned())
    }
}

/// Custom deserializer that handles the Packagist v2 "__unset" marker.
/// "__unset" means the field was removed in this version.
fn deserialize_maybe_unset<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(Value::String(s)) if s == "__unset" => Ok(None),
        Some(v) => T::deserialize(v)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

/// Deserialize a HashMap that might be "__unset"
fn deserialize_hashmap_maybe_unset<'de, D>(
    deserializer: D,
) -> Result<Option<IndexMap<String, String>>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_maybe_unset(deserializer)
}

/// Deserialize Composer's `license` field, which permits either a single
/// string or its canonical array form, while retaining Packagist's
/// `__unset` marker support.
fn deserialize_string_or_vec_maybe_unset<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(Value::String(value)) if value == "__unset" => Ok(None),
        Some(Value::String(value)) => Ok(Some(vec![value])),
        Some(value) => Vec::<String>::deserialize(value)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

/// Composer repository (Packagist-compatible)
pub struct ComposerRepository {
    /// Repository name/identifier
    name: String,
    /// Repository URL
    url: String,
    /// Base URL (derived from url, without packages.json path)
    base_url: String,
    /// In-memory package cache
    packages: RwLock<HashMap<String, Vec<Arc<Package>>>>,
    /// Cold metadata for packages returned through the solver-only cache path.
    deferred_metadata: StdMutex<Vec<DeferredMetadataBatch>>,
    /// HTTP client for API requests
    client: reqwest::Client,
    /// File-based cache for HTTP responses
    file_cache: Option<RepoCache>,
    /// Cache TTL
    cache_ttl: Duration,
    /// Authentication configuration
    auth: Option<Arc<AuthConfig>>,
    /// Per-package loading locks to prevent concurrent loads of the same package
    loading_locks: RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Notification URL from repository metadata
    notify_batch: RwLock<Option<String>>,
    /// Search URL template
    search_url: RwLock<Option<String>>,
    /// Providers API URL (for getting packages that provide a virtual package)
    providers_api_url: RwLock<Option<String>>,
    /// Lazy providers URL (V2 metadata-url)
    lazy_providers_url: RwLock<Option<String>>,
    /// Package names published inline alongside lazy-provider metadata.
    partial_package_names: RwLock<Option<Vec<String>>>,
    /// List URL for package name enumeration
    list_url: RwLock<Option<String>>,
    /// Available packages (explicit list from repo)
    available_packages: RwLock<Option<HashSet<String>>>,
    /// Available package patterns (regex patterns)
    available_package_patterns: RwLock<Option<Vec<Regex>>>,
    /// Whether repo has an available packages list
    has_available_package_list: RwLock<bool>,
    /// Source mirrors (by VCS type: git, hg)
    source_mirrors: RwLock<HashMap<String, Vec<SourceMirror>>>,
    /// Dist mirrors
    dist_mirrors: RwLock<Vec<DistMirror>>,
    /// Whether the root server file has been loaded
    root_loaded: RwLock<bool>,
    /// Repository-advertised filter-list capabilities.
    filter_information: RwLock<Option<ComposerRepositoryFilterInformation>>,
    /// Repository-advertised security advisory capabilities.
    security_advisory_information: RwLock<Option<SecurityAdvisoryInformation>>,
    /// Advisory data captured from per-package metadata responses.
    security_advisories: RwLock<BTreeMap<String, Vec<Value>>>,
    /// Raw per-package filter metadata, evaluated against the active pool.
    package_filter_metadata: RwLock<BTreeMap<String, Value>>,
    /// User repository-level filter-list opt-outs.
    user_filter_config: Value,
    /// Whether we're in degraded mode (network issues but using cache)
    degraded_mode: RwLock<bool>,
    /// Packages that returned 404 (don't re-fetch)
    packages_not_found: RwLock<HashSet<String>>,
}

struct DeferredMetadataBatch {
    content: Arc<Vec<u8>>,
    packages: Vec<(Weak<Package>, Range<usize>)>,
}

impl ComposerRepository {
    /// Create a new Composer repository
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        let url_str = url.into();
        // Normalize URL: ensure it ends without trailing slash
        let url_normalized = url_str.trim_end_matches('/').to_string();

        // Derive base URL (remove packages.json if present)
        let base_url = if url_normalized.ends_with(".json") {
            // Remove the JSON file to get base
            url_normalized
                .rsplit_once('/')
                .map(|(base, _)| base.to_string())
                .unwrap_or_else(|| url_normalized.clone())
        } else {
            url_normalized.clone()
        };

        Self {
            name: name.into(),
            url: url_normalized,
            base_url,
            packages: RwLock::new(HashMap::new()),
            deferred_metadata: StdMutex::new(Vec::new()),
            loading_locks: RwLock::new(HashMap::new()),
            client: reqwest::Client::builder()
                .user_agent("riff-composer/0.1.0")
                .build()
                .unwrap_or_default(),
            file_cache: None,
            cache_ttl: DEFAULT_CACHE_TTL,
            auth: None,
            notify_batch: RwLock::new(None),
            search_url: RwLock::new(None),
            providers_api_url: RwLock::new(None),
            lazy_providers_url: RwLock::new(None),
            partial_package_names: RwLock::new(None),
            list_url: RwLock::new(None),
            available_packages: RwLock::new(None),
            available_package_patterns: RwLock::new(None),
            has_available_package_list: RwLock::new(false),
            source_mirrors: RwLock::new(HashMap::new()),
            dist_mirrors: RwLock::new(Vec::new()),
            root_loaded: RwLock::new(false),
            filter_information: RwLock::new(None),
            security_advisory_information: RwLock::new(None),
            security_advisories: RwLock::new(BTreeMap::new()),
            package_filter_metadata: RwLock::new(BTreeMap::new()),
            user_filter_config: serde_json::json!({}),
            degraded_mode: RwLock::new(false),
            packages_not_found: RwLock::new(HashSet::new()),
        }
    }

    /// Create a Composer repository with file caching enabled.
    pub fn with_cache(name: impl Into<String>, url: impl Into<String>, cache_dir: PathBuf) -> Self {
        let mut repo = Self::new(name, url);
        repo.set_cache_dir(cache_dir);
        repo
    }

    /// Create Packagist.org repository
    pub fn packagist() -> Self {
        Self::new("packagist.org", "https://repo.packagist.org")
    }

    /// Create Packagist.org repository with file caching enabled
    pub fn packagist_with_cache(cache_dir: PathBuf) -> Self {
        Self::with_cache("packagist.org", "https://repo.packagist.org", cache_dir)
    }

    /// Set the cache directory, enabling file-based caching
    pub fn set_cache_dir(&mut self, cache_dir: PathBuf) {
        self.file_cache = Some(RepoCache::new(cache_dir, &self.url));
    }

    /// Set the cache TTL
    pub fn set_cache_ttl(&mut self, ttl: Duration) {
        self.cache_ttl = ttl;
    }

    /// Get the repository URL
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Set authentication configuration
    pub fn set_auth(&mut self, auth: AuthConfig) {
        self.auth = Some(Arc::new(auth));
    }

    /// Configure repository filter-list behavior. `false` disables repository
    /// filter metadata entirely; list keys set to `false` opt out individually.
    pub fn set_user_filter_config(&mut self, config: Value) {
        self.user_filter_config = config;
    }

    /// Apply authentication to a request builder
    fn apply_auth(
        &self,
        mut request: reqwest::RequestBuilder,
        url: &str,
    ) -> reqwest::RequestBuilder {
        if let Some(ref auth) = self.auth {
            match auth.find_for_url(url) {
                crate::config::AuthMatch::HttpBasic(creds) => {
                    request = request.basic_auth(&creds.username, Some(&creds.password));
                }
                crate::config::AuthMatch::Bearer(token) => {
                    request = request.bearer_auth(token);
                }
                crate::config::AuthMatch::GitHubOAuth(token) => {
                    request = request.bearer_auth(token);
                }
                crate::config::AuthMatch::GitLabToken(token) => {
                    request = request.header("PRIVATE-TOKEN", token);
                }
                crate::config::AuthMatch::BitbucketOAuth(creds) => {
                    request = request.basic_auth(&creds.consumer_key, Some(&creds.consumer_secret));
                }
                crate::config::AuthMatch::None => {}
            }
        }
        request
    }

    /// Generate cache key for a package
    fn cache_key(package_name: &str) -> String {
        // Convert vendor/package to vendor~package for safe filesystem use
        format!("provider-{}.json", package_name.replace('/', "~"))
    }

    fn parsed_cache_key(package_name: &str) -> String {
        format!(
            "{}.packages-v{}.msgpack",
            Self::cache_key(package_name),
            PARSED_PACKAGE_CACHE_VERSION
        )
    }

    fn filtered_cache_key(package_name: &str) -> String {
        format!(
            "{}.filtered-v{}.rkyv",
            Self::cache_key(package_name),
            FILTERED_PACKAGE_CACHE_VERSION
        )
    }

    fn canonicalize_url(&self, url: &str) -> String {
        if url.starts_with('/') {
            if let Some(pos) = self.base_url.find("://") {
                let after_scheme = &self.base_url[pos + 3..];
                if let Some(slash_pos) = after_scheme.find('/') {
                    let host_part = &self.base_url[..pos + 3 + slash_pos];
                    return format!("{}{}", host_part, url);
                }
            } else {
                return self.base_url.clone();
            }
            format!("{}{}", self.base_url, url)
        } else {
            url.to_string()
        }
    }

    fn package_name_to_regex(pattern: &str) -> Option<Regex> {
        let escaped = regex::escape(pattern);
        let regex_str = escaped.replace(r"\*", ".*");
        Regex::new(&format!("^{}$", regex_str)).ok()
    }

    async fn load_root_server_file(&self) -> Result<(), String> {
        if *self.root_loaded.read().await {
            return Ok(());
        }

        let packages_url = if self.url.ends_with(".json") {
            self.url.clone()
        } else {
            format!("{}/packages.json", self.url)
        };
        let cache_key = "packages.json".to_string();

        let body = if let Some(ref file_cache) = self.file_cache {
            if let Ok(Some((cached_content, metadata))) = file_cache.read(&cache_key) {
                if let Ok(Some(age)) = file_cache.age(&cache_key) {
                    if age < self.cache_ttl {
                        String::from_utf8_lossy(&cached_content).to_string()
                    } else if let Some(ref last_modified) = metadata.last_modified {
                        match self.fetch_if_modified(&packages_url, last_modified).await {
                            Ok(FetchResult::NotModified) => {
                                file_cache
                                    .write(&cache_key, &cached_content, &metadata)
                                    .ok();
                                String::from_utf8_lossy(&cached_content).to_string()
                            }
                            Ok(FetchResult::Modified(body, new_metadata)) => {
                                file_cache
                                    .write(&cache_key, body.as_bytes(), &new_metadata)
                                    .ok();
                                body
                            }
                            Err(_) => {
                                *self.degraded_mode.write().await = true;
                                String::from_utf8_lossy(&cached_content).to_string()
                            }
                        }
                    } else {
                        match self.fetch_fresh(&packages_url).await {
                            Ok((body, new_metadata)) => {
                                file_cache
                                    .write(&cache_key, body.as_bytes(), &new_metadata)
                                    .ok();
                                body
                            }
                            Err(_) => {
                                *self.degraded_mode.write().await = true;
                                String::from_utf8_lossy(&cached_content).to_string()
                            }
                        }
                    }
                } else {
                    String::from_utf8_lossy(&cached_content).to_string()
                }
            } else {
                match self.fetch_fresh(&packages_url).await {
                    Ok((body, metadata)) => {
                        file_cache
                            .write(&cache_key, body.as_bytes(), &metadata)
                            .ok();
                        body
                    }
                    Err(e) => return Err(e),
                }
            }
        } else {
            match self.fetch_fresh(&packages_url).await {
                Ok((body, _)) => body,
                Err(e) => return Err(e),
            }
        };

        let data: Value = serde_json::from_str(&body)
            .map_err(|e| format!("Failed to parse packages.json: {}", e))?;

        // Composer v1 repositories may publish complete package metadata directly
        // in packages.json. Keep it in the in-memory package cache so subsequent
        // package lookups do not incorrectly fall through to the v2 p2 endpoint.
        let inline_packages = data
            .get("packages")
            .and_then(Value::as_object)
            .cloned()
            .or_else(|| {
                data.as_object().map(|root| {
                    root.iter()
                        .filter_map(|(name, metadata)| {
                            metadata
                                .get("versions")
                                .cloned()
                                .map(|versions| (name.clone(), versions))
                        })
                        .collect()
                })
            })
            .or_else(|| {
                data.as_array().map(|packages| {
                    let mut grouped = serde_json::Map::new();
                    for package in packages {
                        let Some(name) = package.get("name").and_then(Value::as_str) else {
                            continue;
                        };
                        grouped
                            .entry(name.to_string())
                            .or_insert_with(|| Value::Array(Vec::new()))
                            .as_array_mut()
                            .expect("grouped package metadata is always an array")
                            .push(package.clone());
                    }
                    grouped
                })
            })
            .unwrap_or_default();
        if !inline_packages.is_empty() {
            *self.partial_package_names.write().await =
                Some(inline_packages.keys().cloned().collect());
        }
        for (name, versions) in &inline_packages {
            let versions = versions
                .as_object()
                .map(|versions| Value::Array(versions.values().cloned().collect()))
                .or_else(|| versions.as_array().cloned().map(Value::Array))
                .unwrap_or_else(|| Value::Array(Vec::new()));
            let package_metadata = serde_json::json!({
                "packages": { name: versions }
            });
            let package_metadata = serde_json::to_vec(&package_metadata)
                .map_err(|e| format!("Failed to encode package metadata: {e}"))?;
            self.parse_and_cache_response_inner(name, &package_metadata, None, false)
                .await?;
        }

        if let Some(notify) = data.get("notify-batch").and_then(|v| v.as_str()) {
            *self.notify_batch.write().await = Some(self.canonicalize_url(notify));
        } else if let Some(notify) = data.get("notify").and_then(|v| v.as_str()) {
            *self.notify_batch.write().await = Some(self.canonicalize_url(notify));
        }

        if let Some(search) = data.get("search").and_then(|v| v.as_str()) {
            *self.search_url.write().await = Some(self.canonicalize_url(search));
        }

        if let Some(list) = data.get("list").and_then(|v| v.as_str()) {
            *self.list_url.write().await = Some(self.canonicalize_url(list));
        }

        if let Some(providers_api) = data.get("providers-api").and_then(|v| v.as_str()) {
            *self.providers_api_url.write().await = Some(self.canonicalize_url(providers_api));
        }
        if let Some(security) = data.get("security-advisories").and_then(Value::as_object) {
            *self.security_advisory_information.write().await = Some(SecurityAdvisoryInformation {
                metadata: security
                    .get("metadata")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                api_url: security
                    .get("api-url")
                    .and_then(Value::as_str)
                    .map(|url| self.canonicalize_url(url)),
            });
        }
        if let Some(mirrors) = data.get("mirrors").and_then(|v| v.as_array()) {
            let mut source_mirrors = HashMap::new();
            let mut dist_mirrors = Vec::new();

            for mirror in mirrors {
                let preferred = mirror
                    .get("preferred")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if let Some(git_url) = mirror.get("git-url").and_then(|v| v.as_str()) {
                    source_mirrors
                        .entry("git".to_string())
                        .or_insert_with(Vec::new)
                        .push(SourceMirror {
                            url: git_url.to_string(),
                            preferred,
                        });
                }

                if let Some(hg_url) = mirror.get("hg-url").and_then(|v| v.as_str()) {
                    source_mirrors
                        .entry("hg".to_string())
                        .or_insert_with(Vec::new)
                        .push(SourceMirror {
                            url: hg_url.to_string(),
                            preferred,
                        });
                }

                if let Some(dist_url) = mirror.get("dist-url").and_then(|v| v.as_str()) {
                    dist_mirrors.push(DistMirror {
                        url: self.canonicalize_url(dist_url),
                        preferred,
                    });
                }
            }

            *self.source_mirrors.write().await = source_mirrors;
            *self.dist_mirrors.write().await = dist_mirrors;
        }

        if let Some(metadata_url) = data.get("metadata-url").and_then(|v| v.as_str()) {
            *self.lazy_providers_url.write().await = Some(self.canonicalize_url(metadata_url));

            if let Some(filter) = data.get("filter").filter(|filter| filter.is_object()) {
                *self.filter_information.write().await = Some(
                    ComposerRepositoryFilterInformation::from_data_with(filter, |url| {
                        self.canonicalize_url(url)
                    }),
                );
            }

            if let Some(available) = data.get("available-packages").and_then(|v| v.as_array()) {
                let packages: HashSet<String> = available
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_lowercase())
                    .collect();
                *self.available_packages.write().await = Some(packages);
                *self.has_available_package_list.write().await = true;
            }

            if let Some(patterns) = data
                .get("available-package-patterns")
                .and_then(|v| v.as_array())
            {
                let regexes: Vec<Regex> = patterns
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter_map(Self::package_name_to_regex)
                    .collect();
                if !regexes.is_empty() {
                    *self.available_package_patterns.write().await = Some(regexes);
                    *self.has_available_package_list.write().await = true;
                }
            }
        } else if let Some(providers_lazy_url) =
            data.get("providers-lazy-url").and_then(|v| v.as_str())
        {
            *self.lazy_providers_url.write().await =
                Some(self.canonicalize_url(providers_lazy_url));
        }

        *self.root_loaded.write().await = true;
        Ok(())
    }

    async fn lazy_providers_repo_contains(&self, name: &str) -> bool {
        let name_lower = name.to_lowercase();

        if let Some(ref available) = *self.available_packages.read().await {
            if available.contains(&name_lower) {
                return true;
            }
        }

        if let Some(ref patterns) = *self.available_package_patterns.read().await {
            for pattern in patterns {
                if pattern.is_match(&name_lower) {
                    return true;
                }
            }
        }

        !*self.has_available_package_list.read().await
    }

    async fn load_package_list(&self, filter: Option<&str>) -> Result<Vec<String>, String> {
        let list_url = self
            .list_url
            .read()
            .await
            .clone()
            .ok_or_else(|| "No list URL available".to_string())?;

        let url = if let Some(f) = filter {
            format!("{}?filter={}", list_url, urlencoding::encode(f))
        } else {
            list_url
        };

        let cache_key = if filter.is_some() {
            None
        } else {
            Some("package-list.txt".to_string())
        };

        if let (Some(ref key), Some(ref file_cache)) = (&cache_key, &self.file_cache) {
            if let Ok(Some(age)) = file_cache.age(key) {
                if age < self.cache_ttl {
                    if let Ok(Some((content, _))) = file_cache.read(key) {
                        let names: Vec<String> = String::from_utf8_lossy(&content)
                            .lines()
                            .map(|s| s.to_string())
                            .collect();
                        return Ok(names);
                    }
                }
            }
        }

        let (body, _) = self.fetch_fresh(&url).await?;
        let data: Value = serde_json::from_str(&body)
            .map_err(|e| format!("Failed to parse package list: {}", e))?;

        let names: Vec<String> = data
            .get("packageNames")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        if let (Some(ref key), Some(ref file_cache)) = (&cache_key, &self.file_cache) {
            let content = names.join("\n");
            file_cache
                .write(key, content.as_bytes(), &CacheMetadata::default())
                .ok();
        }

        Ok(names)
    }

    async fn load_package_metadata(&self, name: &str) -> Result<Vec<Arc<Package>>, String> {
        self.load_package_metadata_inner(name, None, false).await
    }

    async fn load_package_metadata_with_constraint(
        &self,
        name: &str,
        constraint: &str,
    ) -> Result<Vec<Arc<Package>>, String> {
        if constraint == "*" || constraint.is_empty() {
            return self.load_package_metadata(name).await;
        }
        self.load_package_metadata_inner(name, Some(constraint), false)
            .await
    }

    async fn load_solver_package_metadata_with_constraint(
        &self,
        name: &str,
        constraint: &str,
    ) -> Result<Vec<Arc<Package>>, String> {
        if constraint == "*" || constraint.is_empty() {
            return self.load_package_metadata(name).await;
        }
        self.load_package_metadata_inner(name, Some(constraint), true)
            .await
    }

    async fn load_package_metadata_inner(
        &self,
        name: &str,
        constraint: Option<&str>,
        defer_metadata: bool,
    ) -> Result<Vec<Arc<Package>>, String> {
        let name_lower = name.to_lowercase();
        let name = name_lower.as_str();

        self.load_root_server_file().await.ok();

        if self.packages_not_found.read().await.contains(name) {
            return Ok(Vec::new());
        }

        if *self.has_available_package_list.read().await
            && !self.lazy_providers_repo_contains(name).await
        {
            return Ok(Vec::new());
        }

        {
            let packages = self.packages.read().await;
            if let Some(pkgs) = packages.get(name) {
                log::trace!("Cache hit (memory): {}", name);
                return Ok(Self::filter_packages(pkgs.clone(), constraint));
            }
        }

        let lock = {
            let mut locks = self.loading_locks.write().await;
            locks
                .entry(name.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };

        let _guard = lock.lock().await;

        {
            let packages = self.packages.read().await;
            if let Some(pkgs) = packages.get(name) {
                log::trace!("Cache hit (memory, after lock): {}", name);
                return Ok(Self::filter_packages(pkgs.clone(), constraint));
            }
        }

        let cache_key = Self::cache_key(name);

        let url = if let Some(ref lazy_url) = *self.lazy_providers_url.read().await {
            lazy_url.replace("%package%", name)
        } else {
            format!("{}/p2/{}.json", self.url, name)
        };

        if let (Some(constraint), Some(file_cache)) = (constraint, &self.file_cache) {
            if let Ok(Some(source_sha256)) =
                file_cache.fresh_content_sha256(&cache_key, self.cache_ttl)
            {
                let notify_batch = self.notify_batch.read().await.clone();
                if defer_metadata {
                    if let Some(packages) = self.read_solver_filtered_package_cache_with_digest(
                        name,
                        source_sha256,
                        &notify_batch,
                        constraint,
                    ) {
                        return Ok(packages);
                    }
                } else if let Some(packages) = self.read_filtered_package_cache_with_digest(
                    name,
                    source_sha256,
                    &notify_batch,
                    constraint,
                ) {
                    return Ok(packages.into_iter().map(Arc::new).collect());
                }
            }
        }

        if let Some(ref file_cache) = self.file_cache {
            if let Ok(Some((cached_content, metadata))) = file_cache.read(&cache_key) {
                if let Ok(Some(age)) = file_cache.age(&cache_key) {
                    if age < self.cache_ttl {
                        log::trace!("Cache hit (file, fresh): {} (age: {:?})", name, age);
                        if constraint.is_some() {
                            file_cache
                                .write_metadata_for_content(&cache_key, &cached_content, &metadata)
                                .ok();
                        }
                        if let Ok(result) = self
                            .parse_and_cache_response_inner(
                                name,
                                &cached_content,
                                constraint,
                                defer_metadata,
                            )
                            .await
                        {
                            return Ok(result);
                        }
                    }
                }

                if let Some(last_modified) = &metadata.last_modified {
                    log::debug!("Cache stale, checking: {}", name);
                    match self.fetch_if_modified(&url, last_modified).await {
                        Ok(FetchResult::NotModified) => {
                            log::trace!("Cache valid (304): {}", name);
                            file_cache
                                .write(&cache_key, &cached_content, &metadata)
                                .ok();
                            if let Ok(result) = self
                                .parse_and_cache_response_inner(
                                    name,
                                    &cached_content,
                                    constraint,
                                    defer_metadata,
                                )
                                .await
                            {
                                return Ok(result);
                            }
                        }
                        Ok(FetchResult::Modified(body, new_metadata)) => {
                            log::debug!("Cache updated: {} ({} bytes)", name, body.len());
                            file_cache
                                .write(&cache_key, body.as_bytes(), &new_metadata)
                                .ok();
                            if let Ok(result) = self
                                .parse_and_cache_response_inner(
                                    name,
                                    body.as_bytes(),
                                    constraint,
                                    defer_metadata,
                                )
                                .await
                            {
                                return Ok(result);
                            }
                        }
                        Err(_) => {
                            log::debug!("Network error, using stale cache: {}", name);
                            if let Ok(result) = self
                                .parse_and_cache_response_inner(
                                    name,
                                    &cached_content,
                                    constraint,
                                    defer_metadata,
                                )
                                .await
                            {
                                return Ok(result);
                            }
                        }
                    }
                }
            }
        }

        log::debug!("Cache miss, fetching: {}", name);
        let (body, metadata) = self.fetch_fresh(&url).await?;

        if let Some(ref file_cache) = self.file_cache {
            file_cache
                .write(&cache_key, body.as_bytes(), &metadata)
                .ok();
        }

        self.parse_and_cache_response_inner(name, body.as_bytes(), constraint, defer_metadata)
            .await
    }

    async fn fetch_if_modified(
        &self,
        url: &str,
        last_modified: &str,
    ) -> Result<FetchResult, String> {
        if let Some(path) = url.strip_prefix("file://") {
            let body = tokio::fs::read_to_string(path).await.map_err(|error| {
                format!("Failed to read Composer repository file {path}: {error}")
            })?;
            return Ok(FetchResult::Modified(body, CacheMetadata::default()));
        }
        let request = self
            .client
            .get(url)
            .header("If-Modified-Since", last_modified);
        let request = self.apply_auth(request, url);
        let response = request
            .send()
            .await
            .map_err(|e| format!("Failed to fetch package metadata: {}", e))?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(FetchResult::NotModified);
        }

        if !response.status().is_success() {
            return Err(format!("HTTP error: {}", response.status()));
        }

        let new_last_modified = response
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let body = response
            .text()
            .await
            .map_err(|e| format!("Failed to read response body: {}", e))?;

        let metadata = CacheMetadata {
            last_modified: new_last_modified,
            etag: None,
            ..CacheMetadata::default()
        };

        Ok(FetchResult::Modified(body, metadata))
    }

    async fn fetch_fresh(&self, url: &str) -> Result<(String, CacheMetadata), String> {
        if let Some(path) = url.strip_prefix("file://") {
            let body = tokio::fs::read_to_string(path).await.map_err(|error| {
                format!("Failed to read Composer repository file {path}: {error}")
            })?;
            return Ok((body, CacheMetadata::default()));
        }
        log::debug!("HTTP GET {}", url);
        let start = std::time::Instant::now();

        let request = self.client.get(url);
        let request = self.apply_auth(request, url);
        let response = request
            .send()
            .await
            .map_err(|e| format!("Failed to fetch package metadata: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            log::debug!("HTTP {} {} in {:?}", status.as_u16(), url, start.elapsed());
            if status.as_u16() == 404 {
                return Ok((String::new(), CacheMetadata::default()));
            } else {
                return Err(format!("HTTP {} for {}", status.as_u16(), url));
            }
        }

        let last_modified = response
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let body = response
            .text()
            .await
            .map_err(|e| format!("Failed to read response body: {}", e))?;

        log::debug!(
            "HTTP 200 {} ({} bytes) in {:?}",
            url,
            body.len(),
            start.elapsed()
        );

        let metadata = CacheMetadata {
            last_modified,
            etag: None,
            ..CacheMetadata::default()
        };

        Ok((body, metadata))
    }

    #[cfg(test)]
    async fn parse_and_cache_response(
        &self,
        name: &str,
        body: &[u8],
        constraint: Option<&str>,
    ) -> Result<Vec<Arc<Package>>, String> {
        self.parse_and_cache_response_inner(name, body, constraint, false)
            .await
    }

    async fn parse_and_cache_response_inner(
        &self,
        name: &str,
        body: &[u8],
        constraint: Option<&str>,
        defer_metadata: bool,
    ) -> Result<Vec<Arc<Package>>, String> {
        if body.is_empty() {
            return Ok(Vec::new());
        }

        self.capture_package_policy_metadata(name, body).await;

        let notify_batch = self.notify_batch.read().await.clone();
        if let Some(constraint) = constraint {
            if defer_metadata {
                if let Some(packages) =
                    self.read_solver_filtered_package_cache(name, body, &notify_batch, constraint)
                {
                    return Ok(packages);
                }
            } else if let Some(packages) =
                self.read_filtered_package_cache(name, body, &notify_batch, constraint)
            {
                return Ok(packages.into_iter().map(Arc::new).collect());
            }
        }

        if let Some(packages) = self.read_parsed_package_cache(name, body, &notify_batch) {
            let all_packages: Vec<_> = packages.into_iter().map(Arc::new).collect();
            self.packages
                .write()
                .await
                .insert(name.to_string(), all_packages.clone());
            let result = Self::filter_packages(all_packages, constraint);
            if let Some(constraint) = constraint {
                self.write_filtered_package_cache(name, body, &notify_batch, constraint, &result);
            }
            return Ok(result);
        }

        let data: PackagistResponse = serde_json::from_slice(body)
            .map_err(|e| format!("Failed to parse package metadata: {}", e))?;

        let mut packages = Vec::new();

        if let Some(versions) = data.packages.get(name) {
            let expanded_versions = if data.minified.as_deref() == Some("composer/2.0") {
                Self::expand_minified_versions(versions)?
            } else {
                versions
                    .iter()
                    .enumerate()
                    .map(|(index, version)| {
                        serde_json::from_value(version.clone()).map_err(|error| {
                            format!(
                                "Failed to parse package metadata version at index {index}: {error}"
                            )
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?
            };
            for expanded_data in &expanded_versions {
                let pkg = self.convert_to_package(name, expanded_data, notify_batch.as_deref());
                packages.push(pkg);
            }
        }

        self.write_parsed_package_cache(name, body, &notify_batch, &packages);
        let all_packages: Vec<_> = packages.into_iter().map(Arc::new).collect();

        {
            let mut packages = self.packages.write().await;
            packages.insert(name.to_string(), all_packages.clone());
        }

        let result = Self::filter_packages(all_packages, constraint);
        if let Some(constraint) = constraint {
            self.write_filtered_package_cache(name, body, &notify_batch, constraint, &result);
        }

        Ok(result)
    }

    async fn capture_package_policy_metadata(&self, package_name: &str, body: &[u8]) {
        let Ok(document) = serde_json::from_slice::<Value>(body) else {
            return;
        };
        if let Some(filter) = document.get("filter").filter(|value| value.is_object()) {
            self.package_filter_metadata
                .write()
                .await
                .insert(package_name.to_string(), filter.clone());
        }

        let Some(raw) = document.get("security-advisories") else {
            return;
        };
        let mut captured = BTreeMap::<String, Vec<Value>>::new();
        if let Some(entries) = raw.as_array() {
            captured.insert(package_name.to_string(), entries.clone());
        } else if let Some(packages) = raw.as_object() {
            for (name, entries) in packages {
                let Some(entries) = entries.as_array() else {
                    continue;
                };
                captured.insert(name.clone(), entries.clone());
            }
        }
        self.security_advisories.write().await.extend(captured);
    }

    fn read_parsed_package_cache(
        &self,
        name: &str,
        body: &[u8],
        notify_batch: &Option<String>,
    ) -> Option<Vec<Package>> {
        let cache = self.file_cache.as_ref()?;
        let content = cache.read_data(&Self::parsed_cache_key(name)).ok()??;
        let parsed: ParsedPackageCache = rmp_serde::from_slice(&content).ok()?;
        let source_sha256: [u8; 32] = Sha256::digest(body).into();
        if parsed.version != PARSED_PACKAGE_CACHE_VERSION
            || parsed.source_sha256 != source_sha256
            || parsed.notify_batch != *notify_batch
        {
            return None;
        }
        Some(parsed.packages)
    }

    fn write_parsed_package_cache(
        &self,
        name: &str,
        body: &[u8],
        notify_batch: &Option<String>,
        packages: &[Package],
    ) {
        let Some(cache) = &self.file_cache else {
            return;
        };
        let parsed = ParsedPackageCacheRef {
            version: PARSED_PACKAGE_CACHE_VERSION,
            source_sha256: Sha256::digest(body).into(),
            notify_batch: notify_batch.as_deref(),
            packages,
        };
        if let Ok(content) = rmp_serde::to_vec_named(&parsed) {
            let _ = cache.write_data(&Self::parsed_cache_key(name), &content);
        }
    }

    fn read_filtered_package_cache(
        &self,
        name: &str,
        body: &[u8],
        notify_batch: &Option<String>,
        constraint: &str,
    ) -> Option<Vec<Package>> {
        self.read_filtered_package_cache_with_digest(
            name,
            Sha256::digest(body).into(),
            notify_batch,
            constraint,
        )
    }

    fn read_filtered_package_cache_with_digest(
        &self,
        name: &str,
        source_sha256: [u8; 32],
        notify_batch: &Option<String>,
        constraint: &str,
    ) -> Option<Vec<Package>> {
        let content = self.read_filtered_package_cache_content(name)?;
        Self::parse_filtered_package_cache_entries(
            &content,
            source_sha256,
            notify_batch,
            constraint,
        )?
        .iter()
        .map(|cached| {
            let (package, metadata) = cached.to_solver_package()?;
            CachedPackage::hydrate(package, metadata)
        })
        .collect()
    }

    fn read_solver_filtered_package_cache(
        &self,
        name: &str,
        body: &[u8],
        notify_batch: &Option<String>,
        constraint: &str,
    ) -> Option<Vec<Arc<Package>>> {
        self.read_solver_filtered_package_cache_with_digest(
            name,
            Sha256::digest(body).into(),
            notify_batch,
            constraint,
        )
    }

    fn read_solver_filtered_package_cache_with_digest(
        &self,
        name: &str,
        source_sha256: [u8; 32],
        notify_batch: &Option<String>,
        constraint: &str,
    ) -> Option<Vec<Arc<Package>>> {
        let content = self.read_filtered_package_cache_content(name)?;
        let cached = Self::parse_filtered_package_cache_entries(
            &content,
            source_sha256,
            notify_batch,
            constraint,
        )?;
        let mut packages = Vec::with_capacity(cached.len());
        let mut deferred = Vec::with_capacity(cached.len());
        let content_start = content.as_ptr() as usize;
        for cached_package in cached.iter() {
            let (package, metadata) = cached_package.to_solver_package()?;
            let metadata_start = metadata.as_ptr() as usize - content_start;
            let package = Arc::new(package);
            deferred.push((
                Arc::downgrade(&package),
                metadata_start..metadata_start + metadata.len(),
            ));
            packages.push(package);
        }
        self.deferred_metadata
            .lock()
            .ok()?
            .push(DeferredMetadataBatch {
                content: Arc::new(content),
                packages: deferred,
            });
        Some(packages)
    }

    fn parse_filtered_package_cache_entries<'a>(
        content: &'a [u8],
        source_sha256: [u8; 32],
        notify_batch: &Option<String>,
        constraint: &str,
    ) -> Option<&'a rkyv::vec::ArchivedVec<ArchivedCachedPackage>> {
        let parsed =
            rkyv::access::<ArchivedFilteredPackageCache, rkyv::rancor::Error>(content).ok()?;
        if parsed.version != FILTERED_PACKAGE_CACHE_VERSION
            || parsed.source_sha256 != source_sha256
            || parsed.notify_batch.as_ref().map(|value| value.as_str()) != notify_batch.as_deref()
            || parsed.constraint.as_str() != constraint
        {
            return None;
        }
        Some(&parsed.packages)
    }

    fn read_filtered_package_cache_content(&self, name: &str) -> Option<Vec<u8>> {
        let cache = self.file_cache.as_ref()?;
        cache.read_data(&Self::filtered_cache_key(name)).ok()?
    }

    fn write_filtered_package_cache(
        &self,
        name: &str,
        body: &[u8],
        notify_batch: &Option<String>,
        constraint: &str,
        packages: &[Arc<Package>],
    ) {
        let Some(cache) = &self.file_cache else {
            return;
        };
        let Some(cached_packages): Option<Vec<_>> = packages
            .iter()
            .map(|package| CachedPackage::from_package(package.as_ref()))
            .collect()
        else {
            return;
        };
        let parsed = FilteredPackageCache {
            version: FILTERED_PACKAGE_CACHE_VERSION,
            source_sha256: Sha256::digest(body).into(),
            notify_batch: notify_batch.clone(),
            constraint: constraint.to_owned(),
            packages: cached_packages,
        };
        if let Ok(content) = rkyv::to_bytes::<rkyv::rancor::Error>(&parsed) {
            let _ = cache.write_data(&Self::filtered_cache_key(name), content.as_slice());
        }
    }

    fn filter_packages(packages: Vec<Arc<Package>>, constraint: Option<&str>) -> Vec<Arc<Package>> {
        let Some(constraint) = constraint.filter(|value| !value.is_empty() && *value != "*") else {
            return packages;
        };

        let parsed_constraint = match VersionParser::new().parse_constraints(constraint) {
            Ok(constraint) => constraint,
            Err(_) => return packages,
        };

        packages
            .into_iter()
            .filter(|package| {
                let matches = |version: &str| {
                    Constraint::new(Operator::Equal, version.to_owned())
                        .map(|version| parsed_constraint.matches(&version))
                        .unwrap_or(true)
                };

                matches(&package.version)
                    || parse_branch_aliases(package.extra.as_ref()).values().any(
                        |(normalized, pretty)| {
                            matches(normalized)
                                || Self::branch_alias_matches_constraint(pretty, constraint)
                        },
                    )
            })
            .collect()
    }

    // Composer repositories must return a dev branch when its branch alias is
    // requested. Comparing the `-dev` alias directly applies prerelease rules
    // and rejects ranges such as `3.2.*@dev`; compare its numeric development
    // line instead, while retaining the original dev package for the solver.
    fn branch_alias_matches_constraint(alias: &str, constraint: &str) -> bool {
        let Some(numeric) = alias.strip_suffix("-dev") else {
            return false;
        };
        if !numeric
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
        {
            return false;
        }
        let numeric = numeric
            .split('.')
            .map(|part| {
                if part.eq_ignore_ascii_case("x") {
                    "9999999"
                } else {
                    part
                }
            })
            .collect::<Vec<_>>()
            .join(".");
        Semver::satisfies(&numeric, constraint)
    }

    /// Expand Packagist v2 minified versions to full version data.
    ///
    /// Packagist v2 uses delta compression where each version only includes
    /// fields that changed from the previous version. This function expands
    /// the minified data to full versions.
    fn expand_minified_versions(versions: &[Value]) -> Result<Vec<PackagistVersion>, String> {
        let mut result = Vec::with_capacity(versions.len());
        let mut expanded = serde_json::Map::new();

        for (index, version_data) in versions.iter().enumerate() {
            let changes = version_data.as_object().ok_or_else(|| {
                format!("Package metadata version at index {index} must be an object")
            })?;
            for (field, value) in changes {
                match value {
                    Value::Null => {}
                    Value::String(marker) if marker == "__unset" => {
                        expanded.remove(field);
                    }
                    _ => {
                        expanded.insert(field.clone(), value.clone());
                    }
                }
            }
            result.push(
                serde_json::from_value(Value::Object(expanded.clone())).map_err(|error| {
                    format!("Failed to expand package metadata version at index {index}: {error}")
                })?,
            );
        }

        Ok(result)
    }

    fn convert_to_package(
        &self,
        package_name: &str,
        data: &PackagistVersion,
        notify_batch: Option<&str>,
    ) -> Package {
        let version = data.version_normalized.clone().unwrap_or_else(|| {
            VersionParser::new()
                .normalize(&data.version)
                .unwrap_or_else(|_| data.version.clone())
        });
        let mut pkg = Package::new(package_name, version);
        pkg.pretty_version = Some(data.version.clone().into());

        pkg.description = data.description.clone();
        pkg.homepage = data.homepage.clone();
        pkg.license = data
            .license
            .iter()
            .flatten()
            .map(|value| value.as_str().into())
            .collect();
        pkg.keywords = data
            .keywords
            .iter()
            .flatten()
            .map(|value| value.as_str().into())
            .collect();
        pkg.require = data.require.clone().unwrap_or_default().into();
        pkg.require_dev = data.require_dev.clone().unwrap_or_default().into();
        pkg.conflict = data.conflict.clone().unwrap_or_default().into();
        pkg.provide = data.provide.clone().unwrap_or_default().into();
        pkg.replace = data.replace.clone().unwrap_or_default().into();
        pkg.suggest = data.suggest.clone().unwrap_or_default().into();
        pkg.package_type = data
            .package_type
            .clone()
            .unwrap_or_else(|| "library".to_string())
            .into();
        pkg.bin = data
            .bin
            .iter()
            .flatten()
            .map(|value| value.as_str().into())
            .collect();

        if let Some(source) = &data.source {
            pkg.source = Some(Source::new(
                &source.source_type,
                &source.url,
                &source.reference,
            ));
        }

        if let Some(dist) = &data.dist {
            let mut d = Dist::new(&dist.dist_type, &dist.url);
            if let Some(ref r) = dist.reference {
                d = d.with_reference(r);
            }
            if let Some(ref s) = dist.shasum {
                if !s.is_empty() {
                    d = d.with_shasum(s);
                }
            }
            pkg.dist = Some(d);
        }

        if let Some(authors) = &data.authors {
            pkg.authors = authors
                .iter()
                .map(|a| crate::package::Author {
                    name: a.name.as_deref().map(Into::into),
                    email: a.email.as_deref().map(Into::into),
                    homepage: a.homepage.as_deref().map(Into::into),
                    role: a.role.as_deref().map(Into::into),
                })
                .collect();
        }

        if let Some(al) = &data.autoload {
            pkg.autoload = Some(Self::convert_autoload(al));
        }

        if let Some(al) = &data.autoload_dev {
            pkg.autoload_dev = Some(Self::convert_autoload(al));
        }

        let time = data.time.as_ref();
        if let Some(t) = time {
            pkg.time = chrono::DateTime::parse_from_rfc3339(t)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc));
        }

        pkg.notification_url = data
            .notification_url
            .clone()
            .or_else(|| notify_batch.map(|s| s.to_string()));

        if let Some(s) = &data.support {
            pkg.support = Some(crate::package::Support {
                issues: s.issues.clone(),
                forum: s.forum.clone(),
                wiki: s.wiki.clone(),
                source: s.source.clone(),
                email: s.email.clone(),
                irc: s.irc.clone(),
                docs: s.docs.clone(),
                rss: s.rss.clone(),
                chat: s.chat.clone(),
                security: s.security.clone(),
            });
        }

        if let Some(f) = &data.funding {
            pkg.funding = f
                .iter()
                .map(|pf| crate::package::Funding {
                    funding_type: pf.funding_type.as_deref().map(Into::into),
                    url: pf.url.as_deref().map(Into::into),
                })
                .collect();
        }

        pkg.extra = data.extra.clone();

        pkg
    }

    fn convert_autoload(al: &PackagistAutoload) -> Autoload {
        let mut autoload = Autoload::default();

        for (namespace, paths) in &al.psr4 {
            let path = Self::json_to_autoload_path(paths);
            autoload.psr4.insert(namespace.clone(), path);
        }

        for (namespace, paths) in &al.psr0 {
            let path = Self::json_to_autoload_path(paths);
            autoload.psr0.insert(namespace.clone(), path);
        }

        autoload.classmap = al
            .classmap
            .iter()
            .map(|value| value.as_str().into())
            .collect();
        autoload.files = al.files.iter().map(|value| value.as_str().into()).collect();
        autoload.exclude_from_classmap = al
            .exclude_from_classmap
            .iter()
            .map(|value| value.as_str().into())
            .collect();

        autoload
    }

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

    pub async fn get_package_names(&self, filter: Option<&str>) -> Vec<String> {
        self.load_root_server_file().await.ok();

        if self.list_url.read().await.is_some() {
            return self.load_package_list(filter).await.unwrap_or_default();
        }

        if let Some(ref available) = *self.available_packages.read().await {
            let names: Vec<String> = available.iter().cloned().collect();

            if let Some(f) = filter {
                if let Some(regex) = Self::package_name_to_regex(f) {
                    return names.into_iter().filter(|n| regex.is_match(n)).collect();
                }
            }

            return names;
        }

        if self.lazy_providers_url.read().await.is_some() {
            if let Some(names) = self.partial_package_names.read().await.clone() {
                if let Some(filter) = filter {
                    if let Some(regex) = Self::package_name_to_regex(filter) {
                        return names
                            .into_iter()
                            .filter(|name| regex.is_match(name))
                            .collect();
                    }
                }
                return names;
            }
        }

        Vec::new()
    }

    pub async fn has_filter(&self) -> bool {
        if Self::user_filter_disabled(&self.user_filter_config) {
            return false;
        }
        self.load_root_server_file().await.ok();
        self.filter_information
            .read()
            .await
            .as_ref()
            .is_some_and(|information| information.metadata)
    }

    pub async fn get_filter_lists(&self) -> Vec<String> {
        if Self::user_filter_disabled(&self.user_filter_config) {
            return Vec::new();
        }
        self.load_root_server_file().await.ok();
        let information = self.filter_information.read().await;
        let Some(information) = information.as_ref() else {
            return Vec::new();
        };
        Self::apply_user_filter_config(&information.lists, &self.user_filter_config)
    }

    fn user_filter_disabled(config: &Value) -> bool {
        config == &Value::Bool(false)
    }

    fn apply_user_filter_config(advertised: &[String], config: &Value) -> Vec<String> {
        let skipped = config
            .as_object()
            .into_iter()
            .flat_map(|config| config.iter())
            .filter_map(|(list, enabled)| (enabled == &Value::Bool(false)).then_some(list))
            .collect::<BTreeSet<_>>();
        advertised
            .iter()
            .filter(|list| !skipped.contains(list))
            .cloned()
            .collect()
    }

    pub fn select_filter_source(
        information: &ComposerRepositoryFilterInformation,
        metadata_already_fetched: bool,
    ) -> ComposerRepositoryFilterSource {
        if !metadata_already_fetched && information.api_url.is_some() {
            ComposerRepositoryFilterSource::Api
        } else if !metadata_already_fetched && information.summary_url.is_some() {
            ComposerRepositoryFilterSource::Summary
        } else {
            ComposerRepositoryFilterSource::PackageMetadata
        }
    }

    pub fn filter_summary_candidates(
        summary: &BTreeMap<String, BTreeMap<String, String>>,
        package_versions: &PackageVersions,
        configured_lists: &[String],
    ) -> PackageVersions {
        let mut candidates = PackageVersions::new();
        for list_name in configured_lists {
            let Some(packages) = summary.get(list_name) else {
                continue;
            };
            for (package_name, summary_constraint) in packages {
                let Some((canonical_name, versions)) = package_versions
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(package_name))
                else {
                    continue;
                };
                if versions
                    .iter()
                    .any(|version| Semver::satisfies(version, summary_constraint))
                {
                    candidates.insert(canonical_name.clone(), versions.clone());
                }
            }
        }
        candidates
    }

    pub fn build_filter_entries(
        raw_filter: &Value,
        package_versions: &PackageVersions,
        configured_lists: &[String],
        default_package: Option<&str>,
    ) -> FilterEntriesByList {
        let mut entries =
            FilterListEntryBuilder.build(raw_filter, package_versions, default_package);
        entries.retain(|list, _| configured_lists.contains(list));
        entries
    }

    pub fn security_advisory_api_request(
        api_url: impl Into<String>,
        package_names: &[String],
        transport_options: Value,
    ) -> ComposerRepositoryApiRequest {
        let body = package_names
            .iter()
            .enumerate()
            .map(|(index, package)| {
                let package =
                    url::form_urlencoded::byte_serialize(package.as_bytes()).collect::<String>();
                format!("packages%5B{index}%5D={package}")
            })
            .collect::<Vec<_>>()
            .join("&");
        ComposerRepositoryApiRequest {
            url: api_url.into(),
            method: "POST",
            content_type: "application/x-www-form-urlencoded",
            timeout_seconds: 10,
            body,
            transport_options,
        }
    }

    pub fn filter_security_advisories(
        package_constraints: &BTreeMap<String, String>,
        advisories: BTreeMap<String, Vec<SecurityAdvisory>>,
    ) -> BTreeMap<String, Vec<SecurityAdvisory>> {
        advisories
            .into_iter()
            .filter_map(|(package, advisories)| {
                let constraint = package_constraints
                    .iter()
                    .find(|(requested, _)| requested.eq_ignore_ascii_case(&package))
                    .map(|(_, constraint)| constraint)?;
                let advisories = advisories
                    .into_iter()
                    .filter(|advisory| {
                        let parser = VersionParser::new();
                        let Ok(affected) =
                            parser.parse_constraints_cached(&advisory.affected_versions)
                        else {
                            return false;
                        };
                        affected.intersects(constraint).unwrap_or(false)
                    })
                    .collect::<Vec<_>>();
                (!advisories.is_empty()).then_some((package, advisories))
            })
            .collect()
    }

    async fn matching_security_advisories(
        &self,
        package_versions: &PackageVersions,
        allow_partial: bool,
    ) -> Result<Vec<SecurityAdvisory>, String> {
        self.load_root_server_file().await?;
        let Some(information) = self.security_advisory_information.read().await.clone() else {
            return Ok(Vec::new());
        };
        if !information.metadata && information.api_url.is_none() {
            return Ok(Vec::new());
        }

        let mut remaining = package_versions.clone();
        let mut advisories = Vec::new();
        if information.metadata && (allow_partial || information.api_url.is_none()) {
            for package in package_versions.keys() {
                self.load_package_metadata(package).await?;
            }
            let metadata = self.security_advisories.read().await;
            for (package, entries) in metadata.iter() {
                if !remaining.contains_key(package) {
                    continue;
                }
                for entry in entries {
                    advisories.push(parse_security_advisory(entry, package, !allow_partial)?);
                }
                remaining.remove(package);
            }
        }

        if let Some(api_url) = information
            .api_url
            .as_ref()
            .filter(|_| !remaining.is_empty())
        {
            let package_names = remaining.keys().cloned().collect::<Vec<_>>();
            let request =
                Self::security_advisory_api_request(api_url, &package_names, serde_json::json!({}));
            let request_builder = self
                .client
                .post(&request.url)
                .header(reqwest::header::CONTENT_TYPE, request.content_type)
                .timeout(Duration::from_secs(request.timeout_seconds))
                .body(request.body);
            let response = self
                .apply_auth(request_builder, &request.url)
                .send()
                .await
                .map_err(|error| {
                    format!(
                        "Failed to fetch security advisories from {}: {error}",
                        self.name
                    )
                })?;
            if !response.status().is_success() {
                return Err(format!(
                    "Security advisory endpoint for {} returned HTTP {}",
                    self.name,
                    response.status()
                ));
            }
            let document: Value = response.json().await.map_err(|error| {
                format!(
                    "Failed to parse security advisories from {}: {error}",
                    self.name
                )
            })?;
            let raw = document
                .get("advisories")
                .or_else(|| document.get("security-advisories"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            if let Some(packages) = raw.as_object() {
                for (package, entries) in packages {
                    if !remaining.contains_key(package) {
                        continue;
                    }
                    let Some(entries) = entries.as_array() else {
                        return Err(format!(
                            "Invalid security advisories for {package} from {}: expected an array",
                            self.name
                        ));
                    };
                    for entry in entries {
                        advisories.push(parse_security_advisory(entry, package, true)?);
                    }
                }
            } else if !raw.as_array().is_some_and(Vec::is_empty) {
                return Err(format!(
                    "Invalid security advisories from {}: expected an object or empty array",
                    self.name
                ));
            }
        }
        Ok(filter_advisories_for_versions(package_versions, advisories))
    }

    async fn matching_filter_entries(
        &self,
        package_versions: &PackageVersions,
        configured_lists: &[String],
    ) -> Result<FilterEntriesByList, String> {
        self.load_root_server_file().await?;
        let information = self.filter_information.read().await.clone();
        let Some(information) = information else {
            return Ok(FilterEntriesByList::new());
        };
        if !information.metadata {
            return Ok(FilterEntriesByList::new());
        }
        if Self::user_filter_disabled(&self.user_filter_config) {
            return Ok(FilterEntriesByList::new());
        }
        let advertised =
            Self::apply_user_filter_config(&information.lists, &self.user_filter_config);
        let relevant = configured_lists
            .iter()
            .filter(|list| advertised.iter().any(|provided| provided == *list))
            .cloned()
            .collect::<Vec<_>>();
        if relevant.is_empty() {
            return Ok(FilterEntriesByList::new());
        }

        let has_fresh_metadata = !self.package_filter_metadata.read().await.is_empty();
        if let Some(api_url) = information.api_url.as_ref().filter(|_| !has_fresh_metadata) {
            let package_names = package_versions.keys().cloned().collect::<Vec<_>>();
            let request = crate::filter_list::FilterListApiRequest::post_purls(
                api_url,
                &package_names,
                &relevant,
            )
            .map_err(|error| error.to_string())?;
            let request_builder = self
                .client
                .post(&request.url)
                .header(reqwest::header::CONTENT_TYPE, request.content_type)
                .timeout(Duration::from_secs(request.timeout_seconds))
                .body(request.body);
            let response = self
                .apply_auth(request_builder, &request.url)
                .send()
                .await
                .map_err(|error| {
                    format!("Failed to fetch filter lists from {}: {error}", self.name)
                })?;
            if !response.status().is_success() {
                return Err(format!(
                    "Filter-list endpoint for {} returned HTTP {}",
                    self.name,
                    response.status()
                ));
            }
            let document: Value = response.json().await.map_err(|error| {
                format!("Failed to parse filter lists from {}: {error}", self.name)
            })?;
            return Ok(Self::build_filter_entries(
                document.get("filter").unwrap_or(&Value::Null),
                package_versions,
                &relevant,
                None,
            ));
        }

        let packages_to_load = if let Some(summary_url) = &information.summary_url {
            let (body, _) = self.fetch_fresh(summary_url).await?;
            let document: Value = serde_json::from_str(&body).map_err(|error| {
                format!("Failed to parse filter summary from {}: {error}", self.name)
            })?;
            let summary = document
                .get("filter")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default();
            Self::filter_summary_candidates(&summary, package_versions, &relevant)
        } else {
            package_versions.clone()
        };
        for package in packages_to_load.keys() {
            self.load_package_metadata(package).await?;
        }

        let metadata = self.package_filter_metadata.read().await;
        let mut result = FilterEntriesByList::new();
        for (package, raw) in metadata.iter() {
            let entries =
                Self::build_filter_entries(raw, package_versions, &relevant, Some(package));
            for (list, entries) in entries {
                result.entry(list).or_default().extend(entries);
            }
        }
        Ok(result)
    }

    fn form_encode(value: &str) -> String {
        url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
    }

    pub fn build_search_url(template: &str, query: &str, package_type: Option<&str>) -> String {
        template
            .replace("%query%", &Self::form_encode(query))
            .replace(
                "%type%",
                &Self::form_encode(package_type.unwrap_or_default()),
            )
    }

    fn convert_search_results(data: SearchResponse) -> Vec<SearchResult> {
        data.results
            .into_iter()
            .filter(|result| !result.is_virtual.unwrap_or(false))
            .map(|result| {
                let abandoned = match result.abandoned {
                    Some(Value::Bool(true)) => Some(String::new()),
                    Some(Value::String(replacement)) => Some(replacement),
                    _ => None,
                };
                SearchResult {
                    name: result.name,
                    description: result.description,
                    url: result.url,
                    abandoned,
                    downloads: result.downloads,
                    favers: result.favers,
                }
            })
            .collect()
    }

    pub async fn search_with_type(
        &self,
        query: &str,
        package_type: Option<&str>,
    ) -> Vec<SearchResult> {
        self.load_root_server_file().await.ok();
        let template = self
            .search_url
            .read()
            .await
            .clone()
            .unwrap_or_else(|| format!("{}/search.json?q=%query%&type=%type%", self.url));
        let url = Self::build_search_url(&template, query, package_type);
        let response = match self.client.get(&url).send().await {
            Ok(response) if response.status().is_success() => response,
            _ => return Vec::new(),
        };
        let data: SearchResponse = match response.json().await {
            Ok(data) => data,
            Err(_) => return Vec::new(),
        };
        Self::convert_search_results(data)
    }

    pub async fn load_package_metadata_with_dev(
        &self,
        name: &str,
        include_dev: bool,
    ) -> Result<Vec<Arc<Package>>, String> {
        let mut all_packages = self.load_package_metadata(name).await?;

        if include_dev {
            let dev_name = format!("{}~dev", name);
            if let Ok(dev_packages) = self.load_package_metadata(&dev_name).await {
                let existing_versions: HashSet<_> =
                    all_packages.iter().map(|p| p.version.clone()).collect();

                for pkg in dev_packages {
                    if !existing_versions.contains(&pkg.version) {
                        all_packages.push(pkg);
                    }
                }
            }
        }

        Ok(all_packages)
    }

    pub fn is_stability_acceptable(
        stability: Stability,
        acceptable_stabilities: &HashMap<Stability, u8>,
        package_name: &str,
        stability_flags: &HashMap<String, Stability>,
    ) -> bool {
        if let Some(flag_stability) = stability_flags.get(package_name) {
            return stability.priority() <= flag_stability.priority();
        }

        acceptable_stabilities.contains_key(&stability)
    }

    pub fn filter_by_stability(
        packages: Vec<Arc<Package>>,
        acceptable_stabilities: &HashMap<Stability, u8>,
        stability_flags: &HashMap<String, Stability>,
    ) -> Vec<Arc<Package>> {
        packages
            .into_iter()
            .filter(|pkg| {
                let stability = pkg.stability.unwrap_or(Stability::Stable);
                Self::is_stability_acceptable(
                    stability,
                    acceptable_stabilities,
                    &pkg.name,
                    stability_flags,
                )
            })
            .collect()
    }

    pub async fn get_dist_mirrors(&self) -> Vec<DistMirror> {
        self.load_root_server_file().await.ok();
        self.dist_mirrors.read().await.clone()
    }

    pub async fn get_source_mirrors(&self, vcs_type: &str) -> Vec<SourceMirror> {
        self.load_root_server_file().await.ok();
        self.source_mirrors
            .read()
            .await
            .get(vcs_type)
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait]
impl Repository for ComposerRepository {
    fn name(&self) -> &str {
        &self.name
    }

    async fn has_package(&self, name: &str) -> bool {
        !self.find_packages(name).await.is_empty()
    }

    async fn find_packages(&self, name: &str) -> Vec<Arc<Package>> {
        self.load_package_metadata(name).await.unwrap_or_default()
    }

    async fn find_package(&self, name: &str, version: &str) -> Option<Arc<Package>> {
        let packages = self.find_packages(name).await;
        packages
            .into_iter()
            .find(|p| p.version == version || p.pretty_version.as_deref() == Some(version))
    }

    async fn find_packages_with_constraint(
        &self,
        name: &str,
        constraint: &str,
    ) -> Vec<Arc<Package>> {
        self.load_package_metadata_with_constraint(name, constraint)
            .await
            .unwrap_or_default()
    }

    async fn find_solver_packages_with_constraint(
        &self,
        name: &str,
        constraint: &str,
    ) -> Vec<Arc<Package>> {
        self.load_solver_package_metadata_with_constraint(name, constraint)
            .await
            .unwrap_or_default()
    }

    fn hydrate_package(&self, package: &Arc<Package>) -> Option<Package> {
        let deferred = self.deferred_metadata.lock().ok()?;
        let metadata = deferred.iter().find_map(|batch| {
            batch
                .packages
                .iter()
                .find(|(deferred_package, _)| deferred_package.as_ptr() == Arc::as_ptr(package))
                .map(|(_, range)| &batch.content[range.clone()])
        })?;
        CachedPackage::hydrate(package.as_ref().clone(), metadata)
    }

    fn hydrate_package_for_transaction(&self, package: &Arc<Package>) -> Option<Package> {
        let deferred = self.deferred_metadata.lock().ok()?;
        let metadata = deferred.iter().find_map(|batch| {
            batch
                .packages
                .iter()
                .find(|(deferred_package, _)| deferred_package.as_ptr() == Arc::as_ptr(package))
                .map(|(_, range)| &batch.content[range.clone()])
        })?;
        CachedPackage::hydrate_for_transaction(package.as_ref().clone(), metadata)
    }

    async fn get_packages(&self) -> Vec<Arc<Package>> {
        self.load_root_server_file().await.ok();

        if let Some(ref available) = *self.available_packages.read().await {
            log::debug!("Repository has {} available packages", available.len());
        }

        let packages = self.packages.read().await;
        let mut packages: Vec<_> = packages.values().flatten().cloned().collect();
        packages.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.version.cmp(&right.version))
        });
        packages
    }

    async fn search(&self, query: &str, mode: SearchMode) -> Vec<SearchResult> {
        self.load_root_server_file().await.ok();

        match mode {
            SearchMode::Fulltext => self.search_with_type(query, None).await,
            SearchMode::Vendor => {
                let package_names = self.get_package_names(None).await;

                let regex_str = query
                    .split_whitespace()
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join("|");
                let regex = match Regex::new(&format!("(?i){}", regex_str)) {
                    Ok(r) => r,
                    Err(_) => return Vec::new(),
                };

                let mut vendors = HashSet::new();
                for name in package_names {
                    if let Some(vendor) = name.split('/').next() {
                        if regex.is_match(vendor) {
                            vendors.insert(vendor.to_string());
                        }
                    }
                }

                vendors
                    .into_iter()
                    .map(|name| SearchResult {
                        name,
                        description: None,
                        url: None,
                        abandoned: None,
                        downloads: None,
                        favers: None,
                    })
                    .collect()
            }
            SearchMode::Name => {
                let package_names = self.get_package_names(None).await;

                let regex_str = query
                    .split_whitespace()
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join("|");
                let regex = match Regex::new(&format!("(?i){}", regex_str)) {
                    Ok(r) => r,
                    Err(_) => return Vec::new(),
                };

                package_names
                    .into_iter()
                    .filter(|name| regex.is_match(name))
                    .map(|name| SearchResult {
                        name,
                        description: None,
                        url: None,
                        abandoned: None,
                        downloads: None,
                        favers: None,
                    })
                    .collect()
            }
        }
    }

    async fn search_with_type(
        &self,
        query: &str,
        mode: SearchMode,
        package_type: Option<&str>,
    ) -> Vec<SearchResult> {
        match mode {
            SearchMode::Fulltext => {
                ComposerRepository::search_with_type(self, query, package_type).await
            }
            SearchMode::Name | SearchMode::Vendor => {
                let mut results = self.search(query, mode).await;
                if let Some(package_type) = package_type {
                    let names: std::collections::HashSet<_> = self
                        .get_package_names(Some(package_type))
                        .await
                        .into_iter()
                        .collect();
                    results.retain(|result| names.contains(&result.name));
                }
                results
            }
        }
    }

    async fn get_providers(&self, package_name: &str) -> Vec<ProviderInfo> {
        self.load_root_server_file().await.ok();

        if let Some(ref providers_url) = *self.providers_api_url.read().await {
            let url = providers_url.replace("%package%", package_name);

            let request = self.client.get(&url);
            let request = self.apply_auth(request, &url);

            let response = match request.send().await {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };

            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Vec::new();
            }

            if !response.status().is_success() {
                return Vec::new();
            }

            #[derive(Deserialize)]
            struct ProvidersResponse {
                providers: Vec<ProviderData>,
            }

            #[derive(Deserialize)]
            struct ProviderData {
                name: String,
                description: Option<String>,
                #[serde(rename = "type")]
                package_type: Option<String>,
            }

            let data: ProvidersResponse = match response.json().await {
                Ok(d) => d,
                Err(_) => return Vec::new(),
            };

            return data
                .providers
                .into_iter()
                .map(|p| ProviderInfo {
                    name: p.name,
                    description: p.description,
                    package_type: p.package_type.unwrap_or_else(|| "library".to_string()),
                })
                .collect();
        }

        Vec::new()
    }

    async fn load_packages_batch(
        &self,
        packages: &[(String, Option<String>)],
    ) -> super::traits::LoadResult {
        use super::traits::LoadResult;
        use futures_util::stream::{self, StreamExt};

        const MAX_CONCURRENT: usize = 50;

        let mut result = LoadResult {
            packages: Vec::new(),
            names_found: Vec::new(),
        };

        if packages.is_empty() {
            return result;
        }

        let fetched: Vec<(String, Option<String>, Vec<Arc<Package>>)> =
            stream::iter(packages.iter().cloned())
                .map(|(name, constraint)| {
                    let name_clone = name.clone();
                    async move {
                        let loaded = match constraint.as_deref() {
                            Some(constraint) => {
                                self.load_package_metadata_with_constraint(&name_clone, constraint)
                                    .await
                            }
                            None => self.load_package_metadata(&name_clone).await,
                        };
                        let pkgs = match loaded {
                            Ok(p) => p,
                            Err(e) => {
                                log::warn!("Failed to load package {}: {}", name_clone, e);
                                Vec::new()
                            }
                        };
                        (name, constraint, pkgs)
                    }
                })
                .buffer_unordered(MAX_CONCURRENT)
                .collect()
                .await;

        for (name, _, pkgs) in fetched {
            if pkgs.is_empty() {
                continue;
            }

            result.names_found.push(name);
            result.packages.extend(pkgs);
        }

        result
    }

    async fn get_security_advisories(
        &self,
        package_versions: &PackageVersions,
        allow_partial: bool,
    ) -> Result<Vec<SecurityAdvisory>, String> {
        self.matching_security_advisories(package_versions, allow_partial)
            .await
    }

    async fn get_filter_entries(
        &self,
        package_versions: &PackageVersions,
        configured_lists: &[String],
    ) -> Result<FilterEntriesByList, String> {
        self.matching_filter_entries(package_versions, configured_lists)
            .await
    }
}

fn parse_security_advisory(
    entry: &Value,
    package_name: &str,
    require_full: bool,
) -> Result<SecurityAdvisory, String> {
    let mut entry = entry.clone();
    let object = entry.as_object_mut().ok_or_else(|| {
        format!("Invalid security advisory for {package_name}: expected an object")
    })?;
    if require_full
        && !["title", "sources", "reportedAt"]
            .iter()
            .all(|field| object.contains_key(*field))
    {
        return Err(format!(
            "Advisory for {package_name} could not be loaded as a full advisory"
        ));
    }
    object
        .entry("packageName")
        .or_insert_with(|| Value::String(package_name.to_owned()));
    serde_json::from_value(entry)
        .map_err(|error| format!("Invalid security advisory for {package_name}: {error}"))
}

fn filter_advisories_for_versions(
    package_versions: &PackageVersions,
    advisories: Vec<SecurityAdvisory>,
) -> Vec<SecurityAdvisory> {
    advisories
        .into_iter()
        .filter(|advisory| {
            package_versions
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&advisory.package_name))
                .is_some_and(|(_, versions)| {
                    versions
                        .iter()
                        .any(|version| Semver::satisfies(version, &advisory.affected_versions))
                })
        })
        .collect()
}

/// Packagist API response for package metadata
#[derive(Debug, Deserialize)]
struct PackagistResponse {
    packages: HashMap<String, Vec<Value>>,
    #[serde(default)]
    minified: Option<String>,
}

#[derive(Deserialize)]
struct ParsedPackageCache {
    version: u8,
    source_sha256: [u8; 32],
    notify_batch: Option<String>,
    packages: Vec<Package>,
}

#[derive(Serialize)]
struct ParsedPackageCacheRef<'a> {
    version: u8,
    source_sha256: [u8; 32],
    notify_batch: Option<&'a str>,
    packages: &'a [Package],
}

#[derive(Archive, RkyvSerialize, RkyvDeserialize)]
struct FilteredPackageCache {
    version: u8,
    source_sha256: [u8; 32],
    notify_batch: Option<String>,
    constraint: String,
    packages: Vec<CachedPackage>,
}

/// Package version data from Packagist (v2 minified format)
/// In minified format, only the first version has all fields,
/// subsequent versions only contain changed fields.
/// Fields can be set to "__unset" to indicate removal.
#[derive(Debug, Clone, Deserialize)]
struct PackagistVersion {
    version: String,
    #[serde(default)]
    version_normalized: Option<String>,
    #[serde(default, deserialize_with = "deserialize_maybe_unset")]
    description: Option<String>,
    #[serde(default, deserialize_with = "deserialize_maybe_unset")]
    homepage: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec_maybe_unset")]
    license: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_maybe_unset")]
    keywords: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_maybe_unset")]
    authors: Option<Vec<PackagistAuthor>>,
    #[serde(default, deserialize_with = "deserialize_hashmap_maybe_unset")]
    require: Option<IndexMap<String, String>>,
    #[serde(
        rename = "require-dev",
        default,
        deserialize_with = "deserialize_hashmap_maybe_unset"
    )]
    require_dev: Option<IndexMap<String, String>>,
    #[serde(default, deserialize_with = "deserialize_hashmap_maybe_unset")]
    conflict: Option<IndexMap<String, String>>,
    #[serde(default, deserialize_with = "deserialize_hashmap_maybe_unset")]
    provide: Option<IndexMap<String, String>>,
    #[serde(default, deserialize_with = "deserialize_hashmap_maybe_unset")]
    replace: Option<IndexMap<String, String>>,
    #[serde(default, deserialize_with = "deserialize_hashmap_maybe_unset")]
    suggest: Option<IndexMap<String, String>>,
    #[serde(rename = "type", default, deserialize_with = "deserialize_maybe_unset")]
    package_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_maybe_unset")]
    bin: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_maybe_unset")]
    source: Option<PackagistSource>,
    #[serde(default, deserialize_with = "deserialize_maybe_unset")]
    dist: Option<PackagistDist>,
    #[serde(default, deserialize_with = "deserialize_maybe_unset")]
    autoload: Option<PackagistAutoload>,
    #[serde(
        rename = "autoload-dev",
        default,
        deserialize_with = "deserialize_maybe_unset"
    )]
    autoload_dev: Option<PackagistAutoload>,
    #[serde(default, deserialize_with = "deserialize_maybe_unset")]
    time: Option<String>,
    #[serde(
        rename = "notification-url",
        default,
        deserialize_with = "deserialize_maybe_unset"
    )]
    notification_url: Option<String>,
    #[serde(default, deserialize_with = "deserialize_maybe_unset")]
    support: Option<PackagistSupport>,
    #[serde(default, deserialize_with = "deserialize_maybe_unset")]
    funding: Option<Vec<PackagistFunding>>,
    #[serde(default)]
    extra: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct PackagistAuthor {
    name: Option<String>,
    email: Option<String>,
    homepage: Option<String>,
    role: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PackagistSource {
    #[serde(rename = "type")]
    source_type: String,
    url: String,
    reference: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PackagistDist {
    #[serde(rename = "type")]
    dist_type: String,
    url: String,
    reference: Option<String>,
    shasum: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct PackagistAutoload {
    #[serde(rename = "psr-4", default)]
    psr4: IndexMap<String, serde_json::Value>,
    #[serde(rename = "psr-0", default)]
    psr0: IndexMap<String, serde_json::Value>,
    #[serde(default)]
    classmap: Vec<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(rename = "exclude-from-classmap", default)]
    exclude_from_classmap: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct PackagistSupport {
    #[serde(default)]
    issues: Option<String>,
    #[serde(default)]
    forum: Option<String>,
    #[serde(default)]
    wiki: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    irc: Option<String>,
    #[serde(default)]
    docs: Option<String>,
    #[serde(default)]
    rss: Option<String>,
    #[serde(default)]
    chat: Option<String>,
    #[serde(default)]
    security: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PackagistFunding {
    #[serde(rename = "type")]
    funding_type: Option<String>,
    url: Option<String>,
}

/// Search API response
#[derive(Debug, Deserialize)]
struct SearchResponse {
    results: Vec<SearchResultItem>,
}

#[derive(Debug, Deserialize)]
struct SearchResultItem {
    name: String,
    description: Option<String>,
    url: Option<String>,
    downloads: Option<u64>,
    favers: Option<u64>,
    abandoned: Option<Value>,
    /// Whether this is a virtual package (should be filtered in search results)
    #[serde(rename = "virtual")]
    is_virtual: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test basic delta compression expansion where versions inherit from previous
    #[test]
    fn test_expand_minified_versions_basic_inheritance() {
        // Simulates Packagist v2 response where newer versions omit unchanged fields
        let json = r#"[
            {
                "version": "2.0.0",
                "version_normalized": "2.0.0.0",
                "require": {"php": ">=8.0"},
                "description": "A test package"
            },
            {
                "version": "1.1.0",
                "version_normalized": "1.1.0.0"
            },
            {
                "version": "1.0.0",
                "version_normalized": "1.0.0.0"
            }
        ]"#;

        let versions: Vec<Value> = serde_json::from_str(json).unwrap();
        let expanded = ComposerRepository::expand_minified_versions(&versions).unwrap();

        assert_eq!(expanded.len(), 3);

        // First version has all fields
        assert_eq!(expanded[0].version, "2.0.0");
        assert_eq!(
            expanded[0].require.as_ref().unwrap().get("php").unwrap(),
            ">=8.0"
        );
        assert_eq!(expanded[0].description.as_ref().unwrap(), "A test package");

        // Second version inherits from first
        assert_eq!(expanded[1].version, "1.1.0");
        assert_eq!(
            expanded[1].require.as_ref().unwrap().get("php").unwrap(),
            ">=8.0"
        );
        assert_eq!(expanded[1].description.as_ref().unwrap(), "A test package");

        // Third version inherits from second (which inherited from first)
        assert_eq!(expanded[2].version, "1.0.0");
        assert_eq!(
            expanded[2].require.as_ref().unwrap().get("php").unwrap(),
            ">=8.0"
        );
        assert_eq!(expanded[2].description.as_ref().unwrap(), "A test package");
    }

    /// Test that fields are properly overridden when a version specifies them
    #[test]
    fn test_expand_minified_versions_field_override() {
        let json = r#"[
            {
                "version": "2.0.0",
                "version_normalized": "2.0.0.0",
                "require": {"php": ">=8.0", "ext-json": "*"},
                "description": "Version 2"
            },
            {
                "version": "1.0.0",
                "version_normalized": "1.0.0.0",
                "require": {"php": ">=7.4"},
                "description": "Version 1"
            }
        ]"#;

        let versions: Vec<Value> = serde_json::from_str(json).unwrap();
        let expanded = ComposerRepository::expand_minified_versions(&versions).unwrap();

        assert_eq!(expanded.len(), 2);

        // First version
        assert_eq!(
            expanded[0].require.as_ref().unwrap().get("php").unwrap(),
            ">=8.0"
        );
        assert!(expanded[0]
            .require
            .as_ref()
            .unwrap()
            .contains_key("ext-json"));
        assert_eq!(expanded[0].description.as_ref().unwrap(), "Version 2");

        // Second version overrides require completely (not merged!)
        assert_eq!(
            expanded[1].require.as_ref().unwrap().get("php").unwrap(),
            ">=7.4"
        );
        // ext-json should NOT be present - the entire require block was replaced
        assert!(!expanded[1]
            .require
            .as_ref()
            .unwrap()
            .contains_key("ext-json"));
        assert_eq!(expanded[1].description.as_ref().unwrap(), "Version 1");
    }

    /// Test real-world Packagist v2 payload from doctrine/dbal
    /// This tests the actual delta compression format used by Packagist
    #[test]
    fn test_expand_minified_doctrine_dbal_sample() {
        // Real sample from https://repo.packagist.org/p2/doctrine/dbal.json
        // Versions are ordered newest to oldest
        let json = r#"[
            {
                "version": "3.4.6",
                "version_normalized": "3.4.6.0",
                "require": {
                    "php": "^7.4 || ^8.0",
                    "composer-runtime-api": "^2",
                    "doctrine/cache": "^1.11|^2.0",
                    "doctrine/deprecations": "^0.5.3|^1",
                    "doctrine/event-manager": "^1.0",
                    "psr/cache": "^1|^2|^3",
                    "psr/log": "^1|^2|^3"
                },
                "description": "Powerful PHP database abstraction layer"
            },
            {
                "version": "3.4.5",
                "version_normalized": "3.4.5.0"
            },
            {
                "version": "3.4.4",
                "version_normalized": "3.4.4.0"
            },
            {
                "version": "3.4.3",
                "version_normalized": "3.4.3.0"
            }
        ]"#;

        let versions: Vec<Value> = serde_json::from_str(json).unwrap();
        let expanded = ComposerRepository::expand_minified_versions(&versions).unwrap();

        assert_eq!(expanded.len(), 4);

        // All versions should have the same require (inherited from 3.4.6)
        for (i, v) in expanded.iter().enumerate() {
            let require = v
                .require
                .as_ref()
                .unwrap_or_else(|| panic!("Version {} ({}) should have require", i, v.version));

            assert_eq!(
                require.get("php").unwrap(),
                "^7.4 || ^8.0",
                "Version {} ({}) should have php requirement",
                i,
                v.version
            );
            assert!(
                !require.contains_key("shopware/core"),
                "Version {} ({}) should NOT have shopware/core requirement",
                i,
                v.version
            );
        }

        // Verify version numbers are preserved
        assert_eq!(expanded[0].version, "3.4.6");
        assert_eq!(expanded[1].version, "3.4.5");
        assert_eq!(expanded[2].version, "3.4.4");
        assert_eq!(expanded[3].version, "3.4.3");
    }

    /// Test real-world Packagist v2 payload from symfony packages
    /// Multiple packages providing the same virtual package
    #[test]
    fn test_expand_minified_symfony_sample() {
        // Sample from symfony/console showing provide for psr/log-implementation
        let json = r#"[
            {
                "version": "v7.3.8",
                "version_normalized": "7.3.8.0",
                "require": {
                    "php": ">=8.2",
                    "symfony/polyfill-mbstring": "~1.0",
                    "symfony/service-contracts": "^2.5|^3"
                },
                "provide": {
                    "psr/log-implementation": "1.0|2.0|3.0"
                },
                "description": "Symfony Console Component"
            },
            {
                "version": "v7.3.7",
                "version_normalized": "7.3.7.0"
            },
            {
                "version": "v7.3.0",
                "version_normalized": "7.3.0.0",
                "require": {
                    "php": ">=8.2",
                    "symfony/polyfill-mbstring": "~1.0"
                }
            }
        ]"#;

        let versions: Vec<Value> = serde_json::from_str(json).unwrap();
        let expanded = ComposerRepository::expand_minified_versions(&versions).unwrap();

        assert_eq!(expanded.len(), 3);

        // v7.3.8 has all fields
        assert_eq!(
            expanded[0].require.as_ref().unwrap().get("php").unwrap(),
            ">=8.2"
        );
        assert!(expanded[0]
            .require
            .as_ref()
            .unwrap()
            .contains_key("symfony/service-contracts"));
        assert_eq!(
            expanded[0]
                .provide
                .as_ref()
                .unwrap()
                .get("psr/log-implementation")
                .unwrap(),
            "1.0|2.0|3.0"
        );

        // v7.3.7 inherits from v7.3.8
        assert_eq!(
            expanded[1].require.as_ref().unwrap().get("php").unwrap(),
            ">=8.2"
        );
        assert!(expanded[1]
            .require
            .as_ref()
            .unwrap()
            .contains_key("symfony/service-contracts"));
        assert_eq!(
            expanded[1]
                .provide
                .as_ref()
                .unwrap()
                .get("psr/log-implementation")
                .unwrap(),
            "1.0|2.0|3.0"
        );

        // v7.3.0 overrides require (loses symfony/service-contracts) but keeps provide
        assert_eq!(
            expanded[2].require.as_ref().unwrap().get("php").unwrap(),
            ">=8.2"
        );
        assert!(!expanded[2]
            .require
            .as_ref()
            .unwrap()
            .contains_key("symfony/service-contracts"));
        assert_eq!(
            expanded[2]
                .provide
                .as_ref()
                .unwrap()
                .get("psr/log-implementation")
                .unwrap(),
            "1.0|2.0|3.0"
        );
    }

    /// Test that different packages don't contaminate each other
    /// This is the bug we're trying to prevent
    #[test]
    fn test_expand_minified_no_cross_package_contamination() {
        // Parse two different packages separately
        let doctrine_json = r#"[
            {
                "version": "3.4.6",
                "version_normalized": "3.4.6.0",
                "require": {"php": "^7.4 || ^8.0", "doctrine/cache": "^1.11|^2.0"}
            },
            {
                "version": "3.4.5",
                "version_normalized": "3.4.5.0"
            }
        ]"#;

        let shopware_json = r#"[
            {
                "version": "v6.6.10.10",
                "version_normalized": "6.6.10.10",
                "require": {"php": "~8.2.0 || ~8.3.0 || ~8.4.0", "shopware/core": "v6.6.10.10"}
            },
            {
                "version": "v6.6.10.9",
                "version_normalized": "6.6.10.9"
            }
        ]"#;

        let doctrine_versions: Vec<Value> = serde_json::from_str(doctrine_json).unwrap();
        let shopware_versions: Vec<Value> = serde_json::from_str(shopware_json).unwrap();

        // Expand each package separately (as the real code does)
        let doctrine_expanded =
            ComposerRepository::expand_minified_versions(&doctrine_versions).unwrap();
        let shopware_expanded =
            ComposerRepository::expand_minified_versions(&shopware_versions).unwrap();

        // Doctrine should never have shopware/core
        for v in &doctrine_expanded {
            assert!(
                !v.require.as_ref().unwrap().contains_key("shopware/core"),
                "doctrine/dbal {} should NOT have shopware/core requirement",
                v.version
            );
        }

        // Shopware should have shopware/core
        for v in &shopware_expanded {
            assert!(
                v.require.as_ref().unwrap().contains_key("shopware/core"),
                "shopware/storefront {} should have shopware/core requirement",
                v.version
            );
        }
    }

    /// Test handling of null values in JSON (explicit null vs missing field)
    #[test]
    fn test_expand_minified_null_handling() {
        // In Packagist v2, null means "inherit from previous"
        // but an explicit empty object {} means "this version has no requirements"
        let json = r#"[
            {
                "version": "2.0.0",
                "version_normalized": "2.0.0.0",
                "require": {"php": ">=8.0"},
                "description": "Has requirements"
            },
            {
                "version": "1.0.0",
                "version_normalized": "1.0.0.0",
                "require": null,
                "description": null
            }
        ]"#;

        let versions: Vec<Value> = serde_json::from_str(json).unwrap();
        let expanded = ComposerRepository::expand_minified_versions(&versions).unwrap();

        assert_eq!(expanded.len(), 2);

        // v1.0.0 should inherit from v2.0.0 because require is null
        assert_eq!(
            expanded[1].require.as_ref().unwrap().get("php").unwrap(),
            ">=8.0"
        );
        assert_eq!(
            expanded[1].description.as_ref().unwrap(),
            "Has requirements"
        );
    }

    // Ported from Composer\Test\Util\MetadataMinifierTest::testMinifyExpand.
    #[test]
    fn composer_metadata_minifier_expands_delta_and_unset_values() {
        let minified = serde_json::json!([
            {
                "name": "foo/bar",
                "version": "2.0.0",
                "version_normalized": "2.0.0.0",
                "type": "library",
                "license": ["MIT"],
                "homepage": "https://first.example"
            },
            {
                "version": "1.2.0",
                "version_normalized": "1.2.0.0",
                "license": ["GPL"],
                "homepage": "https://example.org"
            },
            {
                "version": "1.0.0",
                "version_normalized": "1.0.0.0",
                "homepage": "__unset"
            }
        ]);
        let versions = minified.as_array().unwrap();

        let expanded = ComposerRepository::expand_minified_versions(versions).unwrap();

        assert_eq!(
            expanded[0].license.as_deref(),
            Some(&["MIT".to_string()][..])
        );
        assert_eq!(
            expanded[1].license.as_deref(),
            Some(&["GPL".to_string()][..])
        );
        assert_eq!(expanded[1].homepage.as_deref(), Some("https://example.org"));
        assert_eq!(
            expanded[2].license.as_deref(),
            Some(&["GPL".to_string()][..])
        );
        assert_eq!(expanded[2].homepage, None);
    }

    /// Test the full parse flow with a mock response
    #[test]
    fn test_parse_packagist_response_isolates_packages() {
        // This simulates what happens when we parse a response
        // Each package name should be processed independently
        let response_json = r#"{
            "packages": {
                "vendor/package-a": [
                    {
                        "version": "1.0.0",
                        "version_normalized": "1.0.0.0",
                        "require": {"php": ">=7.4", "vendor/dep-a": "^1.0"}
                    }
                ],
                "vendor/package-b": [
                    {
                        "version": "2.0.0",
                        "version_normalized": "2.0.0.0",
                        "require": {"php": ">=8.0", "vendor/dep-b": "^2.0"}
                    }
                ]
            }
        }"#;

        let response: PackagistResponse = serde_json::from_str(response_json).unwrap();

        // Process package-a
        let versions_a = response.packages.get("vendor/package-a").unwrap();
        let expanded_a = ComposerRepository::expand_minified_versions(versions_a).unwrap();

        // Process package-b
        let versions_b = response.packages.get("vendor/package-b").unwrap();
        let expanded_b = ComposerRepository::expand_minified_versions(versions_b).unwrap();

        // Verify no cross-contamination
        assert!(expanded_a[0]
            .require
            .as_ref()
            .unwrap()
            .contains_key("vendor/dep-a"));
        assert!(!expanded_a[0]
            .require
            .as_ref()
            .unwrap()
            .contains_key("vendor/dep-b"));

        assert!(expanded_b[0]
            .require
            .as_ref()
            .unwrap()
            .contains_key("vendor/dep-b"));
        assert!(!expanded_b[0]
            .require
            .as_ref()
            .unwrap()
            .contains_key("vendor/dep-a"));
    }

    #[tokio::test]
    async fn composer_repository_loads_inline_root_package_formats() {
        let cases = [
            (
                serde_json::json!({
                    "foo/bar": {
                        "name": "foo/bar",
                        "versions": {
                            "1.0.0": {"name": "foo/bar", "version": "1.0.0"}
                        }
                    }
                }),
                vec![("foo/bar", "1.0.0")],
            ),
            (
                serde_json::json!({
                    "packages": {
                        "bar/foo": {
                            "3.14": {"name": "bar/foo", "version": "3.14"},
                            "3.145": {"name": "bar/foo", "version": "3.145"}
                        }
                    }
                }),
                vec![("bar/foo", "3.14"), ("bar/foo", "3.145")],
            ),
            (
                serde_json::json!({
                    "packages": {
                        "bar/foo": [
                            {"name": "bar/foo", "version": "3.14"},
                            {"name": "bar/foo", "version": "3.145"}
                        ]
                    }
                }),
                vec![("bar/foo", "3.14"), ("bar/foo", "3.145")],
            ),
            (
                serde_json::json!([
                    {"name": "seld/jsonlint", "version": "dev-main"}
                ]),
                vec![("seld/jsonlint", "dev-main")],
            ),
        ];

        for (index, (root, expected)) in cases.into_iter().enumerate() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join(format!("packages-{index}.json"));
            std::fs::write(&path, serde_json::to_vec(&root).unwrap()).unwrap();
            let repository =
                ComposerRepository::new("fixture", format!("file://{}", path.to_string_lossy()));

            let packages = repository.get_packages().await;
            let actual: Vec<_> = packages
                .iter()
                .map(|package| (package.name.as_str(), package.pretty_version()))
                .collect();

            assert_eq!(actual, expected);
        }
    }

    // ============================================================================
    // Tests for URL canonicalization (matching PHP ComposerRepositoryTest)
    // ============================================================================

    #[test]
    fn test_canonicalize_url_absolute_path() {
        let repo = ComposerRepository::new("test", "https://example.org");
        assert_eq!(
            repo.canonicalize_url("/path/to/file"),
            "https://example.org/path/to/file"
        );
    }

    #[test]
    fn test_canonicalize_url_already_absolute() {
        let repo = ComposerRepository::new("test", "https://should-not-see-me.test");
        assert_eq!(
            repo.canonicalize_url("https://example.org/canonic_url"),
            "https://example.org/canonic_url"
        );
    }

    #[test]
    fn test_canonicalize_url_file_scheme() {
        // For file:// URLs, the path comes right after file:// (no host)
        // When we find "://", after_scheme is "/path/to/repository"
        // The first "/" is at position 0, so host_part is "file://"
        // Result is "file://" + "/file" = "file:///file"
        // This matches PHP behavior for relative paths on file:// URLs
        let repo = ComposerRepository::new("test", "file:///path/to/repository");
        assert_eq!(repo.canonicalize_url("/file"), "file:///file");

        // But absolute URLs are returned unchanged
        assert_eq!(
            repo.canonicalize_url("file:///path/to/other/file"),
            "file:///path/to/other/file"
        );
    }

    #[test]
    fn test_canonicalize_url_with_special_chars() {
        // URLs can contain sequences resembling pattern references
        let repo = ComposerRepository::new("test", "https://example.org");
        assert_eq!(
            repo.canonicalize_url("/path/to/unusual_$0_filename"),
            "https://example.org/path/to/unusual_$0_filename"
        );
    }

    #[test]
    fn composer_repository_canonicalizes_urls() {
        let cases = [
            (
                "https://example.org/path/to/file",
                "/path/to/file",
                "https://example.org",
            ),
            (
                "https://example.org/canonic_url",
                "https://example.org/canonic_url",
                "https://should-not-see-me.test",
            ),
            (
                "file:///path/to/repository/file",
                "/path/to/repository/file",
                "file:///path/to/repository",
            ),
            ("invalid_repo_url", "/path/to/file", "invalid_repo_url"),
            (
                "https://example.org/path/to/unusual_$0_filename",
                "/path/to/unusual_$0_filename",
                "https://example.org",
            ),
        ];

        for (expected, url, repository_url) in cases {
            let repository = ComposerRepository::new("test", repository_url);
            assert_eq!(repository.canonicalize_url(url), expected);
        }
    }

    // ============================================================================
    // Tests for package name pattern matching
    // ============================================================================

    #[test]
    fn test_package_name_to_regex_exact() {
        let regex = ComposerRepository::package_name_to_regex("vendor/package").unwrap();
        assert!(regex.is_match("vendor/package"));
        assert!(!regex.is_match("vendor/package2"));
        assert!(!regex.is_match("other/package"));
    }

    #[test]
    fn test_package_name_to_regex_wildcard_suffix() {
        let regex = ComposerRepository::package_name_to_regex("vendor/*").unwrap();
        assert!(regex.is_match("vendor/package"));
        assert!(regex.is_match("vendor/other-package"));
        assert!(!regex.is_match("other/package"));
    }

    #[test]
    fn test_package_name_to_regex_wildcard_prefix() {
        let regex = ComposerRepository::package_name_to_regex("*/package").unwrap();
        assert!(regex.is_match("vendor/package"));
        assert!(regex.is_match("other/package"));
        assert!(!regex.is_match("vendor/other"));
    }

    #[test]
    fn test_package_name_to_regex_double_wildcard() {
        let regex = ComposerRepository::package_name_to_regex("symfony/*-bundle").unwrap();
        assert!(regex.is_match("symfony/framework-bundle"));
        assert!(regex.is_match("symfony/security-bundle"));
        assert!(!regex.is_match("symfony/console"));
    }

    // ============================================================================
    // Tests for stability filtering
    // ============================================================================

    #[test]
    fn test_stability_acceptable_with_global_config() {
        let mut acceptable = HashMap::new();
        acceptable.insert(Stability::Stable, 0);
        acceptable.insert(Stability::RC, 5);
        let flags = HashMap::new();

        assert!(ComposerRepository::is_stability_acceptable(
            Stability::Stable,
            &acceptable,
            "vendor/package",
            &flags
        ));
        assert!(ComposerRepository::is_stability_acceptable(
            Stability::RC,
            &acceptable,
            "vendor/package",
            &flags
        ));
        assert!(!ComposerRepository::is_stability_acceptable(
            Stability::Beta,
            &acceptable,
            "vendor/package",
            &flags
        ));
        assert!(!ComposerRepository::is_stability_acceptable(
            Stability::Dev,
            &acceptable,
            "vendor/package",
            &flags
        ));
    }

    #[test]
    fn test_stability_acceptable_with_package_flag() {
        let mut acceptable = HashMap::new();
        acceptable.insert(Stability::Stable, 0);

        let mut flags = HashMap::new();
        flags.insert("vendor/dev-package".to_string(), Stability::Dev);

        // Regular package only accepts stable
        assert!(ComposerRepository::is_stability_acceptable(
            Stability::Stable,
            &acceptable,
            "vendor/package",
            &flags
        ));
        assert!(!ComposerRepository::is_stability_acceptable(
            Stability::Dev,
            &acceptable,
            "vendor/package",
            &flags
        ));

        // Package with dev flag accepts dev
        assert!(ComposerRepository::is_stability_acceptable(
            Stability::Dev,
            &acceptable,
            "vendor/dev-package",
            &flags
        ));
        assert!(ComposerRepository::is_stability_acceptable(
            Stability::Stable,
            &acceptable,
            "vendor/dev-package",
            &flags
        ));
    }

    #[test]
    fn test_filter_by_stability() {
        let mut acceptable = HashMap::new();
        acceptable.insert(Stability::Stable, 0);
        acceptable.insert(Stability::RC, 5);
        let flags = HashMap::new();

        let packages = vec![
            Arc::new(Package {
                name: "vendor/stable".to_string(),
                version: "1.0.0".into(),
                stability: Some(Stability::Stable),
                ..Default::default()
            }),
            Arc::new(Package {
                name: "vendor/rc".to_string(),
                version: "1.0.0-RC1".into(),
                stability: Some(Stability::RC),
                ..Default::default()
            }),
            Arc::new(Package {
                name: "vendor/beta".to_string(),
                version: "1.0.0-beta1".into(),
                stability: Some(Stability::Beta),
                ..Default::default()
            }),
            Arc::new(Package {
                name: "vendor/dev".to_string(),
                version: "dev-master".into(),
                stability: Some(Stability::Dev),
                ..Default::default()
            }),
        ];

        let filtered = ComposerRepository::filter_by_stability(packages, &acceptable, &flags);

        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].name, "vendor/stable");
        assert_eq!(filtered[1].name, "vendor/rc");
    }

    // ============================================================================
    // Tests for repository URL construction
    // ============================================================================

    #[test]
    fn test_new_normalizes_trailing_slash() {
        let repo = ComposerRepository::new("test", "https://example.org/");
        assert_eq!(repo.url(), "https://example.org");
    }

    #[test]
    fn test_new_extracts_base_url_from_json_path() {
        let repo = ComposerRepository::new("test", "https://example.org/repo/packages.json");
        assert_eq!(repo.base_url, "https://example.org/repo");
    }

    #[test]
    fn test_packagist_url() {
        let repo = ComposerRepository::packagist();
        assert_eq!(repo.url(), "https://repo.packagist.org");
        assert_eq!(repo.name(), "packagist.org");
    }

    // ============================================================================
    // Tests for search result parsing
    // ============================================================================

    #[test]
    fn test_search_result_with_abandoned_true() {
        let json = r#"{
            "results": [
                {
                    "name": "foo/bar",
                    "description": "A package",
                    "url": "https://packagist.org/packages/foo/bar",
                    "downloads": 1000,
                    "favers": 50,
                    "abandoned": true
                }
            ]
        }"#;

        let response: SearchResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.results.len(), 1);

        let abandoned = &response.results[0].abandoned;
        assert!(matches!(abandoned, Some(Value::Bool(true))));
    }

    #[test]
    fn test_search_result_with_abandoned_replacement() {
        let json = r#"{
            "results": [
                {
                    "name": "foo/bar",
                    "description": "A package",
                    "abandoned": "new/package"
                }
            ]
        }"#;

        let response: SearchResponse = serde_json::from_str(json).unwrap();
        let abandoned = &response.results[0].abandoned;
        assert!(matches!(abandoned, Some(Value::String(s)) if s == "new/package"));
    }

    #[test]
    fn test_search_result_with_virtual_package() {
        let json = r#"{
            "results": [
                {
                    "name": "foo/bar",
                    "description": "A regular package",
                    "virtual": false
                },
                {
                    "name": "psr/log-implementation",
                    "description": "A virtual package",
                    "virtual": true
                }
            ]
        }"#;

        let response: SearchResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.results.len(), 2);
        assert_eq!(response.results[0].is_virtual, Some(false));
        assert_eq!(response.results[1].is_virtual, Some(true));
    }

    // ============================================================================
    // Tests for root server file parsing
    // ============================================================================

    #[test]
    fn test_parse_root_file_with_metadata_url() {
        // Simulates parsing a V2 repository root file
        let json = r#"{
            "packages": {},
            "metadata-url": "/p2/%package%.json",
            "notify-batch": "/downloads/",
            "search": "/search.json?q=%query%&type=%type%",
            "list": "/packages/list.json",
            "providers-api": "/providers/%package%.json",
            "available-packages": ["vendor/package-a", "vendor/package-b"],
            "available-package-patterns": ["symfony/*", "doctrine/*"]
        }"#;

        let data: Value = serde_json::from_str(json).unwrap();

        // Verify metadata-url is present
        assert_eq!(
            data.get("metadata-url").and_then(|v| v.as_str()),
            Some("/p2/%package%.json")
        );

        // Verify available-packages
        let available = data
            .get("available-packages")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(available.len(), 2);

        // Verify available-package-patterns
        let patterns = data
            .get("available-package-patterns")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(patterns.len(), 2);
    }

    #[test]
    fn test_parse_root_file_with_mirrors() {
        let json = r#"{
            "packages": {},
            "metadata-url": "/p2/%package%.json",
            "mirrors": [
                {
                    "dist-url": "https://mirror1.example.org/dist/%package%/%version%/%reference%.%type%",
                    "preferred": true
                },
                {
                    "dist-url": "https://mirror2.example.org/dist/%package%/%version%/%reference%.%type%",
                    "preferred": false
                },
                {
                    "git-url": "https://mirror.example.org/git/%package%.git",
                    "preferred": true
                }
            ]
        }"#;

        let data: Value = serde_json::from_str(json).unwrap();
        let mirrors = data.get("mirrors").and_then(|v| v.as_array()).unwrap();

        assert_eq!(mirrors.len(), 3);

        // First mirror has dist-url and preferred=true
        assert!(mirrors[0].get("dist-url").is_some());
        assert_eq!(
            mirrors[0].get("preferred").and_then(|v| v.as_bool()),
            Some(true)
        );

        // Third mirror has git-url
        assert!(mirrors[2].get("git-url").is_some());
    }

    // ============================================================================
    // Tests for cache key generation
    // ============================================================================

    #[test]
    fn test_cache_key_simple_package() {
        let key = ComposerRepository::cache_key("vendor/package");
        assert_eq!(key, "provider-vendor~package.json");
    }

    #[test]
    fn test_cache_key_nested_vendor() {
        let key = ComposerRepository::cache_key("vendor/sub/package");
        assert_eq!(key, "provider-vendor~sub~package.json");
    }

    #[tokio::test]
    async fn test_parsed_package_cache_reuse_and_invalidation() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = ComposerRepository::packagist_with_cache(temp.path().to_path_buf());
        let body = br#"{
            "packages": {
                "vendor/package": [{
                    "version": "1.0.0",
                    "description": "Cached package"
                }]
            }
        }"#;
        let notify_batch = Some("https://example.org/notify".to_string());
        *repo.notify_batch.write().await = notify_batch.clone();

        let packages = repo
            .parse_and_cache_response("vendor/package", body, None)
            .await
            .unwrap();
        assert_eq!(packages.len(), 1);

        let cached = repo
            .read_parsed_package_cache("vendor/package", body, &notify_batch)
            .unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].version, "1.0.0.0");
        assert_eq!(cached[0].description.as_deref(), Some("Cached package"));

        let changed_body = [body.as_slice(), b" "].concat();
        assert!(repo
            .read_parsed_package_cache("vendor/package", &changed_body, &notify_batch)
            .is_none());
        assert!(repo
            .read_parsed_package_cache("vendor/package", body, &None)
            .is_none());

        repo.file_cache
            .as_ref()
            .unwrap()
            .write_data(
                &ComposerRepository::parsed_cache_key("vendor/package"),
                b"bad",
            )
            .unwrap();
        assert!(repo
            .read_parsed_package_cache("vendor/package", body, &notify_batch)
            .is_none());
    }

    #[tokio::test]
    async fn package_metadata_accepts_a_scalar_composer_license() {
        let repo = ComposerRepository::packagist();
        let body = br#"{
            "packages": {
                "shopware/example": [{
                    "version": "1.0.0",
                    "version_normalized": "1.0.0.0",
                    "license": "MIT"
                }]
            }
        }"#;

        let packages = repo
            .parse_and_cache_response("shopware/example", body, None)
            .await
            .unwrap();

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].license.as_slice(), ["MIT"]);
    }

    #[tokio::test]
    async fn test_filtered_package_cache_tracks_constraint_and_source() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = ComposerRepository::packagist_with_cache(temp.path().to_path_buf());
        let body = br#"{
            "packages": {
                "vendor/package": [
                    {"version": "2.0.0", "version_normalized": "2.0.0.0", "description": "Two"},
                    {"version": "1.0.0", "version_normalized": "1.0.0.0", "description": "One"}
                ]
            }
        }"#;

        let packages = repo
            .parse_and_cache_response("vendor/package", body, Some("^1.0"))
            .await
            .unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version, "1.0.0.0");
        assert_eq!(packages[0].description.as_deref(), Some("One"));
        assert!(repo
            .read_filtered_package_cache("vendor/package", body, &None, "^1.0")
            .is_some());
        assert!(repo
            .read_filtered_package_cache("vendor/package", body, &None, "^2.0")
            .is_none());

        let solver_packages = repo
            .read_solver_filtered_package_cache("vendor/package", body, &None, "^1.0")
            .unwrap();
        assert_eq!(solver_packages.len(), 1);
        assert!(solver_packages[0].description.is_none());
        let hydrated = repo.hydrate_package(&solver_packages[0]).unwrap();
        assert_eq!(hydrated.description.as_deref(), Some("One"));
        assert!(repo
            .hydrate_package(&Arc::new(solver_packages[0].as_ref().clone()))
            .is_none());

        let packages = repo
            .parse_and_cache_response("vendor/package", body, Some("^2.0"))
            .await
            .unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version, "2.0.0.0");
        assert!(repo
            .read_filtered_package_cache("vendor/package", body, &None, "^1.0")
            .is_none());

        let changed_body = [body.as_slice(), b" "].concat();
        assert!(repo
            .read_filtered_package_cache("vendor/package", &changed_body, &None, "^2.0")
            .is_none());

        repo.file_cache
            .as_ref()
            .unwrap()
            .write_data(
                &ComposerRepository::filtered_cache_key("vendor/package"),
                b"bad",
            )
            .unwrap();
        assert!(repo
            .read_filtered_package_cache("vendor/package", body, &None, "^2.0")
            .is_none());
    }

    #[test]
    fn branch_aliases_survive_constraint_filtering() {
        let mut package = Package::new("a/a", "dev-foobar");
        package.extra = Some(serde_json::json!({
            "branch-alias": {"dev-foobar": "3.2.x-dev"}
        }));

        let filtered =
            ComposerRepository::filter_packages(vec![Arc::new(package)], Some("3.2.*@dev"));

        assert_eq!(filtered.len(), 1);
    }

    // ============================================================================
    // Tests for providers API response parsing
    // ============================================================================

    #[test]
    fn test_providers_response_parsing() {
        #[derive(Deserialize)]
        struct ProvidersResponse {
            providers: Vec<ProviderData>,
        }

        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct ProviderData {
            name: String,
            description: Option<String>,
            #[serde(rename = "type")]
            package_type: Option<String>,
        }

        let json = r#"{
            "providers": [
                {
                    "name": "monolog/monolog",
                    "description": "Sends your logs to files, sockets, inboxes, databases and various web services",
                    "type": "library"
                },
                {
                    "name": "symfony/monolog-bundle",
                    "description": "Symfony MonologBundle",
                    "type": "symfony-bundle"
                }
            ]
        }"#;

        let response: ProvidersResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.providers.len(), 2);
        assert_eq!(response.providers[0].name, "monolog/monolog");
        assert_eq!(
            response.providers[1].package_type,
            Some("symfony-bundle".to_string())
        );
    }

    // ============================================================================
    // Tests for dev package name handling
    // ============================================================================

    #[test]
    fn test_dev_package_name_suffix() {
        // The ~dev suffix is used for loading dev versions of packages
        let name = "vendor/package";
        let dev_name = format!("{}~dev", name);
        assert_eq!(dev_name, "vendor/package~dev");
    }

    #[test]
    fn test_dev_package_cache_key() {
        // Dev packages should have their own cache key
        // The / is replaced with ~ so vendor/package~dev becomes vendor~package~dev
        let key = ComposerRepository::cache_key("vendor/package~dev");
        assert_eq!(key, "provider-vendor~package~dev.json");
    }

    // Ported from ComposerRepositoryTest::testWhatProvides.
    #[tokio::test]
    async fn composer_repository_provider_metadata_preserves_branch_aliases() {
        let repository = ComposerRepository::new("test", "https://example.org");
        let packages = repository
            .parse_and_cache_response(
                "a",
                br#"{
                    "packages": {"a": [
                        {"version": "dev-master", "extra": {"branch-alias": {"dev-master": "1.0.x-dev"}}},
                        {"version": "dev-develop", "extra": {"branch-alias": {"dev-develop": "1.1.x-dev"}}},
                        {"version": "0.6"}
                    ]}
                }"#,
                None,
            )
            .await
            .unwrap();
        assert_eq!(packages.len(), 3);
        let aliases = packages
            .iter()
            .flat_map(|package| parse_branch_aliases(package.extra.as_ref()).into_values())
            .collect::<Vec<_>>();
        assert_eq!(packages.len() + aliases.len(), 5);
        assert!(aliases.iter().any(|(_, pretty)| pretty == "1.1.x-dev"));
    }

    // Ported from ComposerRepositoryTest::testSearchWithType.
    #[test]
    fn composer_repository_search_url_includes_the_requested_package_type() {
        let template = "http://example.org/search.json?q=%query%&type=%type%";
        assert_eq!(
            ComposerRepository::build_search_url(template, "foo", Some("composer-plugin")),
            "http://example.org/search.json?q=foo&type=composer-plugin"
        );
        assert_eq!(
            ComposerRepository::build_search_url(template, "foo", Some("library")),
            "http://example.org/search.json?q=foo&type=library"
        );
    }

    // Ported from ComposerRepositoryTest::testSearchWithSpecialChars.
    #[test]
    fn composer_repository_search_url_uses_form_encoding_for_special_characters() {
        assert_eq!(
            ComposerRepository::build_search_url(
                "http://example.org/search.json?q=%query%&type=%type%",
                "foo bar",
                None,
            ),
            "http://example.org/search.json?q=foo+bar&type="
        );
    }

    // Ported from ComposerRepositoryTest::testSearchWithAbandonedPackages.
    #[test]
    fn composer_repository_search_preserves_abandoned_flags_and_replacements() {
        let response: SearchResponse = serde_json::from_value(serde_json::json!({
            "results": [
                {"name": "foo1", "description": null, "abandoned": true},
                {"name": "foo2", "description": null, "abandoned": "bar"}
            ]
        }))
        .unwrap();
        let results = ComposerRepository::convert_search_results(response);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].abandoned.as_deref(), Some(""));
        assert_eq!(results[1].abandoned.as_deref(), Some("bar"));
    }

    // Ported from ComposerRepositoryTest::testGetProviderNamesWillReturnPartialPackageNames.
    #[tokio::test]
    async fn composer_repository_package_names_include_inline_partial_packages() {
        let repository = ComposerRepository::new("test", "http://example.org/packages.json");
        *repository.root_loaded.write().await = true;
        *repository.lazy_providers_url.write().await =
            Some("http://example.org/foo/p/%package%.json".to_owned());
        *repository.partial_package_names.write().await = Some(vec!["foo/bar".to_owned()]);
        assert_eq!(repository.get_package_names(None).await, ["foo/bar"]);
    }

    // Ported from ComposerRepositoryTest's advisory transport-options method.
    #[test]
    fn composer_repository_advisory_request_retains_transport_options() {
        let options = serde_json::json!({"http": {"verify_peer": false}});
        let request = ComposerRepository::security_advisory_api_request(
            "https://example.org/security-advisories",
            &["foo/bar".to_owned()],
            options.clone(),
        );
        assert_eq!(request.transport_options, options);
        assert_eq!(request.method, "POST");
        assert_eq!(request.content_type, "application/x-www-form-urlencoded");
        assert_eq!(request.timeout_seconds, 10);
        assert_eq!(request.body, "packages%5B0%5D=foo%2Fbar");
    }

    fn security_advisory(id: &str, affected_versions: &str) -> SecurityAdvisory {
        SecurityAdvisory {
            advisory_id: id.to_owned(),
            package_name: "foo/bar".to_owned(),
            affected_versions: affected_versions.to_owned(),
            source: None,
            title: None,
            cve: None,
            link: None,
            severity: None,
            reported_at: None,
            sources: None,
        }
    }

    #[test]
    fn composer_repository_advisories_inherit_the_response_map_package_name() {
        let partial = serde_json::json!({
            "advisoryId": "PKSA-test",
            "affectedVersions": "<2.0"
        });
        let advisory = parse_security_advisory(&partial, "foo/bar", false).unwrap();
        assert_eq!(advisory.package_name, "foo/bar");
        assert_eq!(advisory.advisory_id, "PKSA-test");
        assert!(parse_security_advisory(&partial, "foo/bar", true).is_err());

        let full = serde_json::json!({
            "advisoryId": "PKSA-test",
            "affectedVersions": "<2.0",
            "title": "test",
            "reportedAt": "2026-01-01T00:00:00Z",
            "sources": [{"name": "test", "remoteId": "REMOTE-1"}]
        });
        assert!(parse_security_advisory(&full, "foo/bar", true).is_ok());
    }

    // Ported from ComposerRepositoryTest's consecutive advisory-array method.
    #[test]
    fn composer_repository_advisories_are_filtered_into_consecutive_vectors() {
        let filtered = ComposerRepository::filter_security_advisories(
            &BTreeMap::from([("foo/bar".to_owned(), "=1.0.0.0".to_owned())]),
            BTreeMap::from([(
                "foo/bar".to_owned(),
                vec![
                    security_advisory("first", ">=1.0.0,<1.1.0"),
                    security_advisory("second", ">=2.0.0"),
                    security_advisory("third", ">=1.0.0,<1.1.0"),
                ],
            )]),
        );
        assert_eq!(filtered["foo/bar"].len(), 2);
        assert_eq!(filtered["foo/bar"][0].advisory_id, "first");
        assert_eq!(filtered["foo/bar"][1].advisory_id, "third");
    }

    fn package_versions(packages: &[(&str, &[&str])]) -> PackageVersions {
        packages
            .iter()
            .map(|(name, versions)| {
                (
                    (*name).to_owned(),
                    versions
                        .iter()
                        .map(|version| (*version).to_owned())
                        .collect(),
                )
            })
            .collect()
    }

    // Ported from ComposerRepositoryTest::testGetFilterWithMatchingLists.
    #[test]
    fn composer_repository_builds_matching_per_package_filter_entries() {
        let entries = ComposerRepository::build_filter_entries(
            &serde_json::json!({
                "test": [{
                    "constraint": "*",
                    "url": "https://example.org/acme/package/filters.json",
                    "reason": "Malicious code detected",
                    "id": "ID-test"
                }]
            }),
            &package_versions(&[("acme/package", &["1.0.0"])]),
            &["test".to_owned()],
            Some("acme/package"),
        );
        assert_eq!(entries["test"].len(), 1);
        assert_eq!(entries["test"][0].package_name, "acme/package");
        assert_eq!(entries["test"][0].id.as_deref(), Some("ID-test"));
    }

    // Ported from ComposerRepositoryTest's disabled user-filter method.
    #[tokio::test]
    async fn composer_repository_disabled_user_filter_short_circuits_metadata_loading() {
        let mut repository = ComposerRepository::new("test", "https://unreachable.invalid");
        repository.set_user_filter_config(Value::Bool(false));
        assert!(!repository.has_filter().await);
        assert!(repository.get_filter_lists().await.is_empty());
        assert!(!*repository.root_loaded.read().await);
    }

    // Ported from ComposerRepositoryTest's per-list user-filter method.
    #[test]
    fn composer_repository_user_filter_can_opt_out_of_individual_lists() {
        let lists = ComposerRepository::apply_user_filter_config(
            &[
                "malware".to_owned(),
                "typosquatting".to_owned(),
                "deprecated".to_owned(),
            ],
            &serde_json::json!({"typosquatting": false, "unknown-list": false}),
        );
        assert_eq!(lists, ["malware", "deprecated"]);
    }

    // Ported from ComposerRepositoryTest's true-as-no-op user-filter method.
    #[test]
    fn composer_repository_user_filter_true_is_a_no_op() {
        let lists = ComposerRepository::apply_user_filter_config(
            &["malware".to_owned(), "typosquatting".to_owned()],
            &serde_json::json!({"malware": true, "typosquatting": false}),
        );
        assert_eq!(lists, ["malware"]);
    }

    // Ported from ComposerRepositoryTest's summary package-selection method.
    #[test]
    fn composer_repository_filter_summary_selects_only_listed_packages() {
        let selected = ComposerRepository::filter_summary_candidates(
            &BTreeMap::from([(
                "malware".to_owned(),
                BTreeMap::from([("evil/pkg".to_owned(), "^1.0".to_owned())]),
            )]),
            &package_versions(&[("evil/pkg", &["1.0.0"]), ("safe/pkg", &["1.0.0"])]),
            &["malware".to_owned()],
        );
        assert_eq!(selected.keys().collect::<Vec<_>>(), ["evil/pkg"]);
    }

    // Ported from ComposerRepositoryTest's configured-list summary method.
    #[test]
    fn composer_repository_filter_summary_skips_unconfigured_lists() {
        let selected = ComposerRepository::filter_summary_candidates(
            &BTreeMap::from([(
                "typosquatting".to_owned(),
                BTreeMap::from([("lookalike/pkg".to_owned(), "*".to_owned())]),
            )]),
            &package_versions(&[("lookalike/pkg", &["1.0.0"])]),
            &["malware".to_owned()],
        );
        assert!(selected.is_empty());
    }

    // Ported from ComposerRepositoryTest's non-intersecting summary method.
    #[test]
    fn composer_repository_filter_summary_skips_non_intersecting_versions() {
        let selected = ComposerRepository::filter_summary_candidates(
            &BTreeMap::from([(
                "malware".to_owned(),
                BTreeMap::from([("evil/pkg".to_owned(), "^1.0".to_owned())]),
            )]),
            &package_versions(&[("evil/pkg", &["2.5.0"])]),
            &["malware".to_owned()],
        );
        assert!(selected.is_empty());
    }

    fn repository_filter_information(
        summary_url: Option<&str>,
        api_url: Option<&str>,
    ) -> ComposerRepositoryFilterInformation {
        ComposerRepositoryFilterInformation {
            metadata: true,
            lists: vec!["malware".to_owned()],
            summary_url: summary_url.map(str::to_owned),
            api_url: api_url.map(str::to_owned),
        }
    }

    // Ported from ComposerRepositoryTest's already-fetched summary method.
    #[test]
    fn composer_repository_skips_filter_summary_after_metadata_was_fetched() {
        assert_eq!(
            ComposerRepository::select_filter_source(
                &repository_filter_information(Some("summary"), None),
                true,
            ),
            ComposerRepositoryFilterSource::PackageMetadata
        );
    }

    // Ported from ComposerRepositoryTest's API-precedence method.
    #[test]
    fn composer_repository_filter_api_takes_precedence_over_summary_and_metadata() {
        assert_eq!(
            ComposerRepository::select_filter_source(
                &repository_filter_information(Some("summary"), Some("api")),
                false,
            ),
            ComposerRepositoryFilterSource::Api
        );
    }

    // Ported from ComposerRepositoryTest's already-fetched API method.
    #[test]
    fn composer_repository_skips_filter_api_after_metadata_was_fetched() {
        assert_eq!(
            ComposerRepository::select_filter_source(
                &repository_filter_information(None, Some("api")),
                true,
            ),
            ComposerRepositoryFilterSource::PackageMetadata
        );
    }

    // Ported from ComposerRepositoryTest::testGetFilterReusesCachedSummaryOn304.
    #[test]
    fn composer_repository_reuses_cached_filter_summary_after_not_modified() {
        let summary = BTreeMap::from([(
            "malware".to_owned(),
            BTreeMap::from([("evil/pkg".to_owned(), "*".to_owned())]),
        )]);
        let mut document = ConditionalRepositoryDocument::default();
        assert_eq!(
            document
                .resolve(ConditionalRepositoryResponse::Modified(summary.clone()))
                .unwrap(),
            &summary
        );
        assert_eq!(
            document
                .resolve(ConditionalRepositoryResponse::NotModified)
                .unwrap(),
            &summary
        );
    }
}
