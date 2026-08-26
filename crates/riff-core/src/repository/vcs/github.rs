//! GitHub driver - uses GitHub API for repository access.

use std::collections::HashMap;

use super::driver::{parse_github_url, VcsDist, VcsDriver, VcsDriverError, VcsInfo, VcsSource};
use crate::config::AuthConfig;

/// A normalized funding link derived from GitHub's FUNDING.yml format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubFundingLink {
    pub r#type: String,
    pub url: String,
}

/// Recovery behavior when GitHub hides a private repository behind a 404.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHubPrivateAccessStrategy {
    PromptForToken,
    MirrorSsh,
}

/// GitHub driver for GitHub repositories
pub struct GitHubDriver {
    /// Repository URL
    url: String,
    /// GitHub owner
    owner: String,
    /// GitHub repository name
    repo: String,
    /// GitHub API base URL for this repository
    api_url: String,
    /// OAuth token (optional)
    oauth_token: Option<String>,
    /// Default branch reported by the project API
    default_branch: Option<String>,
    /// Checkout URL selected from repository visibility
    repository_url: Option<String>,
    /// Whether GitHub reports the repository as archived
    archived: bool,
}

impl GitHubDriver {
    /// Create a new GitHub driver
    pub fn new(url: impl Into<String>) -> Result<Self, VcsDriverError> {
        let url = url.into();

        let (owner, repo) = parse_github_url(&url)
            .ok_or_else(|| VcsDriverError::InvalidFormat(format!("Invalid GitHub URL: {}", url)))?;

        let api_url = format!("https://api.github.com/repos/{owner}/{repo}");
        Ok(Self {
            url,
            owner,
            repo,
            api_url,
            oauth_token: None,
            default_branch: None,
            repository_url: None,
            archived: false,
        })
    }

    /// Set OAuth token for authentication
    pub fn with_oauth_token(mut self, token: impl Into<String>) -> Self {
        self.oauth_token = Some(token.into());
        self
    }

    /// Configure authentication from AuthConfig
    pub fn with_auth(mut self, auth: &AuthConfig) -> Self {
        // Try to get token for github.com or the specific domain
        if let Some(token) = auth.get_github_oauth("github.com") {
            self.oauth_token = Some(token.to_string());
        }
        self
    }

    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    pub fn repository_url(&self) -> Option<&str> {
        self.repository_url.as_deref()
    }

    pub const fn private_access_strategy(interactive: bool) -> GitHubPrivateAccessStrategy {
        if interactive {
            GitHubPrivateAccessStrategy::PromptForToken
        } else {
            GitHubPrivateAccessStrategy::MirrorSsh
        }
    }

    /// Loads and applies the GitHub repository API response.
    pub fn initialize(&mut self) -> Result<(), VcsDriverError> {
        let project = self.api_request("")?;
        self.initialize_from_project(&project)
    }

    /// Applies repository metadata returned by GitHub's API.
    pub fn initialize_from_project(
        &mut self,
        project: &serde_json::Value,
    ) -> Result<(), VcsDriverError> {
        self.default_branch = project
            .get("default_branch")
            .or_else(|| project.get("master_branch"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if self.default_branch.is_none() {
            return Err(VcsDriverError::InvalidFormat(
                "GitHub repository response is missing its default branch".to_owned(),
            ));
        }
        self.archived = project
            .get("archived")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let owner = project
            .get("owner")
            .and_then(|owner| owner.get("login"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&self.owner);
        let repository = project
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&self.repo);
        let private = project
            .get("private")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        self.repository_url = Some(if private {
            format!("git@github.com:{owner}/{repository}.git")
        } else {
            format!("https://github.com/{owner}/{repository}.git")
        });
        Ok(())
    }

    /// Initializes the observable state used when a private repository cannot
    /// be queried non-interactively and Riff falls back to a mirror checkout.
    pub fn initialize_private_fallback(
        &mut self,
        default_branch: impl Into<String>,
    ) -> Result<(), VcsDriverError> {
        let default_branch = default_branch.into();
        if default_branch.trim().is_empty() {
            return Err(VcsDriverError::InvalidFormat(
                "GitHub fallback branch must not be empty".to_owned(),
            ));
        }
        self.default_branch = Some(default_branch);
        self.repository_url = Some(format!("git@github.com:{}/{}.git", self.owner, self.repo));
        Ok(())
    }

    pub fn dist(&self, reference: impl Into<String>) -> VcsDist {
        let reference = reference.into();
        VcsDist {
            r#type: "zip",
            url: format!(
                "{}/zipball/{}",
                self.api_url,
                urlencoding::encode(&reference)
            ),
            reference,
            shasum: String::new(),
        }
    }

    pub fn source(&self, reference: impl Into<String>) -> Result<VcsSource, VcsDriverError> {
        Ok(VcsSource {
            r#type: "git",
            url: self.repository_url.clone().ok_or_else(|| {
                VcsDriverError::InvalidFormat("GitHub driver is not initialized".to_owned())
            })?,
            reference: reference.into(),
        })
    }

    /// Parses GitHub's documented FUNDING.yml scalar and inline-array forms.
    pub fn parse_funding(funding: &str) -> Vec<GitHubFundingLink> {
        funding
            .lines()
            .filter_map(|line| line.split_once(':'))
            .flat_map(|(platform, values)| {
                parse_funding_values(values)
                    .into_iter()
                    .filter_map(move |value| funding_url(platform.trim(), &value))
            })
            .collect()
    }

    /// Make a GitHub API request using blocking reqwest
    fn api_request(&self, endpoint: &str) -> Result<serde_json::Value, VcsDriverError> {
        let url = format!("{}{}", self.api_url, endpoint);

        let client = reqwest::blocking::Client::new();
        let mut request = client.get(&url);

        // Add authentication if available
        if let Some(ref token) = &self.oauth_token {
            request = request.header("Authorization", format!("token {}", token));
        }

        // Add required headers
        request = request
            .header("Accept", "application/vnd.github.v3+json")
            .header("User-Agent", "riff-composer");

        let response = request
            .send()
            .map_err(|e: reqwest::Error| VcsDriverError::Network(e.to_string()))?;

        let status = response.status();

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(VcsDriverError::NotFound(format!(
                "{}/{}",
                self.owner, self.repo
            )));
        }

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            let body = response.text().unwrap_or_default();
            if body.contains("rate limit") {
                return Err(VcsDriverError::RateLimited(
                    "GitHub API rate limit exceeded".to_string(),
                ));
            }
            return Err(VcsDriverError::AuthRequired(
                "GitHub authentication required".to_string(),
            ));
        }

        if !status.is_success() {
            return Err(VcsDriverError::Network(format!(
                "GitHub API error: {}",
                status
            )));
        }

        response
            .json()
            .map_err(|e| VcsDriverError::InvalidFormat(format!("Invalid JSON response: {}", e)))
    }

    /// Get file content from GitHub API
    fn get_file_content_api(&self, file: &str, ref_name: &str) -> Result<String, VcsDriverError> {
        let endpoint = format!("/contents/{}?ref={}", file, urlencoding::encode(ref_name));
        let response = self.api_request(&endpoint)?;

        Self::decode_file_content_response(file, &response)
    }

    fn decode_file_content_response(
        file: &str,
        response: &serde_json::Value,
    ) -> Result<String, VcsDriverError> {
        // GitHub returns base64 encoded content
        if let Some(content) = response.get("content").and_then(|v| v.as_str()) {
            // Remove newlines from base64
            let content = content.replace('\n', "");
            let decoded = base64_decode(&content).map_err(|e| {
                VcsDriverError::InvalidFormat(format!("Failed to decode base64: {}", e))
            })?;
            return Ok(decoded);
        }

        Err(VcsDriverError::FileNotFound(file.to_string()))
    }

    fn get_funding_api(&self) -> Result<Vec<GitHubFundingLink>, VcsDriverError> {
        let response = self.api_request("/contents/.github/FUNDING.yml")?;
        let content = Self::decode_file_content_response(".github/FUNDING.yml", &response)?;
        Ok(Self::parse_funding(&content))
    }

    /// Applies forge-level metadata which Composer exposes on every package
    /// loaded from this GitHub repository.
    pub fn enrich_composer_manifest(
        &self,
        identifier: &str,
        mut manifest: serde_json::Value,
        funding: &[GitHubFundingLink],
    ) -> serde_json::Value {
        if manifest
            .get("support")
            .is_some_and(|support| !support.is_object())
        {
            manifest["support"] = serde_json::json!({
                "source": format!(
                    "https://github.com/{}/{}/tree/{}",
                    self.owner, self.repo, identifier
                ),
            });
        }
        if self.archived {
            manifest["abandoned"] = serde_json::Value::Bool(true);
        }
        if !funding.is_empty() {
            manifest["funding"] = serde_json::Value::Array(
                funding
                    .iter()
                    .map(|link| serde_json::json!({"type": link.r#type, "url": link.url}))
                    .collect(),
            );
        }
        manifest
    }
}

impl VcsDriver for GitHubDriver {
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
            .ok_or_else(|| VcsDriverError::NotFound("No branches found".to_string()))
    }

    fn get_tags(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        let mut tags = HashMap::new();
        let mut page = 1;

        loop {
            let endpoint = format!("/tags?per_page=100&page={}", page);
            let response = self.api_request(&endpoint)?;

            let items = response
                .as_array()
                .ok_or_else(|| VcsDriverError::InvalidFormat("Expected array".to_string()))?;

            if items.is_empty() {
                break;
            }

            for item in items {
                if let (Some(name), Some(sha)) = (
                    item.get("name").and_then(|v| v.as_str()),
                    item.get("commit")
                        .and_then(|c| c.get("sha"))
                        .and_then(|v| v.as_str()),
                ) {
                    tags.insert(name.to_string(), sha.to_string());
                }
            }

            page += 1;

            // Safety limit
            if page > 100 {
                break;
            }
        }

        Ok(tags)
    }

    fn get_branches(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        let mut branches = HashMap::new();
        let mut page = 1;

        loop {
            let endpoint = format!("/branches?per_page=100&page={}", page);
            let response = self.api_request(&endpoint)?;

            let items = response
                .as_array()
                .ok_or_else(|| VcsDriverError::InvalidFormat("Expected array".to_string()))?;

            if items.is_empty() {
                break;
            }

            for item in items {
                if let (Some(name), Some(sha)) = (
                    item.get("name").and_then(|v| v.as_str()),
                    item.get("commit")
                        .and_then(|c| c.get("sha"))
                        .and_then(|v| v.as_str()),
                ) {
                    branches.insert(name.to_string(), sha.to_string());
                }
            }

            page += 1;

            // Safety limit
            if page > 100 {
                break;
            }
        }

        Ok(branches)
    }

    fn get_composer_information(&self, identifier: &str) -> Result<VcsInfo, VcsDriverError> {
        let content = self.get_file_content("composer.json", identifier)?;

        let manifest: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| VcsDriverError::InvalidFormat(format!("Invalid JSON: {}", e)))?;

        // Try to get commit info for timestamp
        let time = self
            .api_request(&format!("/commits/{}", urlencoding::encode(identifier)))
            .ok()
            .and_then(|info| {
                info.get("commit")
                    .and_then(|c| c.get("committer"))
                    .and_then(|c| c.get("date"))
                    .and_then(|d| d.as_str())
                    .map(|s| s.to_string())
            });
        let funding = self.get_funding_api().unwrap_or_default();
        let manifest = self.enrich_composer_manifest(identifier, manifest, &funding);

        Ok(VcsInfo {
            manifest: Some(manifest),
            identifier: identifier.to_string(),
            time,
        })
    }

    fn get_file_content(&self, file: &str, identifier: &str) -> Result<String, VcsDriverError> {
        self.get_file_content_api(file, identifier)
    }

    fn supports(url: &str, _deep: bool) -> bool {
        parse_github_url(url).is_some()
    }

    fn get_url(&self) -> &str {
        &self.url
    }

    fn get_vcs_type(&self) -> &str {
        "git"
    }
}

fn parse_funding_values(values: &str) -> Vec<String> {
    let values = values.trim();
    let values = values
        .strip_prefix('[')
        .and_then(|values| values.strip_suffix(']'))
        .unwrap_or(values);
    values
        .split(',')
        .map(str::trim)
        .map(|value| value.trim_matches(['"', '\'']))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn funding_url(platform: &str, value: &str) -> Option<GitHubFundingLink> {
    let url = match platform {
        "community_bridge" => format!("https://funding.communitybridge.org/projects/{value}"),
        "github" => format!("https://github.com/{value}"),
        "issuehunt" => format!("https://issuehunt.io/r/{value}"),
        "ko_fi" => format!("https://ko-fi.com/{value}"),
        "liberapay" => format!("https://liberapay.com/{value}"),
        "open_collective" => format!("https://opencollective.com/{value}"),
        "patreon" => format!("https://www.patreon.com/{value}"),
        "tidelift" => format!("https://tidelift.com/funding/github/{value}"),
        "polar" => format!("https://polar.sh/{value}"),
        "buy_me_a_coffee" => format!("https://www.buymeacoffee.com/{value}"),
        "thanks_dev" => format!("https://thanks.dev/{value}"),
        "otechie" => format!("https://otechie.com/{value}"),
        "custom" if value.starts_with("https://") || value.starts_with("http://") => {
            value.to_owned()
        }
        "custom" if !value.contains('/') && value.contains('.') => format!("https://{value}"),
        _ => return None,
    };
    Some(GitHubFundingLink {
        r#type: platform.to_owned(),
        url,
    })
}

/// Simple base64 decoder
fn base64_decode(input: &str) -> Result<String, String> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn decode_char(c: u8) -> Option<u8> {
        ALPHABET.iter().position(|&x| x == c).map(|p| p as u8)
    }

    let input = input.as_bytes();
    let mut output = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits_collected = 0;

    for &byte in input {
        if byte == b'=' || byte == b'\n' || byte == b'\r' || byte == b' ' {
            continue;
        }

        let value = decode_char(byte)
            .ok_or_else(|| format!("Invalid base64 character: {}", byte as char))?;

        buffer = (buffer << 6) | (value as u32);
        bits_collected += 6;

        if bits_collected >= 8 {
            bits_collected -= 8;
            output.push((buffer >> bits_collected) as u8);
            buffer &= (1 << bits_collected) - 1;
        }
    }

    String::from_utf8(output).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(private: bool, archived: bool) -> serde_json::Value {
        serde_json::json!({
            "master_branch": "test_master",
            "private": private,
            "archived": archived,
            "owner": {"login": "composer"},
            "name": "packagist",
        })
    }

    #[test]
    fn composer_github_driver_initializes_private_repository_metadata() {
        assert_eq!(
            GitHubDriver::private_access_strategy(true),
            GitHubPrivateAccessStrategy::PromptForToken
        );
        let mut driver = GitHubDriver::new("http://github.com/composer/packagist").unwrap();
        driver
            .initialize_from_project(&project(true, false))
            .unwrap();
        assert_eq!(driver.get_root_identifier().unwrap(), "test_master");
        assert_eq!(
            driver.repository_url(),
            Some("git@github.com:composer/packagist.git")
        );
        assert_eq!(
            driver.dist("SOMESHA"),
            VcsDist {
                r#type: "zip",
                url: "https://api.github.com/repos/composer/packagist/zipball/SOMESHA".to_owned(),
                reference: "SOMESHA".to_owned(),
                shasum: String::new(),
            }
        );
        assert_eq!(
            driver.source("SOMESHA").unwrap().url,
            "git@github.com:composer/packagist.git"
        );
    }

    #[test]
    fn composer_github_driver_initializes_public_repository_metadata() {
        let mut driver = GitHubDriver::new("http://github.com/composer/packagist").unwrap();
        driver
            .initialize_from_project(&project(false, false))
            .unwrap();
        assert_eq!(driver.get_root_identifier().unwrap(), "test_master");
        assert_eq!(
            driver.repository_url(),
            Some("https://github.com/composer/packagist.git")
        );
        assert_eq!(
            driver.source("SOMESHA").unwrap(),
            VcsSource {
                r#type: "git",
                url: "https://github.com/composer/packagist.git".to_owned(),
                reference: "SOMESHA".to_owned(),
            }
        );
    }

    #[test]
    fn composer_github_driver_preserves_valid_public_project_metadata() {
        let mut driver = GitHubDriver::new("http://github.com/composer/packagist").unwrap();
        driver
            .initialize_from_project(&project(false, false))
            .unwrap();
        let manifest = driver.enrich_composer_manifest(
            "feature/3.2-foo",
            serde_json::json!({
                "support": {"source": "http://github.com/composer/packagist"}
            }),
            &GitHubDriver::parse_funding("custom: https://example.com"),
        );
        assert_eq!(
            manifest["support"]["source"],
            "http://github.com/composer/packagist"
        );
        assert!(manifest.get("abandoned").is_none());
        assert_eq!(manifest["funding"][0]["url"], "https://example.com");
    }

    #[test]
    fn composer_github_driver_normalizes_invalid_support_metadata() {
        let mut driver = GitHubDriver::new("http://github.com/composer/packagist").unwrap();
        driver
            .initialize_from_project(&project(false, false))
            .unwrap();
        let manifest = driver.enrich_composer_manifest(
            "feature/3.2-foo",
            serde_json::json!({"support": "http://github.com/composer/packagist"}),
            &[],
        );
        assert_eq!(
            manifest["support"]["source"],
            "https://github.com/composer/packagist/tree/feature/3.2-foo"
        );
    }

    #[test]
    fn composer_github_driver_parses_funding_formats() {
        let all_named = GitHubDriver::parse_funding(
            "community_bridge: project-name\n\
             github: [userA, userB]\n\
             issuehunt: userName\n\
             ko_fi: userName\n\
             liberapay: userName\n\
             open_collective: userName\n\
             patreon: userName\n\
             tidelift: Platform/Package\n\
             polar: userName\n\
             buy_me_a_coffee: userName\n\
             thanks_dev: u/gh/userName\n\
             otechie: userName",
        );
        assert_eq!(
            all_named,
            [
                (
                    "community_bridge",
                    "https://funding.communitybridge.org/projects/project-name"
                ),
                ("github", "https://github.com/userA"),
                ("github", "https://github.com/userB"),
                ("issuehunt", "https://issuehunt.io/r/userName"),
                ("ko_fi", "https://ko-fi.com/userName"),
                ("liberapay", "https://liberapay.com/userName"),
                ("open_collective", "https://opencollective.com/userName"),
                ("patreon", "https://www.patreon.com/userName"),
                (
                    "tidelift",
                    "https://tidelift.com/funding/github/Platform/Package"
                ),
                ("polar", "https://polar.sh/userName"),
                ("buy_me_a_coffee", "https://www.buymeacoffee.com/userName"),
                ("thanks_dev", "https://thanks.dev/u/gh/userName"),
                ("otechie", "https://otechie.com/userName"),
            ]
            .into_iter()
            .map(|(r#type, url)| GitHubFundingLink {
                r#type: r#type.to_owned(),
                url: url.to_owned(),
            })
            .collect::<Vec<_>>()
        );

        let custom_cases = [
            ("custom: example.com", vec!["https://example.com"]),
            ("custom: [example.com]", vec!["https://example.com"]),
            (
                "custom: \"https://example.com\"",
                vec!["https://example.com"],
            ),
            (
                "custom: [\"https://example.com\"]",
                vec!["https://example.com"],
            ),
            (
                "custom: [\"https://example.com\", example.org]",
                vec!["https://example.com", "https://example.org"],
            ),
            (
                "custom: [example.net/funding, \"https://example.com\", example.org]",
                vec!["https://example.com", "https://example.org"],
            ),
        ];
        for (funding, expected) in custom_cases {
            assert_eq!(
                GitHubDriver::parse_funding(funding)
                    .into_iter()
                    .map(|link| link.url)
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn composer_github_driver_marks_archived_repositories_abandoned() {
        let mut driver = GitHubDriver::new("http://github.com/composer/packagist").unwrap();
        driver
            .initialize_from_project(&project(false, true))
            .unwrap();
        let manifest = driver.enrich_composer_manifest(
            "SOMESHA",
            serde_json::json!({"name": "composer/packagist"}),
            &[],
        );
        assert_eq!(manifest["abandoned"], true);
    }

    #[test]
    fn composer_github_driver_falls_back_noninteractively_for_private_repositories() {
        assert_eq!(
            GitHubDriver::private_access_strategy(false),
            GitHubPrivateAccessStrategy::MirrorSsh
        );
        let mut driver = GitHubDriver::new("http://github.com/composer/packagist").unwrap();
        driver.initialize_private_fallback("test_master").unwrap();
        assert_eq!(driver.get_root_identifier().unwrap(), "test_master");
        assert_eq!(
            driver.source("v0.0.0").unwrap(),
            VcsSource {
                r#type: "git",
                url: "git@github.com:composer/packagist.git".to_owned(),
                reference: "v0.0.0".to_owned(),
            }
        );
        assert_eq!(driver.source("SOMESHA").unwrap().reference, "SOMESHA");
        assert_eq!(
            driver.dist("SOMESHA").url,
            "https://api.github.com/repos/composer/packagist/zipball/SOMESHA"
        );
    }

    #[test]
    fn test_parse_github_url() {
        assert!(GitHubDriver::supports(
            "https://github.com/owner/repo",
            false
        ));
        assert!(GitHubDriver::supports(
            "https://github.com/owner/repo.git",
            false
        ));
        assert!(GitHubDriver::supports(
            "git@github.com:owner/repo.git",
            false
        ));
        assert!(!GitHubDriver::supports(
            "https://gitlab.com/owner/repo",
            false
        ));
    }

    #[test]
    fn test_base64_decode() {
        let encoded = "SGVsbG8gV29ybGQ=";
        let decoded = base64_decode(encoded).unwrap();
        assert_eq!(decoded, "Hello World");
    }

    #[test]
    fn test_base64_decode_multiline() {
        let encoded = "SGVs\nbG8g\nV29y\nbGQ=";
        let decoded = base64_decode(encoded).unwrap();
        assert_eq!(decoded, "Hello World");
    }

    #[test]
    fn composer_github_driver_rejects_invalid_repository_urls() {
        for url in [
            "https://github.com/acme",
            "https://github.com/acme/repository/releases",
            "https://github.com/acme/repository/pulls",
        ] {
            assert!(matches!(
                GitHubDriver::new(url),
                Err(VcsDriverError::InvalidFormat(_))
            ));
        }
    }

    #[test]
    fn composer_github_driver_supports_only_repository_urls() {
        let cases = [
            (false, "https://github.com/acme"),
            (true, "https://github.com/acme/repository"),
            (true, "git@github.com:acme/repository.git"),
            (false, "https://github.com/acme/repository/releases"),
            (false, "https://github.com/acme/repository/pulls"),
        ];

        for (expected, url) in cases {
            assert_eq!(GitHubDriver::supports(url, false), expected, "{url}");
        }
    }

    #[test]
    fn composer_github_driver_preserves_empty_file_content() {
        let response = serde_json::json!({"encoding": "base64", "content": ""});

        assert_eq!(
            GitHubDriver::decode_file_content_response("composer.json", &response).unwrap(),
            ""
        );
    }
}
