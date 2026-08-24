use async_trait::async_trait;
use indexmap::IndexMap;
use regex::Regex;
use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;
use tokio::sync::RwLock;

use super::package_cache::{ArchivedCachedPackage, CachedPackage};
use super::traits::{ProviderInfo, Repository, SearchMode, SearchResult};
use crate::cache::{CacheMetadata, RepoCache};
use crate::config::AuthConfig;
use crate::package::{Autoload, AutoloadPath, Dist, Package, Source, Stability};
use sonata_semver::{Constraint, Operator, VersionParser};

/// Default TTL for cached metadata (10 minutes, matching Composer)
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
                .user_agent("sonata-composer/0.1.0")
                .build()
                .unwrap_or_default(),
            file_cache: None,
            cache_ttl: DEFAULT_CACHE_TTL,
            auth: None,
            notify_batch: RwLock::new(None),
            search_url: RwLock::new(None),
            providers_api_url: RwLock::new(None),
            lazy_providers_url: RwLock::new(None),
            list_url: RwLock::new(None),
            available_packages: RwLock::new(None),
            available_package_patterns: RwLock::new(None),
            has_available_package_list: RwLock::new(false),
            source_mirrors: RwLock::new(HashMap::new()),
            dist_mirrors: RwLock::new(Vec::new()),
            root_loaded: RwLock::new(false),
            degraded_mode: RwLock::new(false),
            packages_not_found: RwLock::new(HashSet::new()),
        }
    }

    /// Create Packagist.org repository
    pub fn packagist() -> Self {
        Self::new("packagist.org", "https://repo.packagist.org")
    }

    /// Create Packagist.org repository with file caching enabled
    pub fn packagist_with_cache(cache_dir: PathBuf) -> Self {
        let mut repo = Self::packagist();
        repo.set_cache_dir(cache_dir);
        repo
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

        if *self.has_available_package_list.read().await {
            if !self.lazy_providers_repo_contains(name).await {
                return Ok(Vec::new());
            }
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
            let expanded_versions = Self::expand_minified_versions(versions);
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
            let (package, metadata) = cached.into_solver_package()?;
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
            let (package, metadata) = cached_package.into_solver_package()?;
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
                match Constraint::new(Operator::Equal, package.version.as_str().to_owned()) {
                    Ok(version) => parsed_constraint.matches(&version),
                    Err(_) => true,
                }
            })
            .collect()
    }

    /// Expand Packagist v2 minified versions to full version data.
    ///
    /// Packagist v2 uses delta compression where each version only includes
    /// fields that changed from the previous version. This function expands
    /// the minified data to full versions.
    fn expand_minified_versions(versions: &[PackagistVersion]) -> Vec<PackagistVersion> {
        let mut result = Vec::with_capacity(versions.len());
        let mut expanded: Option<PackagistVersion> = None;

        for version_data in versions {
            if expanded.is_none() {
                // First version - use as-is, it has all fields
                expanded = Some(version_data.clone());
                result.push(version_data.clone());
                continue;
            }

            // Apply delta: start with previous expanded version, apply changes from current
            let prev = expanded.as_ref().unwrap();
            let new_expanded = PackagistVersion {
                version: version_data.version.clone(),
                // Inherit from previous, override if current has value
                version_normalized: version_data
                    .version_normalized
                    .clone()
                    .or_else(|| prev.version_normalized.clone()),
                description: Self::apply_delta_opt(&version_data.description, &prev.description),
                homepage: Self::apply_delta_opt(&version_data.homepage, &prev.homepage),
                license: Self::apply_delta_opt(&version_data.license, &prev.license),
                keywords: Self::apply_delta_opt(&version_data.keywords, &prev.keywords),
                authors: Self::apply_delta_opt(&version_data.authors, &prev.authors),
                require: Self::apply_delta_hashmap(&version_data.require, &prev.require),
                require_dev: Self::apply_delta_hashmap(
                    &version_data.require_dev,
                    &prev.require_dev,
                ),
                conflict: Self::apply_delta_hashmap(&version_data.conflict, &prev.conflict),
                provide: Self::apply_delta_hashmap(&version_data.provide, &prev.provide),
                replace: Self::apply_delta_hashmap(&version_data.replace, &prev.replace),
                suggest: Self::apply_delta_hashmap(&version_data.suggest, &prev.suggest),
                package_type: Self::apply_delta_opt(&version_data.package_type, &prev.package_type),
                bin: Self::apply_delta_opt(&version_data.bin, &prev.bin),
                source: Self::apply_delta_opt(&version_data.source, &prev.source),
                dist: Self::apply_delta_opt(&version_data.dist, &prev.dist),
                autoload: Self::apply_delta_opt(&version_data.autoload, &prev.autoload),
                autoload_dev: Self::apply_delta_opt(&version_data.autoload_dev, &prev.autoload_dev),
                time: Self::apply_delta_opt(&version_data.time, &prev.time),
                notification_url: Self::apply_delta_opt(
                    &version_data.notification_url,
                    &prev.notification_url,
                ),
                support: Self::apply_delta_opt(&version_data.support, &prev.support),
                funding: Self::apply_delta_opt(&version_data.funding, &prev.funding),
                extra: Self::apply_delta_opt(&version_data.extra, &prev.extra),
            };

            result.push(new_expanded.clone());
            expanded = Some(new_expanded);
        }

        result
    }

    fn apply_delta_opt<T: Clone>(current: &Option<T>, prev: &Option<T>) -> Option<T> {
        current.clone().or_else(|| prev.clone())
    }

    fn apply_delta_hashmap(
        current: &Option<IndexMap<String, String>>,
        prev: &Option<IndexMap<String, String>>,
    ) -> Option<IndexMap<String, String>> {
        current.clone().or_else(|| prev.clone())
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
        pkg.replace_self_version();

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

        Vec::new()
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

        Vec::new()
    }

    async fn search(&self, query: &str, mode: SearchMode) -> Vec<SearchResult> {
        self.load_root_server_file().await.ok();

        match mode {
            SearchMode::Fulltext => {
                let search_url = self.search_url.read().await.clone();
                let url = if let Some(ref base_search) = search_url {
                    base_search
                        .replace("%query%", &urlencoding::encode(query))
                        .replace("%type%", "")
                } else {
                    format!("{}/search.json?q={}", self.url, urlencoding::encode(query))
                };

                let response = match self.client.get(&url).send().await {
                    Ok(r) => r,
                    Err(_) => return Vec::new(),
                };

                if !response.status().is_success() {
                    return Vec::new();
                }

                let data: SearchResponse = match response.json().await {
                    Ok(d) => d,
                    Err(_) => return Vec::new(),
                };

                data.results
                    .into_iter()
                    .filter(|r| !r.is_virtual.unwrap_or(false))
                    .map(|r| {
                        let abandoned = match r.abandoned {
                            Some(Value::Bool(true)) => Some("".to_string()),
                            Some(Value::String(s)) => Some(s),
                            _ => None,
                        };

                        SearchResult {
                            name: r.name,
                            description: r.description,
                            url: r.url,
                            abandoned,
                            downloads: r.downloads,
                            favers: r.favers,
                        }
                    })
                    .collect()
            }
            SearchMode::Vendor => {
                let package_names = self.get_package_names(None).await;

                let regex_str = query
                    .split_whitespace()
                    .map(|w| regex::escape(w))
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
                    .map(|w| regex::escape(w))
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
}

/// Packagist API response for package metadata
#[derive(Debug, Deserialize)]
struct PackagistResponse {
    packages: HashMap<String, Vec<PackagistVersion>>,
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
    #[serde(default, deserialize_with = "deserialize_maybe_unset")]
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

        let versions: Vec<PackagistVersion> = serde_json::from_str(json).unwrap();
        let expanded = ComposerRepository::expand_minified_versions(&versions);

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

        let versions: Vec<PackagistVersion> = serde_json::from_str(json).unwrap();
        let expanded = ComposerRepository::expand_minified_versions(&versions);

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

        let versions: Vec<PackagistVersion> = serde_json::from_str(json).unwrap();
        let expanded = ComposerRepository::expand_minified_versions(&versions);

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

        let versions: Vec<PackagistVersion> = serde_json::from_str(json).unwrap();
        let expanded = ComposerRepository::expand_minified_versions(&versions);

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

        let doctrine_versions: Vec<PackagistVersion> = serde_json::from_str(doctrine_json).unwrap();
        let shopware_versions: Vec<PackagistVersion> = serde_json::from_str(shopware_json).unwrap();

        // Expand each package separately (as the real code does)
        let doctrine_expanded = ComposerRepository::expand_minified_versions(&doctrine_versions);
        let shopware_expanded = ComposerRepository::expand_minified_versions(&shopware_versions);

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

        let versions: Vec<PackagistVersion> = serde_json::from_str(json).unwrap();
        let expanded = ComposerRepository::expand_minified_versions(&versions);

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
        let expanded_a = ComposerRepository::expand_minified_versions(versions_a);

        // Process package-b
        let versions_b = response.packages.get("vendor/package-b").unwrap();
        let expanded_b = ComposerRepository::expand_minified_versions(versions_b);

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
}
