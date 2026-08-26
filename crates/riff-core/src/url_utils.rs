use regex::{Captures, Regex};

/// Redact URL credentials and access tokens before presenting a URL to users.
pub fn sanitize_url(input: &str) -> String {
    let access_token =
        Regex::new(r"(?i)([?&]access_token=)[^&#]*").expect("access-token pattern is valid");
    let sanitized = access_token.replace_all(input, "$1***");
    let user_info =
        Regex::new(r"^(?P<scheme>[A-Za-z][A-Za-z0-9+.-]*://)?(?P<userinfo>[^/@]+)@(?P<rest>.*)$")
            .expect("URL user-info pattern is valid");

    user_info
        .replace(&sanitized, |captures: &Captures<'_>| {
            let prefix = captures.name("scheme").map_or("", |value| value.as_str());
            let user_info = captures.name("userinfo").unwrap().as_str();
            let rest = captures.name("rest").unwrap().as_str();
            let (username, password) = user_info
                .split_once(':')
                .map_or((user_info, None), |(username, password)| {
                    (username, Some(password))
                });
            let username = sanitize_username(username);
            match password {
                Some(_) => format!("{prefix}{username}:***@{rest}"),
                None => format!("{prefix}{username}@{rest}"),
            }
        })
        .into_owned()
}

fn sanitize_username(username: &str) -> String {
    if matches!(username, "x-token-auth" | "gitlab-ci-token") || username.chars().count() < 12 {
        username.to_string()
    } else {
        format!("{}***", username.chars().take(3).collect::<String>())
    }
}

/// Return whether a redirect target is safe for the HTTP downloader.
pub fn is_allowed_redirect(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
}

/// Resolve the credential/config origin for a URL, preserving an explicitly
/// configured GitLab enterprise path or default HTTPS port.
pub fn url_origin(url: &str, gitlab_domains: &[&str]) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return String::new();
    };
    let Some(host) = parsed.host_str() else {
        return String::new();
    };
    for configured in gitlab_domains {
        let authority = configured.split('/').next().unwrap_or(configured);
        let candidate = url::Url::parse(&format!("https://{authority}"));
        let Ok(candidate) = candidate else {
            continue;
        };
        if candidate
            .host_str()
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(host))
            && ports_match(&parsed, &candidate)
        {
            return (*configured).to_owned();
        }
    }
    parsed
        .port()
        .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"))
}

fn ports_match(request: &url::Url, configured: &url::Url) -> bool {
    configured.port().is_none()
        || configured.port() == request.port()
        || (configured.port() == request.port_or_known_default() && request.port().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sanitize_cases() -> &'static [(&'static str, &'static str)] {
        &[
            ("", ""),
            ("https://foo:***@example.org/", "https://foo:bar@example.org/"),
            ("https://foo@example.org/", "https://foo@example.org/"),
            ("https://example.org/", "https://example.org/"),
            ("http://10a***:***@example.org", "http://10a8f08e8d7b7b9:foo@example.org"),
            ("https://foo:***@example.org:123/", "https://foo:bar@example.org:123/"),
            ("https://example.org/foo/bar?access_token=***", "https://example.org/foo/bar?access_token=abcdef"),
            ("https://example.org/foo/bar?foo=bar&access_token=***", "https://example.org/foo/bar?foo=bar&access_token=abcdef"),
            ("https://ghp***:***@github.com/acme/repo", "https://ghp_1234567890abcdefghijklmnopqrstuvwxyzAB:x-oauth-basic@github.com/acme/repo"),
            ("https://git***:***@github.com/acme/repo", "https://github_pat_1234567890abcdefghijkl_1234567890abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVW:x-oauth-basic@github.com/acme/repo"),
            ("http://abc***:***@example.org:123/", "http://abcdefghijkl:bar@example.org:123/"),
            ("https://abc***:***@example.org:123/", "https://abcdefghijklmnop:bar@example.org:123/"),
            ("https://ghp***@github.com/acme/repo", "https://ghp_1234567890abcdefghijklmnopqrstuvwxyzAB@github.com/acme/repo"),
            ("https://git***@github.com/acme/repo", "https://github_pat_1234567890abcdefghijkl_1234567890abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVW@github.com/acme/repo"),
            ("http://10a***@example.org", "http://10a8f08e8d7b7b9@example.org"),
            ("https://abc***@example.org:123/", "https://abcdefghijklmnop@example.org:123/"),
            ("https://x-token-auth:***@bitbucket.org/acme/repo", "https://x-token-auth:secret@bitbucket.org/acme/repo"),
            ("https://gitlab-ci-token:***@gitlab.example.org/", "https://gitlab-ci-token:realtoken@gitlab.example.org/"),
            ("foo:***@example.org/", "foo:bar@example.org/"),
            ("foo@example.org/", "foo@example.org/"),
            ("example.org/", "example.org/"),
            ("10a***:***@example.org", "10a8f08e8d7b7b9:foo@example.org"),
            ("foo:***@example.org:123/", "foo:bar@example.org:123/"),
            ("example.org/foo/bar?access_token=***", "example.org/foo/bar?access_token=abcdef"),
            ("example.org/foo/bar?foo=bar&access_token=***", "example.org/foo/bar?foo=bar&access_token=abcdef"),
            ("abc***:***@example.org:123/", "abcdefghijkl:bar@example.org:123/"),
            ("abc***:***@example.org:123/", "abcdefghijklmnop:bar@example.org:123/"),
            ("ghp***@github.com/acme/repo", "ghp_1234567890abcdefghijklmnopqrstuvwxyzAB@github.com/acme/repo"),
            ("10a***@example.org", "10a8f08e8d7b7b9@example.org"),
            ("abc***@example.org:123/", "abcdefghijklmnop@example.org:123/"),
        ]
    }

    #[test]
    fn composer_url_sanitize_data_provider() {
        for (expected, input) in sanitize_cases() {
            assert_eq!(
                sanitize_url(input),
                *expected,
                "unexpected sanitization for {input}"
            );
        }
    }

    #[test]
    fn composer_url_sanitize_is_idempotent() {
        for (expected, input) in sanitize_cases() {
            assert_eq!(sanitize_url(&sanitize_url(input)), *expected);
        }
    }

    #[test]
    fn composer_url_allowed_redirect_data_provider() {
        for (expected, url) in [
            (true, "http://example.org/foo"),
            (true, "https://example.org/foo"),
            (true, "HTTPS://example.org/foo"),
            (false, "file://localhost/etc/passwd"),
            (false, "file:///etc/passwd"),
            (false, "phar://archive.phar/file"),
            (false, "data://text/plain;base64,Zm9v"),
            (false, "ftp://example.org/foo"),
            (false, "/foo/bar"),
            (false, "example.org/foo"),
        ] {
            assert_eq!(
                is_allowed_redirect(url),
                expected,
                "unexpected redirect policy for {url}"
            );
        }
    }

    #[test]
    fn composer_url_origin_preserves_matching_gitlab_config_origins() {
        for (expected, url, domains) in [
            (
                "gitlab.example.co",
                "https://gitlab.example.co/foo/bar/repository/archive.zip",
                vec!["gitlab.example.com"],
            ),
            (
                "gitlab.example.co",
                "https://gitlab.example.co/foo/bar/repository/archive.zip",
                vec!["gitlab.example.co.uk/gitlab"],
            ),
            (
                "gitlab.example.com",
                "https://gitlab.example.com/foo/bar/repository/archive.zip",
                vec!["gitlab.example.com"],
            ),
            (
                "gitlab.example.com/gitlab",
                "https://gitlab.example.com/foo/bar/repository/archive.zip",
                vec!["gitlab.example.com/gitlab"],
            ),
            (
                "gitlab.example.com:443",
                "https://gitlab.example.com/foo/bar/repository/archive.zip",
                vec!["gitlab.example.com:443"],
            ),
            (
                "gitlab.example.com:443/gitlab",
                "https://gitlab.example.com/foo/bar/repository/archive.zip",
                vec!["gitlab.example.com:443/gitlab"],
            ),
        ] {
            assert_eq!(url_origin(url, &domains), expected, "{url}");
        }
    }
}
