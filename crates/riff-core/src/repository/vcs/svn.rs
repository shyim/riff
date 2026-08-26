//! Subversion command construction, authentication, and repository access.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;

use super::driver::{VcsDriver, VcsDriverError, VcsInfo};
use crate::config::{AuthConfig, HttpBasicCredentials};

const MAX_AUTH_RETRIES: usize = 5;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SvnCommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait SvnProcess: Send + Sync {
    fn run(&self, arguments: &[String]) -> SvnCommandOutput;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemSvnProcess;

impl SvnProcess for SystemSvnProcess {
    fn run(&self, arguments: &[String]) -> SvnCommandOutput {
        let Some((program, arguments)) = arguments.split_first() else {
            return SvnCommandOutput {
                stderr: "empty Subversion command".to_owned(),
                ..SvnCommandOutput::default()
            };
        };
        match Command::new(program).args(arguments).output() {
            Ok(output) => SvnCommandOutput {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            },
            Err(error) => SvnCommandOutput {
                stderr: error.to_string(),
                ..SvnCommandOutput::default()
            },
        }
    }
}

/// Testable Subversion command utility with Composer-compatible authentication.
pub struct Svn<P = SystemSvnProcess> {
    credentials: Option<HttpBasicCredentials>,
    cache_credentials: bool,
    process: Arc<P>,
}

impl Svn<SystemSvnProcess> {
    pub fn new(url: impl Into<String>, auth: &AuthConfig) -> Self {
        Self::with_process(url, auth, Arc::new(SystemSvnProcess))
    }
}

impl<P: SvnProcess> Svn<P> {
    pub fn with_process(url: impl Into<String>, auth: &AuthConfig, process: Arc<P>) -> Self {
        let url = url.into();
        let credentials = credentials_for_url(&url, auth);
        Self {
            credentials,
            cache_credentials: true,
            process,
        }
    }

    pub fn set_cache_credentials(&mut self, cache_credentials: bool) {
        self.cache_credentials = cache_credentials;
    }

    pub fn credential_args(&self) -> Vec<String> {
        let Some(credentials) = &self.credentials else {
            return Vec::new();
        };
        let mut arguments = Vec::with_capacity(5);
        if !self.cache_credentials {
            arguments.push("--no-auth-cache".to_owned());
        }
        arguments.extend([
            "--username".to_owned(),
            credentials.username.clone(),
            "--password".to_owned(),
            credentials.password.clone(),
        ]);
        arguments
    }

    /// Builds argv directly, keeping credentials and targets out of a shell.
    pub fn command(&self, command: &[&str], target: &str) -> Vec<String> {
        let mut arguments = command
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect::<Vec<_>>();
        arguments.push("--non-interactive".to_owned());
        arguments.extend(self.credential_args());
        arguments.extend(["--".to_owned(), target.to_owned()]);
        arguments
    }

    pub fn execute(&self, command: &[&str], target: &str) -> Result<String, VcsDriverError> {
        let arguments = self.command(command, target);
        for attempt in 0..=MAX_AUTH_RETRIES {
            let output = self.process.run(&arguments);
            if output.success {
                return Ok(output.stdout);
            }
            let message = combined_output(&output);
            if !is_authentication_failure(&message) {
                return Err(VcsDriverError::ProcessError(message));
            }
            if attempt == MAX_AUTH_RETRIES {
                return Err(VcsDriverError::AuthRequired(format!(
                    "wrong credentials provided ({message})"
                )));
            }
        }
        unreachable!("the bounded Subversion retry loop always returns")
    }

    pub fn binary_version(&self) -> Option<String> {
        let output = self
            .process
            .run(&["svn".to_owned(), "--version".to_owned()]);
        output
            .success
            .then(|| first_version(&output.stdout))
            .flatten()
    }

    fn list(&self, target: &str) -> Result<String, VcsDriverError> {
        self.execute(&["svn", "ls"], target)
    }
}

/// Subversion repository driver using an injectable command runner.
pub struct SvnDriver<P = SystemSvnProcess> {
    url: String,
    utility: Svn<P>,
}

impl SvnDriver<SystemSvnProcess> {
    pub fn new(url: impl Into<String>, auth: &AuthConfig) -> Self {
        let url = url.into();
        Self {
            utility: Svn::new(url.clone(), auth),
            url,
        }
    }
}

impl<P: SvnProcess> SvnDriver<P> {
    pub fn with_process(url: impl Into<String>, auth: &AuthConfig, process: Arc<P>) -> Self {
        let url = url.into();
        Self {
            utility: Svn::with_process(url.clone(), auth, process),
            url,
        }
    }

    pub fn set_cache_credentials(&mut self, cache_credentials: bool) {
        self.utility.set_cache_credentials(cache_credentials);
    }

    /// Probes the conventional trunk and reports sanitized repository errors.
    pub fn initialize(&self) -> Result<(), VcsDriverError> {
        let trunk = format!("{}/trunk", self.url.trim_end_matches('/'));
        match self.utility.execute(&["svn", "ls", "--verbose"], &trunk) {
            Ok(_) => Ok(()),
            Err(error) => {
                if self.utility.binary_version().is_none() {
                    return Err(VcsDriverError::ProcessError(format!(
                        "Failed to load {}, svn was not found, check that it is installed and in your PATH env.",
                        sanitize_url(&self.url)
                    )));
                }
                let message = match error {
                    VcsDriverError::AuthRequired(message)
                    | VcsDriverError::ProcessError(message) => message,
                    other => other.to_string(),
                };
                Err(VcsDriverError::ProcessError(format!(
                    "Repository {} could not be processed, {}",
                    sanitize_url(&self.url),
                    sanitize_text(&message)
                )))
            }
        }
    }

    pub fn supports_url(url: &str, deep: bool) -> bool {
        let normalized = normalize_url(url);
        let lower = normalized.to_ascii_lowercase();
        if lower.starts_with("svn://")
            || lower.starts_with("svn+ssh://")
            || url::Url::parse(&normalized)
                .ok()
                .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
                .is_some_and(|host| host.starts_with("svn."))
        {
            return true;
        }
        deep && normalized.starts_with("file://")
    }

    fn references(&self, directory: &str) -> Result<HashMap<String, String>, VcsDriverError> {
        let base = format!("{}/{}", self.url.trim_end_matches('/'), directory);
        let output = self.utility.list(&base)?;
        Ok(output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| line.trim_end_matches('/'))
            .map(|name| (name.to_owned(), format!("{base}/{name}")))
            .collect())
    }
}

impl<P: SvnProcess> VcsDriver for SvnDriver<P> {
    fn get_root_identifier(&self) -> Result<String, VcsDriverError> {
        Ok(format!("{}/trunk", self.url.trim_end_matches('/')))
    }

    fn get_tags(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        self.references("tags")
    }

    fn get_branches(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        self.references("branches").or_else(|_| {
            Ok(HashMap::from([(
                "trunk".to_owned(),
                format!("{}/trunk", self.url.trim_end_matches('/')),
            )]))
        })
    }

    fn get_composer_information(&self, identifier: &str) -> Result<VcsInfo, VcsDriverError> {
        let content = self.get_file_content("composer.json", identifier)?;
        let manifest = serde_json::from_str(&content)
            .map_err(|error| VcsDriverError::InvalidFormat(error.to_string()))?;
        Ok(VcsInfo {
            manifest: Some(manifest),
            identifier: identifier.to_owned(),
            time: None,
        })
    }

    fn get_file_content(&self, file: &str, identifier: &str) -> Result<String, VcsDriverError> {
        let target = format!("{}/{}", identifier.trim_end_matches('/'), file);
        self.utility.execute(&["svn", "cat"], &target)
    }

    fn supports(url: &str, deep: bool) -> bool {
        Self::supports_url(url, deep)
    }

    fn get_url(&self) -> &str {
        &self.url
    }

    fn get_vcs_type(&self) -> &str {
        "svn"
    }
}

fn credentials_for_url(url: &str, auth: &AuthConfig) -> Option<HttpBasicCredentials> {
    let parsed = url::Url::parse(url).ok()?;
    if let Some(credentials) = parsed.host_str().and_then(|host| auth.get_http_basic(host)) {
        return Some(credentials.clone());
    }
    let username = parsed.username();
    if username.is_empty() {
        return None;
    }
    Some(HttpBasicCredentials {
        username: username.to_owned(),
        password: parsed.password().unwrap_or_default().to_owned(),
    })
}

fn combined_output(output: &SvnCommandOutput) -> String {
    match (output.stdout.trim(), output.stderr.trim()) {
        ("", stderr) => stderr.to_owned(),
        (stdout, "") => stdout.to_owned(),
        (stdout, stderr) => format!("{stdout}\n{stderr}"),
    }
}

fn is_authentication_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("could not authenticate to server:")
        || lower.contains("authorization failed")
        || lower.contains("authentication failed")
        || lower.contains("svn: e170001:")
        || lower.contains("svn: e215004:")
}

fn first_version(output: &str) -> Option<String> {
    output.split_whitespace().find_map(|word| {
        let version =
            word.trim_matches(|character: char| !character.is_ascii_digit() && character != '.');
        (version.contains('.')
            && version.split('.').all(|component| {
                !component.is_empty() && component.chars().all(|c| c.is_ascii_digit())
            }))
        .then(|| version.to_owned())
    })
}

fn normalize_url(url: &str) -> String {
    let path = std::path::Path::new(url);
    if path.is_absolute() {
        format!("file://{}", path.to_string_lossy().replace('\\', "/"))
    } else {
        url.to_owned()
    }
}

fn sanitize_url(url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_owned();
    };
    if !parsed.username().is_empty() {
        let _ = parsed.set_password(Some("***"));
    }
    parsed.to_string().trim_end_matches('/').to_owned()
}

fn sanitize_text(message: &str) -> String {
    let mut words = message.split_whitespace().peekable();
    let mut sanitized = Vec::new();
    while let Some(word) = words.next() {
        sanitized.push(word.to_owned());
        if word == "--password" && words.next().is_some() {
            sanitized.push("***".to_owned());
        }
    }
    sanitized.join(" ")
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct MockProcess {
        outputs: Mutex<VecDeque<SvnCommandOutput>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl MockProcess {
        fn with_outputs(outputs: impl IntoIterator<Item = SvnCommandOutput>) -> Arc<Self> {
            Arc::new(Self {
                outputs: Mutex::new(outputs.into_iter().collect()),
                calls: Mutex::default(),
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SvnProcess for MockProcess {
        fn run(&self, arguments: &[String]) -> SvnCommandOutput {
            self.calls.lock().unwrap().push(arguments.to_vec());
            self.outputs.lock().unwrap().pop_front().unwrap_or_default()
        }
    }

    fn failure(stderr: &str) -> SvnCommandOutput {
        SvnCommandOutput {
            success: false,
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn composer_svn_extracts_credentials_from_repository_urls() {
        let cases = [
            (
                "http://till:test@svn.example.org/",
                strings(&["--username", "till", "--password", "test"]),
            ),
            ("http://svn.apache.org/", Vec::new()),
            (
                "svn://johndoe@example.org",
                strings(&["--username", "johndoe", "--password", ""]),
            ),
        ];
        for (url, expected) in cases {
            let svn = Svn::new(url, &AuthConfig::default());
            assert_eq!(svn.credential_args(), expected, "{url}");
        }
    }

    #[test]
    fn composer_svn_builds_non_interactive_commands() {
        let svn = Svn::new("http://svn.example.org", &AuthConfig::default());
        assert_eq!(
            svn.command(&["svn", "ls"], "http://svn.example.org"),
            strings(&[
                "svn",
                "ls",
                "--non-interactive",
                "--",
                "http://svn.example.org"
            ])
        );
    }

    #[test]
    fn composer_svn_uses_configured_credentials() {
        let auth: AuthConfig = serde_json::from_value(serde_json::json!({
            "http-basic": {
                "svn.apache.org": {"username": "foo", "password": "bar"}
            }
        }))
        .unwrap();
        let svn = Svn::new("http://svn.apache.org", &auth);
        assert_eq!(
            svn.credential_args(),
            strings(&["--username", "foo", "--password", "bar"])
        );
    }

    #[test]
    fn composer_svn_caches_configured_credentials_by_default() {
        let auth: AuthConfig = serde_json::from_value(serde_json::json!({
            "http-basic": {
                "svn.apache.org": {"username": "foo", "password": "bar"}
            }
        }))
        .unwrap();
        let mut svn = Svn::new("http://svn.apache.org", &auth);
        svn.set_cache_credentials(true);
        assert_eq!(
            svn.credential_args(),
            strings(&["--username", "foo", "--password", "bar"])
        );
    }

    #[test]
    fn composer_svn_can_disable_the_authentication_cache() {
        let auth: AuthConfig = serde_json::from_value(serde_json::json!({
            "http-basic": {
                "svn.apache.org": {"username": "foo", "password": "bar"}
            }
        }))
        .unwrap();
        let mut svn = Svn::new("http://svn.apache.org", &auth);
        svn.set_cache_credentials(false);
        assert_eq!(
            svn.credential_args(),
            strings(&["--no-auth-cache", "--username", "foo", "--password", "bar"])
        );
    }

    #[test]
    fn composer_svn_driver_reports_sanitized_wrong_credentials() {
        let stderr = "svn: OPTIONS of 'https://corp.svn.local/repo': authorization failed: Could not authenticate to server: rejected Basic challenge (https://corp.svn.local/)";
        let mut outputs = (0..=MAX_AUTH_RETRIES)
            .map(|_| failure(stderr))
            .collect::<Vec<_>>();
        outputs.push(SvnCommandOutput {
            success: true,
            stdout: "1.2.3".to_owned(),
            stderr: String::new(),
        });
        let process = MockProcess::with_outputs(outputs);
        let driver = SvnDriver::with_process(
            "https://till:secret@corp.svn.local/repo",
            &AuthConfig::default(),
            Arc::clone(&process),
        );

        let error = driver.initialize().unwrap_err();
        let VcsDriverError::ProcessError(message) = error else {
            panic!("expected a repository process error");
        };
        assert_eq!(
            message,
            format!(
                "Repository https://till:***@corp.svn.local/repo could not be processed, wrong credentials provided ({stderr})"
            )
        );
        let expected = strings(&[
            "svn",
            "ls",
            "--verbose",
            "--non-interactive",
            "--username",
            "till",
            "--password",
            "secret",
            "--",
            "https://till:secret@corp.svn.local/repo/trunk",
        ]);
        assert_eq!(&process.calls()[..=MAX_AUTH_RETRIES], vec![expected; 6]);
        assert_eq!(
            process.calls().last(),
            Some(&strings(&["svn", "--version"]))
        );
    }

    #[test]
    fn composer_svn_driver_supports_svn_repository_urls() {
        for url in [
            "http://svn.apache.org",
            "https://svn.sf.net",
            "svn://example.org",
            "svn+ssh://example.org",
        ] {
            assert!(SvnDriver::<SystemSvnProcess>::supports_url(url, false));
        }
    }
}
