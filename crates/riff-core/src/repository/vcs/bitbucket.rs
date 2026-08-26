//! Bitbucket driver - uses Bitbucket's v2 API for repository access.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use super::driver::{VcsDist, VcsDriver, VcsDriverError, VcsInfo, VcsSource};
use crate::config::AuthConfig;

/// Bitbucket driver for Git repositories hosted on bitbucket.org.
#[derive(Debug)]
pub struct BitbucketDriver {
    url: String,
    workspace: String,
    repo_slug: String,
    api_url: String,
    oauth_token: Option<String>,
    app_password: Option<(String, String)>,
    default_branch: Option<String>,
    clone_https_url: Option<String>,
    home_url: String,
    website: Option<String>,
    has_issues: bool,
    vcs_type: Option<String>,
    tags: Mutex<Option<HashMap<String, String>>>,
    branches: Mutex<Option<HashMap<String, String>>>,
}

impl BitbucketDriver {
    /// Create a driver from the HTTPS repository URL accepted by Composer's
    /// GitBitbucketDriver.
    pub fn new(url: impl Into<String>) -> Result<Self, VcsDriverError> {
        let url = url.into();
        let (workspace, repo_slug) = parse_bitbucket_url(&url).ok_or_else(|| {
            VcsDriverError::InvalidFormat(format!(
                "The Bitbucket repository URL {url} is invalid. It must be the HTTPS URL of a Bitbucket repository."
            ))
        })?;
        let api_url = format!("https://api.bitbucket.org/2.0/repositories/{workspace}/{repo_slug}");
        let home_url = format!("https://bitbucket.org/{workspace}/{repo_slug}");

        Ok(Self {
            url,
            workspace,
            repo_slug,
            api_url,
            oauth_token: None,
            app_password: None,
            default_branch: None,
            clone_https_url: None,
            home_url,
            website: None,
            has_issues: false,
            vcs_type: None,
            tags: Mutex::new(None),
            branches: Mutex::new(None),
        })
    }

    /// Set OAuth token for authentication.
    pub fn with_oauth_token(mut self, token: impl Into<String>) -> Self {
        self.oauth_token = Some(token.into());
        self
    }

    /// Set an app password for authentication.
    pub fn with_app_password(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.app_password = Some((username.into(), password.into()));
        self
    }

    /// Configure authentication from Riff's Composer auth configuration.
    pub fn with_auth(mut self, auth: &AuthConfig) -> Self {
        if let Some(credentials) = auth.get_bitbucket_oauth("bitbucket.org") {
            self.app_password = Some((
                credentials.consumer_key.clone(),
                credentials.consumer_secret.clone(),
            ));
        }
        self
    }

    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    pub fn project_api_url(&self) -> String {
        format!("{}?fields=-project%2C-owner", self.api_url)
    }

    pub fn tags_api_url(&self) -> String {
        format!(
            "{}/refs/tags?pagelen=100&fields=values.name%2Cvalues.target.hash%2Cnext&sort=-target.date",
            self.api_url
        )
    }

    pub fn branches_api_url(&self) -> String {
        format!(
            "{}/refs/branches?pagelen=100&fields=values.name%2Cvalues.target.hash%2Cvalues.heads%2Cnext&sort=-target.date",
            self.api_url
        )
    }

    pub fn file_api_url(&self, identifier: &str, file: &str) -> String {
        format!(
            "{}/src/{}/{}",
            self.api_url,
            urlencoding::encode(identifier),
            file.trim_start_matches('/')
        )
    }

    pub fn commit_api_url(&self, identifier: &str) -> String {
        format!(
            "{}/commit/{}?fields=date",
            self.api_url,
            urlencoding::encode(identifier)
        )
    }

    /// Loads the repository-level fields used by the remaining driver calls.
    pub fn initialize(&mut self) -> Result<(), VcsDriverError> {
        let project = self.api_request_url(&self.project_api_url())?;
        self.initialize_from_project(&project)
    }

    /// Applies a Bitbucket repository API response without requiring network
    /// access. This is also the common parsing path used by `initialize`.
    pub fn initialize_from_project(&mut self, project: &Value) -> Result<(), VcsDriverError> {
        let object = project.as_object().ok_or_else(|| {
            VcsDriverError::InvalidFormat("Expected a Bitbucket repository object".to_owned())
        })?;
        self.default_branch = object
            .get("mainbranch")
            .and_then(|branch| branch.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| Some("master".to_owned()));
        self.vcs_type = object.get("scm").and_then(Value::as_str).map(str::to_owned);
        self.has_issues = object
            .get("has_issues")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.website = object
            .get("website")
            .and_then(Value::as_str)
            .filter(|website| !website.is_empty())
            .map(str::to_owned);
        if let Some(home_url) = object
            .get("links")
            .and_then(|links| links.get("html"))
            .and_then(|html| html.get("href"))
            .and_then(Value::as_str)
        {
            self.home_url = home_url.to_owned();
        }
        self.clone_https_url = object
            .get("links")
            .and_then(|links| links.get("clone"))
            .and_then(Value::as_array)
            .and_then(|links| {
                links.iter().find_map(|link| {
                    (link.get("name").and_then(Value::as_str) == Some("https"))
                        .then(|| link.get("href").and_then(Value::as_str))
                        .flatten()
                        .and_then(strip_clone_username)
                })
            });
        Ok(())
    }

    pub fn dist(&self, reference: impl Into<String>) -> VcsDist {
        let reference = reference.into();
        VcsDist {
            r#type: "zip",
            url: format!("{}/get/{reference}.zip", self.home_url),
            reference,
            shasum: String::new(),
        }
    }

    pub fn source(&self, reference: impl Into<String>) -> VcsSource {
        VcsSource {
            r#type: "git",
            url: self.get_url().to_owned(),
            reference: reference.into(),
        }
    }

    /// Parses and combines Bitbucket reference response pages.
    pub fn references_from_pages(
        pages: &[Value],
    ) -> Result<HashMap<String, String>, VcsDriverError> {
        let mut references = HashMap::new();
        for page in pages {
            extend_references(&mut references, page)?;
        }
        Ok(references)
    }

    /// Primes the tag cache without replacing data already fetched.
    pub fn cache_tags_from_pages(
        &self,
        pages: &[Value],
    ) -> Result<HashMap<String, String>, VcsDriverError> {
        cache_references(&self.tags, Self::references_from_pages(pages)?)
    }

    /// Primes the branch cache without replacing data already fetched.
    pub fn cache_branches_from_pages(
        &self,
        pages: &[Value],
    ) -> Result<HashMap<String, String>, VcsDriverError> {
        cache_references(&self.branches, Self::references_from_pages(pages)?)
    }

    /// Adds Bitbucket's default support and homepage metadata while preserving
    /// valid values provided by the package itself.
    pub fn enrich_composer_manifest(
        &self,
        identifier: &str,
        mut manifest: Value,
    ) -> Result<Value, VcsDriverError> {
        if !manifest.is_object() {
            return Err(VcsDriverError::InvalidFormat(
                "Expected composer.json to contain an object".to_owned(),
            ));
        }
        if manifest
            .get("support")
            .is_some_and(|support| !support.is_object())
        {
            manifest["support"] = Value::Object(serde_json::Map::new());
        }
        if manifest.get("support").is_none() {
            manifest["support"] = Value::Object(serde_json::Map::new());
        }

        let tags = cached(&self.tags)?.unwrap_or_default();
        let branches = cached(&self.branches)?.unwrap_or_default();
        let (label, hash) = reference_label_and_hash(identifier, &tags, &branches);
        let source_url = hash.map_or_else(
            || format!("{}/src", self.home_url),
            |hash| format!("{}/src/{hash}/?at={label}", self.home_url),
        );
        let support = manifest
            .get_mut("support")
            .and_then(Value::as_object_mut)
            .expect("support was normalized to an object");
        support
            .entry("source".to_owned())
            .or_insert_with(|| Value::String(source_url));
        if self.has_issues {
            support.entry("issues".to_owned()).or_insert_with(|| {
                Value::String(format!("{}/issues", self.home_url.trim_end_matches('/')))
            });
        }

        if manifest.get("homepage").is_none() {
            manifest["homepage"] =
                Value::String(self.website.as_deref().unwrap_or(&self.home_url).to_owned());
        }
        Ok(manifest)
    }

    /// Builds the same observable package information as the network-backed
    /// driver from already-decoded deterministic inputs.
    pub fn composer_information_from_parts(
        &self,
        identifier: &str,
        manifest: Value,
        time: Option<String>,
    ) -> Result<VcsInfo, VcsDriverError> {
        Ok(VcsInfo {
            manifest: Some(self.enrich_composer_manifest(identifier, manifest)?),
            identifier: identifier.to_owned(),
            time,
        })
    }

    fn ensure_git_repository(&self) -> Result<(), VcsDriverError> {
        if self.vcs_type.as_deref().is_some_and(|kind| kind != "git") {
            return Err(VcsDriverError::InvalidFormat(format!(
                "{} does not appear to be a git repository, use {} but remember that Bitbucket no longer supports the mercurial repositories. https://bitbucket.org/blog/sunsetting-mercurial-support-in-bitbucket",
                self.url,
                self.get_url()
            )));
        }
        Ok(())
    }

    fn request(&self, url: &str) -> reqwest::blocking::RequestBuilder {
        let client = reqwest::blocking::Client::new();
        let mut request = client.get(url);
        if let Some(token) = &self.oauth_token {
            request = request.header("Authorization", format!("Bearer {token}"));
        } else if let Some((username, password)) = &self.app_password {
            request = request.basic_auth(username, Some(password));
        }
        request
            .header("Accept", "application/json")
            .header("User-Agent", "riff-composer")
    }

    fn api_request_url(&self, url: &str) -> Result<Value, VcsDriverError> {
        let response = self
            .request(url)
            .send()
            .map_err(|error| VcsDriverError::Network(error.to_string()))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(VcsDriverError::NotFound(format!(
                "{}/{}",
                self.workspace, self.repo_slug
            )));
        }
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(VcsDriverError::AuthRequired(
                "Bitbucket authentication required".to_owned(),
            ));
        }
        if !status.is_success() {
            return Err(VcsDriverError::Network(format!(
                "Bitbucket API error: {status}"
            )));
        }
        response.json().map_err(|error| {
            VcsDriverError::InvalidFormat(format!("Invalid Bitbucket JSON response: {error}"))
        })
    }

    fn fetch_references(
        &self,
        first_url: String,
    ) -> Result<HashMap<String, String>, VcsDriverError> {
        let mut references = HashMap::new();
        let mut next_url = Some(first_url);
        for _ in 0..100 {
            let Some(url) = next_url else {
                break;
            };
            let page = self.api_request_url(&url)?;
            extend_references(&mut references, &page)?;
            next_url = page.get("next").and_then(Value::as_str).map(str::to_owned);
        }
        Ok(references)
    }

    fn get_file_content_api(&self, file: &str, identifier: &str) -> Result<String, VcsDriverError> {
        let identifier = if identifier.contains('/') {
            cached(&self.branches)?
                .and_then(|branches| branches.get(identifier).cloned())
                .unwrap_or_else(|| identifier.to_owned())
        } else {
            identifier.to_owned()
        };
        let url = self.file_api_url(&identifier, file);
        let response = self
            .request(&url)
            .send()
            .map_err(|error| VcsDriverError::Network(error.to_string()))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(VcsDriverError::FileNotFound(file.to_owned()));
        }
        if !status.is_success() {
            return Err(VcsDriverError::Network(format!(
                "Bitbucket API error: {status}"
            )));
        }
        response
            .text()
            .map_err(|error| VcsDriverError::Network(error.to_string()))
    }
}

impl VcsDriver for BitbucketDriver {
    fn get_root_identifier(&self) -> Result<String, VcsDriverError> {
        if self.vcs_type.is_some() {
            self.ensure_git_repository()?;
            return Ok(self
                .default_branch
                .clone()
                .unwrap_or_else(|| "master".to_owned()));
        }

        let project = self.api_request_url(&self.project_api_url())?;
        let kind = project.get("scm").and_then(Value::as_str);
        if kind.is_some_and(|kind| kind != "git") {
            let clone_url = project
                .get("links")
                .and_then(|links| links.get("clone"))
                .and_then(Value::as_array)
                .and_then(|links| {
                    links.iter().find_map(|link| {
                        (link.get("name").and_then(Value::as_str) == Some("https"))
                            .then(|| link.get("href").and_then(Value::as_str))
                            .flatten()
                            .and_then(strip_clone_username)
                    })
                })
                .unwrap_or_else(|| self.url.clone());
            return Err(VcsDriverError::InvalidFormat(format!(
                "{} does not appear to be a git repository, use {} but remember that Bitbucket no longer supports the mercurial repositories. https://bitbucket.org/blog/sunsetting-mercurial-support-in-bitbucket",
                self.url, clone_url
            )));
        }
        Ok(project
            .get("mainbranch")
            .and_then(|branch| branch.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("master")
            .to_owned())
    }

    fn get_tags(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        if let Some(tags) = cached(&self.tags)? {
            return Ok(tags);
        }
        cache_references(&self.tags, self.fetch_references(self.tags_api_url())?)
    }

    fn get_branches(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        if let Some(branches) = cached(&self.branches)? {
            return Ok(branches);
        }
        cache_references(
            &self.branches,
            self.fetch_references(self.branches_api_url())?,
        )
    }

    fn get_composer_information(&self, identifier: &str) -> Result<VcsInfo, VcsDriverError> {
        let content = self.get_file_content("composer.json", identifier)?;
        let manifest = serde_json::from_str(&content).map_err(|error| {
            VcsDriverError::InvalidFormat(format!("Invalid composer.json: {error}"))
        })?;
        let time = self
            .api_request_url(&self.commit_api_url(identifier))
            .ok()
            .and_then(|commit| {
                commit
                    .get("date")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        self.get_tags()?;
        self.get_branches()?;
        self.composer_information_from_parts(identifier, manifest, time)
    }

    fn get_file_content(&self, file: &str, identifier: &str) -> Result<String, VcsDriverError> {
        self.get_file_content_api(file, identifier)
    }

    fn supports(url: &str, _deep: bool) -> bool {
        parse_bitbucket_url(url).is_some()
    }

    fn get_url(&self) -> &str {
        self.clone_https_url.as_deref().unwrap_or(&self.url)
    }

    fn get_vcs_type(&self) -> &str {
        "git"
    }
}

/// Parse a Composer-compatible Bitbucket Git URL into workspace and slug.
pub fn parse_bitbucket_url(input: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(input.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("bitbucket.org"))
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    let path = parsed.path().strip_prefix('/')?;
    let path = path.strip_suffix('/').unwrap_or(path);
    let mut parts = path.split('/');
    let workspace = parts.next()?;
    let repo_slug = parts.next()?;
    if workspace.is_empty() || repo_slug.is_empty() || parts.next().is_some() {
        return None;
    }
    let repo_slug = repo_slug.strip_suffix(".git").unwrap_or(repo_slug);
    if repo_slug.is_empty() {
        return None;
    }
    Some((workspace.to_owned(), repo_slug.to_owned()))
}

fn strip_clone_username(input: &str) -> Option<String> {
    let mut url = url::Url::parse(input).ok()?;
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    Some(url.to_string())
}

fn cached(
    cache: &Mutex<Option<HashMap<String, String>>>,
) -> Result<Option<HashMap<String, String>>, VcsDriverError> {
    cache
        .lock()
        .map(|value| value.clone())
        .map_err(|_| VcsDriverError::InvalidFormat("Bitbucket reference cache poisoned".to_owned()))
}

fn cache_references(
    cache: &Mutex<Option<HashMap<String, String>>>,
    references: HashMap<String, String>,
) -> Result<HashMap<String, String>, VcsDriverError> {
    let mut cached = cache.lock().map_err(|_| {
        VcsDriverError::InvalidFormat("Bitbucket reference cache poisoned".to_owned())
    })?;
    Ok(cached.get_or_insert(references).clone())
}

fn extend_references(
    references: &mut HashMap<String, String>,
    page: &Value,
) -> Result<(), VcsDriverError> {
    let values = page
        .get("values")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            VcsDriverError::InvalidFormat("Expected Bitbucket reference values".to_owned())
        })?;
    for value in values {
        if let (Some(name), Some(hash)) = (
            value.get("name").and_then(Value::as_str),
            value
                .get("target")
                .and_then(|target| target.get("hash"))
                .and_then(Value::as_str),
        ) {
            references.insert(name.to_owned(), hash.to_owned());
        }
    }
    Ok(())
}

fn reference_label_and_hash<'a>(
    identifier: &'a str,
    tags: &'a HashMap<String, String>,
    branches: &'a HashMap<String, String>,
) -> (&'a str, Option<&'a str>) {
    if let Some(hash) = tags.get(identifier).or_else(|| branches.get(identifier)) {
        return (identifier, Some(hash));
    }
    if let Some((label, hash)) = tags
        .iter()
        .chain(branches.iter())
        .find(|(_, hash)| hash.as_str() == identifier)
    {
        return (label, Some(hash));
    }
    (identifier, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(scm: &str) -> Value {
        let suffix = if scm == "git" { ".git" } else { "" };
        serde_json::json!({
            "mainbranch": {"name": "main"},
            "scm": scm,
            "website": "",
            "has_issues": false,
            "name": "repo",
            "links": {
                "branches": {"href": "https://api.bitbucket.org/2.0/repositories/user/repo/refs/branches"},
                "tags": {"href": "https://api.bitbucket.org/2.0/repositories/user/repo/refs/tags"},
                "clone": [
                    {"href": format!("https://user@bitbucket.org/user/repo{suffix}"), "name": "https"},
                    {"href": format!("ssh://{scm}@bitbucket.org/user/repo{suffix}"), "name": "ssh"}
                ],
                "html": {"href": "https://bitbucket.org/user/repo"}
            }
        })
    }

    fn reference_page(entries: &[(&str, &str)]) -> Value {
        serde_json::json!({
            "values": entries
                .iter()
                .map(|(name, hash)| serde_json::json!({"name": name, "target": {"hash": hash}}))
                .collect::<Vec<_>>()
        })
    }

    // Ported from Composer\Test\Repository\Vcs\GitBitbucketDriverTest::
    // testGetRootIdentifierWrongScmType.
    #[test]
    fn composer_bitbucket_driver_rejects_mercurial_repository_metadata() {
        let mut driver = BitbucketDriver::new("https://bitbucket.org/user/repo.git").unwrap();
        driver.initialize_from_project(&project("hg")).unwrap();

        let VcsDriverError::InvalidFormat(message) = driver.get_root_identifier().unwrap_err()
        else {
            panic!("expected invalid repository format");
        };
        assert_eq!(
            message,
            "https://bitbucket.org/user/repo.git does not appear to be a git repository, use https://bitbucket.org/user/repo but remember that Bitbucket no longer supports the mercurial repositories. https://bitbucket.org/blog/sunsetting-mercurial-support-in-bitbucket"
        );
    }

    // Ported from Composer\Test\Repository\Vcs\GitBitbucketDriverTest::testDriver.
    #[test]
    fn composer_bitbucket_driver_processes_project_references_and_manifest() {
        let mut driver = BitbucketDriver::new("https://bitbucket.org/user/repo.git").unwrap();
        assert_eq!(
            driver.project_api_url(),
            "https://api.bitbucket.org/2.0/repositories/user/repo?fields=-project%2C-owner"
        );
        driver.initialize_from_project(&project("git")).unwrap();
        assert_eq!(driver.get_root_identifier().unwrap(), "main");
        assert_eq!(
            driver.tags_api_url(),
            "https://api.bitbucket.org/2.0/repositories/user/repo/refs/tags?pagelen=100&fields=values.name%2Cvalues.target.hash%2Cnext&sort=-target.date"
        );
        assert_eq!(
            driver.branches_api_url(),
            "https://api.bitbucket.org/2.0/repositories/user/repo/refs/branches?pagelen=100&fields=values.name%2Cvalues.target.hash%2Cvalues.heads%2Cnext&sort=-target.date"
        );
        assert_eq!(
            driver.file_api_url("main", "composer.json"),
            "https://api.bitbucket.org/2.0/repositories/user/repo/src/main/composer.json"
        );
        assert_eq!(
            driver.commit_api_url("main"),
            "https://api.bitbucket.org/2.0/repositories/user/repo/commit/main?fields=date"
        );

        let tags = driver
            .cache_tags_from_pages(&[reference_page(&[
                ("1.0.1", "9b78a3932143497c519e49b8241083838c8ff8a1"),
                ("1.0.0", "d3393d514318a9267d2f8ebbf463a9aaa389f8eb"),
            ])])
            .unwrap();
        assert_eq!(
            tags,
            HashMap::from([
                (
                    "1.0.1".to_owned(),
                    "9b78a3932143497c519e49b8241083838c8ff8a1".to_owned(),
                ),
                (
                    "1.0.0".to_owned(),
                    "d3393d514318a9267d2f8ebbf463a9aaa389f8eb".to_owned(),
                ),
            ])
        );
        let branches = driver
            .cache_branches_from_pages(&[reference_page(&[(
                "main",
                "937992d19d72b5116c3e8c4a04f960e5fa270b22",
            )])])
            .unwrap();
        assert_eq!(
            branches,
            HashMap::from([(
                "main".to_owned(),
                "937992d19d72b5116c3e8c4a04f960e5fa270b22".to_owned(),
            )])
        );

        let information = driver
            .composer_information_from_parts(
                "main",
                serde_json::json!({
                    "name": "user/repo",
                    "description": "test repo",
                    "license": "GPL",
                    "authors": [{"name": "Name", "email": "local@domain.tld"}],
                    "require": {"creator/package": "^1.0"},
                    "require-dev": {"phpunit/phpunit": "~4.8"}
                }),
                Some("2016-05-17T13:19:52+00:00".to_owned()),
            )
            .unwrap();
        assert_eq!(information.identifier, "main");
        assert_eq!(
            information.time.as_deref(),
            Some("2016-05-17T13:19:52+00:00")
        );
        assert_eq!(
            information.manifest.unwrap(),
            serde_json::json!({
                "name": "user/repo",
                "description": "test repo",
                "license": "GPL",
                "authors": [{"name": "Name", "email": "local@domain.tld"}],
                "require": {"creator/package": "^1.0"},
                "require-dev": {"phpunit/phpunit": "~4.8"},
                "support": {
                    "source": "https://bitbucket.org/user/repo/src/937992d19d72b5116c3e8c4a04f960e5fa270b22/?at=main"
                },
                "homepage": "https://bitbucket.org/user/repo"
            })
        );
    }

    // Ported from Composer\Test\Repository\Vcs\GitBitbucketDriverTest::testGetParams.
    #[test]
    fn composer_bitbucket_driver_builds_url_dist_and_source_metadata() {
        let mut driver = BitbucketDriver::new("https://bitbucket.org/user/repo.git").unwrap();
        driver.initialize_from_project(&project("git")).unwrap();
        assert_eq!(driver.get_url(), "https://bitbucket.org/user/repo.git");
        assert_eq!(
            driver.dist("reference"),
            VcsDist {
                r#type: "zip",
                url: "https://bitbucket.org/user/repo/get/reference.zip".to_owned(),
                reference: "reference".to_owned(),
                shasum: String::new(),
            }
        );
        assert_eq!(
            driver.source("reference"),
            VcsSource {
                r#type: "git",
                url: "https://bitbucket.org/user/repo.git".to_owned(),
                reference: "reference".to_owned(),
            }
        );
    }

    // Ported from Composer\Test\Repository\Vcs\GitBitbucketDriverTest::
    // testInitializeInvalidRepositoryUrl.
    #[test]
    fn composer_bitbucket_driver_rejects_invalid_repository_url() {
        assert!(matches!(
            BitbucketDriver::new("https://bitbucket.org/acme"),
            Err(VcsDriverError::InvalidFormat(_))
        ));
    }

    // Ported from Composer\Test\Repository\Vcs\GitBitbucketDriverTest::testInvalidSupportData.
    #[test]
    fn composer_bitbucket_driver_replaces_invalid_support_metadata() {
        let mut driver = BitbucketDriver::new("https://bitbucket.org/user/repo.git").unwrap();
        driver.initialize_from_project(&project("git")).unwrap();
        driver
            .cache_branches_from_pages(&[reference_page(&[(
                "main",
                "937992d19d72b5116c3e8c4a04f960e5fa270b22",
            )])])
            .unwrap();
        let manifest = driver
            .enrich_composer_manifest(
                "main",
                serde_json::json!({"support": "https://bitbucket.org/user/repo.git"}),
            )
            .unwrap();
        assert_eq!(
            manifest["support"]["source"],
            "https://bitbucket.org/user/repo/src/937992d19d72b5116c3e8c4a04f960e5fa270b22/?at=main"
        );
    }

    // Ported from Composer\Test\Repository\Vcs\GitBitbucketDriverTest::testSupports.
    #[test]
    fn composer_bitbucket_driver_supports_only_https_repository_urls() {
        assert!(BitbucketDriver::supports(
            "https://bitbucket.org/user/repo.git",
            false
        ));
        assert!(!BitbucketDriver::supports(
            "git@bitbucket.org:user/repo.git",
            false
        ));
        assert!(!BitbucketDriver::supports(
            "https://github.com/user/repo.git",
            false
        ));
    }
}
