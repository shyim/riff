//! Authentication configuration for Composer
//!
//! This module handles loading and managing authentication credentials from:
//! - `~/.composer/auth.json` (global)
//! - `./auth.json` (project-local)
//! - Environment variables (COMPOSER_AUTH)
//!
//! # auth.json format
//!
//! ```json
//! {
//!     "http-basic": {
//!         "example.org": {
//!             "username": "user",
//!             "password": "pass"
//!         }
//!     },
//!     "github-oauth": {
//!         "github.com": "token"
//!     },
//!     "gitlab-oauth": {
//!         "gitlab.com": "token"
//!     },
//!     "gitlab-token": {
//!         "gitlab.com": "token"
//!     },
//!     "bitbucket-oauth": {
//!         "bitbucket.org": {
//!             "consumer-key": "key",
//!             "consumer-secret": "secret"
//!         }
//!     },
//!     "bearer": {
//!         "example.org": "token"
//!     }
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::source::ConfigLoader;
use super::StoreAuths;
use crate::error::{Result, RiffError};

/// HTTP Basic authentication credentials
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HttpBasicCredentials {
    pub username: String,
    pub password: String,
}

/// GitLab token authentication (can be simple token or oauth token)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum GitLabAuth {
    /// Simple private token
    Token(String),
    /// OAuth token with explicit key
    OAuth {
        #[serde(rename = "oauth-token")]
        oauth_token: String,
    },
}

impl GitLabAuth {
    /// Get the token string regardless of format
    pub fn token(&self) -> &str {
        match self {
            GitLabAuth::Token(t) => t,
            GitLabAuth::OAuth { oauth_token } => oauth_token,
        }
    }
}

/// Bitbucket OAuth credentials
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BitbucketOAuthCredentials {
    #[serde(rename = "consumer-key")]
    pub consumer_key: String,
    #[serde(rename = "consumer-secret")]
    pub consumer_secret: String,
}

/// Complete authentication configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthConfig {
    /// HTTP Basic authentication by domain
    #[serde(
        rename = "http-basic",
        default,
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub http_basic: HashMap<String, HttpBasicCredentials>,

    /// Bearer token authentication by domain
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub bearer: HashMap<String, String>,

    /// GitHub OAuth tokens by domain
    #[serde(
        rename = "github-oauth",
        default,
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub github_oauth: HashMap<String, String>,

    /// GitLab OAuth tokens by domain
    #[serde(
        rename = "gitlab-oauth",
        default,
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub gitlab_oauth: HashMap<String, String>,

    /// GitLab private tokens by domain
    #[serde(
        rename = "gitlab-token",
        default,
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub gitlab_token: HashMap<String, GitLabAuth>,

    /// Bitbucket OAuth credentials by domain
    #[serde(
        rename = "bitbucket-oauth",
        default,
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub bitbucket_oauth: HashMap<String, BitbucketOAuthCredentials>,
}

impl AuthConfig {
    /// Create a new empty auth config
    pub fn new() -> Self {
        Self::default()
    }

    /// Load auth config from a file
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(path)
            .map_err(|e| RiffError::Config(format!("Failed to read {}: {}", path.display(), e)))?;

        Self::from_json(&contents)
    }

    /// Parse auth config from JSON string
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json)
            .map_err(|e| RiffError::Config(format!("Failed to parse auth.json: {}", e)))
    }

    /// Load auth config from the COMPOSER_AUTH environment variable
    pub fn from_env() -> Result<Option<Self>> {
        match std::env::var("COMPOSER_AUTH") {
            Ok(json) if !json.is_empty() => {
                let config = Self::from_json(&json)?;
                Ok(Some(config))
            }
            _ => Ok(None),
        }
    }

    /// Build complete auth config from all sources
    ///
    /// Priority (highest to lowest):
    /// 1. COMPOSER_AUTH environment variable
    /// 2. Project auth.json (./auth.json)
    /// 3. Global auth.json (~/.composer/auth.json)
    pub fn build<P: AsRef<Path>>(project_dir: Option<P>) -> Result<Self> {
        let loader = ConfigLoader::new(true);
        let mut config = Self::default();

        // 1. Load global auth.json
        let global_auth_path = loader.get_composer_home().join("auth.json");
        if global_auth_path.exists() {
            let global = Self::from_file(&global_auth_path)?;
            config.merge(global);
        }

        // 2. Load project auth.json
        if let Some(project_dir) = project_dir {
            let project_auth_path = project_dir.as_ref().join("auth.json");
            if project_auth_path.exists() {
                let project = Self::from_file(&project_auth_path)?;
                config.merge(project);
            }
        }

        // 3. Load from COMPOSER_AUTH env var (highest priority)
        if let Some(env_config) = Self::from_env()? {
            config.merge(env_config);
        }

        Ok(config)
    }

    /// Merge another auth config into this one (other takes precedence)
    pub fn merge(&mut self, other: AuthConfig) {
        for (domain, creds) in other.http_basic {
            self.http_basic.insert(domain, creds);
        }
        for (domain, token) in other.bearer {
            self.bearer.insert(domain, token);
        }
        for (domain, token) in other.github_oauth {
            self.github_oauth.insert(domain, token);
        }
        for (domain, token) in other.gitlab_oauth {
            self.gitlab_oauth.insert(domain, token);
        }
        for (domain, token) in other.gitlab_token {
            self.gitlab_token.insert(domain, token);
        }
        for (domain, creds) in other.bitbucket_oauth {
            self.bitbucket_oauth.insert(domain, creds);
        }
    }

    /// Save auth config to a file
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();

        // Create parent directory if it doesn't exist
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .map_err(|e| RiffError::Config(format!("Failed to create directory: {}", e)))?;
        }

        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
            RiffError::Config(format!(
                "Failed to create a temporary authentication file in {}: {error}",
                parent.display()
            ))
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| {
                    RiffError::Config(format!(
                        "Failed to secure authentication file {}: {error}",
                        path.display()
                    ))
                })?;
        }

        serde_json::to_writer_pretty(temporary.as_file_mut(), self).map_err(|error| {
            RiffError::Config(format!("Failed to serialize auth config: {error}"))
        })?;
        temporary.as_file_mut().write_all(b"\n").map_err(|error| {
            RiffError::Config(format!(
                "Failed to write authentication file {}: {error}",
                path.display()
            ))
        })?;
        temporary.as_file_mut().sync_all().map_err(|error| {
            RiffError::Config(format!(
                "Failed to flush authentication file {}: {error}",
                path.display()
            ))
        })?;
        temporary.persist(path).map_err(|error| {
            RiffError::Config(format!(
                "Failed to replace authentication file {}: {}",
                path.display(),
                error.error
            ))
        })?;

        Ok(())
    }

    /// Resolve Composer's credential-storage policy without ever prompting.
    ///
    /// A caller with an interactive frontend may pass the user's answer for
    /// `Prompt`. A non-interactive caller passes `None`, which always declines
    /// storage so a background download can never persist secrets implicitly.
    pub fn should_store(policy: &StoreAuths, prompt_answer: Option<&str>) -> Result<bool> {
        match policy {
            StoreAuths::True => Ok(true),
            StoreAuths::False => Ok(false),
            StoreAuths::Prompt => {
                let Some(answer) = prompt_answer else {
                    return Ok(false);
                };
                match answer
                    .trim()
                    .chars()
                    .next()
                    .map(|value| value.to_ascii_lowercase())
                {
                    Some('y') | None => Ok(true),
                    Some('n') => Ok(false),
                    _ => Err(RiffError::Config(
                        "Please answer (y)es or (n)o when storing credentials".to_string(),
                    )),
                }
            }
        }
    }

    /// Persist HTTP basic credentials according to the configured storage
    /// policy, preserving all credentials already present in the auth file.
    pub fn store_http_basic<P: AsRef<Path>>(
        path: P,
        origin: &str,
        credentials: &HttpBasicCredentials,
        policy: &StoreAuths,
        prompt_answer: Option<&str>,
    ) -> Result<bool> {
        if !Self::should_store(policy, prompt_answer)? {
            return Ok(false);
        }

        let origin = normalize_origin(origin);
        if origin.is_empty() {
            return Err(RiffError::Config(
                "Cannot store authentication for an empty origin".to_string(),
            ));
        }

        let path = path.as_ref();
        let mut config = Self::from_file(path)?;
        config.http_basic.insert(origin, credentials.clone());
        config.save(path)?;
        Ok(true)
    }

    /// Get the global auth.json path
    pub fn global_path() -> PathBuf {
        let loader = ConfigLoader::new(true);
        loader.get_composer_home().join("auth.json")
    }

    /// Check if config is empty (no credentials stored)
    pub fn is_empty(&self) -> bool {
        self.http_basic.is_empty()
            && self.bearer.is_empty()
            && self.github_oauth.is_empty()
            && self.gitlab_oauth.is_empty()
            && self.gitlab_token.is_empty()
            && self.bitbucket_oauth.is_empty()
    }

    // ============ Lookup Methods ============

    /// Get HTTP Basic credentials for a domain
    pub fn get_http_basic(&self, domain: &str) -> Option<&HttpBasicCredentials> {
        self.http_basic.get(domain)
    }

    /// Get bearer token for a domain
    pub fn get_bearer(&self, domain: &str) -> Option<&str> {
        self.bearer.get(domain).map(|s| s.as_str())
    }

    /// Get GitHub OAuth token for a domain
    pub fn get_github_oauth(&self, domain: &str) -> Option<&str> {
        self.github_oauth.get(domain).map(|s| s.as_str())
    }

    /// Get GitLab OAuth token for a domain
    pub fn get_gitlab_oauth(&self, domain: &str) -> Option<&str> {
        self.gitlab_oauth.get(domain).map(|s| s.as_str())
    }

    /// Get GitLab token for a domain (either private token or oauth)
    pub fn get_gitlab_token(&self, domain: &str) -> Option<&str> {
        // First check gitlab-token, then fall back to gitlab-oauth
        if let Some(auth) = self.gitlab_token.get(domain) {
            return Some(auth.token());
        }
        self.gitlab_oauth.get(domain).map(|s| s.as_str())
    }

    /// Get Bitbucket OAuth credentials for a domain
    pub fn get_bitbucket_oauth(&self, domain: &str) -> Option<&BitbucketOAuthCredentials> {
        self.bitbucket_oauth.get(domain)
    }

    /// Whether any supported authentication kind is configured for an origin.
    pub fn has_authentication(&self, origin: &str) -> bool {
        let origin = normalize_origin(origin);
        self.http_basic.contains_key(&origin)
            || self.bearer.contains_key(&origin)
            || self.github_oauth.contains_key(&origin)
            || self.gitlab_oauth.contains_key(&origin)
            || self.gitlab_token.contains_key(&origin)
            || self.bitbucket_oauth.contains_key(&origin)
    }

    /// Find the credential origin to use for a request origin.
    ///
    /// Credentials are exact-origin scoped, except for Composer's explicit
    /// API aliases for GitHub and Bitbucket. In particular, GitLab credentials
    /// are never inherited by an API-looking host with a different origin.
    pub fn find_auth_origin(&self, request_origin: &str) -> Option<String> {
        let origin = normalize_origin(request_origin);
        if self.has_authentication(&origin) {
            return Some(origin);
        }

        let canonical = match origin.as_str() {
            "api.github.com" => Some("github.com"),
            "api.bitbucket.org" => Some("bitbucket.org"),
            _ => None,
        }?;
        self.has_authentication(canonical)
            .then(|| canonical.to_string())
    }

    // ============ Setter Methods ============

    /// Set HTTP Basic credentials for a domain
    pub fn set_http_basic(
        &mut self,
        domain: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) {
        self.http_basic.insert(
            domain.into(),
            HttpBasicCredentials {
                username: username.into(),
                password: password.into(),
            },
        );
    }

    /// Set bearer token for a domain
    pub fn set_bearer(&mut self, domain: impl Into<String>, token: impl Into<String>) {
        self.bearer.insert(domain.into(), token.into());
    }

    /// Set GitHub OAuth token for a domain
    pub fn set_github_oauth(&mut self, domain: impl Into<String>, token: impl Into<String>) {
        self.github_oauth.insert(domain.into(), token.into());
    }

    /// Set GitLab OAuth token for a domain
    pub fn set_gitlab_oauth(&mut self, domain: impl Into<String>, token: impl Into<String>) {
        self.gitlab_oauth.insert(domain.into(), token.into());
    }

    /// Set GitLab private token for a domain
    pub fn set_gitlab_token(&mut self, domain: impl Into<String>, token: impl Into<String>) {
        self.gitlab_token
            .insert(domain.into(), GitLabAuth::Token(token.into()));
    }

    /// Set Bitbucket OAuth credentials for a domain
    pub fn set_bitbucket_oauth(
        &mut self,
        domain: impl Into<String>,
        consumer_key: impl Into<String>,
        consumer_secret: impl Into<String>,
    ) {
        self.bitbucket_oauth.insert(
            domain.into(),
            BitbucketOAuthCredentials {
                consumer_key: consumer_key.into(),
                consumer_secret: consumer_secret.into(),
            },
        );
    }

    // ============ Remove Methods ============

    /// Remove HTTP Basic credentials for a domain
    pub fn remove_http_basic(&mut self, domain: &str) -> Option<HttpBasicCredentials> {
        self.http_basic.remove(domain)
    }

    /// Remove bearer token for a domain
    pub fn remove_bearer(&mut self, domain: &str) -> Option<String> {
        self.bearer.remove(domain)
    }

    /// Remove GitHub OAuth token for a domain
    pub fn remove_github_oauth(&mut self, domain: &str) -> Option<String> {
        self.github_oauth.remove(domain)
    }

    /// Remove GitLab OAuth token for a domain
    pub fn remove_gitlab_oauth(&mut self, domain: &str) -> Option<String> {
        self.gitlab_oauth.remove(domain)
    }

    /// Remove GitLab token for a domain
    pub fn remove_gitlab_token(&mut self, domain: &str) -> Option<GitLabAuth> {
        self.gitlab_token.remove(domain)
    }

    /// Remove Bitbucket OAuth credentials for a domain
    pub fn remove_bitbucket_oauth(&mut self, domain: &str) -> Option<BitbucketOAuthCredentials> {
        self.bitbucket_oauth.remove(domain)
    }

    // ============ Domain Matching ============

    /// Find credentials for a URL by extracting and matching the domain
    pub fn find_for_url(&self, url: &str) -> AuthMatch<'_> {
        let request_origin = extract_domain(url);
        let Some(domain) = self.find_auth_origin(&request_origin) else {
            return AuthMatch::None;
        };

        if let Some(creds) = self.get_http_basic(&domain) {
            return AuthMatch::HttpBasic(creds);
        }

        if let Some(token) = self.get_bearer(&domain) {
            return AuthMatch::Bearer(token);
        }

        if let Some(token) = self.get_github_oauth(&domain) {
            return AuthMatch::GitHubOAuth(token);
        }

        if let Some(token) = self.get_gitlab_token(&domain) {
            return AuthMatch::GitLabToken(token);
        }

        if let Some(creds) = self.get_bitbucket_oauth(&domain) {
            return AuthMatch::BitbucketOAuth(creds);
        }

        AuthMatch::None
    }
}

/// Result of looking up authentication for a URL
#[derive(Debug, Clone)]
pub enum AuthMatch<'a> {
    /// No authentication found
    None,
    /// HTTP Basic authentication
    HttpBasic(&'a HttpBasicCredentials),
    /// Bearer token
    Bearer(&'a str),
    /// GitHub OAuth token
    GitHubOAuth(&'a str),
    /// GitLab token (private or oauth)
    GitLabToken(&'a str),
    /// Bitbucket OAuth credentials
    BitbucketOAuth(&'a BitbucketOAuthCredentials),
}

impl<'a> AuthMatch<'a> {
    /// Check if authentication was found
    pub fn is_some(&self) -> bool {
        !matches!(self, AuthMatch::None)
    }

    /// Check if no authentication was found
    pub fn is_none(&self) -> bool {
        matches!(self, AuthMatch::None)
    }
}

/// Extract domain from a URL
fn extract_domain(url: &str) -> String {
    // Handle git@ style URLs
    if url.starts_with("git@") {
        if let Some(host) = url.strip_prefix("git@") {
            if let Some(colon_pos) = host.find(':') {
                return host[..colon_pos].to_lowercase();
            }
        }
    }

    // Handle standard URLs
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            return host.to_lowercase();
        }
    }

    // Fallback: try to extract from common patterns
    let url = url.to_lowercase();
    if let Some(start) = url.find("://") {
        let rest = &url[start + 3..];
        if let Some(end) = rest.find('/') {
            return rest[..end].to_string();
        }
        return rest.to_string();
    }

    url
}

fn normalize_origin(origin: &str) -> String {
    extract_domain(origin.trim())
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{environment_lock, EnvironmentGuard};

    #[test]
    fn test_auth_config_default() {
        let config = AuthConfig::new();
        assert!(config.is_empty());
    }

    #[test]
    fn test_auth_config_from_json() {
        let json = r#"{
            "http-basic": {
                "example.org": {
                    "username": "user",
                    "password": "pass"
                }
            },
            "github-oauth": {
                "github.com": "ghp_token123"
            },
            "gitlab-token": {
                "gitlab.com": "glpat-token123"
            },
            "bitbucket-oauth": {
                "bitbucket.org": {
                    "consumer-key": "key123",
                    "consumer-secret": "secret456"
                }
            },
            "bearer": {
                "private.repo.org": "bearer_token"
            }
        }"#;

        let config = AuthConfig::from_json(json).unwrap();

        assert!(!config.is_empty());

        let basic = config.get_http_basic("example.org").unwrap();
        assert_eq!(basic.username, "user");
        assert_eq!(basic.password, "pass");

        assert_eq!(config.get_github_oauth("github.com"), Some("ghp_token123"));
        assert_eq!(
            config.get_gitlab_token("gitlab.com"),
            Some("glpat-token123")
        );
        assert_eq!(config.get_bearer("private.repo.org"), Some("bearer_token"));

        let bb = config.get_bitbucket_oauth("bitbucket.org").unwrap();
        assert_eq!(bb.consumer_key, "key123");
        assert_eq!(bb.consumer_secret, "secret456");
    }

    #[test]
    fn test_gitlab_token_formats() {
        // Simple token format
        let json1 = r#"{
            "gitlab-token": {
                "gitlab.com": "simple_token"
            }
        }"#;
        let config1 = AuthConfig::from_json(json1).unwrap();
        assert_eq!(config1.get_gitlab_token("gitlab.com"), Some("simple_token"));

        // OAuth token format
        let json2 = r#"{
            "gitlab-token": {
                "gitlab.com": {
                    "oauth-token": "oauth_token"
                }
            }
        }"#;
        let config2 = AuthConfig::from_json(json2).unwrap();
        assert_eq!(config2.get_gitlab_token("gitlab.com"), Some("oauth_token"));
    }

    #[test]
    fn test_auth_config_merge() {
        let mut config1 = AuthConfig::new();
        config1.set_github_oauth("github.com", "token1");
        config1.set_http_basic("example.org", "user1", "pass1");

        let mut config2 = AuthConfig::new();
        config2.set_github_oauth("github.com", "token2"); // Should override
        config2.set_gitlab_oauth("gitlab.com", "gitlab_token");

        config1.merge(config2);

        assert_eq!(config1.get_github_oauth("github.com"), Some("token2"));
        assert_eq!(config1.get_gitlab_oauth("gitlab.com"), Some("gitlab_token"));
        assert!(config1.get_http_basic("example.org").is_some());
    }

    #[test]
    fn test_extract_domain() {
        assert_eq!(
            extract_domain("https://github.com/owner/repo"),
            "github.com"
        );
        assert_eq!(
            extract_domain("https://api.github.com/repos/owner/repo"),
            "api.github.com"
        );
        assert_eq!(
            extract_domain("git@github.com:owner/repo.git"),
            "github.com"
        );
        assert_eq!(
            extract_domain("https://gitlab.example.com/group/repo"),
            "gitlab.example.com"
        );
        assert_eq!(
            extract_domain("https://bitbucket.org/owner/repo"),
            "bitbucket.org"
        );
    }

    #[test]
    fn test_find_for_url() {
        let mut config = AuthConfig::new();
        config.set_github_oauth("github.com", "gh_token");
        config.set_gitlab_token("gitlab.com", "gl_token");
        config.set_http_basic("private.example.org", "user", "pass");

        // GitHub URL
        let auth = config.find_for_url("https://github.com/owner/repo");
        assert!(matches!(auth, AuthMatch::GitHubOAuth("gh_token")));

        // GitLab URL
        let auth = config.find_for_url("https://gitlab.com/owner/repo");
        assert!(matches!(auth, AuthMatch::GitLabToken("gl_token")));

        // HTTP Basic URL
        let auth = config.find_for_url("https://private.example.org/packages.json");
        assert!(matches!(auth, AuthMatch::HttpBasic(_)));

        // Unknown URL
        let auth = config.find_for_url("https://unknown.org/repo");
        assert!(auth.is_none());
    }

    #[test]
    fn test_setters_and_removers() {
        let mut config = AuthConfig::new();

        config.set_github_oauth("github.com", "token");
        assert_eq!(config.get_github_oauth("github.com"), Some("token"));

        config.remove_github_oauth("github.com");
        assert_eq!(config.get_github_oauth("github.com"), None);

        config.set_bitbucket_oauth("bitbucket.org", "key", "secret");
        let bb = config.get_bitbucket_oauth("bitbucket.org").unwrap();
        assert_eq!(bb.consumer_key, "key");
        assert_eq!(bb.consumer_secret, "secret");
    }

    #[test]
    fn test_serialize_roundtrip() {
        let mut config = AuthConfig::new();
        config.set_github_oauth("github.com", "token");
        config.set_http_basic("example.org", "user", "pass");

        let json = serde_json::to_string(&config).unwrap();
        let parsed = AuthConfig::from_json(&json).unwrap();

        assert_eq!(parsed.get_github_oauth("github.com"), Some("token"));
        let basic = parsed.get_http_basic("example.org").unwrap();
        assert_eq!(basic.username, "user");
    }

    #[test]
    fn test_composer_auth_env_var() {
        let _lock = environment_lock();
        let _environment = EnvironmentGuard::set(
            "COMPOSER_AUTH",
            Some(r#"{"github-oauth":{"github.com":"env_token"}}"#),
        );

        let result = AuthConfig::from_env().unwrap();
        assert!(result.is_some());

        let config = result.unwrap();
        assert_eq!(config.get_github_oauth("github.com"), Some("env_token"));
    }

    #[test]
    fn test_composer_auth_env_var_empty() {
        let _lock = environment_lock();
        let _environment = EnvironmentGuard::set("COMPOSER_AUTH", Some(""));

        let result = AuthConfig::from_env().unwrap();
        assert!(result.is_none());
    }

    // Ported from Composer\Test\Util\AuthHelperTest::testFindAuthOrigin.
    #[test]
    fn composer_auth_helper_finds_only_exact_and_explicit_api_origins() {
        let mut config = AuthConfig::new();
        config.set_github_oauth("github.com", "github-token");
        config.set_bitbucket_oauth("bitbucket.org", "key", "secret");
        config.set_gitlab_oauth("gitlab.com", "gitlab-token");

        assert_eq!(
            config.find_auth_origin("github.com").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            config.find_auth_origin("api.github.com").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            config.find_auth_origin("api.bitbucket.org").as_deref(),
            Some("bitbucket.org")
        );
        assert_eq!(config.find_auth_origin("api.gitlab.com"), None);

        config.remove_bitbucket_oauth("bitbucket.org");
        assert_eq!(config.find_auth_origin("bitbucket.org"), None);
    }

    // Ported from Composer\Test\Util\AuthHelperTest::testStoreAuthAutomatically.
    #[test]
    fn composer_auth_helper_stores_credentials_automatically() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("auth.json");
        let mut initial = AuthConfig::new();
        initial.set_bearer("packages.example.org", "existing-token");
        initial.save(&path).unwrap();
        let credentials = HttpBasicCredentials {
            username: "my_username".to_string(),
            password: "my_password".to_string(),
        };

        assert!(AuthConfig::store_http_basic(
            &path,
            "github.com",
            &credentials,
            &StoreAuths::True,
            None,
        )
        .unwrap());

        let stored = AuthConfig::from_file(&path).unwrap();
        assert_eq!(stored.get_http_basic("github.com"), Some(&credentials));
        assert_eq!(
            stored.get_bearer("packages.example.org"),
            Some("existing-token")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    // Ported from Composer\Test\Util\AuthHelperTest::testStoreAuthWithPromptYesAnswer.
    #[test]
    fn composer_auth_helper_stores_credentials_after_explicit_yes() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("auth.json");
        let credentials = HttpBasicCredentials {
            username: "my_username".to_string(),
            password: "my_password".to_string(),
        };

        assert!(AuthConfig::store_http_basic(
            &path,
            "github.com",
            &credentials,
            &StoreAuths::Prompt,
            Some("yes"),
        )
        .unwrap());
        assert_eq!(
            AuthConfig::from_file(path)
                .unwrap()
                .get_http_basic("github.com"),
            Some(&credentials)
        );
    }

    // Ported from Composer\Test\Util\AuthHelperTest::testStoreAuthWithPromptNoAnswer.
    #[test]
    fn composer_auth_helper_does_not_store_credentials_after_no() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("auth.json");
        let credentials = HttpBasicCredentials {
            username: "my_username".to_string(),
            password: "my_password".to_string(),
        };

        assert!(!AuthConfig::store_http_basic(
            &path,
            "github.com",
            &credentials,
            &StoreAuths::Prompt,
            Some("no"),
        )
        .unwrap());
        assert!(!path.exists());
    }

    // Ported from Composer\Test\Util\AuthHelperTest::testStoreAuthWithPromptInvalidAnswer.
    #[test]
    fn composer_auth_helper_rejects_invalid_storage_answer() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("auth.json");
        let credentials = HttpBasicCredentials {
            username: "my_username".to_string(),
            password: "my_password".to_string(),
        };

        let error = AuthConfig::store_http_basic(
            &path,
            "github.com",
            &credentials,
            &StoreAuths::Prompt,
            Some("invalid"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("Please answer (y)es or (n)o"));
        assert!(!path.exists());
    }

    #[test]
    fn noninteractive_prompt_policy_never_stores_credentials() {
        assert!(!AuthConfig::should_store(&StoreAuths::Prompt, None).unwrap());
    }
}
