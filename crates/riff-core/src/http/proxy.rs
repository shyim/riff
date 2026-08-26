//! Typed proxy configuration shared by HTTP backends and diagnostics.

use std::collections::HashMap;

use super::NoProxyPattern;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProxyParseError {
    #[error("Invalid proxy URL {0:?}")]
    InvalidUrl(String),
    #[error("Proxy URL must use http or https: {0:?}")]
    UnsupportedScheme(String),
    #[error("Proxy URL must contain a valid host and port: {0:?}")]
    MissingEndpoint(String),
}

/// A validated and normalized proxy environment item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyItem {
    url: String,
    auth: Option<(String, Option<String>)>,
    status: String,
}

impl ProxyItem {
    pub fn parse(value: &str) -> Result<Self, ProxyParseError> {
        if value
            .bytes()
            .any(|byte| matches!(byte, b'\r' | b'\n' | b'\t'))
        {
            return Err(ProxyParseError::InvalidUrl(value.to_owned()));
        }
        let explicit_scheme = value.contains("://");
        if !explicit_scheme && !value.contains(':') {
            return Err(ProxyParseError::MissingEndpoint(value.to_owned()));
        }
        let candidate = if explicit_scheme {
            value.to_owned()
        } else {
            format!("http://{value}")
        };
        let parsed = url::Url::parse(&candidate)
            .map_err(|_| ProxyParseError::InvalidUrl(value.to_owned()))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(ProxyParseError::UnsupportedScheme(value.to_owned()));
        }
        let host = parsed
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| ProxyParseError::MissingEndpoint(value.to_owned()))?;
        if parsed.port() == Some(0) {
            return Err(ProxyParseError::MissingEndpoint(value.to_owned()));
        }
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| ProxyParseError::MissingEndpoint(value.to_owned()))?;
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        let url = format!("{}://{host}:{port}", parsed.scheme());
        let auth = (!parsed.username().is_empty()).then(|| {
            (
                parsed.username().to_owned(),
                parsed.password().map(ToOwned::to_owned),
            )
        });
        let redacted_auth = auth.as_ref().map(|(_, password)| {
            if password.is_some() {
                "***:***@"
            } else {
                "***@"
            }
        });
        let status = format!(
            "{}://{}{host}:{port}",
            parsed.scheme(),
            redacted_auth.unwrap_or_default()
        );
        Ok(Self { url, auth, status })
    }

    pub fn into_request(self) -> ProxyRequest {
        ProxyRequest {
            url: Some(self.url),
            auth: self.auth,
            status: self.status,
            excluded_by_no_proxy: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyStreamOptions {
    pub proxy: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub request_full_uri: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProxyCurlOptions {
    /// `Some("")` explicitly disables libcurl's implicit environment proxy.
    pub proxy: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub ca_file: Option<String>,
    pub ca_path: Option<String>,
}

/// Proxy selection for one HTTP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyRequest {
    url: Option<String>,
    auth: Option<(String, Option<String>)>,
    status: String,
    excluded_by_no_proxy: bool,
}

impl ProxyRequest {
    pub fn none() -> Self {
        Self {
            url: None,
            auth: None,
            status: String::new(),
            excluded_by_no_proxy: false,
        }
    }

    pub fn excluded() -> Self {
        Self {
            status: "excluded by no_proxy".to_owned(),
            excluded_by_no_proxy: true,
            ..Self::none()
        }
    }

    pub fn is_secure(&self) -> bool {
        self.url
            .as_deref()
            .is_some_and(|url| url.starts_with("https://"))
    }

    pub fn is_excluded_by_no_proxy(&self) -> bool {
        self.excluded_by_no_proxy
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn formatted_status(&self, format: Option<&str>) -> Result<String, String> {
        if self.status.is_empty() || format.is_none() {
            return Ok(self.status.clone());
        }
        let format = format.unwrap();
        if format.matches("%s").count() != 1 {
            return Err("Proxy status format must contain exactly one %s placeholder".to_owned());
        }
        Ok(format.replacen("%s", &self.status, 1))
    }

    pub fn stream_options(&self) -> Option<ProxyStreamOptions> {
        let url = self.url.as_ref()?;
        let parsed = url::Url::parse(url).ok()?;
        let transport = if parsed.scheme() == "https" {
            "ssl"
        } else {
            "tcp"
        };
        let proxy = format!(
            "{transport}://{}:{}",
            parsed.host_str()?,
            parsed.port_or_known_default()?
        );
        let (username, password) = self.auth.as_ref().map_or((None, None), |(user, password)| {
            (
                Some(percent_decode(user)),
                password.as_deref().map(percent_decode),
            )
        });
        Some(ProxyStreamOptions {
            proxy,
            username,
            password,
            request_full_uri: parsed.scheme() == "http",
        })
    }

    pub fn curl_options(&self, ca_file: Option<&str>, ca_path: Option<&str>) -> ProxyCurlOptions {
        let Some(url) = self.url.as_ref() else {
            return ProxyCurlOptions {
                proxy: Some(String::new()),
                ..Default::default()
            };
        };
        let (username, password) = self.auth.as_ref().map_or((None, None), |(user, password)| {
            (Some(user.clone()), password.clone())
        });
        ProxyCurlOptions {
            proxy: Some(url.clone()),
            username,
            password,
            ca_file: self
                .is_secure()
                .then(|| ca_file.map(ToOwned::to_owned))
                .flatten(),
            ca_path: self
                .is_secure()
                .then(|| ca_path.map(ToOwned::to_owned))
                .flatten(),
        }
    }
}

fn percent_decode(value: &str) -> String {
    urlencoding::decode(value)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| value.to_owned())
}

/// Immutable proxy selection snapshot built from process environment variables.
#[derive(Debug, Clone, Default)]
pub struct ProxyEnvironment {
    http: Option<String>,
    https: Option<String>,
    no_proxy: Option<String>,
}

impl ProxyEnvironment {
    pub fn from_map(environment: &HashMap<String, String>) -> Self {
        let value = |lower: &str, upper: &str| {
            environment
                .get(lower)
                .or_else(|| environment.get(upper))
                .cloned()
        };
        let http =
            value("http_proxy", "HTTP_PROXY").or_else(|| value("cgi_http_proxy", "CGI_HTTP_PROXY"));
        Self {
            http,
            https: value("https_proxy", "HTTPS_PROXY"),
            no_proxy: value("no_proxy", "NO_PROXY"),
        }
    }

    pub fn for_request(&self, request_url: &str) -> Result<ProxyRequest, ProxyParseError> {
        let request_url = url::Url::parse(request_url)
            .map_err(|_| ProxyParseError::InvalidUrl(request_url.to_owned()))?;
        let proxy = match request_url.scheme() {
            "http" => self.http.as_deref(),
            "https" => self.https.as_deref(),
            _ => None,
        };
        let Some(proxy) = proxy else {
            return Ok(ProxyRequest::none());
        };
        if self
            .no_proxy
            .as_deref()
            .is_some_and(|pattern| NoProxyPattern::new(pattern).matches(&request_url))
        {
            return Ok(ProxyRequest::excluded());
        }
        ProxyItem::parse(proxy).map(ProxyItem::into_request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(values: &[(&str, &str)]) -> HashMap<String, String> {
        values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn composer_proxy_item_rejects_malformed_urls() {
        for value in [
            "http://user\rname@localhost:80",
            "http://user\nname@localhost:80",
            "http://user\tname@localhost:80",
            "localhost",
            "scheme://localhost",
            "http://localhost:0",
            "http://localhost:65536",
        ] {
            assert!(ProxyItem::parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn composer_proxy_item_normalizes_and_redacts_status_urls() {
        for (value, expected) in [
            ("http://proxy.com:8888", "http://proxy.com:8888"),
            ("HTTP://proxy.com:8888", "http://proxy.com:8888"),
            ("proxy.com:80", "http://proxy.com:80"),
            ("http://proxy.com", "http://proxy.com:80"),
            ("https://proxy.com", "https://proxy.com:443"),
            ("http://user@proxy.com:6180", "http://***@proxy.com:6180"),
            (
                "http://user:p%40ss@proxy.com:6180",
                "http://***:***@proxy.com:6180",
            ),
        ] {
            assert_eq!(
                ProxyItem::parse(value).unwrap().into_request().status(),
                expected
            );
        }
    }

    #[test]
    fn composer_request_proxy_none_factory_disables_implicit_proxies() {
        let proxy = ProxyRequest::none();
        assert_eq!(proxy.curl_options(None, None).proxy.as_deref(), Some(""));
        assert_eq!(proxy.stream_options(), None);
        assert_eq!(proxy.status(), "");
    }

    #[test]
    fn composer_request_proxy_no_proxy_factory_marks_exclusion() {
        let proxy = ProxyRequest::excluded();
        assert_eq!(proxy.curl_options(None, None).proxy.as_deref(), Some(""));
        assert_eq!(proxy.stream_options(), None);
        assert_eq!(proxy.status(), "excluded by no_proxy");
        assert!(proxy.is_excluded_by_no_proxy());
    }

    #[test]
    fn composer_request_proxy_reports_secure_proxy_protocols() {
        assert!(!ProxyItem::parse("http://proxy.com:80")
            .unwrap()
            .into_request()
            .is_secure());
        assert!(ProxyItem::parse("https://proxy.com:443")
            .unwrap()
            .into_request()
            .is_secure());
        assert!(!ProxyRequest::none().is_secure());
    }

    #[test]
    fn composer_request_proxy_rejects_invalid_status_formats() {
        let proxy = ProxyItem::parse("http://proxy.com:80")
            .unwrap()
            .into_request();
        assert!(proxy.formatted_status(Some("using proxy")).is_err());
        assert!(proxy.formatted_status(Some("%s and %s")).is_err());
    }

    #[test]
    fn composer_request_proxy_formats_status() {
        let proxy = ProxyItem::parse("http://proxy.com:80")
            .unwrap()
            .into_request();
        assert_eq!(
            ProxyRequest::none()
                .formatted_status(Some("proxy (%s)"))
                .unwrap(),
            ""
        );
        assert_eq!(proxy.formatted_status(None).unwrap(), "http://proxy.com:80");
        assert_eq!(
            proxy.formatted_status(Some("proxy (%s)")).unwrap(),
            "proxy (http://proxy.com:80)"
        );
    }

    #[test]
    fn composer_request_proxy_builds_curl_options() {
        let none = ProxyRequest::none().curl_options(None, None);
        assert_eq!(none.proxy.as_deref(), Some(""));
        let plain = ProxyItem::parse("http://proxy.com:80")
            .unwrap()
            .into_request()
            .curl_options(None, None);
        assert_eq!(plain.proxy.as_deref(), Some("http://proxy.com:80"));
        let auth = ProxyItem::parse("http://user:p%40ss@proxy.com:80")
            .unwrap()
            .into_request()
            .curl_options(None, None);
        assert_eq!(auth.username.as_deref(), Some("user"));
        assert_eq!(auth.password.as_deref(), Some("p%40ss"));
    }

    #[test]
    fn composer_request_proxy_builds_secure_curl_options() {
        let ca_file = ProxyItem::parse("https://proxy.com:443")
            .unwrap()
            .into_request()
            .curl_options(Some("/certs/bundle.pem"), None);
        assert_eq!(ca_file.ca_file.as_deref(), Some("/certs/bundle.pem"));
        let ca_path = ProxyItem::parse("https://user:p%40ss@proxy.com:443")
            .unwrap()
            .into_request()
            .curl_options(None, Some("/certs"));
        assert_eq!(ca_path.ca_path.as_deref(), Some("/certs"));
        assert_eq!(ca_path.username.as_deref(), Some("user"));
    }

    #[test]
    fn composer_proxy_manager_rejects_bad_selected_proxy_urls() {
        let manager = ProxyEnvironment::from_map(&environment(&[("http_proxy", "localhost")]));
        assert!(manager.for_request("http://example.com").is_err());
    }

    #[test]
    fn composer_proxy_manager_prefers_lowercase_environment_variables() {
        for (values, request) in [
            (
                vec![
                    ("HTTP_PROXY", "http://upper.com"),
                    ("http_proxy", "http://lower.com"),
                ],
                "http://repo.org",
            ),
            (
                vec![
                    ("CGI_HTTP_PROXY", "http://upper.com"),
                    ("cgi_http_proxy", "http://lower.com"),
                ],
                "http://repo.org",
            ),
            (
                vec![
                    ("HTTPS_PROXY", "http://upper.com"),
                    ("https_proxy", "http://lower.com"),
                ],
                "https://repo.org",
            ),
        ] {
            let manager = ProxyEnvironment::from_map(&environment(&values));
            assert_eq!(
                manager.for_request(request).unwrap().status(),
                "http://lower.com:80"
            );
        }
    }

    #[test]
    fn composer_proxy_manager_uses_cgi_proxy_only_as_http_fallback() {
        let cgi =
            ProxyEnvironment::from_map(&environment(&[("CGI_HTTP_PROXY", "http://cgi.com:80")]));
        assert_eq!(
            cgi.for_request("http://repo.org").unwrap().status(),
            "http://cgi.com:80"
        );
        let http = ProxyEnvironment::from_map(&environment(&[
            ("http_proxy", "http://http.com:80"),
            ("CGI_HTTP_PROXY", "http://cgi.com:80"),
        ]));
        assert_eq!(
            http.for_request("http://repo.org").unwrap().status(),
            "http://http.com:80"
        );
    }

    #[test]
    fn composer_proxy_manager_does_not_cross_protocol_proxy_settings() {
        let https =
            ProxyEnvironment::from_map(&environment(&[("https_proxy", "https://proxy.com:443")]));
        assert_eq!(https.for_request("http://repo.org").unwrap().status(), "");
        let http =
            ProxyEnvironment::from_map(&environment(&[("http_proxy", "http://proxy.com:80")]));
        assert_eq!(http.for_request("https://repo.org").unwrap().status(), "");
    }

    #[test]
    fn composer_proxy_manager_builds_request_options_and_no_proxy_exclusions() {
        let manager = ProxyEnvironment::from_map(&environment(&[
            ("http_proxy", "http://user:p%40ss@proxy.com"),
            ("https_proxy", "https://proxy.com:443"),
            ("no_proxy", "other.repo.org"),
        ]));
        let http = manager.for_request("http://repo.org").unwrap();
        assert_eq!(
            http.stream_options(),
            Some(ProxyStreamOptions {
                proxy: "tcp://proxy.com:80".to_owned(),
                username: Some("user".to_owned()),
                password: Some("p@ss".to_owned()),
                request_full_uri: true,
            })
        );
        let https = manager.for_request("https://repo.org").unwrap();
        assert_eq!(https.stream_options().unwrap().proxy, "ssl://proxy.com:443");
        let excluded = manager.for_request("https://other.repo.org").unwrap();
        assert!(excluded.is_excluded_by_no_proxy());
        assert_eq!(excluded.status(), "excluded by no_proxy");
    }
}
