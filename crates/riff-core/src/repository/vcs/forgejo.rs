//! Forgejo repository URL normalization and API-backed repository access.

use std::collections::HashMap;

use serde_json::Value;

use super::driver::{VcsDist, VcsDriver, VcsDriverError, VcsInfo, VcsSource};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoUrl {
    pub origin_url: String,
    pub owner: String,
    pub repository: String,
    pub api_url: String,
}

impl ForgejoUrl {
    pub fn parse(input: &str) -> Result<Self, VcsDriverError> {
        let input = input.trim().trim_end_matches('/');
        let (origin, path) = if let Some(scp) = input.strip_prefix("git@") {
            let (origin, path) = scp.split_once(':').ok_or_else(|| {
                VcsDriverError::InvalidFormat(format!("Invalid Forgejo URL: {input}"))
            })?;
            (origin.to_owned(), path.to_owned())
        } else {
            let parsed = url::Url::parse(input).map_err(|_| {
                VcsDriverError::InvalidFormat(format!("Invalid Forgejo URL: {input}"))
            })?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(VcsDriverError::InvalidFormat(format!(
                    "Invalid Forgejo URL: {input}"
                )));
            }
            let host = parsed.host_str().ok_or_else(|| {
                VcsDriverError::InvalidFormat(format!("Invalid Forgejo URL: {input}"))
            })?;
            let origin = parsed
                .port()
                .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"));
            (origin, parsed.path().trim_matches('/').to_owned())
        };
        let path = path.strip_suffix(".git").unwrap_or(&path);
        let mut segments = path.split('/');
        let owner = segments.next().filter(|owner| !owner.is_empty());
        let repository = segments.next().filter(|repository| !repository.is_empty());
        let (Some(owner), Some(repository)) = (owner, repository) else {
            return Err(VcsDriverError::InvalidFormat(format!(
                "Invalid Forgejo URL: {input}"
            )));
        };
        if segments.next().is_some() {
            return Err(VcsDriverError::InvalidFormat(format!(
                "Invalid Forgejo URL: {input}"
            )));
        }
        Ok(Self {
            origin_url: origin.clone(),
            owner: owner.to_owned(),
            repository: repository.to_owned(),
            api_url: format!("https://{origin}/api/v1/repos/{owner}/{repository}"),
        })
    }

    pub fn generate_ssh_url(&self) -> String {
        format!(
            "git@{}:{}/{}.git",
            self.origin_url, self.owner, self.repository
        )
    }
}

impl TryFrom<&str> for ForgejoUrl {
    type Error = VcsDriverError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoRepositoryData {
    pub default_branch: String,
    pub has_issues: bool,
    pub archived: bool,
    pub private: bool,
    pub html_url: String,
    pub ssh_url: String,
    pub clone_url: String,
}

impl ForgejoRepositoryData {
    fn from_api(project: &Value) -> Result<Self, VcsDriverError> {
        fn required_string(project: &Value, field: &str) -> Result<String, VcsDriverError> {
            project
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    VcsDriverError::InvalidFormat(format!(
                        "Forgejo repository response is missing {field}"
                    ))
                })
        }

        Ok(Self {
            default_branch: required_string(project, "default_branch")?,
            has_issues: project
                .get("has_issues")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            archived: project
                .get("archived")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            private: project
                .get("private")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            html_url: required_string(project, "html_url")?,
            ssh_url: required_string(project, "ssh_url")?,
            clone_url: required_string(project, "clone_url")?,
        })
    }
}

/// Forgejo driver for configured Forgejo installations such as Codeberg.
pub struct ForgejoDriver {
    original_url: String,
    location: ForgejoUrl,
    token: Option<String>,
    repository: Option<ForgejoRepositoryData>,
}

impl ForgejoDriver {
    pub fn new(url: impl Into<String>) -> Result<Self, VcsDriverError> {
        let original_url = url.into();
        let location = ForgejoUrl::parse(&original_url)?;
        Ok(Self {
            original_url,
            location,
            token: None,
            repository: None,
        })
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn api_url(&self) -> &str {
        &self.location.api_url
    }

    pub fn repository_url(&self) -> Option<&str> {
        self.repository.as_ref().map(|repository| {
            if repository.private {
                repository.ssh_url.as_str()
            } else {
                repository.clone_url.as_str()
            }
        })
    }

    pub fn initialize(&mut self) -> Result<(), VcsDriverError> {
        let project = self.api_request("")?;
        self.initialize_from_project(&project)
    }

    /// Applies repository metadata returned by Forgejo's repository API.
    pub fn initialize_from_project(&mut self, project: &Value) -> Result<(), VcsDriverError> {
        self.repository = Some(ForgejoRepositoryData::from_api(project)?);
        Ok(())
    }

    pub fn dist(&self, reference: impl Into<String>) -> VcsDist {
        let reference = reference.into();
        VcsDist {
            r#type: "zip",
            url: format!(
                "{}/archive/{}.zip",
                self.location.api_url,
                urlencoding::encode(&reference)
            ),
            reference,
            shasum: String::new(),
        }
    }

    pub fn source(&self, reference: impl Into<String>) -> Result<VcsSource, VcsDriverError> {
        Ok(VcsSource {
            r#type: "git",
            url: self.repository_url().map(str::to_owned).ok_or_else(|| {
                VcsDriverError::InvalidFormat("Forgejo driver is not initialized".to_owned())
            })?,
            reference: reference.into(),
        })
    }

    /// Parses Forgejo branch response pages into branch-to-commit mappings.
    pub fn branches_from_pages(pages: &[Value]) -> Result<HashMap<String, String>, VcsDriverError> {
        references_from_pages(pages, "id")
    }

    /// Parses Forgejo tag response pages into tag-to-commit mappings.
    pub fn tags_from_pages(pages: &[Value]) -> Result<HashMap<String, String>, VcsDriverError> {
        references_from_pages(pages, "sha")
    }

    /// Decodes a Forgejo contents API response, including an empty file.
    pub fn decode_file_content_response(
        file: &str,
        response: &Value,
    ) -> Result<String, VcsDriverError> {
        if response.as_array().is_some_and(Vec::is_empty) {
            return Ok("[]".to_owned());
        }
        if response.get("encoding").and_then(Value::as_str) != Some("base64") {
            return Err(VcsDriverError::InvalidFormat(format!(
                "Forgejo did not return base64 content for {file}"
            )));
        }
        let content = response
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| VcsDriverError::FileNotFound(file.to_owned()))?;
        decode_base64(content).map_err(|error| {
            VcsDriverError::InvalidFormat(format!("Failed to decode {file}: {error}"))
        })
    }

    /// Checks both Forgejo URL shape and the user-configured domain allowlist.
    pub fn supports_with_domains(url: &str, domains: &[&str]) -> bool {
        ForgejoUrl::parse(url).is_ok_and(|location| {
            domains
                .iter()
                .any(|domain| location.origin_url.eq_ignore_ascii_case(domain))
        })
    }

    fn api_request(&self, endpoint: &str) -> Result<Value, VcsDriverError> {
        let client = reqwest::blocking::Client::new();
        let mut request = client.get(format!("{}{}", self.location.api_url, endpoint));
        if let Some(token) = &self.token {
            request = request.header("Authorization", format!("token {token}"));
        }
        let response = request
            .header("Accept", "application/json")
            .header("User-Agent", "riff")
            .send()
            .map_err(|error| VcsDriverError::Network(error.to_string()))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(VcsDriverError::NotFound(format!(
                "{}/{}",
                self.location.owner, self.location.repository
            )));
        }
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(VcsDriverError::AuthRequired(
                "Forgejo authentication required".to_owned(),
            ));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(VcsDriverError::RateLimited(
                "Forgejo API rate limit exceeded".to_owned(),
            ));
        }
        if !status.is_success() {
            return Err(VcsDriverError::Network(format!(
                "Forgejo API error: {status}"
            )));
        }
        response.json().map_err(|error| {
            VcsDriverError::InvalidFormat(format!("Invalid Forgejo JSON response: {error}"))
        })
    }
}

impl VcsDriver for ForgejoDriver {
    fn get_root_identifier(&self) -> Result<String, VcsDriverError> {
        self.repository
            .as_ref()
            .map(|repository| repository.default_branch.clone())
            .ok_or_else(|| {
                VcsDriverError::InvalidFormat("Forgejo driver is not initialized".to_owned())
            })
    }

    fn get_tags(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        Self::tags_from_pages(&[self.api_request("/tags?per_page=100")?])
    }

    fn get_branches(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        Self::branches_from_pages(&[self.api_request("/branches?per_page=100")?])
    }

    fn get_composer_information(&self, identifier: &str) -> Result<VcsInfo, VcsDriverError> {
        let content = self.get_file_content("composer.json", identifier)?;
        let manifest = serde_json::from_str(&content).map_err(|error| {
            VcsDriverError::InvalidFormat(format!("Invalid composer.json: {error}"))
        })?;
        let time = self
            .api_request(&format!(
                "/git/commits/{}?verification=false&files=false",
                urlencoding::encode(identifier)
            ))
            .ok()
            .and_then(|commit| {
                commit
                    .get("commit")
                    .and_then(|commit| commit.get("committer"))
                    .and_then(|committer| committer.get("date"))
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
        let response = self.api_request(&format!(
            "/contents/{}?ref={}",
            file,
            urlencoding::encode(identifier)
        ))?;
        Self::decode_file_content_response(file, &response)
    }

    fn supports(url: &str, _deep: bool) -> bool {
        Self::supports_with_domains(url, &["codeberg.org"])
    }

    fn get_url(&self) -> &str {
        self.repository_url().unwrap_or(&self.original_url)
    }

    fn get_vcs_type(&self) -> &str {
        "git"
    }
}

fn references_from_pages(
    pages: &[Value],
    commit_field: &str,
) -> Result<HashMap<String, String>, VcsDriverError> {
    let mut references = HashMap::new();
    for page in pages {
        let items = page.as_array().ok_or_else(|| {
            VcsDriverError::InvalidFormat("Forgejo references response must be an array".to_owned())
        })?;
        for item in items {
            let name = item.get("name").and_then(Value::as_str).ok_or_else(|| {
                VcsDriverError::InvalidFormat(
                    "Forgejo reference response is missing name".to_owned(),
                )
            })?;
            let reference = item
                .get("commit")
                .and_then(|commit| commit.get(commit_field))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    VcsDriverError::InvalidFormat(format!(
                        "Forgejo reference response is missing commit.{commit_field}"
                    ))
                })?;
            references.insert(name.to_owned(), reference.to_owned());
        }
    }
    Ok(references)
}

fn decode_base64(input: &str) -> Result<String, String> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut output = Vec::new();
    let mut buffer = 0_u32;
    let mut bits_collected = 0;
    for byte in input.bytes() {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let value = ALPHABET
            .iter()
            .position(|candidate| *candidate == byte)
            .ok_or_else(|| format!("invalid base64 character {}", byte as char))?
            as u32;
        buffer = (buffer << 6) | value;
        bits_collected += 6;
        if bits_collected >= 8 {
            bits_collected -= 8;
            output.push((buffer >> bits_collected) as u8);
            buffer &= (1 << bits_collected) - 1;
        }
    }
    String::from_utf8(output).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Value {
        serde_json::json!({
            "default_branch": "main",
            "has_issues": true,
            "archived": false,
            "private": false,
            "html_url": "https://codeberg.org/acme/repo",
            "ssh_url": "git@codeberg.org:acme/repo.git",
            "clone_url": "https://codeberg.org/acme/repo.git",
        })
    }

    #[test]
    fn composer_forgejo_url_parses_https_and_ssh_repository_urls() {
        for url in [
            "git@codeberg.org:acme/repo.git",
            "https://codeberg.org/acme/repo",
            "https://codeberg.org/acme/repo.git",
        ] {
            let parsed = ForgejoUrl::parse(url).unwrap();
            assert_eq!(parsed.origin_url, "codeberg.org");
            assert_eq!(parsed.owner, "acme");
            assert_eq!(parsed.repository, "repo");
            assert_eq!(
                parsed.api_url,
                "https://codeberg.org/api/v1/repos/acme/repo"
            );
        }
    }

    #[test]
    fn composer_forgejo_url_rejects_non_repository_urls() {
        assert!(matches!(
            ForgejoUrl::parse("https://example.org"),
            Err(VcsDriverError::InvalidFormat(_))
        ));
    }

    #[test]
    fn composer_forgejo_url_generates_ssh_checkout_url() {
        let parsed = ForgejoUrl::parse("https://codeberg.org/acme/repo").unwrap();
        assert_eq!(parsed.generate_ssh_url(), "git@codeberg.org:acme/repo.git");
    }

    #[test]
    fn composer_forgejo_driver_initializes_public_repository() {
        let mut driver = ForgejoDriver::new("https://codeberg.org/acme/repo.git").unwrap();
        driver.initialize_from_project(&project()).unwrap();

        assert_eq!(driver.get_root_identifier().unwrap(), "main");
        assert_eq!(
            driver.dist("SOMESHA"),
            VcsDist {
                r#type: "zip",
                url: "https://codeberg.org/api/v1/repos/acme/repo/archive/SOMESHA.zip".to_owned(),
                reference: "SOMESHA".to_owned(),
                shasum: String::new(),
            }
        );
        assert_eq!(
            driver.source("SOMESHA").unwrap(),
            VcsSource {
                r#type: "git",
                url: "https://codeberg.org/acme/repo.git".to_owned(),
                reference: "SOMESHA".to_owned(),
            }
        );
    }

    #[test]
    fn composer_forgejo_driver_parses_branches() {
        let page = serde_json::json!([
            {"name": "main", "commit": {"id": "SOMESHA"}}
        ]);

        assert_eq!(
            ForgejoDriver::branches_from_pages(&[page]).unwrap(),
            HashMap::from([("main".to_owned(), "SOMESHA".to_owned())])
        );
    }

    #[test]
    fn composer_forgejo_driver_parses_tags() {
        let page = serde_json::json!([
            {"name": "1.0", "commit": {"sha": "SOMESHA"}}
        ]);

        assert_eq!(
            ForgejoDriver::tags_from_pages(&[page]).unwrap(),
            HashMap::from([("1.0".to_owned(), "SOMESHA".to_owned())])
        );
    }

    #[test]
    fn composer_forgejo_driver_preserves_empty_file_content() {
        let response = serde_json::json!({"encoding": "base64", "content": ""});

        assert_eq!(
            ForgejoDriver::decode_file_content_response("composer.json", &response).unwrap(),
            ""
        );
    }

    #[test]
    fn composer_forgejo_driver_supports_configured_domains() {
        let domains = ["codeberg.org"];
        assert!(!ForgejoDriver::supports_with_domains(
            "https://example.org/acme/repo",
            &domains
        ));
        assert!(ForgejoDriver::supports_with_domains(
            "https://codeberg.org/acme/repository",
            &domains
        ));
    }
}
