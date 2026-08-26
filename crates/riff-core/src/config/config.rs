use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::source::{ConfigLoader, ConfigSource, RawConfig};
use crate::error::{Result, RiffError};
use crate::util::expand_path;

/// Preferred installation method
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PreferredInstall {
    Auto,
    Source,
    #[default]
    Dist,
    Patterns(IndexMap<String, String>),
}

impl PreferredInstall {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "auto" => Some(PreferredInstall::Auto),
            "source" => Some(PreferredInstall::Source),
            "dist" => Some(PreferredInstall::Dist),
            _ => None,
        }
    }
}

impl Serialize for PreferredInstall {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            PreferredInstall::Auto => serializer.serialize_str("auto"),
            PreferredInstall::Source => serializer.serialize_str("source"),
            PreferredInstall::Dist => serializer.serialize_str("dist"),
            PreferredInstall::Patterns(patterns) => patterns.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for PreferredInstall {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(value) = value.as_str() {
            return PreferredInstall::from_str(value).ok_or_else(|| {
                serde::de::Error::custom(format!("invalid preferred-install value {value}"))
            });
        }
        let patterns = serde_json::from_value::<IndexMap<String, String>>(value)
            .map_err(serde::de::Error::custom)?;
        Ok(PreferredInstall::Patterns(patterns))
    }
}

/// How to handle authentication storage
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreAuths {
    #[serde(rename = "true")]
    True,
    #[serde(rename = "false")]
    False,
    #[default]
    Prompt,
}

impl StoreAuths {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "true" => Some(StoreAuths::True),
            "false" => Some(StoreAuths::False),
            "prompt" => Some(StoreAuths::Prompt),
            _ => None,
        }
    }
}

/// How to handle uncommitted changes
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscardChanges {
    #[serde(rename = "true")]
    True,
    #[serde(rename = "false")]
    #[default]
    False,
    Stash,
}

impl DiscardChanges {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "true" => Some(DiscardChanges::True),
            "false" => Some(DiscardChanges::False),
            "stash" => Some(DiscardChanges::Stash),
            _ => None,
        }
    }
}

/// Platform check configuration
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlatformCheck {
    #[default]
    PhpOnly,
    True,
    False,
}

impl PlatformCheck {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "php-only" => Some(PlatformCheck::PhpOnly),
            "true" => Some(PlatformCheck::True),
            "false" => Some(PlatformCheck::False),
            _ => None,
        }
    }
}

/// Plugin allowlist configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AllowPlugins {
    Bool(bool),
    Map(HashMap<String, bool>),
}

impl Default for AllowPlugins {
    fn default() -> Self {
        AllowPlugins::Bool(true)
    }
}

/// Audit configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditConfig {
    #[serde(default)]
    pub ignore: Vec<String>,

    #[serde(default = "default_audit_abandoned")]
    pub abandoned: String,

    #[serde(rename = "block-abandoned", skip_serializing_if = "Option::is_none")]
    pub block_abandoned: Option<bool>,
}

fn default_audit_abandoned() -> String {
    "fail".to_string()
}

impl Default for AuditConfig {
    fn default() -> Self {
        AuditConfig {
            ignore: Vec::new(),
            abandoned: default_audit_abandoned(),
            block_abandoned: None,
        }
    }
}

/// HTTP Basic authentication credentials
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpBasicAuth {
    pub username: String,
    pub password: String,
}

/// GitLab token authentication
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GitLabToken {
    Token(String),
    OAuth {
        #[serde(rename = "oauth-token")]
        oauth_token: String,
    },
}

/// Bitbucket OAuth configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BitbucketOAuth {
    #[serde(rename = "consumer-key")]
    pub consumer_key: String,

    #[serde(rename = "consumer-secret")]
    pub consumer_secret: String,
}

/// Main Composer configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Ordered repositories merged from global and project configuration.
    #[serde(skip)]
    pub repositories: IndexMap<String, serde_json::Value>,

    // Directories
    #[serde(rename = "vendor-dir", default = "default_vendor_dir")]
    pub vendor_dir: PathBuf,

    #[serde(rename = "bin-dir", default = "default_bin_dir")]
    pub bin_dir: PathBuf,

    #[serde(rename = "cache-dir", skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,

    #[serde(rename = "data-dir", skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,

    #[serde(rename = "home", skip_serializing_if = "Option::is_none")]
    pub home_dir: Option<PathBuf>,

    // Cache settings
    #[serde(rename = "cache-files-dir", skip_serializing_if = "Option::is_none")]
    pub cache_files_dir: Option<PathBuf>,

    #[serde(rename = "cache-repo-dir", skip_serializing_if = "Option::is_none")]
    pub cache_repo_dir: Option<PathBuf>,

    #[serde(rename = "cache-vcs-dir", skip_serializing_if = "Option::is_none")]
    pub cache_vcs_dir: Option<PathBuf>,

    #[serde(rename = "cache-ttl", default = "default_cache_ttl")]
    pub cache_ttl: u64,

    #[serde(rename = "cache-files-ttl", skip_serializing_if = "Option::is_none")]
    pub cache_files_ttl: Option<u64>,

    #[serde(
        rename = "cache-files-maxsize",
        default = "default_cache_files_maxsize"
    )]
    pub cache_files_maxsize: u64,

    #[serde(rename = "cache-read-only", default)]
    pub cache_read_only: bool,

    // Behavior
    #[serde(rename = "process-timeout", default = "default_process_timeout")]
    pub process_timeout: u64,

    #[serde(rename = "use-include-path", default)]
    pub use_include_path: bool,

    #[serde(rename = "use-parent-dir", skip_serializing_if = "Option::is_none")]
    pub use_parent_dir: Option<String>,

    #[serde(rename = "preferred-install", default)]
    pub preferred_install: PreferredInstall,

    #[serde(rename = "store-auths", default)]
    pub store_auths: StoreAuths,

    #[serde(rename = "notify-on-install", default = "default_true")]
    pub notify_on_install: bool,

    #[serde(rename = "discard-changes", default)]
    pub discard_changes: DiscardChanges,

    #[serde(rename = "optimize-autoloader", default)]
    pub optimize_autoloader: bool,

    #[serde(rename = "sort-packages", default)]
    pub sort_packages: bool,

    #[serde(rename = "classmap-authoritative", default)]
    pub classmap_authoritative: bool,

    #[serde(rename = "apcu-autoloader", default)]
    pub apcu_autoloader: bool,

    #[serde(rename = "bump-after-update", skip_serializing_if = "Option::is_none")]
    pub bump_after_update: Option<String>,

    #[serde(rename = "prepend-autoloader", default = "default_true")]
    pub prepend_autoloader: bool,

    #[serde(rename = "autoloader-suffix", skip_serializing_if = "Option::is_none")]
    pub autoloader_suffix: Option<String>,

    #[serde(rename = "lock", default = "default_true")]
    pub lock: bool,

    #[serde(rename = "allow-missing-requirements", default)]
    pub allow_missing_requirements: bool,

    #[serde(rename = "platform-check", default)]
    pub platform_check: PlatformCheck,

    #[serde(rename = "allow-plugins", default)]
    pub allow_plugins: AllowPlugins,

    #[serde(default)]
    pub audit: AuditConfig,

    /// Lossless merged representation used by dependency policy. The typed
    /// compatibility view above is retained for callers of the historic API.
    #[serde(skip)]
    pub audit_policy: serde_json::Value,

    #[serde(default = "default_policy")]
    pub policy: serde_json::Value,

    #[serde(rename = "source-fallback", default)]
    pub source_fallback: bool,

    // Network - Security
    #[serde(rename = "secure-http", default = "default_true")]
    pub secure_http: bool,

    #[serde(rename = "disable-tls", default)]
    pub disable_tls: bool,

    #[serde(rename = "secure-svn-domains", default)]
    pub secure_svn_domains: Vec<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub cafile: Option<PathBuf>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub capath: Option<PathBuf>,

    // Network - Protocols
    #[serde(rename = "github-protocols", default = "default_github_protocols")]
    pub github_protocols: Vec<String>,

    #[serde(skip)]
    github_protocols_unfiltered: Option<Vec<String>>,

    #[serde(rename = "gitlab-protocol", skip_serializing_if = "Option::is_none")]
    pub gitlab_protocol: Option<String>,

    // Network - Domains
    #[serde(rename = "github-domains", default = "default_github_domains")]
    pub github_domains: Vec<String>,

    #[serde(rename = "gitlab-domains", default = "default_gitlab_domains")]
    pub gitlab_domains: Vec<String>,

    #[serde(rename = "bitbucket-domains", default)]
    pub bitbucket_domains: Vec<String>,

    #[serde(rename = "forgejo-domains", default = "default_forgejo_domains")]
    pub forgejo_domains: Vec<String>,

    // Network - API settings
    #[serde(rename = "use-github-api", default = "default_true")]
    pub use_github_api: bool,

    #[serde(rename = "github-expose-hostname", default = "default_true")]
    pub github_expose_hostname: bool,

    #[serde(rename = "bitbucket-expose-hostname", default = "default_true")]
    pub bitbucket_expose_hostname: bool,

    // Authentication
    #[serde(rename = "http-basic", default)]
    pub http_basic: HashMap<String, HttpBasicAuth>,

    #[serde(default)]
    pub bearer: HashMap<String, String>,

    #[serde(rename = "github-oauth", default)]
    pub github_oauth: HashMap<String, String>,

    #[serde(rename = "gitlab-oauth", default)]
    pub gitlab_oauth: HashMap<String, String>,

    #[serde(rename = "gitlab-token", default)]
    pub gitlab_token: HashMap<String, GitLabToken>,

    #[serde(rename = "bitbucket-oauth", default)]
    pub bitbucket_oauth: HashMap<String, BitbucketOAuth>,

    #[serde(rename = "forgejo-token", default)]
    pub forgejo_token: HashMap<String, String>,

    // Platform overrides
    #[serde(default)]
    pub platform: HashMap<String, serde_json::Value>,

    // Archive settings
    #[serde(rename = "archive-format", default = "default_archive_format")]
    pub archive_format: String,

    #[serde(rename = "archive-dir", default = "default_archive_dir")]
    pub archive_dir: PathBuf,

    // Misc
    #[serde(rename = "htaccess-protect", default = "default_true")]
    pub htaccess_protect: bool,

    #[serde(rename = "bin-compat", default = "default_bin_compat")]
    pub bin_compat: String,

    #[serde(rename = "custom-headers", default)]
    pub custom_headers: HashMap<String, String>,

    #[serde(rename = "client-certificate", default)]
    pub client_certificate: HashMap<String, serde_json::Value>,

    // Internal tracking
    #[serde(skip)]
    base_dir: Option<PathBuf>,

    #[serde(skip)]
    sources: HashMap<String, ConfigSource>,

    #[serde(skip)]
    values: HashMap<String, serde_json::Value>,
}

// Default value functions
fn default_vendor_dir() -> PathBuf {
    PathBuf::from("vendor")
}

fn default_bin_dir() -> PathBuf {
    PathBuf::from("vendor/bin")
}

fn default_process_timeout() -> u64 {
    300
}

fn default_cache_ttl() -> u64 {
    15552000 // 6 months in seconds
}

fn default_cache_files_maxsize() -> u64 {
    300 * 1024 * 1024 // 300 MiB
}

fn default_github_protocols() -> Vec<String> {
    vec!["https".to_string(), "ssh".to_string()]
}

fn composer_github_protocols() -> Vec<String> {
    vec!["https".to_string(), "ssh".to_string(), "git".to_string()]
}

fn default_github_domains() -> Vec<String> {
    vec!["github.com".to_string()]
}

fn default_gitlab_domains() -> Vec<String> {
    vec!["gitlab.com".to_string()]
}

fn default_forgejo_domains() -> Vec<String> {
    vec!["codeberg.org".to_string()]
}

fn default_archive_format() -> String {
    "tar".to_string()
}

fn default_archive_dir() -> PathBuf {
    PathBuf::from(".")
}

fn default_bin_compat() -> String {
    "auto".to_string()
}

fn default_true() -> bool {
    true
}

fn default_policy() -> serde_json::Value {
    serde_json::Value::Bool(true)
}

impl Default for Config {
    fn default() -> Self {
        let mut config = Config {
            repositories: IndexMap::from([(
                "packagist.org".to_string(),
                serde_json::json!({
                    "type": "composer",
                    "url": "https://repo.packagist.org"
                }),
            )]),

            // Directories
            vendor_dir: default_vendor_dir(),
            bin_dir: default_bin_dir(),
            cache_dir: None,
            data_dir: None,
            home_dir: None,
            cache_files_dir: None,
            cache_repo_dir: None,
            cache_vcs_dir: None,
            cache_ttl: default_cache_ttl(),
            cache_files_ttl: None,
            cache_files_maxsize: default_cache_files_maxsize(),
            cache_read_only: false,

            // Behavior
            process_timeout: default_process_timeout(),
            use_include_path: false,
            use_parent_dir: Some("prompt".to_string()),
            preferred_install: PreferredInstall::default(),
            store_auths: StoreAuths::default(),
            notify_on_install: true,
            discard_changes: DiscardChanges::default(),
            optimize_autoloader: false,
            sort_packages: false,
            classmap_authoritative: false,
            apcu_autoloader: false,
            bump_after_update: None,
            prepend_autoloader: true,
            autoloader_suffix: None,
            lock: true,
            allow_missing_requirements: false,
            platform_check: PlatformCheck::default(),
            allow_plugins: AllowPlugins::default(),
            audit: AuditConfig::default(),
            audit_policy: serde_json::json!({}),
            policy: default_policy(),
            source_fallback: false,

            // Network - Security
            secure_http: true,
            disable_tls: false,
            secure_svn_domains: Vec::new(),
            cafile: None,
            capath: None,

            // Network - Protocols
            github_protocols: default_github_protocols(),
            github_protocols_unfiltered: Some(composer_github_protocols()),
            gitlab_protocol: None,

            // Network - Domains
            github_domains: default_github_domains(),
            gitlab_domains: default_gitlab_domains(),
            bitbucket_domains: Vec::new(),
            forgejo_domains: default_forgejo_domains(),

            // Network - API settings
            use_github_api: true,
            github_expose_hostname: true,
            bitbucket_expose_hostname: true,

            // Authentication
            http_basic: HashMap::new(),
            bearer: HashMap::new(),
            github_oauth: HashMap::new(),
            gitlab_oauth: HashMap::new(),
            gitlab_token: HashMap::new(),
            bitbucket_oauth: HashMap::new(),
            forgejo_token: HashMap::new(),

            // Platform overrides
            platform: HashMap::new(),

            // Archive settings
            archive_format: default_archive_format(),
            archive_dir: default_archive_dir(),

            // Misc
            htaccess_protect: true,
            bin_compat: default_bin_compat(),
            custom_headers: HashMap::new(),
            client_certificate: HashMap::new(),

            // Internal
            base_dir: None,
            sources: HashMap::new(),
            values: HashMap::new(),
        };
        for key in config.config_keys() {
            config.sources.insert(key, ConfigSource::Default);
        }
        config
    }
}

impl Config {
    /// Create a new Config with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new Config with defaults and base directory
    pub fn with_base_dir<P: AsRef<Path>>(base_dir: P) -> Self {
        Self {
            base_dir: Some(base_dir.as_ref().to_path_buf()),
            ..Self::default()
        }
    }

    /// Build configuration from all sources (defaults, global, project, env)
    pub fn build<P: AsRef<Path>>(project_dir: Option<P>, use_environment: bool) -> Result<Self> {
        let loader = ConfigLoader::new(use_environment);
        let mut config = Self::default();

        if let Some(ref dir) = project_dir {
            config.base_dir = Some(dir.as_ref().to_path_buf());
        }

        for key in config.config_keys() {
            config.sources.insert(key, ConfigSource::Default);
        }

        // 1. Load global config from ~/.composer/config.json
        let global_config = loader.load_global_config()?;
        config.merge_raw_config(global_config, ConfigSource::Global)?;

        // 2. Load project config from composer.json
        if let Some(project_dir) = &project_dir {
            let project_config = loader.load_project_config(project_dir)?;
            config.merge_raw_config(project_config, ConfigSource::Project)?;
        }

        // 3. Apply environment variable overrides
        if use_environment {
            config.apply_env_overrides(&loader);
        }

        // 4. Resolve computed paths
        config.resolve_paths(&loader);

        Ok(config)
    }

    /// Set base directory (must be absolute path)
    pub fn set_base_dir<P: AsRef<Path>>(&mut self, base_dir: P) {
        self.base_dir = Some(base_dir.as_ref().to_path_buf());
    }

    /// Get base directory
    pub fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    /// Get the source of a configuration value
    pub fn get_source(&self, key: &str) -> Option<&ConfigSource> {
        self.sources.get(key)
    }

    /// Get vendor directory (resolved as absolute path)
    pub fn get_vendor_dir(&self) -> PathBuf {
        self.resolve_path(&self.vendor_dir)
    }

    /// Get vendor directory without resolving it against the project directory.
    pub fn get_vendor_dir_relative(&self) -> PathBuf {
        self.process_path(&self.vendor_dir)
    }

    /// Get bin directory (resolved as absolute path)
    pub fn get_bin_dir(&self) -> PathBuf {
        self.resolve_path(&self.bin_dir)
    }

    /// Get bin directory without resolving it against the project directory.
    pub fn get_bin_dir_relative(&self) -> PathBuf {
        self.process_path(&self.bin_dir)
    }

    /// Get cache directory (resolved as absolute path)
    pub fn get_cache_dir(&self, loader: &ConfigLoader) -> PathBuf {
        if let Some(ref cache_dir) = self.cache_dir {
            self.resolve_path(cache_dir)
        } else {
            loader.get_cache_dir()
        }
    }

    /// Get data directory (resolved as absolute path)
    pub fn get_data_dir(&self, loader: &ConfigLoader) -> PathBuf {
        if let Some(ref data_dir) = self.data_dir {
            self.resolve_path(data_dir)
        } else {
            loader.get_composer_home()
        }
    }

    /// Resolve a string configuration value and any `{$key}` references it contains.
    pub fn get_string(&self, key: &str) -> Option<String> {
        self.raw_string(key)
            .map(|value| self.process_string(&value))
    }

    /// Get audit settings with Composer environment overrides applied.
    pub fn audit_with_environment(&self, loader: &ConfigLoader) -> Result<AuditConfig> {
        let mut audit = self.audit.clone();
        if let Some(abandoned) = loader.get_composer_env("COMPOSER_AUDIT_ABANDONED") {
            if !matches!(abandoned.as_str(), "ignore" | "report" | "fail") {
                return Err(RiffError::Config(format!(
                    "Invalid value for COMPOSER_AUDIT_ABANDONED: {abandoned}"
                )));
            }
            audit.abandoned = abandoned;
        }
        if let Some(block_abandoned) = loader.get_env_bool("security-blocking-abandoned") {
            audit.block_abandoned = Some(block_abandoned);
        }
        Ok(audit)
    }

    /// Get dependency policy with the master environment switch applied.
    pub fn policy_with_environment(&self, loader: &ConfigLoader) -> serde_json::Value {
        match loader.get_env_bool("policy") {
            Some(false) => serde_json::Value::Bool(false),
            Some(true) if self.policy == serde_json::Value::Bool(false) => {
                serde_json::Value::Bool(true)
            }
            _ => self.policy.clone(),
        }
    }

    /// Warning emitted before constructing network transports with TLS protection disabled.
    pub fn tls_protection_warning(&self) -> Option<&'static str> {
        self.disable_tls
            .then_some("You are running Riff with SSL/TLS protection disabled.")
    }

    /// Reject known insecure repository transports when secure HTTP is enabled.
    pub fn prohibit_url_by_config(&self, url: &str) -> Result<()> {
        let Some((scheme, _)) = url.split_once("://") else {
            return Ok(());
        };
        let scheme = scheme.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "git" | "ftp" | "svn")
            || !self.secure_http
            || self.disable_tls
        {
            return Ok(());
        }

        let hostname = url::Url::parse(url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_string));
        if scheme == "svn"
            && hostname
                .as_ref()
                .is_some_and(|host| self.secure_svn_domains.contains(host))
        {
            return Ok(());
        }

        Err(RiffError::Repository(format!(
            "Your configuration does not allow connections to {url}"
        )))
    }

    /// Describe insecure TLS verification options for a repository URL.
    pub fn repository_url_warnings(
        &self,
        url: &str,
        verify_peer: Option<bool>,
        verify_peer_name: Option<bool>,
    ) -> Vec<String> {
        let Some(hostname) = url::Url::parse(url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_string))
        else {
            return Vec::new();
        };
        let disabled = [
            (verify_peer == Some(false)).then_some("verify_peer"),
            (verify_peer_name == Some(false)).then_some("verify_peer_name"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if disabled.is_empty() {
            Vec::new()
        } else {
            vec![format!(
                "Warning: Accessing {hostname} with {} disabled.",
                disabled.join(" and ")
            )]
        }
    }

    /// Resolve a path relative to base_dir if not absolute
    fn resolve_path(&self, path: &Path) -> PathBuf {
        let path = self.process_path(path);
        if path.is_absolute() || is_stream_wrapper(&path.to_string_lossy()) {
            path
        } else if let Some(ref base) = self.base_dir {
            base.join(path)
        } else {
            path
        }
    }

    fn process_path(&self, path: &Path) -> PathBuf {
        let value = self.process_string(&path.to_string_lossy());
        let trimmed = value.trim_end_matches(['/', '\\']);
        PathBuf::from(if trimmed.is_empty() {
            value.as_str()
        } else {
            trimmed
        })
    }

    fn process_string(&self, value: &str) -> String {
        let mut value = value.to_string();
        for _ in 0..16 {
            let Some(start) = value.find("{$") else {
                break;
            };
            let Some(relative_end) = value[start + 2..].find('}') else {
                break;
            };
            let end = start + 2 + relative_end;
            let key = &value[start + 2..end];
            let Some(replacement) = self.raw_string(key) else {
                break;
            };
            value.replace_range(start..=end, &replacement);
        }
        expand_path(&value)
    }

    fn raw_string(&self, key: &str) -> Option<String> {
        self.values
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                let path = match key {
                    "vendor-dir" => Some(&self.vendor_dir),
                    "bin-dir" => Some(&self.bin_dir),
                    "cache-dir" => self.cache_dir.as_ref(),
                    "data-dir" => self.data_dir.as_ref(),
                    "cache-files-dir" => self.cache_files_dir.as_ref(),
                    "cache-repo-dir" => self.cache_repo_dir.as_ref(),
                    "cache-vcs-dir" => self.cache_vcs_dir.as_ref(),
                    _ => None,
                }?;
                Some(path.to_string_lossy().into_owned())
            })
    }

    /// Merge raw configuration from a source
    fn merge_raw_config(&mut self, raw: RawConfig, source: ConfigSource) -> Result<()> {
        if let Some(config_map) = raw.config {
            for (key, value) in config_map {
                self.merge_config_value(&key, value, source.clone())?;
            }
        }
        if let Some(repositories) = raw.repositories {
            self.merge_repositories(repositories, source);
        }
        Ok(())
    }

    fn merge_repositories(&mut self, repositories: serde_json::Value, source: ConfigSource) {
        let entries = match repositories {
            serde_json::Value::Array(repositories) => repositories
                .into_iter()
                .enumerate()
                .map(|(index, repository)| (index.to_string(), repository, true))
                .collect::<Vec<_>>(),
            serde_json::Value::Object(repositories) => repositories
                .into_iter()
                .map(|(name, repository)| (name, repository, false))
                .collect(),
            _ => return,
        };
        let mut prioritized = IndexMap::new();
        let mut malformed = Vec::new();

        for (name, repository, anonymous) in entries {
            if repository == serde_json::Value::Bool(false) && !anonymous {
                self.repositories
                    .shift_remove(normalize_repository_name(&name));
                continue;
            }
            if let Some(disabled) = disabled_repository_name(&repository) {
                self.repositories
                    .shift_remove(normalize_repository_name(disabled));
                continue;
            }
            if !repository.is_object() {
                malformed.push((name, repository));
                continue;
            }

            if is_packagist_repository(&repository) {
                self.repositories.shift_remove("packagist.org");
            }
            let name = if anonymous {
                name
            } else {
                normalize_repository_name(&name).to_string()
            };
            self.repositories.shift_remove(&name);
            self.sources
                .insert(format!("repositories.{name}"), source.clone());
            prioritized.insert(name, repository);
        }

        prioritized.extend(std::mem::take(&mut self.repositories));
        for (name, repository) in malformed {
            prioritized.insert(name, repository);
        }
        self.repositories = prioritized;
    }

    /// Merge a single configuration value
    fn merge_config_value(
        &mut self,
        key: &str,
        value: serde_json::Value,
        source: ConfigSource,
    ) -> Result<()> {
        match key {
            "vendor-dir" => {
                if let Some(s) = value.as_str() {
                    self.vendor_dir = PathBuf::from(s);
                    self.sources.insert(key.to_string(), source);
                }
            }
            "bin-dir" => {
                if let Some(s) = value.as_str() {
                    self.bin_dir = PathBuf::from(s);
                    self.sources.insert(key.to_string(), source);
                }
            }
            "cache-dir" => {
                if let Some(s) = value.as_str() {
                    self.cache_dir = Some(PathBuf::from(s));
                    self.sources.insert(key.to_string(), source);
                }
            }
            "data-dir" => {
                if let Some(s) = value.as_str() {
                    self.data_dir = Some(PathBuf::from(s));
                    self.sources.insert(key.to_string(), source);
                }
            }
            "process-timeout" => {
                if let Some(n) = value.as_u64() {
                    self.process_timeout = n;
                    self.sources.insert(key.to_string(), source);
                }
            }
            "use-include-path" => {
                if let Some(b) = value.as_bool() {
                    self.use_include_path = b;
                    self.sources.insert(key.to_string(), source);
                }
            }
            "preferred-install" => {
                if let Ok(incoming) = serde_json::from_value::<PreferredInstall>(value) {
                    self.preferred_install =
                        merge_preferred_install(self.preferred_install.clone(), incoming);
                    self.sources.insert(key.to_string(), source);
                }
            }
            "store-auths" => {
                if let Some(s) = value.as_str() {
                    if let Some(sa) = StoreAuths::from_str(s) {
                        self.store_auths = sa;
                        self.sources.insert(key.to_string(), source);
                    }
                }
            }
            "notify-on-install" => {
                if let Some(b) = value.as_bool() {
                    self.notify_on_install = b;
                    self.sources.insert(key.to_string(), source);
                }
            }
            "discard-changes" => {
                if let Some(s) = value.as_str() {
                    if let Some(dc) = DiscardChanges::from_str(s) {
                        self.discard_changes = dc;
                        self.sources.insert(key.to_string(), source);
                    }
                } else if let Some(b) = value.as_bool() {
                    self.discard_changes = if b {
                        DiscardChanges::True
                    } else {
                        DiscardChanges::False
                    };
                    self.sources.insert(key.to_string(), source);
                }
            }
            "autoloader-suffix" => {
                if let Some(suffix) = value.as_str() {
                    self.autoloader_suffix = Some(suffix.to_string());
                    self.sources.insert(key.to_string(), source);
                } else if value.is_null() {
                    self.autoloader_suffix = None;
                    self.sources.insert(key.to_string(), source);
                }
            }
            "optimize-autoloader" => {
                if let Some(b) = value.as_bool() {
                    self.optimize_autoloader = b;
                    self.sources.insert(key.to_string(), source);
                }
            }
            "sort-packages" => {
                if let Some(b) = value.as_bool() {
                    self.sort_packages = b;
                    self.sources.insert(key.to_string(), source);
                }
            }
            "classmap-authoritative" => {
                if let Some(b) = value.as_bool() {
                    self.classmap_authoritative = b;
                    self.sources.insert(key.to_string(), source);
                }
            }
            "apcu-autoloader" => {
                if let Some(b) = value.as_bool() {
                    self.apcu_autoloader = b;
                    self.sources.insert(key.to_string(), source);
                }
            }
            "archive-format" => {
                if let Some(format) = value.as_str() {
                    self.archive_format = format.to_owned();
                    self.sources.insert(key.to_string(), source);
                }
            }
            "archive-dir" => {
                if let Some(directory) = value.as_str() {
                    self.archive_dir = PathBuf::from(directory);
                    self.sources.insert(key.to_string(), source);
                }
            }
            "bump-after-update" => {
                let mode = match value {
                    serde_json::Value::Bool(true) => Some("all".to_string()),
                    serde_json::Value::Bool(false) => None,
                    serde_json::Value::String(mode)
                        if matches!(mode.as_str(), "all" | "dev" | "no-dev") =>
                    {
                        Some(mode)
                    }
                    _ => return Ok(()),
                };
                self.bump_after_update = mode;
                self.sources.insert(key.to_string(), source);
            }
            "secure-http" => {
                if let Some(b) = value.as_bool() {
                    self.secure_http = b;
                    self.refresh_github_protocols();
                    self.sources.insert(key.to_string(), source);
                }
            }
            "disable-tls" => {
                let boolish = value.as_bool().or_else(|| {
                    value.as_str().and_then(|value| match value {
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    })
                });
                if let Some(b) = boolish {
                    self.disable_tls = b;
                    self.sources.insert(key.to_string(), source);
                }
            }
            "lock" => {
                let boolish = value.as_bool().or_else(|| {
                    value.as_str().and_then(|value| match value {
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    })
                });
                if let Some(b) = boolish {
                    self.lock = b;
                    self.sources.insert(key.to_string(), source);
                }
            }
            "allow-missing-requirements" => {
                if let Some(b) = value.as_bool() {
                    self.allow_missing_requirements = b;
                    self.sources.insert(key.to_string(), source);
                }
            }
            "platform-check" => {
                if let Some(s) = value.as_str() {
                    if let Some(pc) = PlatformCheck::from_str(s) {
                        self.platform_check = pc;
                        self.sources.insert(key.to_string(), source);
                    }
                } else if let Some(b) = value.as_bool() {
                    self.platform_check = if b {
                        PlatformCheck::True
                    } else {
                        PlatformCheck::False
                    };
                    self.sources.insert(key.to_string(), source);
                }
            }
            "allow-plugins" => {
                if let Ok(allow_plugins) = serde_json::from_value(value) {
                    match (&mut self.allow_plugins, allow_plugins) {
                        (AllowPlugins::Map(current), AllowPlugins::Map(next)) => {
                            current.extend(next);
                        }
                        (current, next) => *current = next,
                    }
                    self.sources.insert(key.to_string(), source);
                }
            }
            "audit" => {
                merge_object_value(&mut self.audit_policy, value.clone());
                if let Some(object) = value.as_object() {
                    if let Some(ignore) = object.get("ignore").and_then(|value| value.as_array()) {
                        self.audit.ignore.extend(
                            ignore
                                .iter()
                                .filter_map(|value| value.as_str().map(str::to_string)),
                        );
                    }
                    if let Some(abandoned) =
                        object.get("abandoned").and_then(|value| value.as_str())
                    {
                        self.audit.abandoned = abandoned.to_string();
                    }
                    if let Some(block_abandoned) = object
                        .get("block-abandoned")
                        .and_then(serde_json::Value::as_bool)
                    {
                        self.audit.block_abandoned = Some(block_abandoned);
                    }
                    self.sources.insert(key.to_string(), source);
                }
            }
            "policy" => {
                merge_policy_value(&mut self.policy, value);
                self.sources.insert(key.to_string(), source);
            }
            "source-fallback" => {
                let boolish = value.as_bool().or_else(|| {
                    value.as_str().and_then(|value| match value {
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    })
                });
                if let Some(source_fallback) = boolish {
                    self.source_fallback = source_fallback;
                    self.sources.insert(key.to_string(), source);
                }
            }
            "github-protocols" => {
                if let Some(arr) = value.as_array() {
                    self.github_protocols_unfiltered = Some(
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect(),
                    );
                    self.refresh_github_protocols();
                    self.sources.insert(key.to_string(), source);
                }
            }
            "github-domains" => {
                if let Some(arr) = value.as_array() {
                    let new_domains: Vec<String> = arr
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                    // Merge with existing
                    for domain in new_domains {
                        if !self.github_domains.contains(&domain) {
                            self.github_domains.push(domain);
                        }
                    }
                    self.sources.insert(key.to_string(), source);
                }
            }
            "gitlab-domains" => {
                if let Some(arr) = value.as_array() {
                    let new_domains: Vec<String> = arr
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                    // Merge with existing
                    for domain in new_domains {
                        if !self.gitlab_domains.contains(&domain) {
                            self.gitlab_domains.push(domain);
                        }
                    }
                    self.sources.insert(key.to_string(), source);
                }
            }
            "platform" => {
                if let Some(obj) = value.as_object() {
                    for (k, v) in obj {
                        if v.is_string() || v == &serde_json::Value::Bool(false) {
                            self.platform.insert(k.clone(), v.clone());
                        }
                    }
                    self.sources.insert(key.to_string(), source);
                }
            }
            "github-oauth" => {
                let obj = value
                    .as_object()
                    .ok_or_else(|| RiffError::Config("github-oauth must be an object".into()))?;
                for (k, v) in obj {
                    if let Some(s) = v.as_str() {
                        self.github_oauth.insert(k.clone(), s.to_string());
                    }
                }
                self.sources.insert(key.to_string(), source);
            }
            "gitlab-oauth" => {
                if let Some(obj) = value.as_object() {
                    for (k, v) in obj {
                        if let Some(s) = v.as_str() {
                            self.gitlab_oauth.insert(k.clone(), s.to_string());
                        }
                    }
                    self.sources.insert(key.to_string(), source);
                }
            }
            _ => {
                // For unknown keys, store the source but don't fail
                self.values.insert(key.to_string(), value);
                self.sources.insert(key.to_string(), source);
            }
        }

        Ok(())
    }

    fn refresh_github_protocols(&mut self) {
        let configured = self
            .github_protocols_unfiltered
            .as_ref()
            .unwrap_or(&self.github_protocols);
        self.github_protocols = configured
            .iter()
            .filter(|protocol| !self.secure_http || protocol.as_str() != "git")
            .cloned()
            .collect();
    }

    /// Apply environment variable overrides
    fn apply_env_overrides(&mut self, loader: &ConfigLoader) {
        // Process timeout
        if let Some(timeout) = loader.get_env_u64("process-timeout") {
            self.process_timeout = timeout;
            self.sources.insert(
                "process-timeout".to_string(),
                ConfigSource::Environment("COMPOSER_PROCESS_TIMEOUT".to_string()),
            );
        }

        // Cache directory
        if let Some(cache_dir) = loader.get_env_path("cache-dir") {
            self.cache_dir = Some(cache_dir);
            self.sources.insert(
                "cache-dir".to_string(),
                ConfigSource::Environment("COMPOSER_CACHE_DIR".to_string()),
            );
        }

        // Vendor directory
        if let Some(vendor_dir) = loader.get_env_path("vendor-dir") {
            self.vendor_dir = vendor_dir;
            self.sources.insert(
                "vendor-dir".to_string(),
                ConfigSource::Environment("COMPOSER_VENDOR_DIR".to_string()),
            );
        }

        // Bin directory
        if let Some(bin_dir) = loader.get_env_path("bin-dir") {
            self.bin_dir = bin_dir;
            self.sources.insert(
                "bin-dir".to_string(),
                ConfigSource::Environment("COMPOSER_BIN_DIR".to_string()),
            );
        }

        // Discard changes
        if let Some(discard) = loader.get_env_config("discard-changes") {
            if let Some(dc) = DiscardChanges::from_str(&discard) {
                self.discard_changes = dc;
                self.sources.insert(
                    "discard-changes".to_string(),
                    ConfigSource::Environment("COMPOSER_DISCARD_CHANGES".to_string()),
                );
            }
        }

        // Cache read-only
        if let Some(readonly) = loader.get_env_bool("cache-read-only") {
            self.cache_read_only = readonly;
            self.sources.insert(
                "cache-read-only".to_string(),
                ConfigSource::Environment("COMPOSER_CACHE_READ_ONLY".to_string()),
            );
        }

        // Htaccess protect
        if let Some(htaccess) = loader.get_env_bool("htaccess-protect") {
            self.htaccess_protect = htaccess;
            self.sources.insert(
                "htaccess-protect".to_string(),
                ConfigSource::Environment("COMPOSER_HTACCESS_PROTECT".to_string()),
            );
        }
    }

    /// Resolve computed paths (e.g., {$vendor-dir}/bin)
    fn resolve_paths(&mut self, loader: &ConfigLoader) {
        if self.home_dir.is_none() {
            self.home_dir = Some(loader.get_composer_home());
        }

        if self.cache_dir.is_none() {
            self.cache_dir = Some(loader.get_cache_dir());
        }

        if self.data_dir.is_none() {
            self.data_dir = self.home_dir.clone();
        }

        if self.cache_files_dir.is_none() {
            self.cache_files_dir = Some(self.cache_dir.as_ref().unwrap().join("files"));
        }
        if self.cache_repo_dir.is_none() {
            self.cache_repo_dir = Some(self.cache_dir.as_ref().unwrap().join("repo"));
        }
        if self.cache_vcs_dir.is_none() {
            self.cache_vcs_dir = Some(self.cache_dir.as_ref().unwrap().join("vcs"));
        }

        let bin_dir_str = self.bin_dir.to_string_lossy();
        if bin_dir_str.contains("{$vendor-dir}") {
            let vendor_dir_str = self.vendor_dir.to_string_lossy();
            let resolved = bin_dir_str.replace("{$vendor-dir}", &vendor_dir_str);
            self.bin_dir = PathBuf::from(resolved);
        }
    }

    /// Get all configuration keys
    fn config_keys(&self) -> Vec<String> {
        vec![
            "vendor-dir".to_string(),
            "bin-dir".to_string(),
            "cache-dir".to_string(),
            "data-dir".to_string(),
            "process-timeout".to_string(),
            "use-include-path".to_string(),
            "preferred-install".to_string(),
            "store-auths".to_string(),
            "notify-on-install".to_string(),
            "discard-changes".to_string(),
            "autoloader-suffix".to_string(),
            "optimize-autoloader".to_string(),
            "sort-packages".to_string(),
            "classmap-authoritative".to_string(),
            "apcu-autoloader".to_string(),
            "archive-format".to_string(),
            "archive-dir".to_string(),
            "bump-after-update".to_string(),
            "secure-http".to_string(),
            "disable-tls".to_string(),
            "lock".to_string(),
            "platform-check".to_string(),
            "allow-plugins".to_string(),
            "audit".to_string(),
            "policy".to_string(),
            "source-fallback".to_string(),
            "github-protocols".to_string(),
            "github-domains".to_string(),
            "gitlab-domains".to_string(),
        ]
    }
}

fn merge_policy_value(current: &mut serde_json::Value, incoming: serde_json::Value) {
    let incoming = if incoming == serde_json::Value::Bool(true) {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        incoming
    };
    if incoming == serde_json::Value::Bool(false) {
        *current = incoming;
        return;
    }
    let serde_json::Value::Object(incoming) = incoming else {
        return;
    };
    let mut merged = match std::mem::take(current) {
        serde_json::Value::Object(current) => current,
        _ => serde_json::Map::new(),
    };

    for (list_name, mut list_config) in incoming {
        if list_name == "ignore-unreachable" {
            merged.insert(list_name, list_config);
            continue;
        }
        if list_config == serde_json::Value::Bool(true) {
            list_config = serde_json::Value::Object(serde_json::Map::new());
        }
        if list_config == serde_json::Value::Bool(false) {
            merged.insert(list_name, list_config);
            continue;
        }

        let existing = merged.remove(&list_name).map(|existing| {
            if existing == serde_json::Value::Bool(true) {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                existing
            }
        });
        let list_config = match (existing, list_config) {
            (
                Some(serde_json::Value::Object(mut existing)),
                serde_json::Value::Object(incoming),
            ) => {
                for (key, value) in incoming {
                    let value = if matches!(
                        key.as_str(),
                        "ignore" | "ignore-id" | "ignore-severity" | "ignore-source"
                    ) {
                        match (existing.remove(&key), value) {
                            (
                                Some(serde_json::Value::Array(mut existing)),
                                serde_json::Value::Array(incoming),
                            ) => {
                                existing.extend(incoming);
                                serde_json::Value::Array(existing)
                            }
                            (_, value) => value,
                        }
                    } else {
                        value
                    };
                    existing.insert(key, value);
                }
                serde_json::Value::Object(existing)
            }
            (_, incoming) => incoming,
        };
        merged.insert(list_name, list_config);
    }
    *current = serde_json::Value::Object(merged);
}

fn merge_object_value(current: &mut serde_json::Value, incoming: serde_json::Value) {
    let serde_json::Value::Object(incoming) = incoming else {
        return;
    };
    let current = current
        .as_object_mut()
        .expect("merged config object is initialized as an object");
    for (key, value) in incoming {
        match (current.get_mut(&key), value) {
            (Some(serde_json::Value::Object(current)), serde_json::Value::Object(incoming)) => {
                let mut nested = serde_json::Value::Object(std::mem::take(current));
                merge_object_value(&mut nested, serde_json::Value::Object(incoming));
                *current = nested
                    .as_object_mut()
                    .map(std::mem::take)
                    .unwrap_or_default();
            }
            (_, value) => {
                current.insert(key, value);
            }
        }
    }
}

fn merge_preferred_install(
    current: PreferredInstall,
    incoming: PreferredInstall,
) -> PreferredInstall {
    if !matches!(current, PreferredInstall::Patterns(_))
        && !matches!(incoming, PreferredInstall::Patterns(_))
    {
        return incoming;
    }

    let mut patterns = preferred_install_patterns(current);
    for (pattern, method) in preferred_install_patterns(incoming) {
        patterns.insert(pattern, method);
    }
    if let Some(wildcard) = patterns.shift_remove("*") {
        patterns.insert("*".to_string(), wildcard);
    }
    PreferredInstall::Patterns(patterns)
}

fn preferred_install_patterns(value: PreferredInstall) -> IndexMap<String, String> {
    match value {
        PreferredInstall::Auto => IndexMap::from([("*".to_string(), "auto".to_string())]),
        PreferredInstall::Source => IndexMap::from([("*".to_string(), "source".to_string())]),
        PreferredInstall::Dist => IndexMap::from([("*".to_string(), "dist".to_string())]),
        PreferredInstall::Patterns(patterns) => patterns,
    }
}

fn normalize_repository_name(name: &str) -> &str {
    if name == "packagist" {
        "packagist.org"
    } else {
        name
    }
}

fn disabled_repository_name(repository: &serde_json::Value) -> Option<&str> {
    let repository = repository.as_object()?;
    if repository.len() != 1 {
        return None;
    }
    repository.iter().next().and_then(|(name, disabled)| {
        (disabled == &serde_json::Value::Bool(false)).then_some(name.as_str())
    })
}

fn is_packagist_repository(repository: &serde_json::Value) -> bool {
    let Some(repository) = repository.as_object() else {
        return false;
    };
    if repository.get("type").and_then(serde_json::Value::as_str) != Some("composer") {
        return false;
    }
    repository
        .get("url")
        .and_then(serde_json::Value::as_str)
        .and_then(|url| url::Url::parse(url).ok())
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| host == "packagist.org" || host.ends_with(".packagist.org"))
}

fn is_stream_wrapper(value: &str) -> bool {
    value.split_once("://").is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && scheme.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{environment_lock, EnvironmentGuard};

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.vendor_dir, PathBuf::from("vendor"));
        assert_eq!(config.bin_dir, PathBuf::from("vendor/bin"));
        assert_eq!(config.process_timeout, 300);
        assert!(config.secure_http);
        assert!(!config.disable_tls);
        assert_eq!(config.preferred_install, PreferredInstall::Dist);
        assert_eq!(config.store_auths, StoreAuths::Prompt);
    }

    #[test]
    fn test_allow_plugins_config_merge() {
        let mut config = Config::default();
        config
            .merge_config_value(
                "allow-plugins",
                serde_json::json!({"phpstan/*": true, "vendor/plugin": false}),
                ConfigSource::Project,
            )
            .unwrap();

        assert_eq!(
            config.allow_plugins,
            AllowPlugins::Map(HashMap::from([
                ("phpstan/*".to_string(), true),
                ("vendor/plugin".to_string(), false),
            ]))
        );
    }

    #[test]
    fn test_preferred_install_from_str() {
        assert_eq!(
            PreferredInstall::from_str("auto"),
            Some(PreferredInstall::Auto)
        );
        assert_eq!(
            PreferredInstall::from_str("source"),
            Some(PreferredInstall::Source)
        );
        assert_eq!(
            PreferredInstall::from_str("dist"),
            Some(PreferredInstall::Dist)
        );
        assert_eq!(PreferredInstall::from_str("invalid"), None);
    }

    #[test]
    fn test_bump_after_update_config_merge() {
        let mut config = Config::default();
        config
            .merge_config_value(
                "bump-after-update",
                serde_json::Value::Bool(true),
                ConfigSource::Project,
            )
            .unwrap();
        assert_eq!(config.bump_after_update.as_deref(), Some("all"));

        config
            .merge_config_value(
                "bump-after-update",
                serde_json::Value::String("dev".to_string()),
                ConfigSource::Project,
            )
            .unwrap();
        assert_eq!(config.bump_after_update.as_deref(), Some("dev"));

        config
            .merge_config_value(
                "bump-after-update",
                serde_json::Value::Bool(false),
                ConfigSource::Project,
            )
            .unwrap();
        assert_eq!(config.bump_after_update, None);
    }

    #[test]
    fn test_store_auths_from_str() {
        assert_eq!(StoreAuths::from_str("true"), Some(StoreAuths::True));
        assert_eq!(StoreAuths::from_str("false"), Some(StoreAuths::False));
        assert_eq!(StoreAuths::from_str("prompt"), Some(StoreAuths::Prompt));
        assert_eq!(StoreAuths::from_str("invalid"), None);
    }

    #[test]
    fn test_discard_changes_from_str() {
        assert_eq!(DiscardChanges::from_str("true"), Some(DiscardChanges::True));
        assert_eq!(
            DiscardChanges::from_str("false"),
            Some(DiscardChanges::False)
        );
        assert_eq!(
            DiscardChanges::from_str("stash"),
            Some(DiscardChanges::Stash)
        );
        assert_eq!(DiscardChanges::from_str("invalid"), None);
    }

    #[test]
    fn test_platform_check_from_str() {
        assert_eq!(
            PlatformCheck::from_str("php-only"),
            Some(PlatformCheck::PhpOnly)
        );
        assert_eq!(PlatformCheck::from_str("true"), Some(PlatformCheck::True));
        assert_eq!(PlatformCheck::from_str("false"), Some(PlatformCheck::False));
        assert_eq!(PlatformCheck::from_str("invalid"), None);
    }

    #[test]
    fn test_config_with_base_dir() {
        let config = Config::with_base_dir("/path/to/project");
        assert_eq!(config.base_dir, Some(PathBuf::from("/path/to/project")));
    }

    #[test]
    fn test_resolve_path() {
        let config = Config {
            base_dir: Some(PathBuf::from("/project")),
            ..Config::default()
        };

        let resolved = config.resolve_path(&PathBuf::from("vendor"));
        assert_eq!(resolved, PathBuf::from("/project/vendor"));

        let resolved = config.resolve_path(&PathBuf::from("/absolute/path"));
        assert_eq!(resolved, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn composer_config_preferred_install_string_is_overridden() {
        let mut config = Config::default();
        config
            .merge_config_value(
                "preferred-install",
                serde_json::json!("source"),
                ConfigSource::Global,
            )
            .unwrap();
        config
            .merge_config_value(
                "preferred-install",
                serde_json::json!("dist"),
                ConfigSource::Project,
            )
            .unwrap();
        assert_eq!(config.preferred_install, PreferredInstall::Dist);
    }

    #[test]
    fn composer_config_merges_preferred_install_patterns_with_wildcard_last() {
        let mut config = Config::default();
        config
            .merge_config_value(
                "preferred-install",
                serde_json::json!("dist"),
                ConfigSource::Global,
            )
            .unwrap();
        config
            .merge_config_value(
                "preferred-install",
                serde_json::json!({"foo/*": "source"}),
                ConfigSource::Project,
            )
            .unwrap();

        assert_eq!(
            config.preferred_install,
            PreferredInstall::Patterns(IndexMap::from([
                ("foo/*".to_string(), "source".to_string()),
                ("*".to_string(), "dist".to_string()),
            ]))
        );
    }

    #[test]
    fn composer_config_layers_repositories_and_packagist_defaults() {
        let repository = |repository_type: &str, url: &str| serde_json::json!({"type": repository_type, "url": url});
        let packagist = repository("composer", "https://repo.packagist.org");
        let merge = |system: Option<serde_json::Value>, local: serde_json::Value| {
            let mut config = Config::default();
            if let Some(system) = system {
                config
                    .merge_raw_config(
                        RawConfig {
                            repositories: Some(system),
                            ..RawConfig::default()
                        },
                        ConfigSource::Global,
                    )
                    .unwrap();
            }
            config
                .merge_raw_config(
                    RawConfig {
                        repositories: Some(local),
                        ..RawConfig::default()
                    },
                    ConfigSource::Project,
                )
                .unwrap();
            config.repositories
        };

        assert_eq!(
            merge(None, serde_json::json!([])),
            IndexMap::from([("packagist.org".to_string(), packagist.clone())])
        );
        assert!(merge(None, serde_json::json!([{"packagist.org": false}])).is_empty());
        assert!(merge(None, serde_json::json!([{"packagist": false}])).is_empty());
        assert_eq!(
            merge(
                None,
                serde_json::json!([
                    {"type": "vcs", "url": "git://github.com/composer/composer.git"},
                    {"type": "pear", "url": "http://pear.composer.org"}
                ])
            ),
            IndexMap::from([
                (
                    "0".to_string(),
                    repository("vcs", "git://github.com/composer/composer.git")
                ),
                (
                    "1".to_string(),
                    repository("pear", "http://pear.composer.org")
                ),
                ("packagist.org".to_string(), packagist.clone()),
            ])
        );

        let system = serde_json::json!({
            "example.com": {"type": "composer", "url": "http://example.com"}
        });
        let example = repository("composer", "http://example.com");
        assert_eq!(
            merge(Some(system.clone()), serde_json::json!([])),
            IndexMap::from([
                ("example.com".to_string(), example.clone()),
                ("packagist.org".to_string(), packagist.clone()),
            ])
        );
        assert_eq!(
            merge(
                Some(system.clone()),
                serde_json::json!([
                    {"packagist.org": false},
                    {"type": "composer", "url": "http://packagist.org"}
                ])
            ),
            IndexMap::from([
                (
                    "1".to_string(),
                    repository("composer", "http://packagist.org")
                ),
                ("example.com".to_string(), example.clone()),
            ])
        );
        assert_eq!(
            merge(
                Some(system),
                serde_json::json!({
                    "packagist.org": {
                        "type": "composer",
                        "url": "http://packagistnew.org"
                    }
                })
            ),
            IndexMap::from([
                (
                    "packagist.org".to_string(),
                    repository("composer", "http://packagistnew.org")
                ),
                ("example.com".to_string(), example),
            ])
        );
        assert_eq!(
            merge(
                None,
                serde_json::json!([{
                    "type": "composer",
                    "url": "https://repo.packagist.org"
                }])
            ),
            IndexMap::from([("0".to_string(), packagist.clone())])
        );
        assert_eq!(
            merge(
                None,
                serde_json::json!({
                    "example": {
                        "type": "composer",
                        "url": "https://repo.packagist.org"
                    }
                })
            ),
            IndexMap::from([("example".to_string(), packagist.clone())])
        );
        assert_eq!(
            merge(
                None,
                serde_json::json!({"type": "vcs", "url": "http://example.com"})
            ),
            IndexMap::from([
                ("packagist.org".to_string(), packagist),
                ("type".to_string(), serde_json::json!("vcs")),
                ("url".to_string(), serde_json::json!("http://example.com")),
            ])
        );
    }

    #[test]
    fn composer_config_merges_github_oauth_hosts() {
        let mut config = Config::default();
        for (source, value) in [
            (ConfigSource::Global, serde_json::json!({"foo": "bar"})),
            (ConfigSource::Project, serde_json::json!({"bar": "baz"})),
        ] {
            config
                .merge_config_value("github-oauth", value, source)
                .unwrap();
        }
        assert_eq!(
            config.github_oauth,
            HashMap::from([
                ("foo".to_string(), "bar".to_string()),
                ("bar".to_string(), "baz".to_string()),
            ])
        );
    }

    #[test]
    fn composer_config_github_protocols_are_overridden() {
        let mut config = Config::default();
        config
            .merge_config_value(
                "github-protocols",
                serde_json::json!(["https", "ssh"]),
                ConfigSource::Global,
            )
            .unwrap();
        config
            .merge_config_value(
                "github-protocols",
                serde_json::json!(["https"]),
                ConfigSource::Project,
            )
            .unwrap();
        assert_eq!(config.github_protocols, ["https"]);
    }

    #[test]
    fn composer_config_disables_insecure_git_protocol_by_default() {
        let mut config = Config::default();
        config
            .merge_config_value(
                "github-protocols",
                serde_json::json!(["https", "git"]),
                ConfigSource::Project,
            )
            .unwrap();
        assert_eq!(config.github_protocols, ["https"]);

        config
            .merge_config_value(
                "secure-http",
                serde_json::json!(false),
                ConfigSource::Project,
            )
            .unwrap();
        assert_eq!(config.github_protocols, ["https", "git"]);
    }

    #[test]
    fn composer_config_disable_tls_accepts_boolean_strings() {
        let mut config = Config::default();
        config
            .merge_config_value(
                "disable-tls",
                serde_json::json!("false"),
                ConfigSource::Global,
            )
            .unwrap();
        assert!(!config.disable_tls);
        config
            .merge_config_value(
                "disable-tls",
                serde_json::json!("true"),
                ConfigSource::Project,
            )
            .unwrap();
        assert!(config.disable_tls);
    }

    #[test]
    fn composer_config_auth_maps_default_to_empty() {
        let config = Config::default();
        assert!(config.bitbucket_oauth.is_empty());
        assert!(config.github_oauth.is_empty());
        assert!(config.gitlab_oauth.is_empty());
        assert!(config.gitlab_token.is_empty());
        assert!(config.forgejo_token.is_empty());
        assert!(config.http_basic.is_empty());
        assert!(config.bearer.is_empty());
    }

    #[test]
    fn composer_config_merges_plugin_maps() {
        let mut config = Config::default();
        config
            .merge_config_value(
                "allow-plugins",
                serde_json::json!({"some/plugin": true}),
                ConfigSource::Global,
            )
            .unwrap();
        config
            .merge_config_value(
                "allow-plugins",
                serde_json::json!({"another/plugin": true}),
                ConfigSource::Project,
            )
            .unwrap();
        assert_eq!(
            config.allow_plugins,
            AllowPlugins::Map(HashMap::from([
                ("some/plugin".to_string(), true),
                ("another/plugin".to_string(), true),
            ]))
        );
    }

    #[test]
    fn composer_config_plugin_map_overrides_global_boolean() {
        let mut config = Config::default();
        config
            .merge_config_value(
                "allow-plugins",
                serde_json::json!(true),
                ConfigSource::Global,
            )
            .unwrap();
        config
            .merge_config_value(
                "allow-plugins",
                serde_json::json!({"another/plugin": true}),
                ConfigSource::Project,
            )
            .unwrap();
        assert_eq!(
            config.allow_plugins,
            AllowPlugins::Map(HashMap::from([("another/plugin".to_string(), true)]))
        );
    }

    #[test]
    fn composer_config_local_boolean_allows_all_plugins() {
        let mut config = Config::default();
        config
            .merge_config_value(
                "allow-plugins",
                serde_json::json!({"some/plugin": true}),
                ConfigSource::Global,
            )
            .unwrap();
        config
            .merge_config_value(
                "allow-plugins",
                serde_json::json!(true),
                ConfigSource::Project,
            )
            .unwrap();
        assert_eq!(config.allow_plugins, AllowPlugins::Bool(true));
    }

    #[test]
    fn composer_config_expands_value_and_home_references() {
        let mut config = Config::default();
        for (key, value) in [
            ("a", serde_json::json!("b")),
            ("c", serde_json::json!("{$a}")),
            ("bin-dir", serde_json::json!("$HOME")),
            ("cache-dir", serde_json::json!("~/foo/")),
        ] {
            config
                .merge_config_value(key, value, ConfigSource::Project)
                .unwrap();
        }

        let home = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())
            .unwrap();
        assert_eq!(config.get_string("c").as_deref(), Some("b"));
        assert_eq!(config.get_bin_dir(), PathBuf::from(&home));
        assert_eq!(
            config.get_cache_dir(&ConfigLoader::new(false)),
            PathBuf::from(home).join("foo")
        );
    }

    #[test]
    fn composer_config_resolves_and_trims_directory_paths() {
        let mut config = Config::with_base_dir("/foo/bar");
        for (key, value) in [
            ("bin-dir", serde_json::json!("$HOME/foo")),
            ("cache-dir", serde_json::json!("/baz/")),
            ("vendor-dir", serde_json::json!("vendor")),
        ] {
            config
                .merge_config_value(key, value, ConfigSource::Project)
                .unwrap();
        }
        let home = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())
            .unwrap();

        assert_eq!(config.get_vendor_dir(), PathBuf::from("/foo/bar/vendor"));
        assert_eq!(config.get_bin_dir(), PathBuf::from(home).join("foo"));
        assert_eq!(
            config.get_cache_dir(&ConfigLoader::new(false)),
            PathBuf::from("/baz")
        );
    }

    #[test]
    fn composer_config_preserves_stream_wrapper_directories() {
        let mut config = Config::with_base_dir("/foo/bar");
        config
            .merge_config_value(
                "cache-dir",
                serde_json::json!("s3://baz/"),
                ConfigSource::Project,
            )
            .unwrap();

        assert_eq!(
            config.get_cache_dir(&ConfigLoader::new(false)),
            PathBuf::from("s3://baz")
        );
    }

    #[test]
    fn composer_config_can_fetch_relative_directory_paths() {
        let mut config = Config::with_base_dir("/foo/bar");
        config
            .merge_config_value(
                "bin-dir",
                serde_json::json!("{$vendor-dir}/foo"),
                ConfigSource::Project,
            )
            .unwrap();
        config
            .merge_config_value(
                "vendor-dir",
                serde_json::json!("vendor"),
                ConfigSource::Project,
            )
            .unwrap();

        assert_eq!(config.get_vendor_dir(), PathBuf::from("/foo/bar/vendor"));
        assert_eq!(config.get_bin_dir(), PathBuf::from("/foo/bar/vendor/foo"));
        assert_eq!(config.get_vendor_dir_relative(), PathBuf::from("vendor"));
        assert_eq!(config.get_bin_dir_relative(), PathBuf::from("vendor/foo"));
    }

    #[test]
    fn composer_config_allows_secure_and_custom_urls() {
        let config = Config::default();
        for url in [
            "https://packagist.org",
            "git@github.com:composer/composer.git",
            "hg://user:pass@my.satis/satis",
            r"\myserver\myplace.git",
            "file://myserver.localhost/mygit.git",
            "file://example.org/mygit.git",
            "git:Department/Repo.git",
            "ssh://[user@]host.xz[:port]/path/to/repo.git/",
        ] {
            config.prohibit_url_by_config(url).unwrap();
        }
    }

    #[test]
    fn composer_config_prohibits_known_insecure_urls() {
        let config = Config::default();
        for url in [
            "http://packagist.org",
            "http://10.1.0.1/satis",
            "http://127.0.0.1/satis",
            "http://💛@example.org",
            "svn://localhost/trunk",
            "svn://will.not.resolve/trunk",
            "svn://192.168.0.1/trunk",
            "svn://1.2.3.4/trunk",
            "git://5.6.7.8/git.git",
        ] {
            let error = config.prohibit_url_by_config(url).unwrap_err();
            assert!(error.to_string().contains(&format!(
                "configuration does not allow connections to {url}"
            )));
        }
    }

    #[test]
    fn composer_config_warns_when_tls_peer_verification_is_disabled() {
        let warnings = Config::default().repository_url_warnings(
            "https://example.org",
            Some(false),
            Some(false),
        );
        assert_eq!(
            warnings,
            ["Warning: Accessing example.org with verify_peer and verify_peer_name disabled."]
        );
    }

    // Ported from Composer\Test\FactoryTest::testDefaultValuesAreAsExpected.
    #[test]
    fn composer_factory_warns_before_using_disabled_tls_protection() {
        let config = Config {
            disable_tls: true,
            ..Config::default()
        };
        assert_eq!(
            config.tls_protection_warning(),
            Some("You are running Riff with SSL/TLS protection disabled.")
        );
    }

    #[test]
    fn composer_config_process_timeout_uses_environment_override() {
        let _lock = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_PROCESS_TIMEOUT", Some("0"));
        let mut config = Config::default();
        config.apply_env_overrides(&ConfigLoader::new(true));

        assert_eq!(config.process_timeout, 0);
    }

    #[test]
    fn composer_config_htaccess_protect_uses_environment_override() {
        let _lock = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_HTACCESS_PROTECT", Some("0"));
        let mut config = Config::default();
        config.apply_env_overrides(&ConfigLoader::new(true));

        assert!(!config.htaccess_protect);
    }

    #[test]
    fn composer_config_tracks_value_sources() {
        let mut config = Config::default();
        assert_eq!(
            config.get_source("process-timeout"),
            Some(&ConfigSource::Default)
        );

        config
            .merge_config_value(
                "process-timeout",
                serde_json::json!(1),
                ConfigSource::Named("phpunit-test".to_string()),
            )
            .unwrap();
        assert_eq!(
            config
                .get_source("process-timeout")
                .map(ConfigSource::as_str),
            Some("phpunit-test")
        );
    }

    #[test]
    fn composer_config_tracks_environment_value_sources() {
        let _lock = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_HTACCESS_PROTECT", Some("0"));
        let mut config = Config::default();
        config.apply_env_overrides(&ConfigLoader::new(true));

        assert_eq!(
            config
                .get_source("htaccess-protect")
                .map(ConfigSource::as_str),
            Some("COMPOSER_HTACCESS_PROTECT")
        );
    }

    #[test]
    fn composer_config_merges_audit_settings_and_environment_overrides() {
        let _lock = environment_lock();
        let _audit_abandoned = EnvironmentGuard::set("COMPOSER_AUDIT_ABANDONED", None);
        let _block_abandoned = EnvironmentGuard::set("COMPOSER_SECURITY_BLOCKING_ABANDONED", None);
        let mut config = Config::default();
        assert_eq!(config.audit.abandoned, "fail");
        assert!(config.audit.ignore.is_empty());

        std::env::set_var("COMPOSER_AUDIT_ABANDONED", "ignore");
        assert_eq!(
            config
                .audit_with_environment(&ConfigLoader::new(true))
                .unwrap()
                .abandoned,
            "ignore"
        );
        std::env::remove_var("COMPOSER_AUDIT_ABANDONED");

        config
            .merge_config_value(
                "audit",
                serde_json::json!({"ignore": ["A", "B"]}),
                ConfigSource::Global,
            )
            .unwrap();
        config
            .merge_config_value(
                "audit",
                serde_json::json!({"ignore": ["A", "C"]}),
                ConfigSource::Project,
            )
            .unwrap();
        assert_eq!(config.audit.ignore, ["A", "B", "A", "C"]);

        std::env::set_var("COMPOSER_SECURITY_BLOCKING_ABANDONED", "1");
        assert_eq!(
            config
                .audit_with_environment(&ConfigLoader::new(true))
                .unwrap()
                .block_abandoned,
            Some(true)
        );
        std::env::set_var("COMPOSER_SECURITY_BLOCKING_ABANDONED", "0");
        assert_eq!(
            config
                .audit_with_environment(&ConfigLoader::new(true))
                .unwrap()
                .block_abandoned,
            Some(false)
        );
    }

    #[test]
    fn composer_config_merges_dependency_policy_and_master_switch() {
        let _lock = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_POLICY", None);
        let mut config = Config::default();
        assert_eq!(config.policy, serde_json::json!(true));

        config
            .merge_config_value(
                "policy",
                serde_json::json!({"advisories": {"ignore": ["acme/package"]}}),
                ConfigSource::Global,
            )
            .unwrap();
        config
            .merge_config_value(
                "policy",
                serde_json::json!({"advisories": {"ignore-severities": ["low"]}}),
                ConfigSource::Project,
            )
            .unwrap();
        assert_eq!(
            config.policy["advisories"],
            serde_json::json!({
                "ignore": ["acme/package"],
                "ignore-severities": ["low"]
            })
        );

        std::env::set_var("COMPOSER_POLICY", "1");
        assert_eq!(
            config.policy_with_environment(&ConfigLoader::new(true)),
            config.policy
        );
        std::env::remove_var("COMPOSER_POLICY");
        config
            .merge_config_value("policy", serde_json::json!(true), ConfigSource::Project)
            .unwrap();
        assert!(config.policy.is_object());
        config
            .merge_config_value("policy", serde_json::json!(false), ConfigSource::Project)
            .unwrap();
        assert_eq!(config.policy, serde_json::json!(false));

        std::env::set_var("COMPOSER_POLICY", "1");
        assert_eq!(
            config.policy_with_environment(&ConfigLoader::new(true)),
            serde_json::json!(true)
        );
        std::env::set_var("COMPOSER_POLICY", "0");
        assert_eq!(
            config.policy_with_environment(&ConfigLoader::new(true)),
            serde_json::json!(false)
        );
        std::env::remove_var("COMPOSER_POLICY");
        config
            .merge_config_value("policy", serde_json::json!(true), ConfigSource::Project)
            .unwrap();
        assert_eq!(config.policy, serde_json::json!({}));
    }

    #[test]
    fn composer_config_policy_true_and_empty_object_layer_equally() {
        let mut from_true = Config {
            policy: serde_json::json!({}),
            ..Config::default()
        };
        from_true
            .merge_config_value(
                "policy",
                serde_json::json!({"advisories": true}),
                ConfigSource::Global,
            )
            .unwrap();
        from_true
            .merge_config_value(
                "policy",
                serde_json::json!({"advisories": {"audit": "report"}}),
                ConfigSource::Project,
            )
            .unwrap();

        let mut from_empty = Config {
            policy: serde_json::json!({}),
            ..Config::default()
        };
        from_empty
            .merge_config_value(
                "policy",
                serde_json::json!({"advisories": {}}),
                ConfigSource::Global,
            )
            .unwrap();
        from_empty
            .merge_config_value(
                "policy",
                serde_json::json!({"advisories": {"audit": "report"}}),
                ConfigSource::Project,
            )
            .unwrap();

        assert_eq!(from_true.policy, from_empty.policy);
    }

    #[test]
    fn composer_config_policy_false_overrides_true_and_empty_equally() {
        let mut from_true = Config {
            policy: serde_json::json!({}),
            ..Config::default()
        };
        from_true
            .merge_config_value(
                "policy",
                serde_json::json!({"advisories": true}),
                ConfigSource::Global,
            )
            .unwrap();
        from_true
            .merge_config_value(
                "policy",
                serde_json::json!({"advisories": false}),
                ConfigSource::Project,
            )
            .unwrap();

        let mut from_empty = Config {
            policy: serde_json::json!({}),
            ..Config::default()
        };
        from_empty
            .merge_config_value(
                "policy",
                serde_json::json!({"advisories": {}}),
                ConfigSource::Global,
            )
            .unwrap();
        from_empty
            .merge_config_value(
                "policy",
                serde_json::json!({"advisories": false}),
                ConfigSource::Project,
            )
            .unwrap();

        assert_eq!(from_true.policy, from_empty.policy);
        assert_eq!(from_true.policy["advisories"], serde_json::json!(false));
    }

    #[test]
    fn composer_config_policy_master_true_preserves_details() {
        let mut config = Config {
            policy: serde_json::json!({}),
            ..Config::default()
        };
        config
            .merge_config_value(
                "policy",
                serde_json::json!({"advisories": {"block": false}}),
                ConfigSource::Global,
            )
            .unwrap();
        config
            .merge_config_value("policy", serde_json::json!(true), ConfigSource::Project)
            .unwrap();

        assert_eq!(
            config.policy["advisories"],
            serde_json::json!({"block": false})
        );
    }

    #[test]
    fn composer_config_policy_deep_merges_advisory_ignore_lists() {
        let mut config = Config::default();
        config
            .merge_config_value(
                "policy",
                serde_json::json!({"advisories": {
                    "ignore": ["vendor/global-1", "vendor/global-2"],
                    "ignore-id": ["CVE-1111"],
                    "ignore-severity": ["low"],
                    "block": true
                }}),
                ConfigSource::Global,
            )
            .unwrap();
        config
            .merge_config_value(
                "policy",
                serde_json::json!({"advisories": {
                    "ignore": ["vendor/project-1"],
                    "ignore-id": ["CVE-2222"],
                    "ignore-severity": ["medium"],
                    "audit": "report"
                }}),
                ConfigSource::Project,
            )
            .unwrap();

        let advisories = &config.policy["advisories"];
        assert_eq!(
            advisories["ignore"],
            serde_json::json!(["vendor/global-1", "vendor/global-2", "vendor/project-1"])
        );
        assert_eq!(
            advisories["ignore-id"],
            serde_json::json!(["CVE-1111", "CVE-2222"])
        );
        assert_eq!(
            advisories["ignore-severity"],
            serde_json::json!(["low", "medium"])
        );
        assert_eq!(advisories["block"], serde_json::json!(true));
        assert_eq!(advisories["audit"], serde_json::json!("report"));
    }

    #[test]
    fn composer_config_policy_deep_merges_all_list_types() {
        let mut config = Config::default();
        config
            .merge_config_value(
                "policy",
                serde_json::json!({
                    "malware": {
                        "ignore": ["vendor/global-malware"],
                        "ignore-source": ["source-global"]
                    },
                    "abandoned": {"ignore": ["vendor/global-abandoned"]},
                    "custom-list": {"ignore": ["vendor/global-custom"]}
                }),
                ConfigSource::Global,
            )
            .unwrap();
        config
            .merge_config_value(
                "policy",
                serde_json::json!({
                    "malware": {
                        "ignore": ["vendor/project-malware"],
                        "ignore-source": ["source-project"]
                    },
                    "abandoned": {"ignore": ["vendor/project-abandoned"]},
                    "custom-list": {"ignore": ["vendor/project-custom"]}
                }),
                ConfigSource::Project,
            )
            .unwrap();

        assert_eq!(
            config.policy["malware"]["ignore"],
            serde_json::json!(["vendor/global-malware", "vendor/project-malware"])
        );
        assert_eq!(
            config.policy["malware"]["ignore-source"],
            serde_json::json!(["source-global", "source-project"])
        );
        assert_eq!(
            config.policy["abandoned"]["ignore"],
            serde_json::json!(["vendor/global-abandoned", "vendor/project-abandoned"])
        );
        assert_eq!(
            config.policy["custom-list"]["ignore"],
            serde_json::json!(["vendor/global-custom", "vendor/project-custom"])
        );
    }

    #[test]
    fn composer_config_source_fallback_defaults_to_false() {
        assert!(!Config::default().source_fallback);
    }

    #[test]
    fn composer_config_source_fallback_can_be_disabled() {
        let mut config = Config::default();
        config
            .merge_config_value(
                "source-fallback",
                serde_json::json!(false),
                ConfigSource::Project,
            )
            .unwrap();
        assert!(!config.source_fallback);
    }

    #[test]
    fn composer_config_source_fallback_accepts_boolean_strings() {
        let mut config = Config::default();
        config
            .merge_config_value(
                "source-fallback",
                serde_json::json!("false"),
                ConfigSource::Global,
            )
            .unwrap();
        assert!(!config.source_fallback);
        config
            .merge_config_value(
                "source-fallback",
                serde_json::json!("true"),
                ConfigSource::Project,
            )
            .unwrap();
        assert!(config.source_fallback);
    }

    #[test]
    fn composer_auth_config_rejects_non_object_github_oauth() {
        let error = Config::default()
            .merge_config_value(
                "github-oauth",
                serde_json::json!("foo"),
                ConfigSource::Project,
            )
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Configuration error: github-oauth must be an object"
        );
    }
}
