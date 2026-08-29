//! HTTP client for Composer package manager operations.
//!
//! This module provides a wrapper around `reqwest` with Composer-specific features:
//! - Automatic retry logic with exponential backoff
//! - Progress tracking for downloads
//! - Custom User-Agent and Accept-Encoding headers
//! - Connection pooling and timeout handling
//! - Proxy and custom CA certificate support
//!
//! # Examples
//!
//! Basic usage:
//! ```no_run
//! use riff_core::http::HttpClient;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let client = HttpClient::new()?;
//!
//! // Simple GET request
//! let response = client.get("https://repo.packagist.org/packages.json").await?;
//!
//! // GET and parse JSON
//! #[derive(serde::Deserialize)]
//! struct PackagesJson {
//!     packages: Vec<String>,
//! }
//! let packages: PackagesJson = client.get_json("https://repo.packagist.org/packages.json").await?;
//!
//! // Download a file with progress tracking
//! client.download(
//!     "https://example.com/package.zip",
//!     "/tmp/package.zip".as_ref(),
//!     Some(|downloaded, total| {
//!         println!("Downloaded {}/{} bytes", downloaded, total);
//!     })
//! ).await?;
//! # Ok(())
//! # }
//! ```
//!
//! Custom configuration:
//! ```no_run
//! use riff_core::http::{HttpClient, HttpClientConfig};
//! use std::time::Duration;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = HttpClientConfig::new()
//!     .with_timeout(Duration::from_secs(60))
//!     .with_max_retries(5)
//!     .with_proxy("http://proxy.example.com:8080".to_string());
//!
//! let client = HttpClient::with_config(config)?;
//! # Ok(())
//! # }
//! ```

use reqwest::{Client, Response, StatusCode};
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};

use crate::config::AuthConfig;
use crate::url_utils::sanitize_url;

use super::auth::authentication_options;
use super::no_proxy::NoProxyPattern;
use super::transport::{
    redirect_is_allowed, HttpRequestOptions, HttpTransportPolicyError, PreparedHttpUrl,
};

const DEFAULT_USER_AGENT: &str = "Composer/2.0 (riff-core)";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_RETRIES: u32 = 3;
const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(1);
const DOWNLOAD_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("Request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("HTTP {status}: {url}")]
    HttpStatus { status: u16, url: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Max retries exceeded for {url}")]
    MaxRetries { url: String },

    #[error("Failed to parse JSON response from {url}: {reason}")]
    JsonParse { url: String, reason: String },

    #[error("Failed to configure authentication for {origin}: {reason}")]
    Authentication { origin: String, reason: String },

    #[error(transparent)]
    Policy(#[from] HttpTransportPolicyError),

    #[error("Access to \"{url}\" is blocked.")]
    AccessBlocked { url: String },

    #[error("Could not follow the redirect because only http and https redirects are supported")]
    DisallowedRedirect,
}

fn decode_json_response<T: DeserializeOwned>(url: &str, body: &str) -> Result<T, HttpError> {
    serde_json::from_str(body).map_err(|error| HttpError::JsonParse {
        url: sanitize_url(url),
        reason: error.to_string(),
    })
}

pub struct HttpClient {
    client: Client,
    user_agent: String,
    max_retries: u32,
    retry_delay: Duration,
    auth: Option<Arc<AuthConfig>>,
}

impl HttpClient {
    pub fn new() -> Result<Self, reqwest::Error> {
        Self::with_config(HttpClientConfig::default())
    }

    pub fn with_config(config: HttpClientConfig) -> Result<Self, reqwest::Error> {
        let mut builder = Client::builder()
            .timeout(config.timeout)
            .connect_timeout(config.connect_timeout)
            .gzip(true)
            .user_agent(&config.user_agent)
            // Redirects are handled below so access guards and scheme policy
            // apply to every hop, including targets reqwest itself rejects.
            .redirect(reqwest::redirect::Policy::none());

        // Add proxy if configured
        if let Some(proxy_url) = &config.proxy {
            let no_proxy = config.no_proxy.clone().or_else(|| {
                std::env::var("NO_PROXY")
                    .or_else(|_| std::env::var("no_proxy"))
                    .ok()
                    .filter(|pattern| !pattern.trim().is_empty())
            });
            let proxy = if let Some(no_proxy) = no_proxy {
                reqwest::Proxy::all(proxy_url)?;
                let matcher = NoProxyPattern::new(&no_proxy);
                let proxy_url = proxy_url.clone();
                reqwest::Proxy::custom(move |url| {
                    (!matcher.matches(url)).then(|| proxy_url.clone())
                })
            } else {
                reqwest::Proxy::all(proxy_url)?
            };
            builder = builder.proxy(proxy);
        }

        // Add custom CA certificate if configured
        if let Some(cafile) = &config.cafile {
            if let Ok(cert_bytes) = std::fs::read(cafile) {
                if let Ok(cert) = reqwest::Certificate::from_pem(&cert_bytes) {
                    builder = builder.add_root_certificate(cert);
                }
            }
        }

        let client = builder.build()?;

        Ok(Self {
            client,
            user_agent: config.user_agent,
            max_retries: config.max_retries,
            retry_delay: config.retry_delay,
            auth: config.auth.map(Arc::new),
        })
    }

    /// Set authentication configuration
    pub fn with_auth(mut self, auth: AuthConfig) -> Self {
        self.auth = Some(Arc::new(auth));
        self
    }

    /// Set authentication configuration (shared)
    pub fn with_auth_shared(mut self, auth: Arc<AuthConfig>) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Perform GET request with automatic retries
    pub async fn get(&self, url: &str) -> Result<Response, HttpError> {
        self.get_with_options(url, &HttpRequestOptions::default())
            .await
    }

    /// Perform a GET request with per-request transport policy.
    pub async fn get_with_options(
        &self,
        url: &str,
        options: &HttpRequestOptions,
    ) -> Result<Response, HttpError> {
        self.get_with_accept_encoding(url, "gzip", options).await
    }

    async fn get_with_accept_encoding(
        &self,
        url: &str,
        accept_encoding: &'static str,
        options: &HttpRequestOptions,
    ) -> Result<Response, HttpError> {
        let mut prepared = PreparedHttpUrl::parse(url)?;
        if options.access_is_blocked(prepared.url()) {
            return Err(HttpError::AccessBlocked {
                url: sanitize_url(prepared.url().as_str()),
            });
        }

        let mut last_error;
        let mut attempt = 0;
        let mut authentication_retried = false;
        let mut redirects = 0_u8;

        loop {
            match self
                .execute_get(&prepared, accept_encoding, options, authentication_retried)
                .await
            {
                Ok(response) => {
                    // Check for HTTP errors
                    let status = response.status();
                    if status.is_success() || status == StatusCode::NOT_MODIFIED {
                        return Ok(response);
                    } else if status.is_redirection() && status != StatusCode::NOT_MODIFIED {
                        let Some(location) = response.headers().get(reqwest::header::LOCATION)
                        else {
                            return Err(HttpError::HttpStatus {
                                status: status.as_u16(),
                                url: sanitize_url(prepared.url().as_str()),
                            });
                        };
                        let location = location.to_str().map_err(|_| {
                            HttpTransportPolicyError::InvalidUrl("redirect target".to_string())
                        })?;
                        let target = prepared.url().join(location).map_err(|_| {
                            HttpTransportPolicyError::InvalidUrl(sanitize_url(location))
                        })?;
                        if !redirect_is_allowed(target.as_str()) {
                            return Err(HttpError::DisallowedRedirect);
                        }
                        redirects += 1;
                        if redirects > 20 {
                            return Err(HttpError::MaxRetries {
                                url: sanitize_url(target.as_str()),
                            });
                        }
                        prepared = PreparedHttpUrl::parse(target.as_str())?;
                        if options.access_is_blocked(prepared.url()) {
                            return Err(HttpError::AccessBlocked {
                                url: sanitize_url(prepared.url().as_str()),
                            });
                        }
                        continue;
                    } else if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
                        && options.retry_auth_failure()
                        && !authentication_retried
                        && !options.authentication_retry_headers().is_empty()
                    {
                        authentication_retried = true;
                        continue;
                    } else if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
                        // Retry on server errors and rate limits
                        last_error = HttpError::HttpStatus {
                            status: status.as_u16(),
                            url: sanitize_url(prepared.url().as_str()),
                        };
                    } else {
                        // Don't retry on client errors (4xx except 429)
                        return Err(HttpError::HttpStatus {
                            status: status.as_u16(),
                            url: sanitize_url(prepared.url().as_str()),
                        });
                    }
                }
                Err(error @ HttpError::DisallowedRedirect) => return Err(error),
                Err(e) => {
                    last_error = e;
                }
            }

            // Don't sleep after the last attempt
            if attempt < self.max_retries {
                // Exponential backoff: 1s, 2s, 4s, 8s, etc.
                let delay = self.retry_delay * 2_u32.pow(attempt);
                tokio::time::sleep(delay).await;
                attempt += 1;
            } else {
                break;
            }
        }

        // All retries exhausted
        Err(last_error)
    }

    /// Execute a GET request without retries
    async fn execute_get(
        &self,
        prepared: &PreparedHttpUrl,
        accept_encoding: &'static str,
        options: &HttpRequestOptions,
        authentication_retry: bool,
    ) -> Result<Response, HttpError> {
        let url = prepared.url().as_str();
        let mut request = self.client.get(prepared.url().clone());

        // Apply authentication if available
        if let Some(ref auth) = self.auth {
            request = self.apply_auth(request, url, auth)?;
        }

        // Explicit URL credentials take precedence over configured credentials.
        if let Some(authentication) = prepared.authentication() {
            request =
                request.basic_auth(authentication.username(), Some(authentication.password()));
        }

        for (name, value) in options.headers() {
            request = request.header(name, value);
        }
        if authentication_retry {
            for (name, value) in options.authentication_retry_headers() {
                request = request.header(name, value);
            }
        }
        // The representation requested by the operation wins over defaults.
        request = request.header("Accept-Encoding", accept_encoding);

        Ok(request.send().await?)
    }

    /// Apply authentication to a request based on the URL
    fn apply_auth(
        &self,
        mut request: reqwest::RequestBuilder,
        url: &str,
        auth: &AuthConfig,
    ) -> Result<reqwest::RequestBuilder, HttpError> {
        let options = authentication_options(auth, url, url, &[]).map_err(|error| {
            let origin = url::Url::parse(url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| "request origin".to_string());
            HttpError::Authentication {
                origin,
                reason: error.to_string(),
            }
        })?;
        for header in options.headers() {
            request = request.header(header.name(), header.value().clone());
        }
        Ok(request)
    }

    /// GET JSON and deserialize
    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, HttpError> {
        let response = self.get(url).await?;
        let text = response.text().await?;

        decode_json_response(url, &text)
    }

    /// Download file with progress callback
    pub async fn download<F>(
        &self,
        url: &str,
        dest: &Path,
        progress: Option<F>,
    ) -> Result<(), HttpError>
    where
        F: Fn(u64, u64),
    {
        self.download_with_options(url, dest, progress, &HttpRequestOptions::default())
            .await
    }

    /// Download a file using per-request policy and headers.
    pub async fn download_with_options<F>(
        &self,
        url: &str,
        dest: &Path,
        progress: Option<F>,
        options: &HttpRequestOptions,
    ) -> Result<(), HttpError>
    where
        F: Fn(u64, u64),
    {
        // Archives are already compressed. Asking intermediaries for the
        // original representation avoids redundant HTTP decompression.
        let response = self
            .get_with_accept_encoding(url, "identity", options)
            .await?;

        // Get total size from Content-Length header
        let total_size = response.content_length().unwrap_or(0);

        // Create parent directories if they don't exist
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Create the file
        let file = File::create(dest).await?;
        let mut file = BufWriter::with_capacity(DOWNLOAD_BUFFER_SIZE, file);
        let mut downloaded: u64 = 0;

        // Stream the response body
        let mut stream = response.bytes_stream();

        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            downloaded += chunk.len() as u64;

            // Call progress callback if provided
            if let Some(ref callback) = progress {
                callback(downloaded, total_size);
            }
        }

        file.flush().await?;

        Ok(())
    }

    /// Download to memory
    pub async fn download_bytes(&self, url: &str) -> Result<Vec<u8>, HttpError> {
        let response = self.get(url).await?;
        let bytes = response.bytes().await?;
        Ok(bytes.to_vec())
    }

    /// Download a response body to memory using per-request policy.
    pub async fn download_bytes_with_options(
        &self,
        url: &str,
        options: &HttpRequestOptions,
    ) -> Result<Vec<u8>, HttpError> {
        let response = self.get_with_options(url, options).await?;
        let bytes = response.bytes().await?;
        Ok(bytes.to_vec())
    }

    /// Get the configured user agent
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// Get the maximum number of retries
    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new().expect("Failed to create default HTTP client")
    }
}

#[derive(Debug, Clone)]
pub struct HttpClientConfig {
    pub timeout: Duration,
    pub connect_timeout: Duration,
    pub max_retries: u32,
    pub retry_delay: Duration,
    pub proxy: Option<String>,
    pub no_proxy: Option<String>,
    pub cafile: Option<PathBuf>,
    pub user_agent: String,
    pub auth: Option<AuthConfig>,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_delay: DEFAULT_RETRY_DELAY,
            proxy: None,
            no_proxy: None,
            cafile: None,
            user_agent: DEFAULT_USER_AGENT.to_string(),
            auth: None,
        }
    }
}

impl HttpClientConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    pub fn with_retry_delay(mut self, retry_delay: Duration) -> Self {
        self.retry_delay = retry_delay;
        self
    }

    pub fn with_proxy(mut self, proxy: String) -> Self {
        self.proxy = Some(proxy);
        self
    }

    pub fn with_no_proxy(mut self, no_proxy: String) -> Self {
        self.no_proxy = Some(no_proxy);
        self
    }

    pub fn with_cafile(mut self, cafile: PathBuf) -> Self {
        self.cafile = Some(cafile);
        self
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = user_agent;
        self
    }

    pub fn with_auth(mut self, auth: AuthConfig) -> Self {
        self.auth = Some(auth);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthMatch, BitbucketOAuthCredentials, HttpBasicCredentials};

    #[test]
    fn test_config_builder() {
        let config = HttpClientConfig::new()
            .with_timeout(Duration::from_secs(60))
            .with_max_retries(5)
            .with_user_agent("Test/1.0".to_string());

        assert_eq!(config.timeout, Duration::from_secs(60));
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.user_agent, "Test/1.0");
    }

    // ============ Authentication Tests ============
    // Based on Composer's AuthHelperTest.php patterns

    #[test]
    fn test_auth_config_with_github_oauth() {
        let mut auth = AuthConfig::default();
        auth.github_oauth
            .insert("github.com".to_string(), "ghp_token123".to_string());

        let config = HttpClientConfig::new().with_auth(auth);
        assert!(config.auth.is_some());

        let auth = config.auth.as_ref().unwrap();
        assert_eq!(auth.get_github_oauth("github.com"), Some("ghp_token123"));
    }

    #[test]
    fn test_auth_config_with_gitlab_token() {
        let mut auth = AuthConfig::default();
        auth.gitlab_token.insert(
            "gitlab.com".to_string(),
            crate::config::GitLabAuth::Token("glpat-token".to_string()),
        );

        let config = HttpClientConfig::new().with_auth(auth);
        assert!(config.auth.is_some());

        let auth = config.auth.as_ref().unwrap();
        assert_eq!(auth.get_gitlab_token("gitlab.com"), Some("glpat-token"));
    }

    #[test]
    fn test_auth_config_with_bitbucket_oauth() {
        let mut auth = AuthConfig::default();
        auth.bitbucket_oauth.insert(
            "bitbucket.org".to_string(),
            BitbucketOAuthCredentials {
                consumer_key: "my_key".to_string(),
                consumer_secret: "my_secret".to_string(),
            },
        );

        let config = HttpClientConfig::new().with_auth(auth);
        let auth = config.auth.as_ref().unwrap();

        let creds = auth.get_bitbucket_oauth("bitbucket.org").unwrap();
        assert_eq!(creds.consumer_key, "my_key");
        assert_eq!(creds.consumer_secret, "my_secret");
    }

    #[test]
    fn test_auth_config_with_http_basic() {
        let mut auth = AuthConfig::default();
        auth.http_basic.insert(
            "private.example.org".to_string(),
            HttpBasicCredentials {
                username: "user".to_string(),
                password: "pass".to_string(),
            },
        );

        let config = HttpClientConfig::new().with_auth(auth);
        let auth = config.auth.as_ref().unwrap();

        let creds = auth.get_http_basic("private.example.org").unwrap();
        assert_eq!(creds.username, "user");
        assert_eq!(creds.password, "pass");
    }

    #[test]
    fn test_auth_config_with_bearer() {
        let mut auth = AuthConfig::default();
        auth.bearer.insert(
            "api.example.org".to_string(),
            "bearer_token_xyz".to_string(),
        );

        let config = HttpClientConfig::new().with_auth(auth);
        let auth = config.auth.as_ref().unwrap();

        assert_eq!(auth.get_bearer("api.example.org"), Some("bearer_token_xyz"));
    }

    #[test]
    fn http_client_applies_typed_authentication_policy_to_requests() {
        let client = HttpClient::new().unwrap();
        let mut auth = AuthConfig::new();
        auth.set_github_oauth("github.com", "secret-token");
        let request = client
            .apply_auth(
                client.client.get("https://api.github.com/repos/acme/demo"),
                "https://api.github.com/repos/acme/demo",
                &auth,
            )
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(
            request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("token secret-token")
        );

        let unrelated = client
            .apply_auth(
                client
                    .client
                    .get("https://github.example.org/packages.json"),
                "https://github.example.org/packages.json",
                &auth,
            )
            .unwrap()
            .build()
            .unwrap();
        assert!(!unrelated.headers().contains_key("authorization"));
    }

    #[test]
    fn test_auth_find_for_github_url() {
        let mut auth = AuthConfig::default();
        auth.github_oauth
            .insert("github.com".to_string(), "ghp_token".to_string());

        // Standard GitHub URL
        let result = auth.find_for_url("https://github.com/owner/repo");
        assert!(matches!(result, AuthMatch::GitHubOAuth("ghp_token")));

        // API GitHub URL
        let result = auth.find_for_url("https://api.github.com/repos/owner/repo");
        assert!(matches!(result, AuthMatch::GitHubOAuth("ghp_token")));

        // Credentials are never inherited by a different, non-API origin.
        let result =
            auth.find_for_url("https://raw.githubusercontent.com/owner/repo/main/file.txt");
        assert!(matches!(result, AuthMatch::None));
    }

    #[test]
    fn test_auth_find_for_gitlab_url() {
        let mut auth = AuthConfig::default();
        auth.gitlab_token.insert(
            "gitlab.com".to_string(),
            crate::config::GitLabAuth::Token("glpat-token".to_string()),
        );

        let result = auth.find_for_url("https://gitlab.com/group/project");
        assert!(matches!(result, AuthMatch::GitLabToken("glpat-token")));
    }

    #[test]
    fn test_auth_find_for_bitbucket_url() {
        let mut auth = AuthConfig::default();
        auth.bitbucket_oauth.insert(
            "bitbucket.org".to_string(),
            BitbucketOAuthCredentials {
                consumer_key: "key".to_string(),
                consumer_secret: "secret".to_string(),
            },
        );

        let result = auth.find_for_url("https://bitbucket.org/owner/repo");
        assert!(matches!(result, AuthMatch::BitbucketOAuth(_)));

        if let AuthMatch::BitbucketOAuth(creds) = result {
            assert_eq!(creds.consumer_key, "key");
        }
    }

    #[test]
    fn test_auth_http_basic_takes_priority() {
        // HTTP Basic should take priority over other auth methods for the same domain
        let mut auth = AuthConfig::default();
        auth.http_basic.insert(
            "github.com".to_string(),
            HttpBasicCredentials {
                username: "user".to_string(),
                password: "pass".to_string(),
            },
        );
        auth.github_oauth
            .insert("github.com".to_string(), "ghp_token".to_string());

        let result = auth.find_for_url("https://github.com/owner/repo");
        // HTTP Basic should match first based on find_for_url implementation
        assert!(matches!(result, AuthMatch::HttpBasic(_)));
    }

    #[test]
    fn test_auth_no_match_returns_none() {
        let auth = AuthConfig::default();
        let result = auth.find_for_url("https://unknown.example.org/path");
        assert!(matches!(result, AuthMatch::None));
    }

    #[test]
    fn test_client_with_auth_method() {
        let mut auth = AuthConfig::default();
        auth.github_oauth
            .insert("github.com".to_string(), "token".to_string());

        let client = HttpClient::new().unwrap().with_auth(auth);
        assert!(client.auth.is_some());
    }

    #[test]
    fn test_client_with_auth_shared() {
        let mut auth = AuthConfig::default();
        auth.github_oauth
            .insert("github.com".to_string(), "token".to_string());
        let shared = Arc::new(auth);

        let client = HttpClient::new()
            .unwrap()
            .with_auth_shared(Arc::clone(&shared));
        assert!(client.auth.is_some());
    }

    #[test]
    fn test_default_config() {
        let config = HttpClientConfig::default();

        assert_eq!(config.timeout, DEFAULT_TIMEOUT);
        assert_eq!(config.connect_timeout, DEFAULT_CONNECT_TIMEOUT);
        assert_eq!(config.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(config.retry_delay, DEFAULT_RETRY_DELAY);
        assert_eq!(config.user_agent, DEFAULT_USER_AGENT);
        assert!(config.proxy.is_none());
        assert!(config.cafile.is_none());
    }

    #[tokio::test]
    async fn test_client_creation() {
        let client = HttpClient::new();
        assert!(client.is_ok());

        let client = client.unwrap();
        assert_eq!(client.user_agent(), DEFAULT_USER_AGENT);
        assert_eq!(client.max_retries(), DEFAULT_MAX_RETRIES);
    }

    #[tokio::test]
    async fn test_client_with_config() {
        let config = HttpClientConfig::new()
            .with_timeout(Duration::from_secs(60))
            .with_max_retries(5);

        let client = HttpClient::with_config(config);
        assert!(client.is_ok());

        let client = client.unwrap();
        assert_eq!(client.max_retries(), 5);
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_get_request() {
        let client = HttpClient::new().unwrap();
        let response = client.get("https://httpbin.org/get").await;
        assert!(response.is_ok());
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_get_json() {
        use serde::Deserialize;

        #[derive(Deserialize, Debug)]
        struct Response {
            url: String,
        }

        let client = HttpClient::new().unwrap();
        let response: Result<Response, _> = client.get_json("https://httpbin.org/get").await;
        assert!(response.is_ok());
        assert_eq!(response.unwrap().url, "https://httpbin.org/get");
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_download_bytes() {
        let client = HttpClient::new().unwrap();
        let bytes = client.download_bytes("https://httpbin.org/bytes/100").await;
        assert!(bytes.is_ok());
        assert_eq!(bytes.unwrap().len(), 100);
    }

    #[tokio::test]
    async fn test_download_streams_identity_encoded_response_to_file() {
        use tempfile::TempDir;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let expected_body = vec![b'z'; DOWNLOAD_BUFFER_SIZE * 2 + 17];
        let response_body = expected_body.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buffer[..read]);
            }

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(&response_body).await.unwrap();
            String::from_utf8(request).unwrap()
        });

        let temp = TempDir::new().unwrap();
        let destination = temp.path().join("archive.zip");
        HttpClient::new()
            .unwrap()
            .download(
                &format!("http://{address}/archive.zip"),
                &destination,
                None::<fn(u64, u64)>,
            )
            .await
            .unwrap();

        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.contains("accept-encoding: identity\r\n"));
        assert_eq!(tokio::fs::read(destination).await.unwrap(), expected_body);
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_download_file() {
        use tempfile::TempDir;

        let client = HttpClient::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let dest = temp_dir.path().join("test_file.bin");

        let result = client
            .download("https://httpbin.org/bytes/100", &dest, None::<fn(u64, u64)>)
            .await;

        assert!(result.is_ok());
        assert!(dest.exists());

        let metadata = tokio::fs::metadata(&dest).await.unwrap();
        assert_eq!(metadata.len(), 100);
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_download_file_with_progress() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        use tempfile::TempDir;

        let client = HttpClient::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let dest = temp_dir.path().join("test_file.bin");

        let downloaded = Arc::new(AtomicU64::new(0));
        let downloaded_clone = Arc::clone(&downloaded);

        let result = client
            .download(
                "https://httpbin.org/bytes/1000",
                &dest,
                Some(move |bytes, _total| {
                    downloaded_clone.store(bytes, Ordering::SeqCst);
                }),
            )
            .await;

        assert!(result.is_ok());
        assert!(dest.exists());
        assert_eq!(downloaded.load(Ordering::SeqCst), 1000);
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_error_404() {
        let client = HttpClient::new().unwrap();
        let response = client.get("https://httpbin.org/status/404").await;
        assert!(response.is_err());

        if let Err(HttpError::HttpStatus { status, .. }) = response {
            assert_eq!(status, 404);
        } else {
            panic!("Expected HttpStatus error");
        }
    }

    // ============ Error Handling Tests ============
    // Based on Composer's HttpDownloaderTest.php patterns

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_error_401_unauthorized() {
        let client = HttpClient::new().unwrap();
        let response = client.get("https://httpbin.org/status/401").await;
        assert!(response.is_err());

        if let Err(HttpError::HttpStatus { status, .. }) = response {
            assert_eq!(status, 401);
        } else {
            panic!("Expected HttpStatus error for 401");
        }
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_error_403_forbidden() {
        let client = HttpClient::new().unwrap();
        let response = client.get("https://httpbin.org/status/403").await;
        assert!(response.is_err());

        if let Err(HttpError::HttpStatus { status, .. }) = response {
            assert_eq!(status, 403);
        } else {
            panic!("Expected HttpStatus error for 403");
        }
    }

    #[test]
    fn test_http_error_display() {
        let err = HttpError::HttpStatus {
            status: 404,
            url: "https://example.com/not-found".to_string(),
        };
        assert_eq!(err.to_string(), "HTTP 404: https://example.com/not-found");

        let err = HttpError::MaxRetries {
            url: "https://example.com/timeout".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "Max retries exceeded for https://example.com/timeout"
        );

        let err = HttpError::JsonParse {
            url: "https://example.com/packages.json".to_string(),
            reason: "unexpected token".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "Failed to parse JSON response from https://example.com/packages.json: unexpected token"
        );
    }

    // Ported from Composer\Test\Util\Http\ResponseTest::testDecodeJsonParsesValidBody.
    #[test]
    fn composer_http_response_decodes_valid_json() {
        let decoded: serde_json::Value =
            decode_json_response("https://example.org/packages.json", r#"{"foo":"bar"}"#).unwrap();

        assert_eq!(decoded, serde_json::json!({"foo": "bar"}));
    }

    // Ported from Composer\Test\Util\Http\ResponseTest::
    // testDecodeJsonDoesNotLeakResponseBodyOnParseError.
    #[test]
    fn composer_http_response_json_errors_report_url_without_body() {
        let url = "http://169.254.169.254/latest/meta-data/iam/security-credentials";
        let error =
            decode_json_response::<serde_json::Value>(url, r#"{"k":"secret-value-LEAKMARKER" X}"#)
                .unwrap_err();
        let message = error.to_string();

        assert!(message.contains(url));
        assert!(!message.contains("LEAKMARKER"));
    }

    #[test]
    fn test_config_with_proxy() {
        let config =
            HttpClientConfig::new().with_proxy("http://proxy.example.com:8080".to_string());

        assert_eq!(
            config.proxy,
            Some("http://proxy.example.com:8080".to_string())
        );
    }

    #[test]
    fn test_config_with_cafile() {
        let config = HttpClientConfig::new().with_cafile(PathBuf::from("/path/to/ca.crt"));

        assert_eq!(config.cafile, Some(PathBuf::from("/path/to/ca.crt")));
    }

    #[test]
    fn test_config_with_retry_delay() {
        let config = HttpClientConfig::new().with_retry_delay(Duration::from_millis(500));

        assert_eq!(config.retry_delay, Duration::from_millis(500));
    }

    #[test]
    fn test_config_with_connect_timeout() {
        let config = HttpClientConfig::new().with_connect_timeout(Duration::from_secs(5));

        assert_eq!(config.connect_timeout, Duration::from_secs(5));
    }

    #[test]
    fn test_config_all_builder_methods() {
        let auth = AuthConfig::default();
        let config = HttpClientConfig::new()
            .with_timeout(Duration::from_secs(120))
            .with_connect_timeout(Duration::from_secs(15))
            .with_max_retries(10)
            .with_retry_delay(Duration::from_millis(200))
            .with_proxy("http://proxy:8080".to_string())
            .with_cafile(PathBuf::from("/ca.pem"))
            .with_user_agent("CustomAgent/1.0".to_string())
            .with_auth(auth);

        assert_eq!(config.timeout, Duration::from_secs(120));
        assert_eq!(config.connect_timeout, Duration::from_secs(15));
        assert_eq!(config.max_retries, 10);
        assert_eq!(config.retry_delay, Duration::from_millis(200));
        assert_eq!(config.proxy, Some("http://proxy:8080".to_string()));
        assert_eq!(config.cafile, Some(PathBuf::from("/ca.pem")));
        assert_eq!(config.user_agent, "CustomAgent/1.0");
        assert!(config.auth.is_some());
    }

    #[test]
    fn test_default_client_creation() {
        let client = HttpClient::default();
        assert_eq!(client.user_agent(), DEFAULT_USER_AGENT);
        assert_eq!(client.max_retries(), DEFAULT_MAX_RETRIES);
    }

    // ============ URL and Domain Extraction Tests ============
    // Based on Composer's AuthHelper domain matching

    #[test]
    fn test_github_subdomain_does_not_inherit_auth() {
        let mut auth = AuthConfig::default();
        auth.github_oauth
            .insert("github.com".to_string(), "token".to_string());

        // codeload.github.com is a distinct origin and must not inherit a token.
        let result =
            auth.find_for_url("https://codeload.github.com/owner/repo/zip/refs/heads/main");
        assert!(matches!(result, AuthMatch::None));
    }

    #[test]
    fn test_gitlab_self_hosted() {
        let mut auth = AuthConfig::default();
        auth.gitlab_token.insert(
            "gitlab.mycompany.com".to_string(),
            crate::config::GitLabAuth::Token("private-token".to_string()),
        );

        let result = auth.find_for_url("https://gitlab.mycompany.com/group/project");
        assert!(matches!(result, AuthMatch::GitLabToken("private-token")));
    }

    #[test]
    fn test_bearer_auth_for_custom_domain() {
        let mut auth = AuthConfig::default();
        auth.bearer.insert(
            "packages.mycompany.com".to_string(),
            "secret-token".to_string(),
        );

        let result = auth.find_for_url("https://packages.mycompany.com/composer/packages.json");
        assert!(matches!(result, AuthMatch::Bearer("secret-token")));
    }

    #[test]
    fn test_http_basic_for_private_packagist() {
        let mut auth = AuthConfig::default();
        auth.http_basic.insert(
            "repo.packagist.com".to_string(),
            HttpBasicCredentials {
                username: "token".to_string(),
                password: "secret123".to_string(),
            },
        );

        let result = auth.find_for_url("https://repo.packagist.com/myorg/packages.json");
        assert!(matches!(result, AuthMatch::HttpBasic(_)));

        if let AuthMatch::HttpBasic(creds) = result {
            assert_eq!(creds.username, "token");
            assert_eq!(creds.password, "secret123");
        }
    }

    // ============ Retry Behavior Tests (Unit) ============

    #[test]
    fn test_retry_config_zero_retries() {
        let config = HttpClientConfig::new().with_max_retries(0);
        assert_eq!(config.max_retries, 0);

        let client = HttpClient::with_config(config).unwrap();
        assert_eq!(client.max_retries(), 0);
    }

    #[test]
    fn test_retry_config_high_retries() {
        let config = HttpClientConfig::new().with_max_retries(100);
        assert_eq!(config.max_retries, 100);
    }

    #[test]
    fn test_exponential_backoff_calculation() {
        // Verify the exponential backoff formula: delay * 2^attempt
        let base_delay = Duration::from_secs(1);

        // Attempt 0: 1 * 2^0 = 1 second
        assert_eq!(base_delay * 2_u32.pow(0), Duration::from_secs(1));

        // Attempt 1: 1 * 2^1 = 2 seconds
        assert_eq!(base_delay * 2_u32.pow(1), Duration::from_secs(2));

        // Attempt 2: 1 * 2^2 = 4 seconds
        assert_eq!(base_delay * 2_u32.pow(2), Duration::from_secs(4));

        // Attempt 3: 1 * 2^3 = 8 seconds
        assert_eq!(base_delay * 2_u32.pow(3), Duration::from_secs(8));
    }

    // ============ AuthMatch Tests ============

    #[test]
    fn test_auth_match_is_some() {
        let auth = AuthMatch::Bearer("token");
        assert!(auth.is_some());
        assert!(!auth.is_none());
    }

    #[test]
    fn test_auth_match_is_none() {
        let auth = AuthMatch::None;
        assert!(!auth.is_some());
        assert!(auth.is_none());
    }

    #[test]
    fn test_auth_match_variants() {
        // Test all AuthMatch variants are constructable
        let _none = AuthMatch::None;

        let creds = HttpBasicCredentials {
            username: "u".to_string(),
            password: "p".to_string(),
        };
        let _basic = AuthMatch::HttpBasic(&creds);
        let _bearer = AuthMatch::Bearer("token");
        let _github = AuthMatch::GitHubOAuth("ghp_xxx");
        let _gitlab = AuthMatch::GitLabToken("glpat-xxx");

        let bb_creds = BitbucketOAuthCredentials {
            consumer_key: "key".to_string(),
            consumer_secret: "secret".to_string(),
        };
        let _bitbucket = AuthMatch::BitbucketOAuth(&bb_creds);
    }
}
