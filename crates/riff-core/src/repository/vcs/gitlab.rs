//! GitLab driver - uses GitLab API for repository access.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use super::driver::{VcsDist, VcsDriver, VcsDriverError, VcsInfo, VcsSource};
use crate::config::AuthConfig;

/// Transport options applied to GitLab API requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitLabRequestOptions {
    pub verify_tls: bool,
}

impl Default for GitLabRequestOptions {
    fn default() -> Self {
        Self { verify_tls: true }
    }
}

/// Preferred protocol for source checkouts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GitLabProtocol {
    #[default]
    Auto,
    Http,
    Ssh,
}

pub type GitLabDist = VcsDist;
pub type GitLabSource = VcsSource;

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitLabLocation {
    host: String,
    authority: String,
    scheme: String,
    base_path: String,
    project_path: String,
    web_url: String,
}

impl GitLabLocation {
    fn api_url(&self) -> String {
        let base_path = if self.base_path.is_empty() {
            String::new()
        } else {
            format!("/{}", self.base_path)
        };
        format!(
            "{}://{}{}/api/v4/projects/{}",
            self.scheme,
            self.authority,
            base_path,
            encode_gitlab_path(&self.project_path)
        )
    }
}

#[derive(Debug)]
struct GitLabApiResponse {
    body: String,
    next_url: Option<String>,
}

/// GitLab driver for GitLab repositories.
pub struct GitLabDriver {
    url: String,
    api_host: String,
    project_path: String,
    api_url: String,
    private_token: Option<String>,
    request_options: GitLabRequestOptions,
    protocol: GitLabProtocol,
    default_branch: Option<String>,
    repository_url: Option<String>,
    web_url: Option<String>,
    tags: Mutex<Option<HashMap<String, String>>>,
    branches: Mutex<Option<HashMap<String, String>>>,
}

impl GitLabDriver {
    /// Creates a driver for standard GitLab hosts and GitLab installations whose
    /// URL contains a `/gitlab` base path.
    pub fn new(url: impl Into<String>) -> Result<Self, VcsDriverError> {
        Self::new_with_domains(url, &[])
    }

    /// Creates a driver using configured GitLab domains. Entries may include a
    /// base path, for example `mycompany.com/nested/gitlab`.
    pub fn new_with_domains(
        url: impl Into<String>,
        domains: &[&str],
    ) -> Result<Self, VcsDriverError> {
        let input = url.into();
        let location = parse_gitlab_location(&input, domains)
            .ok_or_else(|| VcsDriverError::InvalidFormat(format!("Invalid GitLab URL: {input}")))?;
        let api_url = location.api_url();

        Ok(Self {
            url: location.web_url,
            api_host: location.host,
            project_path: location.project_path,
            api_url,
            private_token: None,
            request_options: GitLabRequestOptions::default(),
            protocol: GitLabProtocol::Auto,
            default_branch: None,
            repository_url: None,
            web_url: None,
            tags: Mutex::new(None),
            branches: Mutex::new(None),
        })
    }

    pub fn with_private_token(mut self, token: impl Into<String>) -> Self {
        self.private_token = Some(token.into());
        self
    }

    pub fn with_auth(mut self, auth: &AuthConfig) -> Self {
        if let Some(token) = auth.get_gitlab_token(&self.api_host) {
            self.private_token = Some(token.to_string());
        } else if let Some(token) = auth.get_gitlab_token("gitlab.com") {
            self.private_token = Some(token.to_string());
        }
        self
    }

    pub fn with_request_options(mut self, options: GitLabRequestOptions) -> Self {
        self.request_options = options;
        self
    }

    pub fn request_options(&self) -> GitLabRequestOptions {
        self.request_options
    }

    pub fn with_protocol(mut self, protocol: GitLabProtocol) -> Self {
        self.protocol = protocol;
        self
    }

    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    pub fn project_path(&self) -> &str {
        &self.project_path
    }

    pub fn repository_url(&self) -> Option<&str> {
        self.repository_url.as_deref()
    }

    /// Loads and applies the single-project API response.
    pub fn initialize(&mut self) -> Result<(), VcsDriverError> {
        let project = self.api_json("")?;
        self.initialize_from_project(&project)
    }

    /// Applies a single-project API response. Keeping this separate from I/O
    /// makes metadata selection reusable by cached and fixture-backed callers.
    pub fn initialize_from_project(&mut self, project: &Value) -> Result<(), VcsDriverError> {
        self.default_branch = project
            .get("default_branch")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if self.default_branch.is_none() {
            return Err(VcsDriverError::InvalidFormat(
                "GitLab project response is missing default_branch".to_owned(),
            ));
        }

        let http_url = project.get("http_url_to_repo").and_then(Value::as_str);
        let ssh_url = project.get("ssh_url_to_repo").and_then(Value::as_str);
        let public_or_anonymous = project
            .get("visibility")
            .and_then(Value::as_str)
            .is_none_or(|visibility| visibility == "public");
        let selected = match self.protocol {
            GitLabProtocol::Http => http_url.or(ssh_url),
            GitLabProtocol::Ssh => ssh_url.or(http_url),
            GitLabProtocol::Auto if public_or_anonymous => http_url.or(ssh_url),
            GitLabProtocol::Auto => ssh_url.or(http_url),
        }
        .ok_or_else(|| {
            VcsDriverError::InvalidFormat(
                "GitLab project response is missing repository URLs".to_owned(),
            )
        })?;

        self.repository_url = Some(selected.to_owned());
        self.web_url = project
            .get("web_url")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(())
    }

    pub fn dist(&self, reference: impl Into<String>) -> GitLabDist {
        let reference = reference.into();
        GitLabDist {
            r#type: "zip",
            url: format!(
                "{}/repository/archive.zip?sha={}",
                self.api_url,
                urlencoding::encode(&reference)
            ),
            reference,
            shasum: String::new(),
        }
    }

    pub fn source(&self, reference: impl Into<String>) -> Result<GitLabSource, VcsDriverError> {
        Ok(GitLabSource {
            r#type: "git",
            url: self.repository_url.clone().ok_or_else(|| {
                VcsDriverError::InvalidFormat("GitLab driver is not initialized".to_owned())
            })?,
            reference: reference.into(),
        })
    }

    /// Parses and merges GitLab branch or tag response pages.
    pub fn references_from_pages(
        pages: &[Value],
    ) -> Result<HashMap<String, String>, VcsDriverError> {
        let mut references = HashMap::new();
        for page in pages {
            extend_references(&mut references, page)?;
        }
        Ok(references)
    }

    /// Primes the tag cache without replacing an already-loaded value.
    pub fn cache_tags_from_pages(
        &self,
        pages: &[Value],
    ) -> Result<HashMap<String, String>, VcsDriverError> {
        cache_references(&self.tags, Self::references_from_pages(pages)?)
    }

    /// Primes the branch cache without replacing an already-loaded value.
    pub fn cache_branches_from_pages(
        &self,
        pages: &[Value],
    ) -> Result<HashMap<String, String>, VcsDriverError> {
        cache_references(&self.branches, Self::references_from_pages(pages)?)
    }

    /// Extracts the next page URL from an RFC 8288-style Link header.
    pub fn pagination_next_url(link: &str) -> Option<String> {
        link.split(',').find_map(|entry| {
            let mut parts = entry.trim().split(';');
            let url = parts.next()?.trim().strip_prefix('<')?.strip_suffix('>')?;
            parts
                .any(|part| part.trim().eq_ignore_ascii_case("rel=\"next\""))
                .then(|| url.to_owned())
        })
    }

    fn send_api_url(&self, url: &str) -> Result<GitLabApiResponse, VcsDriverError> {
        let client = reqwest::blocking::Client::builder()
            .danger_accept_invalid_certs(!self.request_options.verify_tls)
            .build()
            .map_err(|error| VcsDriverError::Network(error.to_string()))?;
        let mut request = client.get(url);
        if let Some(token) = &self.private_token {
            request = request.header("PRIVATE-TOKEN", token);
        }
        let response = request
            .header("Accept", "application/json")
            .header("User-Agent", "riff-composer")
            .send()
            .map_err(|error| VcsDriverError::Network(error.to_string()))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(VcsDriverError::NotFound(self.project_path.clone()));
        }
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(VcsDriverError::AuthRequired(
                "GitLab authentication required".to_owned(),
            ));
        }
        if !status.is_success() {
            return Err(VcsDriverError::Network(format!(
                "GitLab API error: {status}"
            )));
        }
        let next_url = response
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|value| value.to_str().ok())
            .and_then(Self::pagination_next_url);
        let body = response
            .text()
            .map_err(|error| VcsDriverError::Network(error.to_string()))?;
        Ok(GitLabApiResponse { body, next_url })
    }

    fn api_response(&self, endpoint: &str) -> Result<GitLabApiResponse, VcsDriverError> {
        self.send_api_url(&format!("{}{}", self.api_url, endpoint))
    }

    fn api_json(&self, endpoint: &str) -> Result<Value, VcsDriverError> {
        let response = self.api_response(endpoint)?;
        serde_json::from_str(&response.body).map_err(|error| {
            VcsDriverError::InvalidFormat(format!("Invalid GitLab JSON response: {error}"))
        })
    }

    fn get_file_content_api(&self, file: &str, reference: &str) -> Result<String, VcsDriverError> {
        let endpoint = format!(
            "/repository/files/{}/raw?ref={}",
            encode_gitlab_path(file),
            urlencoding::encode(reference)
        );
        self.api_response(&endpoint).map(|response| response.body)
    }

    fn fetch_references(&self, kind: &str) -> Result<HashMap<String, String>, VcsDriverError> {
        let mut references = HashMap::new();
        let mut next_url = Some(format!("{}/repository/{kind}?per_page=100", self.api_url));
        for _ in 0..100 {
            let Some(url) = next_url else {
                break;
            };
            let response = self.send_api_url(&url)?;
            let page: Value = serde_json::from_str(&response.body).map_err(|error| {
                VcsDriverError::InvalidFormat(format!("Invalid GitLab JSON response: {error}"))
            })?;
            extend_references(&mut references, &page)?;
            next_url = response.next_url;
        }
        Ok(references)
    }

    fn normalize_composer_manifest(&self, identifier: &str, mut manifest: Value) -> Value {
        let invalid_support = manifest
            .get("support")
            .is_some_and(|support| !support.is_object());
        if invalid_support {
            let web_url = self.web_url.as_deref().unwrap_or(&self.url);
            manifest["support"] = serde_json::json!({
                "source": format!("{}/-/tree/{}", web_url.trim_end_matches('/'), identifier),
            });
        }
        manifest
    }

    fn cached(
        cache: &Mutex<Option<HashMap<String, String>>>,
    ) -> Result<Option<HashMap<String, String>>, VcsDriverError> {
        cache.lock().map(|cached| cached.clone()).map_err(|_| {
            VcsDriverError::InvalidFormat("GitLab reference cache poisoned".to_owned())
        })
    }
}

impl VcsDriver for GitLabDriver {
    fn get_root_identifier(&self) -> Result<String, VcsDriverError> {
        if let Some(branch) = &self.default_branch {
            return Ok(branch.clone());
        }
        let branches = self.get_branches()?;
        for branch in ["main", "master", "trunk", "default"] {
            if branches.contains_key(branch) {
                return Ok(branch.to_owned());
            }
        }
        branches
            .keys()
            .next()
            .cloned()
            .ok_or_else(|| VcsDriverError::NotFound("No branches found".to_owned()))
    }

    fn get_tags(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        if let Some(tags) = Self::cached(&self.tags)? {
            return Ok(tags);
        }
        cache_references(&self.tags, self.fetch_references("tags")?)
    }

    fn get_branches(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        if let Some(branches) = Self::cached(&self.branches)? {
            return Ok(branches);
        }
        cache_references(&self.branches, self.fetch_references("branches")?)
    }

    fn get_composer_information(&self, identifier: &str) -> Result<VcsInfo, VcsDriverError> {
        let content = self.get_file_content("composer.json", identifier)?;
        let manifest = serde_json::from_str(&content).map_err(|error| {
            VcsDriverError::InvalidFormat(format!("Invalid composer.json: {error}"))
        })?;
        let manifest = self.normalize_composer_manifest(identifier, manifest);
        let time = self
            .api_json(&format!(
                "/repository/commits/{}",
                urlencoding::encode(identifier)
            ))
            .ok()
            .and_then(|info| {
                info.get("committed_date")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        Ok(VcsInfo {
            manifest: Some(manifest),
            identifier: identifier.to_owned(),
            time,
        })
    }

    fn get_file_content(&self, file: &str, identifier: &str) -> Result<String, VcsDriverError> {
        self.get_file_content_api(file, identifier)
    }

    fn supports(url: &str, _deep: bool) -> bool {
        Self::supports_with_domains(url, &[])
    }

    fn get_url(&self) -> &str {
        &self.url
    }

    fn get_vcs_type(&self) -> &str {
        "git"
    }
}

impl GitLabDriver {
    pub fn supports_with_domains(url: &str, domains: &[&str]) -> bool {
        parse_gitlab_location(url, domains).is_some()
    }
}

fn cache_references(
    cache: &Mutex<Option<HashMap<String, String>>>,
    references: HashMap<String, String>,
) -> Result<HashMap<String, String>, VcsDriverError> {
    let mut cached = cache
        .lock()
        .map_err(|_| VcsDriverError::InvalidFormat("GitLab reference cache poisoned".to_owned()))?;
    Ok(cached.get_or_insert(references).clone())
}

fn extend_references(
    references: &mut HashMap<String, String>,
    response: &Value,
) -> Result<(), VcsDriverError> {
    let items = response.as_array().ok_or_else(|| {
        VcsDriverError::InvalidFormat("Expected GitLab reference array".to_owned())
    })?;
    for item in items {
        if let (Some(name), Some(identifier)) = (
            item.get("name").and_then(Value::as_str),
            item.get("commit")
                .and_then(|commit| commit.get("id"))
                .and_then(Value::as_str),
        ) {
            references.insert(name.to_owned(), identifier.to_owned());
        }
    }
    Ok(())
}

fn encode_gitlab_path(value: &str) -> String {
    urlencoding::encode(value).replace('.', "%2E")
}

fn parse_gitlab_location(url: &str, domains: &[&str]) -> Option<GitLabLocation> {
    let input = url.trim().trim_end_matches('/');
    let (scheme, host, authority, raw_path) = if let Some(scp) = input.strip_prefix("git@") {
        let (host, path) = scp.split_once(':')?;
        (
            "https".to_owned(),
            host.to_ascii_lowercase(),
            host.to_owned(),
            path.to_owned(),
        )
    } else {
        let parsed = url::Url::parse(input).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return None;
        }
        let host = parsed.host_str()?.to_ascii_lowercase();
        let authority = parsed
            .port()
            .map_or_else(|| host.clone(), |port| format!("{host}:{port}"));
        (
            parsed.scheme().to_owned(),
            host,
            authority,
            parsed.path().trim_matches('/').to_owned(),
        )
    };
    let raw_path = raw_path.strip_suffix(".git").unwrap_or(&raw_path);
    let path_segments = raw_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    let configured_base = domains
        .iter()
        .filter_map(|domain| configured_gitlab_base(domain, &host, &path_segments))
        .max_by_key(Vec::len);
    let base_segments = if let Some(configured) = configured_base {
        configured
    } else if host.contains("gitlab") {
        Vec::new()
    } else {
        let gitlab_index = path_segments
            .iter()
            .rposition(|segment| segment.eq_ignore_ascii_case("gitlab"))?;
        if path_segments.len().saturating_sub(gitlab_index + 1) < 2 {
            return None;
        }
        path_segments[..=gitlab_index].to_vec()
    };
    if !path_segments.starts_with(&base_segments) {
        return None;
    }
    let project_segments = &path_segments[base_segments.len()..];
    if project_segments.len() < 2 {
        return None;
    }
    let base_path = base_segments.join("/");
    let project_path = project_segments.join("/");
    let full_path = if base_path.is_empty() {
        project_path.clone()
    } else {
        format!("{base_path}/{project_path}")
    };
    Some(GitLabLocation {
        host,
        authority: authority.clone(),
        scheme: scheme.clone(),
        base_path,
        project_path,
        web_url: format!("{scheme}://{authority}/{full_path}"),
    })
}

fn configured_gitlab_base<'a>(
    domain: &'a str,
    host: &str,
    path_segments: &[&str],
) -> Option<Vec<&'a str>> {
    let configured = domain
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_matches('/');
    let mut parts = configured.split('/');
    let configured_host = parts.next()?.split(':').next()?.to_ascii_lowercase();
    if configured_host != host {
        return None;
    }
    let base = parts.collect::<Vec<_>>();
    path_segments.starts_with(&base).then_some(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(visibility: Option<&str>) -> Value {
        let mut project = serde_json::json!({
            "default_branch": "mymaster",
            "http_url_to_repo": "https://gitlab.com/mygroup/myproject.git",
            "ssh_url_to_repo": "git@gitlab.com:mygroup/myproject.git",
            "web_url": "https://gitlab.com/mygroup/myproject",
        });
        if let Some(visibility) = visibility {
            project["visibility"] = Value::String(visibility.to_owned());
        }
        project
    }

    fn reference_page(entries: &[(&str, &str)]) -> Value {
        Value::Array(
            entries
                .iter()
                .map(|(name, identifier)| {
                    serde_json::json!({"name": name, "commit": {"id": identifier}})
                })
                .collect(),
        )
    }

    #[test]
    fn composer_gitlab_driver_initializes_standard_repository_urls() {
        for (url, api_url, normalized_url) in [
            (
                "https://gitlab.com/mygroup/myproject",
                "https://gitlab.com/api/v4/projects/mygroup%2Fmyproject",
                "https://gitlab.com/mygroup/myproject",
            ),
            (
                "http://gitlab.com/mygroup/myproject",
                "http://gitlab.com/api/v4/projects/mygroup%2Fmyproject",
                "http://gitlab.com/mygroup/myproject",
            ),
            (
                "git@gitlab.com:mygroup/myproject",
                "https://gitlab.com/api/v4/projects/mygroup%2Fmyproject",
                "https://gitlab.com/mygroup/myproject",
            ),
        ] {
            let mut driver = GitLabDriver::new(url).unwrap();
            driver
                .initialize_from_project(&project(Some("private")))
                .unwrap();
            assert_eq!(driver.api_url(), api_url);
            assert_eq!(driver.get_root_identifier().unwrap(), "mymaster");
            assert_eq!(
                driver.repository_url(),
                Some("git@gitlab.com:mygroup/myproject.git")
            );
            assert_eq!(driver.get_url(), normalized_url);
        }
    }

    #[test]
    fn composer_gitlab_driver_prefers_http_for_public_projects() {
        let mut driver = GitLabDriver::new("https://gitlab.com/mygroup/myproject").unwrap();
        driver
            .initialize_from_project(&project(Some("public")))
            .unwrap();
        assert_eq!(
            driver.repository_url(),
            Some("https://gitlab.com/mygroup/myproject.git")
        );
    }

    #[test]
    fn composer_gitlab_driver_treats_missing_visibility_as_public() {
        let mut driver = GitLabDriver::new("https://gitlab.com/mygroup/myproject").unwrap();
        driver.initialize_from_project(&project(None)).unwrap();
        assert_eq!(
            driver.repository_url(),
            Some("https://gitlab.com/mygroup/myproject.git")
        );
    }

    #[test]
    fn composer_gitlab_driver_preserves_port_in_api_and_repository_urls() {
        let mut driver =
            GitLabDriver::new("https://gitlab.mycompany.com:5443/mygroup/myproject").unwrap();
        let metadata = serde_json::json!({
            "default_branch": "1.0.x",
            "http_url_to_repo": "https://gitlab.mycompany.com:5443/mygroup/myproject.git",
            "path_with_namespace": "mygroup/myproject",
            "web_url": "https://gitlab.mycompany.com:5443/mygroup/myproject",
        });
        driver.initialize_from_project(&metadata).unwrap();
        assert_eq!(
            driver.api_url(),
            "https://gitlab.mycompany.com:5443/api/v4/projects/mygroup%2Fmyproject"
        );
        assert_eq!(driver.get_root_identifier().unwrap(), "1.0.x");
        assert_eq!(
            driver.repository_url(),
            Some("https://gitlab.mycompany.com:5443/mygroup/myproject.git")
        );
    }

    #[test]
    fn composer_gitlab_driver_normalizes_invalid_support_metadata() {
        let mut driver = GitLabDriver::new("https://gitlab.com/mygroup/myproject").unwrap();
        driver
            .initialize_from_project(&project(Some("private")))
            .unwrap();
        let manifest = driver.normalize_composer_manifest(
            "main",
            serde_json::json!({"support": "https://gitlab.com/mygroup/myproject"}),
        );
        assert_eq!(
            manifest["support"]["source"],
            "https://gitlab.com/mygroup/myproject/-/tree/main"
        );
    }

    #[test]
    fn composer_gitlab_driver_builds_distribution_metadata() {
        let driver = GitLabDriver::new("https://gitlab.com/mygroup/myproject").unwrap();
        let reference = "c3ebdbf9cceddb82cd2089aaef8c7b992e536363";
        assert_eq!(
            driver.dist(reference),
            GitLabDist {
                r#type: "zip",
                url: format!(
                    "https://gitlab.com/api/v4/projects/mygroup%2Fmyproject/repository/archive.zip?sha={reference}"
                ),
                reference: reference.to_owned(),
                shasum: String::new(),
            }
        );
    }

    #[test]
    fn composer_gitlab_driver_builds_private_source_metadata() {
        let mut driver = GitLabDriver::new("https://gitlab.com/mygroup/myproject").unwrap();
        driver
            .initialize_from_project(&project(Some("private")))
            .unwrap();
        let reference = "c3ebdbf9cceddb82cd2089aaef8c7b992e536363";
        assert_eq!(
            driver.source(reference).unwrap(),
            GitLabSource {
                r#type: "git",
                url: "git@gitlab.com:mygroup/myproject.git".to_owned(),
                reference: reference.to_owned(),
            }
        );
    }

    #[test]
    fn composer_gitlab_driver_builds_public_source_metadata() {
        let mut driver = GitLabDriver::new("https://gitlab.com/mygroup/myproject").unwrap();
        driver
            .initialize_from_project(&project(Some("public")))
            .unwrap();
        assert_eq!(
            driver.source("abc123").unwrap().url,
            "https://gitlab.com/mygroup/myproject.git"
        );
    }

    #[test]
    fn composer_gitlab_driver_caches_tags() {
        let driver = GitLabDriver::new("https://gitlab.com/mygroup/myproject").unwrap();
        let expected = HashMap::from([
            (
                "v1.0.0".to_owned(),
                "092ed2c762bbae331e3f51d4a17f67310bf99a81".to_owned(),
            ),
            (
                "v2.0.0".to_owned(),
                "8e8f60b3ec86d63733db3bd6371117a758027ec6".to_owned(),
            ),
        ]);
        assert_eq!(
            driver
                .cache_tags_from_pages(&[reference_page(&[
                    ("v1.0.0", "092ed2c762bbae331e3f51d4a17f67310bf99a81"),
                    ("v2.0.0", "8e8f60b3ec86d63733db3bd6371117a758027ec6"),
                ])])
                .unwrap(),
            expected
        );
        assert_eq!(
            driver
                .cache_tags_from_pages(&[reference_page(&[("replacement", "other")])])
                .unwrap(),
            expected
        );
        assert_eq!(driver.get_tags().unwrap(), expected);
    }

    #[test]
    fn composer_gitlab_driver_follows_paginated_reference_links() {
        let first = reference_page(&[("mymaster", "97eda36b"), ("staging", "502cffe4")]);
        let second = reference_page(&[("stagingdupe", "502cffe4")]);
        assert_eq!(
            GitLabDriver::pagination_next_url(
                "<http://gitlab.com/api/v4/projects/id/repository/branches?page=2>; rel=\"next\", <http://gitlab.com/api/v4/projects/id/repository/branches?page=3>; rel=\"last\""
            ),
            Some(
                "http://gitlab.com/api/v4/projects/id/repository/branches?page=2".to_owned()
            )
        );
        assert_eq!(
            GitLabDriver::references_from_pages(&[first, second]).unwrap(),
            HashMap::from([
                ("mymaster".to_owned(), "97eda36b".to_owned()),
                ("staging".to_owned(), "502cffe4".to_owned()),
                ("stagingdupe".to_owned(), "502cffe4".to_owned()),
            ])
        );
    }

    #[test]
    fn composer_gitlab_driver_caches_branches() {
        let driver = GitLabDriver::new("https://gitlab.com/mygroup/myproject").unwrap();
        let expected = HashMap::from([
            ("mymaster".to_owned(), "97eda36b".to_owned()),
            ("staging".to_owned(), "502cffe4".to_owned()),
        ]);
        assert_eq!(
            driver
                .cache_branches_from_pages(&[reference_page(&[
                    ("mymaster", "97eda36b"),
                    ("staging", "502cffe4"),
                ])])
                .unwrap(),
            expected
        );
        assert_eq!(
            driver
                .cache_branches_from_pages(&[reference_page(&[("replacement", "other")])])
                .unwrap(),
            expected
        );
        assert_eq!(driver.get_branches().unwrap(), expected);
    }

    #[test]
    fn composer_gitlab_driver_supports_configured_domains_and_subgroups() {
        let domains = [
            "mycompany.com/gitlab",
            "othercompany.com/nested/gitlab",
            "gitlab.com",
        ];
        for url in [
            "http://gitlab.com/foo/bar",
            "http://gitlab.mycompany.com:5443/foo/bar",
            "http://gitlab.com/foo/bar/",
            "http://gitlab.com/foo/bar.baz.git",
            "https://gitlab.com/foo/bar",
            "https://gitlab.mycompany.com:5443/foo/bar",
            "https://gitlab.com/foo/bar.git",
            "git@gitlab.com:foo/bar.git",
            "http://mycompany.com/gitlab/mygroup/myproject",
            "https://mycompany.com/gitlab/mygroup/myproject",
            "http://othercompany.com/nested/gitlab/mygroup/myproject",
            "https://othercompany.com/nested/gitlab/mygroup/myproject",
            "http://gitlab.com/mygroup/mysubgroup/mysubsubgroup/myproject",
            "https://gitlab.com/mygroup/mysubgroup/mysubsubgroup/myproject",
        ] {
            assert!(GitLabDriver::supports_with_domains(url, &domains), "{url}");
        }
        assert!(!GitLabDriver::supports_with_domains(
            "git@example.com:foo/bar.git",
            &domains
        ));
        assert!(!GitLabDriver::supports_with_domains(
            "http://example.com/foo/bar",
            &domains
        ));
    }

    #[test]
    fn composer_gitlab_driver_derives_subdirectory_api_url() {
        let driver = GitLabDriver::new_with_domains(
            "https://mycompany.com/gitlab/mygroup/my-pro.ject",
            &["mycompany.com/gitlab"],
        )
        .unwrap();
        assert_eq!(
            driver.api_url(),
            "https://mycompany.com/gitlab/api/v4/projects/mygroup%2Fmy-pro%2Eject"
        );
    }

    #[test]
    fn composer_gitlab_driver_derives_subgroup_api_url() {
        let driver = GitLabDriver::new("https://gitlab.com/mygroup/mysubgroup/myproject").unwrap();
        assert_eq!(
            driver.api_url(),
            "https://gitlab.com/api/v4/projects/mygroup%2Fmysubgroup%2Fmyproject"
        );
    }

    #[test]
    fn composer_gitlab_driver_derives_subdirectory_subgroup_api_url() {
        let driver = GitLabDriver::new_with_domains(
            "https://mycompany.com/gitlab/mygroup/mysubgroup/myproject",
            &["mycompany.com/gitlab"],
        )
        .unwrap();
        assert_eq!(
            driver.api_url(),
            "https://mycompany.com/gitlab/api/v4/projects/mygroup%2Fmysubgroup%2Fmyproject"
        );
    }

    #[test]
    fn composer_gitlab_driver_forwards_tls_request_options() {
        let options = GitLabRequestOptions { verify_tls: false };
        let driver = GitLabDriver::new("https://gitlab.mycompany.local/mygroup/myproject")
            .unwrap()
            .with_request_options(options);
        assert_eq!(driver.request_options(), options);
    }

    #[test]
    fn composer_gitlab_driver_honors_http_protocol_override() {
        let mut driver = GitLabDriver::new("git@gitlab.com:mygroup/myproject")
            .unwrap()
            .with_protocol(GitLabProtocol::Http);
        driver
            .initialize_from_project(&project(Some("private")))
            .unwrap();
        assert_eq!(
            driver.repository_url(),
            Some("https://gitlab.com/mygroup/myproject.git")
        );
    }
}
