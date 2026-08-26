//! Origin-scoped HTTP authentication policy.
//!
//! This module deliberately contains no prompting. Callers can prepare request
//! options and decide whether refreshed credentials justify one retry without
//! exposing secrets to diagnostics or persisting them as a side effect.

use std::fmt;

use reqwest::header::{HeaderName, HeaderValue};
use serde::Deserialize;
use thiserror::Error;

use crate::config::{AuthConfig, GitLabAuth, HttpBasicCredentials};

const BITBUCKET_ACCESS_TOKEN_URL: &str = "https://bitbucket.org/site/oauth2/access_token";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AuthenticationPolicyError {
    #[error("authentication contains an invalid HTTP header")]
    InvalidHeader,
    #[error("custom authentication headers must be a JSON array of valid header strings")]
    InvalidCustomHeaders,
    #[error("client certificate authentication must contain valid certificate options")]
    InvalidClientCertificate,
}

/// A validated HTTP header whose debug representation never exposes its value.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthenticationHeader {
    name: HeaderName,
    value: HeaderValue,
    display_name: String,
}

impl AuthenticationHeader {
    fn parse(line: &str) -> Result<Self, AuthenticationPolicyError> {
        if line.contains('\r') || line.contains('\n') {
            return Err(AuthenticationPolicyError::InvalidHeader);
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(AuthenticationPolicyError::InvalidHeader)?;
        let display_name = name.trim().to_string();
        let name = HeaderName::from_bytes(display_name.as_bytes())
            .map_err(|_| AuthenticationPolicyError::InvalidHeader)?;
        let value = HeaderValue::from_str(value.trim())
            .map_err(|_| AuthenticationPolicyError::InvalidHeader)?;
        Ok(Self {
            name,
            value,
            display_name,
        })
    }

    fn new(name: &'static str, value: String) -> Result<Self, AuthenticationPolicyError> {
        let name = HeaderName::from_static(name);
        let value =
            HeaderValue::from_str(&value).map_err(|_| AuthenticationPolicyError::InvalidHeader)?;
        let display_name = match name.as_str() {
            "authorization" => "Authorization",
            "private-token" => "PRIVATE-TOKEN",
            other => other,
        }
        .to_string();
        Ok(Self {
            name,
            value,
            display_name,
        })
    }

    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// Explicit access for applying the header to a request. The value is
    /// intentionally omitted from `Debug` and all diagnostics.
    pub fn value(&self) -> &HeaderValue {
        &self.value
    }

    pub fn as_line(&self) -> String {
        format!(
            "{}: {}",
            self.display_name,
            self.value.to_str().unwrap_or("<non-utf8>")
        )
    }
}

impl fmt::Debug for AuthenticationHeader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthenticationHeader")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
pub struct ClientCertificateOptions {
    pub local_cert: String,
    pub local_pk: String,
    pub passphrase: String,
}

impl fmt::Debug for ClientCertificateOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientCertificateOptions")
            .field("local_cert", &self.local_cert)
            .field("local_pk", &self.local_pk)
            .field("passphrase", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AuthenticationOptions {
    headers: Vec<AuthenticationHeader>,
    client_certificate: Option<ClientCertificateOptions>,
    diagnostic: Option<String>,
}

impl fmt::Debug for AuthenticationOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthenticationOptions")
            .field("headers", &self.headers)
            .field("client_certificate", &self.client_certificate)
            .field("diagnostic", &self.diagnostic)
            .finish()
    }
}

impl AuthenticationOptions {
    pub fn headers(&self) -> &[AuthenticationHeader] {
        &self.headers
    }

    pub fn header_lines(&self) -> Vec<String> {
        self.headers
            .iter()
            .map(AuthenticationHeader::as_line)
            .collect()
    }

    pub fn client_certificate(&self) -> Option<&ClientCertificateOptions> {
        self.client_certificate.as_ref()
    }

    /// A display-safe explanation suitable for verbose logs.
    pub fn diagnostic(&self) -> Option<&str> {
        self.diagnostic.as_deref()
    }
}

/// Build request options while preserving pre-existing headers.
pub fn authentication_options(
    config: &AuthConfig,
    request_origin: &str,
    url: &str,
    existing_headers: &[&str],
) -> Result<AuthenticationOptions, AuthenticationPolicyError> {
    let mut options = AuthenticationOptions {
        headers: existing_headers
            .iter()
            .map(|header| AuthenticationHeader::parse(header))
            .collect::<Result<_, _>>()?,
        client_certificate: None,
        diagnostic: None,
    };

    let Some(origin) = config.find_auth_origin(request_origin) else {
        return Ok(options);
    };

    if let Some(credentials) = config.http_basic.get(&origin) {
        add_http_basic_options(&mut options, &origin, url, credentials)?;
        return Ok(options);
    }
    if let Some(token) = config.bearer.get(&origin) {
        add_header(&mut options, "authorization", format!("Bearer {token}"))?;
        return Ok(options);
    }
    if let Some(token) = config.github_oauth.get(&origin) {
        if is_github_api_url(url) {
            add_header(&mut options, "authorization", format!("token {token}"))?;
            options.diagnostic = Some("Using GitHub token authentication".to_string());
        }
        return Ok(options);
    }
    if let Some(token) = config.gitlab_oauth.get(&origin) {
        add_header(&mut options, "authorization", format!("Bearer {token}"))?;
        options.diagnostic = Some("Using GitLab OAuth token authentication".to_string());
        return Ok(options);
    }
    if let Some(token) = config.gitlab_token.get(&origin) {
        match token {
            GitLabAuth::OAuth { oauth_token } => {
                add_header(
                    &mut options,
                    "authorization",
                    format!("Bearer {oauth_token}"),
                )?;
                options.diagnostic = Some("Using GitLab OAuth token authentication".to_string());
            }
            GitLabAuth::Token(token) => {
                add_header(&mut options, "private-token", token.clone())?;
                options.diagnostic = Some("Using GitLab private token authentication".to_string());
            }
        }
        return Ok(options);
    }
    if let Some(credentials) = config.bitbucket_oauth.get(&origin) {
        add_basic_header(
            &mut options,
            &credentials.consumer_key,
            &credentials.consumer_secret,
        )?;
        options.diagnostic = Some(format!(
            "Using HTTP basic authentication with username \"{}\"",
            sanitize_username(&credentials.consumer_key)
        ));
    }

    Ok(options)
}

fn add_http_basic_options(
    options: &mut AuthenticationOptions,
    origin: &str,
    url: &str,
    credentials: &HttpBasicCredentials,
) -> Result<(), AuthenticationPolicyError> {
    let username = &credentials.username;
    let password = &credentials.password;

    if password == "bearer" {
        add_header(options, "authorization", format!("Bearer {username}"))?;
    } else if password == "custom-headers" {
        let headers: Vec<String> = serde_json::from_str(username)
            .map_err(|_| AuthenticationPolicyError::InvalidCustomHeaders)?;
        for header in headers {
            options.headers.push(
                AuthenticationHeader::parse(&header)
                    .map_err(|_| AuthenticationPolicyError::InvalidCustomHeaders)?,
            );
        }
        options.diagnostic = Some("Using custom HTTP headers for authentication".to_string());
    } else if origin == "github.com" && password == "x-oauth-basic" {
        if is_github_api_url(url) {
            add_header(options, "authorization", format!("token {username}"))?;
            options.diagnostic = Some("Using GitHub token authentication".to_string());
        }
    } else if is_gitlab_origin(origin)
        && matches!(
            password.as_str(),
            "oauth2" | "private-token" | "gitlab-ci-token"
        )
    {
        if password == "oauth2" {
            add_header(options, "authorization", format!("Bearer {username}"))?;
            options.diagnostic = Some("Using GitLab OAuth token authentication".to_string());
        } else {
            add_header(options, "private-token", username.clone())?;
            options.diagnostic = Some("Using GitLab private token authentication".to_string());
        }
    } else if origin == "bitbucket.org"
        && url != BITBUCKET_ACCESS_TOKEN_URL
        && username == "x-token-auth"
    {
        if !is_public_bitbucket_download(url) {
            add_header(options, "authorization", format!("Bearer {password}"))?;
            options.diagnostic = Some("Using Bitbucket OAuth token authentication".to_string());
        }
    } else if username == "client-certificate" {
        options.client_certificate = Some(
            serde_json::from_str(password)
                .map_err(|_| AuthenticationPolicyError::InvalidClientCertificate)?,
        );
        options.diagnostic = Some("Using SSL client certificate".to_string());
    } else {
        add_basic_header(options, username, password)?;
        options.diagnostic = Some(format!(
            "Using HTTP basic authentication with username \"{}\"",
            sanitize_username(username)
        ));
    }

    Ok(())
}

fn add_header(
    options: &mut AuthenticationOptions,
    name: &'static str,
    value: String,
) -> Result<(), AuthenticationPolicyError> {
    options
        .headers
        .push(AuthenticationHeader::new(name, value)?);
    Ok(())
}

fn add_basic_header(
    options: &mut AuthenticationOptions,
    username: &str,
    password: &str,
) -> Result<(), AuthenticationPolicyError> {
    let encoded = encode_base64(format!("{username}:{password}").as_bytes());
    add_header(options, "authorization", format!("Basic {encoded}"))
}

fn encode_base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    encoded
}

fn sanitize_username(username: &str) -> String {
    const PUBLIC_MARKERS: &[&str] = &[
        "private-token",
        "x-token-auth",
        "oauth2",
        "gitlab-ci-token",
        "x-oauth-basic",
    ];
    if PUBLIC_MARKERS.contains(&username) {
        return username.to_string();
    }
    if username.chars().count() >= 12 {
        return format!("{}***", username.chars().take(3).collect::<String>());
    }
    username.to_string()
}

fn is_github_api_url(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https") && url.host_str() == Some("api.github.com")
}

fn is_gitlab_origin(origin: &str) -> bool {
    origin == "gitlab.com" || origin.ends_with(".gitlab.com") || origin.contains("gitlab")
}

/// Whether a URL is a Bitbucket public-download target where Bitbucket
/// credentials must not be sent.
pub fn is_public_bitbucket_download(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    if !host.contains("bitbucket.org") {
        return true;
    }

    url.path().split('/').nth(3) == Some("downloads")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticationRetryDecision {
    pub retry: bool,
    pub store_auth: bool,
    pub replacement: Option<HttpBasicCredentials>,
}

impl AuthenticationRetryDecision {
    fn no_retry() -> Self {
        Self {
            retry: false,
            store_auth: false,
            replacement: None,
        }
    }

    fn retry(replacement: Option<HttpBasicCredentials>) -> Self {
        Self {
            retry: true,
            store_auth: false,
            replacement,
        }
    }
}

/// Decide whether credentials already refreshed by a sibling request justify a
/// non-interactive retry. This function never asks for or stores credentials.
pub fn authentication_retry_decision(
    origin: &str,
    status: u16,
    retry_count: u32,
    current: Option<&HttpBasicCredentials>,
    refreshed: Option<&HttpBasicCredentials>,
) -> AuthenticationRetryDecision {
    if origin == "github.com" && status == 403 && retry_count == 0 && current.is_some() {
        return AuthenticationRetryDecision::retry(None);
    }

    if origin == "bitbucket.org" && matches!(status, 401 | 403) && retry_count == 0 {
        if let Some(refreshed) = refreshed.filter(|value| value.username == "x-token-auth") {
            let replacement = (current != Some(refreshed)).then(|| refreshed.clone());
            return AuthenticationRetryDecision::retry(replacement);
        }
        if current.is_some_and(|value| value.username == "x-token-auth") {
            return AuthenticationRetryDecision::retry(None);
        }
    }

    if is_gitlab_origin(origin)
        && matches!(status, 401 | 404)
        && current.is_some()
        && current == refreshed
    {
        return AuthenticationRetryDecision::no_retry();
    }

    AuthenticationRetryDecision::no_retry()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(username: &str, password: &str) -> HttpBasicCredentials {
        HttpBasicCredentials {
            username: username.to_string(),
            password: password.to_string(),
        }
    }

    fn config_with_basic(origin: &str, username: &str, password: &str) -> AuthConfig {
        let mut config = AuthConfig::new();
        config.set_http_basic(origin, username, password);
        config
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testAddAuthenticationHeaderWithoutAuthCredentials.
    #[test]
    fn composer_auth_helper_preserves_headers_without_credentials() {
        let options = authentication_options(
            &AuthConfig::new(),
            "http://example.org",
            "file:///tmp/composer.json",
            &["Accept-Encoding: gzip", "Connection: close"],
        )
        .unwrap();

        assert_eq!(
            options.header_lines(),
            ["Accept-Encoding: gzip", "Connection: close"]
        );
        assert_eq!(options.diagnostic(), None);
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testAddAuthenticationHeaderWithBearerPassword.
    #[test]
    fn composer_auth_helper_adds_bearer_header() {
        let config = config_with_basic("example.org", "my_username", "bearer");
        let options = authentication_options(
            &config,
            "example.org",
            "https://example.org/packages.json",
            &["Connection: close"],
        )
        .unwrap();

        assert_eq!(
            options.header_lines(),
            ["Connection: close", "Authorization: Bearer my_username"]
        );
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testAddAuthenticationHeaderWithGithubToken.
    #[test]
    fn composer_auth_helper_adds_github_token_only_to_api_origin() {
        let config = config_with_basic("github.com", "my_username", "x-oauth-basic");
        let api =
            authentication_options(&config, "github.com", "https://api.github.com/", &[]).unwrap();
        let web =
            authentication_options(&config, "github.com", "https://github.com/owner/repo", &[])
                .unwrap();

        assert_eq!(api.header_lines(), ["Authorization: token my_username"]);
        assert_eq!(api.diagnostic(), Some("Using GitHub token authentication"));
        assert!(web.headers().is_empty());
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testAddAuthenticationHeaderWithGitlabOathToken.
    #[test]
    fn composer_auth_helper_adds_gitlab_oauth_header() {
        let config = config_with_basic("gitlab.com", "my_username", "oauth2");
        let options =
            authentication_options(&config, "gitlab.com", "https://api.gitlab.com/", &[]).unwrap();

        assert_eq!(
            options.header_lines(),
            ["Authorization: Bearer my_username"]
        );
        assert_eq!(
            options.diagnostic(),
            Some("Using GitLab OAuth token authentication")
        );
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testAddAuthenticationOptionsForClientCertificate.
    #[test]
    fn composer_auth_helper_builds_client_certificate_options() {
        let certificate = serde_json::json!({
            "local_cert": "certificate value",
            "local_pk": "key value",
            "passphrase": "passphrase value",
        });
        let config = config_with_basic(
            "example.org",
            "client-certificate",
            &certificate.to_string(),
        );
        let options =
            authentication_options(&config, "example.org", "https://example.org/", &[]).unwrap();

        assert_eq!(
            options.client_certificate(),
            Some(&ClientCertificateOptions {
                local_cert: "certificate value".to_string(),
                local_pk: "key value".to_string(),
                passphrase: "passphrase value".to_string(),
            })
        );
        assert!(options.headers().is_empty());
        assert!(!format!("{options:?}").contains("passphrase value"));
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testAddAuthenticationHeaderWithGitlabPrivateToken.
    #[test]
    fn composer_auth_helper_adds_gitlab_private_token_headers() {
        for marker in ["private-token", "gitlab-ci-token"] {
            let config = config_with_basic("gitlab.com", "my_username", marker);
            let options =
                authentication_options(&config, "gitlab.com", "https://api.gitlab.com/", &[])
                    .unwrap();
            assert_eq!(options.header_lines(), ["PRIVATE-TOKEN: my_username"]);
            assert_eq!(
                options.diagnostic(),
                Some("Using GitLab private token authentication")
            );
        }
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testAddAuthenticationHeaderWithBitbucketOathToken.
    #[test]
    fn composer_auth_helper_adds_bitbucket_oauth_bearer_header() {
        let config = config_with_basic("bitbucket.org", "x-token-auth", "my_password");
        let options = authentication_options(
            &config,
            "bitbucket.org",
            "https://bitbucket.org/site/oauth2/authorize",
            &[],
        )
        .unwrap();

        assert_eq!(
            options.header_lines(),
            ["Authorization: Bearer my_password"]
        );
        assert_eq!(
            options.diagnostic(),
            Some("Using Bitbucket OAuth token authentication")
        );
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testAddAuthenticationHeaderWithBitbucketPublicUrl.
    #[test]
    fn composer_auth_helper_omits_auth_for_bitbucket_public_downloads() {
        let config = config_with_basic("bitbucket.org", "x-token-auth", "my_password");
        for url in [
            "https://bitbucket.org/user/repo/downloads/whatever",
            "https://bbuseruploads.s3.amazonaws.com/id/downloads/file",
        ] {
            let options =
                authentication_options(&config, "bitbucket.org", url, &["Connection: close"])
                    .unwrap();
            assert_eq!(options.header_lines(), ["Connection: close"]);
            assert_eq!(options.diagnostic(), None);
        }
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testAddAuthenticationHeaderWithBasicHttpAuthentication.
    #[test]
    fn composer_auth_helper_adds_basic_authentication_for_provider_cases() {
        let cases = [
            (
                BITBUCKET_ACCESS_TOKEN_URL,
                "bitbucket.org",
                "x-token-auth",
                "my_password",
                "Authorization: Basic eC10b2tlbi1hdXRoOm15X3Bhc3N3b3Jk",
            ),
            (
                "https://some-api.url.com",
                "some-api.url.com",
                "my_username",
                "my_password",
                "Authorization: Basic bXlfdXNlcm5hbWU6bXlfcGFzc3dvcmQ=",
            ),
            (
                "https://gitlab.com",
                "gitlab.com",
                "my_username",
                "my_password",
                "Authorization: Basic bXlfdXNlcm5hbWU6bXlfcGFzc3dvcmQ=",
            ),
        ];

        for (url, origin, username, password, expected) in cases {
            let config = config_with_basic(origin, username, password);
            let options = authentication_options(&config, origin, url, &[]).unwrap();
            assert_eq!(options.header_lines(), [expected]);
            assert_eq!(
                options.diagnostic(),
                Some(format!(
                    "Using HTTP basic authentication with username \"{username}\""
                ))
                .as_deref()
            );
        }
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testAddAuthenticationHeaderWithBasicHttpAuthenticationMasksTokenUsername.
    #[test]
    fn composer_auth_helper_masks_token_username_only_in_diagnostic() {
        let token = "ghp_1234567890abcdefghijklmnopqrstuvwxyzAB";
        let config = config_with_basic("some-api.url.com", token, "x-oauth-basic");
        let options =
            authentication_options(&config, "some-api.url.com", "https://some-api.url.com", &[])
                .unwrap();

        assert_eq!(
            options.header_lines(),
            [
                "Authorization: Basic Z2hwXzEyMzQ1Njc4OTBhYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ekFCOngtb2F1dGgtYmFzaWM="
            ]
        );
        assert_eq!(
            options.diagnostic(),
            Some("Using HTTP basic authentication with username \"ghp***\"")
        );
        assert!(!format!("{options:?}").contains(token));
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testAddAuthenticationHeaderWithCustomHeaders.
    #[test]
    fn composer_auth_helper_adds_validated_custom_headers() {
        let custom = serde_json::json!(["API-TOKEN: abc123", "X-CUSTOM-HEADER: value"]);
        let config = config_with_basic("example.org", &custom.to_string(), "custom-headers");
        let options = authentication_options(
            &config,
            "example.org",
            "https://example.org/packages.json",
            &["Connection: close"],
        )
        .unwrap();

        assert_eq!(
            options.header_lines(),
            [
                "Connection: close",
                "API-TOKEN: abc123",
                "X-CUSTOM-HEADER: value"
            ]
        );
        assert_eq!(
            options.diagnostic(),
            Some("Using custom HTTP headers for authentication")
        );

        let injected = serde_json::json!(["X-Token: safe\r\nX-Leak: secret"]);
        let invalid = config_with_basic("example.org", &injected.to_string(), "custom-headers");
        assert_eq!(
            authentication_options(&invalid, "example.org", "https://example.org", &[]),
            Err(AuthenticationPolicyError::InvalidCustomHeaders)
        );
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testIsPublicBitBucketDownloadWithBitbucketPublicUrl.
    #[test]
    fn composer_auth_helper_recognizes_bitbucket_public_downloads() {
        assert!(is_public_bitbucket_download(
            "https://bitbucket.org/user/repo/downloads/whatever"
        ));
        assert!(is_public_bitbucket_download(
            "https://bbuseruploads.s3.amazonaws.com/id/downloads/file"
        ));
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testIsPublicBitBucketDownloadWithNonBitbucketPublicUrl.
    #[test]
    fn composer_auth_helper_rejects_non_public_bitbucket_url() {
        assert!(!is_public_bitbucket_download(
            "https://bitbucket.org/site/oauth2/authorize"
        ));
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testPromptAuthIfNeededGitLabNoAuthChange.
    #[test]
    fn composer_auth_helper_does_not_retry_unchanged_gitlab_credentials() {
        let current = basic("gitlab-user", "gitlab-password");
        let decision =
            authentication_retry_decision("gitlab.com", 404, 0, Some(&current), Some(&current));

        assert_eq!(decision, AuthenticationRetryDecision::no_retry());
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testPromptAuthIfNeededMultipleBitbucketDownloads.
    #[test]
    fn composer_auth_helper_retries_parallel_bitbucket_downloads_with_refreshed_token() {
        let client = basic("bitbucket_client_id", "bitbucket_client_secret");
        let token = basic("x-token-auth", "bitbucket_access_token");
        let first =
            authentication_retry_decision("bitbucket.org", 401, 0, Some(&client), Some(&token));
        let sibling =
            authentication_retry_decision("bitbucket.org", 401, 0, Some(&token), Some(&token));

        assert_eq!(first, AuthenticationRetryDecision::retry(Some(token)));
        assert_eq!(sibling, AuthenticationRetryDecision::retry(None));
    }

    // Ported from Composer\Test\Util\AuthHelperTest::
    // testPromptAuthIfNeededMultipleGithubDownloads.
    #[test]
    fn composer_auth_helper_retries_parallel_github_download_once_without_prompting() {
        let token = basic("github-token", "x-oauth-basic");
        assert_eq!(
            authentication_retry_decision("github.com", 403, 0, Some(&token), None),
            AuthenticationRetryDecision::retry(None)
        );
        assert_eq!(
            authentication_retry_decision("github.com", 403, 1, Some(&token), None),
            AuthenticationRetryDecision::no_retry()
        );
    }
}
