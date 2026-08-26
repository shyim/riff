//! HTTP transport policy shared by package metadata and archive downloads.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use regex::Regex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use riff_semver::Semver;
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use crate::url_utils::{is_allowed_redirect, sanitize_url};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HttpTransportPolicyError {
    #[error("invalid HTTP URL: {0}")]
    InvalidUrl(String),
    #[error("invalid HTTP header")]
    InvalidHeader,
}

/// Credentials embedded in a URL, decoded and scoped to their request origin.
#[derive(Clone, PartialEq, Eq)]
pub struct UrlAuthentication {
    origin: String,
    username: String,
    password: String,
}

impl UrlAuthentication {
    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn password(&self) -> &str {
        &self.password
    }
}

impl fmt::Debug for UrlAuthentication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UrlAuthentication")
            .field("origin", &self.origin)
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .finish()
    }
}

/// A request URL with any user-info removed before it reaches the transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedHttpUrl {
    url: Url,
    authentication: Option<UrlAuthentication>,
}

impl PreparedHttpUrl {
    pub fn parse(input: &str) -> Result<Self, HttpTransportPolicyError> {
        let mut url = Url::parse(input)
            .map_err(|_| HttpTransportPolicyError::InvalidUrl(sanitize_url(input)))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(HttpTransportPolicyError::InvalidUrl(sanitize_url(input)));
        }

        let authentication = if url.username().is_empty() && url.password().is_none() {
            None
        } else {
            let username = urlencoding::decode(url.username())
                .map_err(|_| HttpTransportPolicyError::InvalidUrl(sanitize_url(input)))?
                .into_owned();
            let password = urlencoding::decode(url.password().unwrap_or_default())
                .map_err(|_| HttpTransportPolicyError::InvalidUrl(sanitize_url(input)))?
                .into_owned();
            let origin = url.port().map_or_else(
                || url.host_str().unwrap().to_string(),
                |port| format!("{}:{port}", url.host_str().unwrap()),
            );
            Some(UrlAuthentication {
                origin,
                username,
                password,
            })
        };

        url.set_username("")
            .map_err(|_| HttpTransportPolicyError::InvalidUrl(sanitize_url(input)))?;
        url.set_password(None)
            .map_err(|_| HttpTransportPolicyError::InvalidUrl(sanitize_url(input)))?;

        Ok(Self {
            url,
            authentication,
        })
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn authentication(&self) -> Option<&UrlAuthentication> {
        self.authentication.as_ref()
    }
}

/// TLS settings represented independently from the underlying HTTP backend.
///
/// The resolved defaults mirror Composer's security posture while rustls is
/// responsible for cipher selection and certificate verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsOptions {
    verify_peer: Option<bool>,
    sni: Option<bool>,
    verify_depth: Option<u8>,
    disable_compression: Option<bool>,
    allow_self_signed: Option<bool>,
    cafile: Option<PathBuf>,
}

impl TlsOptions {
    pub fn secure_defaults() -> Self {
        Self {
            verify_peer: Some(true),
            sni: Some(true),
            verify_depth: Some(7),
            disable_compression: Some(true),
            allow_self_signed: Some(false),
            cafile: None,
        }
    }

    pub fn overrides() -> Self {
        Self {
            verify_peer: None,
            sni: None,
            verify_depth: None,
            disable_compression: None,
            allow_self_signed: None,
            cafile: None,
        }
    }

    pub fn with_allow_self_signed(mut self, allow: bool) -> Self {
        self.allow_self_signed = Some(allow);
        self
    }

    pub fn with_cafile(mut self, path: impl Into<PathBuf>) -> Self {
        self.cafile = Some(path.into());
        self
    }

    fn merge(&mut self, overrides: &Self) {
        if overrides.verify_peer.is_some() {
            self.verify_peer = overrides.verify_peer;
        }
        if overrides.sni.is_some() {
            self.sni = overrides.sni;
        }
        if overrides.verify_depth.is_some() {
            self.verify_depth = overrides.verify_depth;
        }
        if overrides.disable_compression.is_some() {
            self.disable_compression = overrides.disable_compression;
        }
        if overrides.allow_self_signed.is_some() {
            self.allow_self_signed = overrides.allow_self_signed;
        }
        if overrides.cafile.is_some() {
            self.cafile.clone_from(&overrides.cafile);
        }
    }

    pub fn verify_peer(&self) -> bool {
        self.verify_peer.unwrap_or(true)
    }

    pub fn sni(&self) -> bool {
        self.sni.unwrap_or(true)
    }

    pub fn verify_depth(&self) -> u8 {
        self.verify_depth.unwrap_or(7)
    }

    pub fn disable_compression(&self) -> bool {
        self.disable_compression.unwrap_or(true)
    }

    pub fn allow_self_signed(&self) -> bool {
        self.allow_self_signed.unwrap_or(false)
    }

    pub fn cafile(&self) -> Option<&Path> {
        self.cafile.as_deref()
    }
}

type UrlAccessGuard = Arc<dyn Fn(&Url) -> bool + Send + Sync>;

/// Per-request headers and policy controls.
#[derive(Clone)]
pub struct HttpRequestOptions {
    headers: HeaderMap,
    tls: TlsOptions,
    prevent_url_access: Option<UrlAccessGuard>,
    retry_auth_failure: bool,
    authentication_retry_headers: HeaderMap,
}

impl fmt::Debug for HttpRequestOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpRequestOptions")
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("tls", &self.tls)
            .field(
                "prevent_url_access",
                &self.prevent_url_access.as_ref().map(|_| "<callback>"),
            )
            .field("retry_auth_failure", &self.retry_auth_failure)
            .field(
                "authentication_retry_headers",
                &self.authentication_retry_headers.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Default for HttpRequestOptions {
    fn default() -> Self {
        let mut headers = HeaderMap::new();
        headers.insert("accept-encoding", HeaderValue::from_static("gzip"));
        Self {
            headers,
            tls: TlsOptions::secure_defaults(),
            prevent_url_access: None,
            retry_auth_failure: true,
            authentication_retry_headers: HeaderMap::new(),
        }
    }
}

impl HttpRequestOptions {
    /// An empty override set suitable for merging into configured defaults.
    pub fn overrides() -> Self {
        Self {
            headers: HeaderMap::new(),
            tls: TlsOptions::overrides(),
            prevent_url_access: None,
            retry_auth_failure: true,
            authentication_retry_headers: HeaderMap::new(),
        }
    }

    pub fn with_header(
        mut self,
        name: &str,
        value: &str,
    ) -> Result<Self, HttpTransportPolicyError> {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| HttpTransportPolicyError::InvalidHeader)?;
        let value =
            HeaderValue::from_str(value).map_err(|_| HttpTransportPolicyError::InvalidHeader)?;
        self.headers.insert(name, value);
        Ok(self)
    }

    pub fn with_tls(mut self, tls: TlsOptions) -> Self {
        self.tls = tls;
        self
    }

    pub fn with_prevent_url_access<F>(mut self, callback: F) -> Self
    where
        F: Fn(&Url) -> bool + Send + Sync + 'static,
    {
        self.prevent_url_access = Some(Arc::new(callback));
        self
    }

    pub fn with_retry_auth_failure(mut self, enabled: bool) -> Self {
        self.retry_auth_failure = enabled;
        self
    }

    pub fn with_authentication_retry_header(
        mut self,
        name: &str,
        value: &str,
    ) -> Result<Self, HttpTransportPolicyError> {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| HttpTransportPolicyError::InvalidHeader)?;
        let value =
            HeaderValue::from_str(value).map_err(|_| HttpTransportPolicyError::InvalidHeader)?;
        self.authentication_retry_headers.insert(name, value);
        Ok(self)
    }

    pub fn merged_with(&self, overrides: &Self) -> Self {
        let mut merged = self.clone();
        for (name, value) in &overrides.headers {
            merged.headers.insert(name.clone(), value.clone());
        }
        merged.tls.merge(&overrides.tls);
        if overrides.prevent_url_access.is_some() {
            merged
                .prevent_url_access
                .clone_from(&overrides.prevent_url_access);
        }
        merged.retry_auth_failure = overrides.retry_auth_failure;
        for (name, value) in &overrides.authentication_retry_headers {
            merged
                .authentication_retry_headers
                .insert(name.clone(), value.clone());
        }
        merged
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn tls(&self) -> &TlsOptions {
        &self.tls
    }

    pub(crate) fn access_is_blocked(&self, url: &Url) -> bool {
        self.prevent_url_access
            .as_ref()
            .is_some_and(|callback| callback(url))
    }

    pub(crate) fn retry_auth_failure(&self) -> bool {
        self.retry_auth_failure
    }

    pub(crate) fn authentication_retry_headers(&self) -> &HeaderMap {
        &self.authentication_retry_headers
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpNoticeLevel {
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpNotice {
    pub level: HttpNoticeLevel,
    pub message: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct VersionedHttpNotice {
    pub versions: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct HttpWarningMetadata {
    pub warning: Option<String>,
    pub info: Option<String>,
    pub warning_versions: Option<String>,
    pub info_versions: Option<String>,
    #[serde(default)]
    pub warnings: Vec<VersionedHttpNotice>,
    #[serde(default)]
    pub infos: Vec<VersionedHttpNotice>,
}

/// Select repository-provided notices applicable to the running client.
pub fn applicable_http_notices(
    source_url: &str,
    client_version: &str,
    metadata: &HttpWarningMetadata,
    decorated: bool,
) -> Vec<HttpNotice> {
    let mut notices = Vec::new();
    let sanitized_url = sanitize_url(source_url);

    for (level, message, constraint) in [
        (
            HttpNoticeLevel::Warning,
            metadata.warning.as_deref(),
            metadata.warning_versions.as_deref(),
        ),
        (
            HttpNoticeLevel::Info,
            metadata.info.as_deref(),
            metadata.info_versions.as_deref(),
        ),
    ] {
        if let Some(message) = message.filter(|_| version_matches(client_version, constraint)) {
            notices.push(HttpNotice {
                level,
                message: format_notice(level, &sanitized_url, message, decorated),
            });
        }
    }

    for (level, specs) in [
        (HttpNoticeLevel::Warning, metadata.warnings.as_slice()),
        (HttpNoticeLevel::Info, metadata.infos.as_slice()),
    ] {
        notices.extend(
            specs
                .iter()
                .filter(|spec| version_matches(client_version, Some(&spec.versions)))
                .map(|spec| HttpNotice {
                    level,
                    message: format_notice(level, &sanitized_url, &spec.message, decorated),
                }),
        );
    }

    notices
}

fn version_matches(version: &str, constraint: Option<&str>) -> bool {
    constraint.is_none_or(|constraint| Semver::satisfies(version, constraint))
}

fn format_notice(level: HttpNoticeLevel, url: &str, message: &str, decorated: bool) -> String {
    static ANSI_ESCAPE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new("\\x1b\\[[;\\d]*m").expect("valid ANSI escape pattern"));
    let message = if decorated {
        message.to_string()
    } else {
        ANSI_ESCAPE.replace_all(message, "").into_owned()
    };
    let label = match level {
        HttpNoticeLevel::Warning => "Warning",
        HttpNoticeLevel::Info => "Info",
    };
    format!("{label} from {url}: {message}")
}

/// Progress state independent of any terminal rendering implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferProgress {
    bytes_max: u64,
    enabled: bool,
    last_percent: Option<u8>,
}

impl TransferProgress {
    pub fn new(enabled: bool) -> Self {
        Self {
            bytes_max: 0,
            enabled,
            last_percent: None,
        }
    }

    pub fn set_file_size(&mut self, bytes_max: u64) {
        self.bytes_max = bytes_max;
    }

    pub fn bytes_max(&self) -> u64 {
        self.bytes_max
    }

    /// Return a newly reportable five-percent step, excluding the final 100%.
    pub fn observe(&mut self, bytes_transferred: u64) -> Option<u8> {
        if !self.enabled || self.bytes_max == 0 {
            return None;
        }
        let percent = ((bytes_transferred as f64 / self.bytes_max as f64) * 100.0)
            .round()
            .clamp(0.0, 100.0) as u8;
        if percent == 100 || !percent.is_multiple_of(5) || self.last_percent == Some(percent) {
            return None;
        }
        self.last_percent = Some(percent);
        Some(percent)
    }

    pub fn last_percent(&self) -> Option<u8> {
        self.last_percent
    }
}

pub fn redirect_is_allowed(url: &str) -> bool {
    is_allowed_redirect(url)
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::config::AuthConfig;
    use crate::http::{HttpClient, HttpClientConfig, HttpError};

    use super::*;

    struct LocalResponse {
        status: &'static str,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    }

    impl LocalResponse {
        fn ok(body: impl AsRef<[u8]>) -> Self {
            Self {
                status: "200 OK",
                headers: Vec::new(),
                body: body.as_ref().to_vec(),
            }
        }

        fn status(status: &'static str) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: Vec::new(),
            }
        }

        fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
            self.headers.push((name, value.into()));
            self
        }
    }

    async fn serve(
        responses: Vec<LocalResponse>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).await.unwrap();
                    assert_ne!(read, 0);
                    request.extend_from_slice(&buffer[..read]);
                }
                requests.push(String::from_utf8(request).unwrap());

                let mut head = format!(
                    "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    response.status,
                    response.body.len()
                );
                for (name, value) in response.headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str("\r\n");
                stream.write_all(head.as_bytes()).await.unwrap();
                stream.write_all(&response.body).await.unwrap();
            }
            requests
        });
        (format!("http://{address}"), handle)
    }

    fn has_header(request: &str, name: &str, value: &str) -> bool {
        request.lines().any(|line| {
            line.split_once(':')
                .is_some_and(|(actual_name, actual_value)| {
                    actual_name.eq_ignore_ascii_case(name) && actual_value.trim() == value
                })
        })
    }

    // Ported from Composer\Test\Util\HttpDownloaderTest::
    // testCaptureAuthenticationParamsFromUrl.
    #[tokio::test]
    async fn composer_http_downloader_captures_authentication_from_url() {
        let (origin, server) = serve(vec![LocalResponse::ok("ok")]).await;
        let url = origin.replacen("http://", "http://user:pass@", 1);

        let response = HttpClient::new().unwrap().get(&url).await.unwrap();
        assert_eq!(response.status(), 200);

        let requests = server.await.unwrap();
        assert!(has_header(
            &requests[0],
            "authorization",
            "Basic dXNlcjpwYXNz"
        ));
        assert!(!requests[0].contains("user:pass@"));
        let prepared = PreparedHttpUrl::parse(&url).unwrap();
        let authentication = prepared.authentication().unwrap();
        assert_eq!(authentication.username(), "user");
        assert_eq!(authentication.password(), "pass");
        assert_eq!(
            authentication.origin(),
            origin.trim_start_matches("http://")
        );
    }

    // Ported from Composer\Test\Util\HttpDownloaderTest::
    // testPreventUrlAccessCallableBlocksDownload.
    #[tokio::test]
    async fn composer_http_downloader_prevention_callback_blocks_download() {
        let options = HttpRequestOptions::default().with_prevent_url_access(|url| {
            url.host_str() == Some("example.org") && url.path() == "/blocked"
        });
        let error = HttpClient::new()
            .unwrap()
            .get_with_options("https://example.org/blocked", &options)
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Access to \"https://example.org/blocked\" is blocked."
        );
    }

    // Ported from Composer\Test\Util\HttpDownloaderTest::testOutputWarnings.
    #[test]
    fn composer_http_downloader_filters_and_formats_repository_notices() {
        let empty =
            applicable_http_notices("$URL", "2.9.0", &HttpWarningMetadata::default(), false);
        assert!(empty.is_empty());

        let filtered = HttpWarningMetadata {
            warning: Some("old warning msg".to_string()),
            warning_versions: Some("<2.0".to_string()),
            warnings: vec![VersionedHttpNotice {
                versions: "<2.2".to_string(),
                message: "should not appear".to_string(),
            }],
            ..HttpWarningMetadata::default()
        };
        assert!(applicable_http_notices("$URL", "2.9.0", &filtered, false).is_empty());

        let metadata = HttpWarningMetadata {
            warning: Some("old warning msg".to_string()),
            info: Some("old \x1b[32minfo\x1b[0m msg".to_string()),
            warning_versions: Some(">=2.0".to_string()),
            info_versions: Some(">=2.0".to_string()),
            warnings: vec![
                VersionedHttpNotice {
                    versions: "<2.2".to_string(),
                    message: "should not appear".to_string(),
                },
                VersionedHttpNotice {
                    versions: ">=2.2-dev".to_string(),
                    message: "visible warning".to_string(),
                },
            ],
            infos: vec![VersionedHttpNotice {
                versions: ">=2.2-dev".to_string(),
                message: "visible info".to_string(),
            }],
        };
        let notices = applicable_http_notices("$URL", "2.9.0", &metadata, false);

        assert_eq!(
            notices,
            [
                HttpNotice {
                    level: HttpNoticeLevel::Warning,
                    message: "Warning from $URL: old warning msg".to_string(),
                },
                HttpNotice {
                    level: HttpNoticeLevel::Info,
                    message: "Info from $URL: old info msg".to_string(),
                },
                HttpNotice {
                    level: HttpNoticeLevel::Warning,
                    message: "Warning from $URL: visible warning".to_string(),
                },
                HttpNotice {
                    level: HttpNoticeLevel::Info,
                    message: "Info from $URL: visible info".to_string(),
                },
            ]
        );
    }

    // Ported from Composer\Test\Util\RemoteFilesystemTest::testGetOptionsForUrl.
    #[test]
    fn composer_remote_filesystem_builds_default_request_options() {
        let options = HttpRequestOptions::default();
        assert_eq!(options.headers()["accept-encoding"], "gzip");
        assert_eq!(options.headers().len(), 1);
    }

    // Ported from Composer\Test\Util\RemoteFilesystemTest::
    // testGetOptionsForUrlWithAuthorization.
    #[tokio::test]
    async fn composer_remote_filesystem_adds_configured_authorization() {
        let (origin, server) = serve(vec![LocalResponse::ok("authorized")]).await;
        let mut auth = AuthConfig::new();
        auth.set_http_basic("127.0.0.1", "login", "password");

        let bytes = HttpClient::new()
            .unwrap()
            .with_auth(auth)
            .download_bytes(&format!("{origin}/package.zip"))
            .await
            .unwrap();
        assert_eq!(bytes, b"authorized");

        let requests = server.await.unwrap();
        assert!(has_header(
            &requests[0],
            "authorization",
            "Basic bG9naW46cGFzc3dvcmQ="
        ));
    }

    // Ported from Composer\Test\Util\RemoteFilesystemTest::
    // testGetOptionsForUrlWithStreamOptions.
    #[test]
    fn composer_remote_filesystem_merges_tls_overrides() {
        let defaults = HttpRequestOptions::default();
        let overrides = HttpRequestOptions::overrides()
            .with_tls(TlsOptions::overrides().with_allow_self_signed(true));
        let merged = defaults.merged_with(&overrides);

        assert!(merged.tls().allow_self_signed());
        assert!(merged.tls().verify_peer());
        assert!(merged.tls().sni());
    }

    // Ported from Composer\Test\Util\RemoteFilesystemTest::
    // testGetOptionsForUrlWithCallOptionsKeepsHeader.
    #[test]
    fn composer_remote_filesystem_merges_call_headers_with_defaults() {
        let overrides = HttpRequestOptions::overrides()
            .with_header("Foo", "bar")
            .unwrap();
        let merged = HttpRequestOptions::default().merged_with(&overrides);

        assert_eq!(merged.headers()["foo"], "bar");
        assert!(merged.headers().len() > 1);
        assert_eq!(merged.headers()["accept-encoding"], "gzip");
    }

    // Ported from Composer\Test\Util\RemoteFilesystemTest::testCallbackGetFileSize.
    #[test]
    fn composer_remote_filesystem_progress_tracks_file_size() {
        let mut progress = TransferProgress::new(true);
        progress.set_file_size(20);
        assert_eq!(progress.bytes_max(), 20);
    }

    // Ported from Composer\Test\Util\RemoteFilesystemTest::testCallbackGetNotifyProgress.
    #[test]
    fn composer_remote_filesystem_progress_reports_five_percent_steps() {
        let mut progress = TransferProgress::new(true);
        progress.set_file_size(20);

        assert_eq!(progress.observe(10), Some(50));
        assert_eq!(progress.last_percent(), Some(50));
        assert_eq!(progress.observe(10), None);
        assert_eq!(progress.observe(20), None);
    }

    // Ported from Composer\Test\Util\RemoteFilesystemTest::
    // testCallbackGetPassesThrough404.
    #[tokio::test]
    async fn composer_remote_filesystem_passes_through_http_status() {
        let (origin, server) = serve(vec![LocalResponse::status("404 Not Found")]).await;
        let error = HttpClient::new()
            .unwrap()
            .get(&format!("{origin}/missing"))
            .await
            .unwrap_err();
        server.await.unwrap();

        assert!(matches!(error, HttpError::HttpStatus { status: 404, .. }));
    }

    // Ported from Composer\Test\Util\RemoteFilesystemTest::testGetContents.
    #[tokio::test]
    async fn composer_remote_filesystem_gets_contents() {
        let (origin, server) = serve(vec![LocalResponse::ok("testGetContents")]).await;
        let contents = HttpClient::new()
            .unwrap()
            .download_bytes(&format!("{origin}/fixture"))
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(contents, b"testGetContents");
    }

    // Ported from Composer\Test\Util\RemoteFilesystemTest::testCopy.
    #[tokio::test]
    async fn composer_remote_filesystem_copies_response_to_file() {
        let (origin, server) = serve(vec![LocalResponse::ok("testCopy")]).await;
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("copied.txt");

        HttpClient::new()
            .unwrap()
            .download(
                &format!("{origin}/fixture"),
                &destination,
                None::<fn(u64, u64)>,
            )
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(tokio::fs::read(destination).await.unwrap(), b"testCopy");
    }

    // Ported from Composer\Test\Util\RemoteFilesystemTest::
    // testCopyWithNoRetryOnFailure.
    #[tokio::test]
    async fn composer_remote_filesystem_does_not_retry_auth_failure_when_disabled() {
        let (origin, server) = serve(vec![LocalResponse::status("401 Unauthorized")]).await;
        let options = HttpRequestOptions::default()
            .with_retry_auth_failure(false)
            .with_authentication_retry_header("Authorization", "Bearer refreshed")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let error = HttpClient::with_config(HttpClientConfig::new().with_max_retries(0))
            .unwrap()
            .download_with_options(
                &format!("{origin}/private.zip"),
                &temp.path().join("copy.zip"),
                None::<fn(u64, u64)>,
                &options,
            )
            .await
            .unwrap_err();
        let requests = server.await.unwrap();

        assert!(matches!(error, HttpError::HttpStatus { status: 401, .. }));
        assert_eq!(requests.len(), 1);
    }

    // Ported from Composer\Test\Util\RemoteFilesystemTest::
    // testCopyWithSuccessOnRetry.
    #[tokio::test]
    async fn composer_remote_filesystem_copies_after_authentication_retry() {
        let (origin, server) = serve(vec![
            LocalResponse::status("401 Unauthorized"),
            LocalResponse::ok("Copied"),
        ])
        .await;
        let options = HttpRequestOptions::default()
            .with_authentication_retry_header("Authorization", "Bearer refreshed")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("copy.zip");

        HttpClient::with_config(HttpClientConfig::new().with_max_retries(0))
            .unwrap()
            .download_with_options(
                &format!("{origin}/private.zip"),
                &destination,
                None::<fn(u64, u64)>,
                &options,
            )
            .await
            .unwrap();
        let requests = server.await.unwrap();

        assert_eq!(tokio::fs::read(destination).await.unwrap(), b"Copied");
        assert_eq!(requests.len(), 2);
        assert!(!requests[0].contains("Bearer refreshed"));
        assert!(has_header(
            &requests[1],
            "authorization",
            "Bearer refreshed"
        ));
    }

    // Ported from Composer\Test\Util\RemoteFilesystemTest::
    // testRedirectToDisallowedSchemeIsRejected.
    #[tokio::test]
    async fn composer_remote_filesystem_rejects_disallowed_redirect_schemes() {
        for location in [
            "file://localhost/etc/passwd",
            "phar://archive.phar/file",
            "data://text/plain;base64,Zm9v",
        ] {
            let (origin, server) = serve(vec![
                LocalResponse::status("302 Found").with_header("Location", location)
            ])
            .await;
            let error = HttpClient::with_config(HttpClientConfig::new().with_max_retries(0))
                .unwrap()
                .get(&format!("{origin}/redirect"))
                .await
                .unwrap_err();
            server.await.unwrap();

            assert!(
                matches!(error, HttpError::DisallowedRedirect),
                "{location}: {error}"
            );
        }
    }

    // Ported from Composer\Test\Util\RemoteFilesystemTest::
    // testGetOptionsForUrlCreatesSecureTlsDefaults.
    #[test]
    fn composer_remote_filesystem_uses_secure_tls_defaults() {
        let tls = TlsOptions::secure_defaults().with_cafile("/some/path/file.crt");

        assert!(tls.verify_peer());
        assert!(tls.sni());
        assert_eq!(tls.verify_depth(), 7);
        assert!(tls.disable_compression());
        assert!(!tls.allow_self_signed());
        assert_eq!(tls.cafile(), Some(Path::new("/some/path/file.crt")));
    }

    // Ported from Composer\Test\Util\RemoteFilesystemTest::testBitBucketPublicDownload.
    #[tokio::test]
    async fn composer_remote_filesystem_downloads_public_bitbucket_content_without_credentials() {
        assert!(crate::http::is_public_bitbucket_download(
            "https://bitbucket.org/acme/demo/downloads/release.zip"
        ));
        let (origin, server) = serve(vec![LocalResponse::ok("1234")]).await;

        let contents = HttpClient::new()
            .unwrap()
            .download_bytes(&format!("{origin}/public-download"))
            .await
            .unwrap();
        let requests = server.await.unwrap();

        assert_eq!(contents, b"1234");
        assert!(!requests[0].to_ascii_lowercase().contains("authorization:"));
    }
}
