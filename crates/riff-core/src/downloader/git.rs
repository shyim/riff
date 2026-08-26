//! Git repository downloader.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::{Result, RiffError};
use riff_semver::Comparator;

/// A shell-free Git process request which can be executed by the system or a
/// deterministic test double.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitProcessCommand {
    pub program: String,
    pub args: Vec<String>,
    pub working_directory: Option<PathBuf>,
}

impl GitProcessCommand {
    pub fn new<I, S>(program: impl Into<String>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            working_directory: None,
        }
    }

    pub fn in_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(directory.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitProcessOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl GitProcessOutput {
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            success: true,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    pub fn failure(stderr: impl Into<String>) -> Self {
        Self {
            success: false,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
}

pub trait GitProcess: Send + Sync {
    fn execute(&self, command: &GitProcessCommand)
        -> std::result::Result<GitProcessOutput, String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGitProcess;

impl GitProcess for SystemGitProcess {
    fn execute(
        &self,
        command: &GitProcessCommand,
    ) -> std::result::Result<GitProcessOutput, String> {
        let mut process = Command::new(&command.program);
        process.args(&command.args);
        if let Some(directory) = &command.working_directory {
            process.current_dir(directory);
        }
        let output = process.output().map_err(|error| error.to_string())?;
        Ok(GitProcessOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Credentials which may be tried after an unauthenticated Git operation
/// fails. Values are intentionally omitted from `Debug` output.
#[derive(Clone, Default)]
pub struct GitAuthentication {
    github_token: Option<String>,
    bitbucket_credentials: Option<(String, String)>,
    bitbucket_oauth_token: Option<String>,
}

impl std::fmt::Debug for GitAuthentication {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GitAuthentication")
            .field(
                "github_token",
                &self.github_token.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "bitbucket_credentials",
                &self.bitbucket_credentials.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "bitbucket_oauth_token",
                &self.bitbucket_oauth_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl GitAuthentication {
    pub fn with_github_token(mut self, token: impl Into<String>) -> Self {
        self.github_token = Some(token.into());
        self
    }

    pub fn with_bitbucket_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.bitbucket_credentials = Some((username.into(), password.into()));
        self
    }

    pub fn with_bitbucket_oauth_token(mut self, token: impl Into<String>) -> Self {
        self.bitbucket_oauth_token = Some(token.into());
        self
    }
}

enum GitAttempt {
    Url(String),
    BitbucketConfigToken(String),
}

/// Runs URL-sensitive Git commands with Composer-compatible forge fallbacks.
pub struct GitRemoteExecutor<P = SystemGitProcess> {
    process: P,
    github_protocols: Vec<String>,
}

impl<P: GitProcess> GitRemoteExecutor<P> {
    pub fn new(process: P) -> Self {
        Self {
            process,
            github_protocols: vec!["https".to_owned(), "ssh".to_owned()],
        }
    }

    pub fn with_github_protocols<I, S>(mut self, protocols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.github_protocols = protocols.into_iter().map(Into::into).collect();
        self
    }

    pub fn run_command<F>(
        &self,
        url: &str,
        authentication: &GitAuthentication,
        command_for_url: F,
    ) -> Result<()>
    where
        F: Fn(&str) -> GitProcessCommand,
    {
        let mut last_error = String::new();
        for attempt in self.attempts(url, authentication) {
            match attempt {
                GitAttempt::Url(candidate) => {
                    let output = self.execute(&command_for_url(&candidate))?;
                    if output.success {
                        return Ok(());
                    }
                    last_error = output.stderr;
                }
                GitAttempt::BitbucketConfigToken(repository) => {
                    let output = self.execute(&GitProcessCommand::new(
                        "git",
                        ["config", "bitbucket.accesstoken"],
                    ))?;
                    let token = output.stdout.trim();
                    if output.success && !token.is_empty() {
                        let candidate =
                            bitbucket_authenticated_url(&repository, "x-token-auth", token);
                        let output = self.execute(&command_for_url(&candidate))?;
                        if output.success {
                            return Ok(());
                        }
                        last_error = output.stderr;
                    }
                }
            }
        }

        let _ = self.execute(&GitProcessCommand::new("git", ["--version"]))?;
        Err(RiffError::Git(format!(
            "Git command failed for {}{}",
            crate::url_utils::sanitize_url(url),
            if last_error.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", last_error.trim())
            }
        )))
    }

    /// Synchronizes a bare mirror and always removes credentials from its
    /// persisted origin URL, including after an update failure.
    pub fn sync_mirror(&self, url: &str, directory: &Path) -> Result<bool> {
        let updated = if directory.exists() {
            if !self
                .execute(
                    &GitProcessCommand::new("git", ["rev-parse", "--git-dir"])
                        .in_directory(directory),
                )?
                .success
            {
                false
            } else {
                self.execute(
                    &GitProcessCommand::new("git", ["remote", "-v"]).in_directory(directory),
                )?;
                self.execute(
                    &GitProcessCommand::new("git", ["remote", "set-url", "origin", "--", url])
                        .in_directory(directory),
                )?;
                let updated = self
                    .execute(
                        &GitProcessCommand::new("git", ["remote", "update", "--prune", "origin"])
                            .in_directory(directory),
                    )?
                    .success;
                if !updated {
                    let _ = self.execute(&GitProcessCommand::new("git", ["--version"]))?;
                }
                updated
            }
        } else {
            self.execute(&GitProcessCommand::new(
                "git",
                [
                    "clone".to_owned(),
                    "--mirror".to_owned(),
                    "--".to_owned(),
                    url.to_owned(),
                    directory.to_string_lossy().into_owned(),
                ],
            ))?
            .success
        };

        if updated || directory.exists() {
            self.execute(&GitProcessCommand::new("git", ["remote", "-v"]).in_directory(directory))?;
            let sanitized = strip_url_credentials(url);
            self.execute(
                &GitProcessCommand::new(
                    "git",
                    ["remote", "set-url", "origin", "--", sanitized.as_str()],
                )
                .in_directory(directory),
            )?;
        }
        Ok(updated)
    }

    fn execute(&self, command: &GitProcessCommand) -> Result<GitProcessOutput> {
        self.process
            .execute(command)
            .map_err(|error| RiffError::Git(format!("failed to execute Git: {error}")))
    }

    fn attempts(&self, url: &str, authentication: &GitAuthentication) -> Vec<GitAttempt> {
        if let Some(repository) = github_repository(url) {
            let preferred_protocol = self
                .github_protocols
                .first()
                .map(String::as_str)
                .unwrap_or("https");
            let preferred = github_url(preferred_protocol, &repository, false)
                .unwrap_or_else(|| url.to_owned());
            let mut attempts = vec![GitAttempt::Url(preferred)];
            if let Some(token) = &authentication.github_token {
                if preferred_protocol == "https" {
                    attempts.push(GitAttempt::Url(
                        github_url("https", &repository, true).unwrap(),
                    ));
                }
                attempts.push(GitAttempt::Url(format!(
                    "https://token:{token}@github.com/{repository}.git"
                )));
            }
            return attempts;
        }

        if let Some(repository) = bitbucket_repository(url) {
            let mut attempts = vec![GitAttempt::Url(url.to_owned())];
            if let Some((username, password)) = &authentication.bitbucket_credentials {
                let username = if password.starts_with("ATAT_") {
                    "x-bitbucket-api-token-auth"
                } else {
                    username
                };
                attempts.push(GitAttempt::Url(bitbucket_authenticated_url(
                    &repository,
                    username,
                    password,
                )));
            } else if url.starts_with("http") || authentication.bitbucket_oauth_token.is_some() {
                attempts.push(GitAttempt::BitbucketConfigToken(repository.clone()));
            }
            if let Some(token) = &authentication.bitbucket_oauth_token {
                attempts.push(GitAttempt::Url(bitbucket_authenticated_url(
                    &repository,
                    "x-token-auth",
                    token,
                )));
            } else if authentication.bitbucket_credentials.is_none() && url.starts_with("http") {
                attempts.push(GitAttempt::Url(format!(
                    "git@bitbucket.org:{repository}.git"
                )));
            }
            return attempts;
        }

        vec![GitAttempt::Url(url.to_owned())]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateDirection {
    Upgrade,
    Downgrade,
}

impl UpdateDirection {
    pub const fn progress_verb(self) -> &'static str {
        match self {
            Self::Upgrade => "Upgrading",
            Self::Downgrade => "Downgrading",
        }
    }
}

/// Git repository downloader.
///
/// Riff already relies on the `git` executable for repository inspection. Using
/// it here as well avoids linking libgit2 and OpenSSL into the Riff build.
pub struct GitDownloader {
    /// SSH key path for authentication (optional)
    ssh_key: Option<PathBuf>,
    /// Whether to use the system SSH agent
    use_ssh_agent: bool,
    /// Ordered GitHub transports to try for fetches.
    github_protocols: Vec<String>,
}

impl GitDownloader {
    /// Create a new Git downloader
    pub fn new() -> Self {
        Self {
            ssh_key: None,
            use_ssh_agent: true,
            github_protocols: vec!["https".into(), "ssh".into()],
        }
    }

    /// Set SSH key for authentication
    pub fn with_ssh_key(mut self, path: impl Into<PathBuf>) -> Self {
        self.ssh_key = Some(path.into());
        self
    }

    /// Disable SSH agent
    pub fn without_ssh_agent(mut self) -> Self {
        self.use_ssh_agent = false;
        self
    }

    /// Set the ordered GitHub transports used for clone and update attempts.
    pub fn with_github_protocols<I, S>(mut self, protocols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.github_protocols = protocols.into_iter().map(Into::into).collect();
        self
    }

    /// Return the fetch candidates and push URL for a GitHub repository URL.
    pub fn github_transport_urls(&self, url: &str) -> (Vec<String>, Option<String>) {
        let Some(repository) = github_repository(url) else {
            return (vec![url.to_owned()], None);
        };

        let mut fetch_urls = self
            .github_protocols
            .iter()
            .filter_map(|protocol| github_url(protocol, &repository, false))
            .collect::<Vec<_>>();
        if fetch_urls.is_empty() {
            fetch_urls.push(url.to_owned());
        }

        let push_url = if self
            .github_protocols
            .iter()
            .any(|protocol| protocol == "ssh")
        {
            github_url("ssh", &repository, true)
        } else {
            self.github_protocols
                .first()
                .and_then(|protocol| github_url(protocol, &repository, true))
        };

        (fetch_urls, push_url)
    }

    /// Clone a repository
    pub fn clone(&self, url: &str, dest: &Path, reference: Option<&str>) -> Result<()> {
        self.clone_from_urls(&[url], url, dest, reference)
    }

    /// Clone from the first working source URL and then restore the canonical remote URL.
    pub fn clone_from_urls(
        &self,
        urls: &[&str],
        canonical_url: &str,
        dest: &Path,
        reference: Option<&str>,
    ) -> Result<()> {
        validate_reference(reference)?;
        if dest.exists() {
            return Err(RiffError::Git(format!(
                "clone destination '{}' already exists",
                dest.display()
            )));
        }

        let mut failures = Vec::new();
        for url in urls {
            let (candidates, _) = self.github_transport_urls(url);
            for candidate in candidates {
                let mut command = self.command();
                command
                    .arg("clone")
                    .arg("--no-checkout")
                    .arg("--")
                    .arg(&candidate)
                    .arg(dest);
                match self.run(&mut command) {
                    Ok(()) => {
                        self.configure_origin(dest, canonical_url)?;
                        if let Some(reference) = reference {
                            self.checkout(dest, reference)?;
                        }
                        return Ok(());
                    }
                    Err(error) => {
                        failures.push(format!("{candidate}: {error}"));
                        remove_failed_clone(dest)?;
                    }
                }
            }
        }

        Err(RiffError::Git(format!(
            "failed to clone from all source URLs: {}",
            failures.join("; ")
        )))
    }

    /// Clone through a reusable bare mirror, avoiding another network fetch when
    /// the requested reference is already cached.
    pub fn clone_with_cache(
        &self,
        url: &str,
        dest: &Path,
        cache: &Path,
        reference: Option<&str>,
    ) -> Result<()> {
        validate_reference(reference)?;
        if dest.exists() {
            return Err(RiffError::Git(format!(
                "clone destination '{}' already exists",
                dest.display()
            )));
        }

        if !cache.exists() {
            if let Some(parent) = cache.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut mirror = self.command();
            mirror
                .arg("clone")
                .arg("--mirror")
                .arg("--")
                .arg(url)
                .arg(cache);
            self.run(&mut mirror)?;
            self.set_remote_url(cache, &strip_url_credentials(url))?;
        } else if reference.is_some_and(|reference| {
            self.reference_commit(cache, reference)
                .ok()
                .flatten()
                .is_none()
        }) {
            self.set_remote_url(cache, url)?;
            let update = self.fetch(cache);
            self.set_remote_url(cache, &strip_url_credentials(url))?;
            update?;
        } else {
            self.set_remote_url(cache, &strip_url_credentials(url))?;
        }

        let mut clone = self.command();
        clone
            .arg("clone")
            .arg("--no-checkout")
            .arg("--reference")
            .arg(cache)
            .arg("--dissociate")
            .arg("--")
            .arg(cache)
            .arg(dest);
        if let Err(error) = self.run(&mut clone) {
            remove_failed_clone(dest)?;
            return Err(error);
        }
        self.configure_origin(dest, url)?;

        if let Some(reference) = reference {
            self.checkout(dest, reference)?;
        }

        Ok(())
    }

    /// Update an existing repository
    pub fn update(&self, repo_path: &Path, reference: Option<&str>) -> Result<()> {
        validate_reference(reference)?;
        self.fetch(repo_path)?;

        if let Some(reference) = reference {
            self.checkout(repo_path, reference)?;
        }

        Ok(())
    }

    /// Update from the first source that contains the requested reference.
    pub fn update_from_urls(
        &self,
        repo_path: &Path,
        urls: &[&str],
        canonical_url: &str,
        reference: Option<&str>,
    ) -> Result<()> {
        validate_reference(reference)?;
        if !Self::is_git_repo(repo_path) {
            return Err(RiffError::Git(format!(
                "'{}' is not a Git repository",
                repo_path.display()
            )));
        }

        if let Some(reference) = reference {
            if self.reference_commit(repo_path, reference)?.is_some() {
                self.configure_origin(repo_path, canonical_url)?;
                return self.checkout(repo_path, reference);
            }
        }

        let mut failures = Vec::new();
        for url in urls {
            let (candidates, _) = self.github_transport_urls(url);
            for candidate in candidates {
                let result = self
                    .set_remote_url(repo_path, &candidate)
                    .and_then(|()| self.fetch(repo_path))
                    .and_then(|()| match reference {
                        Some(reference) => self.checkout(repo_path, reference),
                        None => Ok(()),
                    });
                match result {
                    Ok(()) => {
                        self.configure_origin(repo_path, canonical_url)?;
                        return Ok(());
                    }
                    Err(error) => failures.push(format!("{candidate}: {error}")),
                }
            }
        }

        Err(RiffError::Git(format!(
            "failed to update from all source URLs: {}",
            failures.join("; ")
        )))
    }

    /// Remove a clean Git checkout.
    pub fn remove(&self, repo_path: &Path) -> Result<()> {
        if !repo_path.exists() {
            return Ok(());
        }
        if Self::is_git_repo(repo_path) && Self::has_local_changes(repo_path)? {
            return Err(RiffError::Git(format!(
                "refusing to remove '{}' with local changes",
                repo_path.display()
            )));
        }
        std::fs::remove_dir_all(repo_path)?;
        Ok(())
    }

    pub const fn installation_source(&self) -> &'static str {
        "source"
    }

    pub fn update_direction(old_version: &str, new_version: &str) -> UpdateDirection {
        if Comparator::greater_than(old_version, new_version) {
            UpdateDirection::Downgrade
        } else {
            UpdateDirection::Upgrade
        }
    }

    /// Get the current commit hash
    pub fn get_head_commit(repo_path: &Path) -> Result<String> {
        let output = Command::new("git")
            .current_dir(repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .map_err(|error| Self::execution_error(error, "rev-parse HEAD"))?;

        Self::successful_stdout(output, "rev-parse HEAD").map(|stdout| stdout.trim().to_owned())
    }

    /// Check if a path is a git repository
    pub fn is_git_repo(path: &Path) -> bool {
        Command::new("git")
            .current_dir(path)
            .args(["rev-parse", "--git-dir"])
            .output()
            .is_ok_and(|output| output.status.success())
    }

    /// Check whether tracked or untracked working-tree changes would be lost.
    pub fn has_local_changes(path: &Path) -> Result<bool> {
        let output = Command::new("git")
            .current_dir(path)
            .args(["status", "--porcelain", "--untracked-files=normal"])
            .output()
            .map_err(|error| Self::execution_error(error, "status --porcelain"))?;
        Self::successful_stdout(output, "status --porcelain")
            .map(|stdout| !stdout.trim().is_empty())
    }

    fn checkout(&self, repo_path: &Path, reference: &str) -> Result<()> {
        let commit = self
            .reference_commit(repo_path, reference)?
            .ok_or_else(|| RiffError::Git(format!("reference '{reference}' was not found")))?;
        let mut command = self.command();
        command
            .current_dir(repo_path)
            .args(["checkout", "--detach", "--quiet", commit.as_str()]);
        self.run(&mut command)
    }

    fn reference_commit(&self, repo_path: &Path, reference: &str) -> Result<Option<String>> {
        let candidates = [
            reference.to_owned(),
            format!("refs/heads/{reference}"),
            format!("refs/tags/{reference}"),
            format!("refs/remotes/origin/{reference}"),
        ];

        for candidate in candidates {
            let revision = format!("{candidate}^{{commit}}");
            let output = Command::new("git")
                .current_dir(repo_path)
                .args(["rev-parse", "--verify", "--quiet", &revision])
                .output()
                .map_err(|error| Self::execution_error(error, "rev-parse reference"))?;

            if output.status.success() {
                let commit = String::from_utf8_lossy(&output.stdout);
                return Ok(Some(commit.trim().to_owned()));
            }
        }

        Ok(None)
    }

    fn fetch(&self, repo_path: &Path) -> Result<()> {
        let mut command = self.command();
        command.current_dir(repo_path).args([
            "fetch",
            "origin",
            "+refs/heads/*:refs/remotes/origin/*",
        ]);
        self.run(&mut command)
    }

    fn configure_origin(&self, repo_path: &Path, canonical_url: &str) -> Result<()> {
        let (fetch_urls, push_url) = self.github_transport_urls(canonical_url);
        let fetch_url = fetch_urls
            .first()
            .map(String::as_str)
            .unwrap_or(canonical_url);
        self.set_remote_url(repo_path, fetch_url)?;
        if let Some(push_url) = push_url {
            let mut command = self.command();
            command
                .current_dir(repo_path)
                .args(["remote", "set-url", "--push", "origin", "--", &push_url]);
            self.run(&mut command)?;
        }
        Ok(())
    }

    fn set_remote_url(&self, repo_path: &Path, url: &str) -> Result<()> {
        let mut command = self.command();
        command
            .current_dir(repo_path)
            .args(["remote", "set-url", "origin", "--", url]);
        self.run(&mut command)
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");

        if !self.use_ssh_agent {
            command.env_remove("SSH_AUTH_SOCK");
        }

        if let Some(key_path) = &self.ssh_key {
            let key_path = shell_quote(key_path.to_string_lossy().as_ref());
            command.env(
                "GIT_SSH_COMMAND",
                format!("ssh -i {key_path} -o IdentitiesOnly=yes"),
            );
        }

        if let (Ok(username), Ok(_)) = (
            std::env::var("COMPOSER_AUTH_USER"),
            std::env::var("COMPOSER_AUTH_PASS"),
        ) {
            command
                .arg("-c")
                .arg(format!("credential.username={username}"))
                .arg("-c")
                .arg("credential.helper=")
                .arg("-c")
                .arg(
                    "credential.helper=!f() { test \"$1\" != get || printf 'password=%s\\n' \"$COMPOSER_AUTH_PASS\"; }; f",
                );
        }

        command
    }

    fn run(&self, command: &mut Command) -> Result<()> {
        let description = format!("{command:?}");
        let output = command
            .output()
            .map_err(|error| Self::execution_error(error, &description))?;
        Self::successful_stdout(output, &description).map(|_| ())
    }

    fn successful_stdout(output: Output, operation: &str) -> Result<String> {
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        let detail = if detail.is_empty() {
            format!("process exited with {}", output.status)
        } else {
            detail.to_owned()
        };

        Err(RiffError::Git(format!("{operation}: {detail}")))
    }

    fn execution_error(error: std::io::Error, operation: &str) -> RiffError {
        RiffError::Git(format!("failed to execute git for {operation}: {error}"))
    }
}

impl Default for GitDownloader {
    fn default() -> Self {
        Self::new()
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn validate_reference(reference: Option<&str>) -> Result<()> {
    if reference.is_some_and(|reference| reference.trim().is_empty()) {
        return Err(RiffError::Git(
            "source reference must not be empty".to_owned(),
        ));
    }
    Ok(())
}

fn remove_failed_clone(destination: &Path) -> Result<()> {
    if destination.is_dir() {
        std::fs::remove_dir_all(destination)?;
    } else if destination.exists() {
        std::fs::remove_file(destination)?;
    }
    Ok(())
}

fn github_repository(url: &str) -> Option<String> {
    let repository = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let repository = repository.trim_matches('/').trim_end_matches(".git");
    repository.contains('/').then(|| repository.to_owned())
}

fn github_url(protocol: &str, repository: &str, include_git_suffix: bool) -> Option<String> {
    let suffix = if include_git_suffix { ".git" } else { "" };
    match protocol {
        "https" => Some(format!("https://github.com/{repository}{suffix}")),
        "http" => Some(format!("http://github.com/{repository}{suffix}")),
        "ssh" => Some(format!("git@github.com:{repository}{suffix}")),
        "git" => Some(format!("git://github.com/{repository}{suffix}")),
        _ => None,
    }
}

fn bitbucket_repository(url: &str) -> Option<String> {
    let repository = url
        .strip_prefix("https://bitbucket.org/")
        .or_else(|| url.strip_prefix("http://bitbucket.org/"))
        .or_else(|| url.strip_prefix("git@bitbucket.org:"))?;
    let repository = repository.trim_matches('/').trim_end_matches(".git");
    let mut segments = repository.split('/');
    let owner = segments.next()?;
    let name = segments.next()?;
    (segments.next().is_none() && !owner.is_empty() && !name.is_empty())
        .then(|| format!("{owner}/{name}"))
}

fn bitbucket_authenticated_url(repository: &str, username: &str, token: &str) -> String {
    format!(
        "https://{}:{}@bitbucket.org/{repository}.git",
        urlencoding::encode(username),
        urlencoding::encode(token)
    )
}

/// Remove URL user-info before persisting a Git remote. Unlike diagnostic
/// sanitization this removes both username and password instead of masking
/// them, so credentials cannot remain in a mirror's config file.
pub fn strip_url_credentials(input: &str) -> String {
    let Ok(mut url) = url::Url::parse(input) else {
        return input.to_owned();
    };
    if url.cannot_be_a_base() || url.host_str().is_none() {
        return input.to_owned();
    }
    if url.set_username("").is_err() || url.set_password(None).is_err() {
        return input.to_owned();
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::Mutex;
    use tempfile::TempDir;

    struct FakeGitProcess {
        outputs: Mutex<VecDeque<GitProcessOutput>>,
        commands: Mutex<Vec<GitProcessCommand>>,
    }

    impl FakeGitProcess {
        fn new(outputs: impl IntoIterator<Item = GitProcessOutput>) -> Self {
            Self {
                outputs: Mutex::new(outputs.into_iter().collect()),
                commands: Mutex::new(Vec::new()),
            }
        }

        fn commands(&self) -> Vec<GitProcessCommand> {
            self.commands.lock().unwrap().clone()
        }
    }

    impl GitProcess for FakeGitProcess {
        fn execute(
            &self,
            command: &GitProcessCommand,
        ) -> std::result::Result<GitProcessOutput, String> {
            self.commands.lock().unwrap().push(command.clone());
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| format!("unexpected Git command: {command:?}"))
        }
    }

    fn remote_command(url: &str) -> GitProcessCommand {
        GitProcessCommand::new("git-command", [url])
    }

    fn attempted_urls(process: &FakeGitProcess) -> Vec<String> {
        process
            .commands()
            .into_iter()
            .filter(|command| command.program == "git-command")
            .filter_map(|command| command.args.into_iter().next())
            .collect()
    }

    fn git(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn create_test_git_repo() -> TempDir {
        let repo = TempDir::new().unwrap();
        git(repo.path(), &["init", "--quiet"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test"]);
        git(repo.path(), &["config", "commit.gpgsign", "false"]);
        fs::write(repo.path().join(".gitattributes"), "* -text\n").unwrap();
        fs::write(repo.path().join("README.md"), "first\n").unwrap();
        git(repo.path(), &["add", ".gitattributes", "README.md"]);
        git(repo.path(), &["commit", "--quiet", "-m", "first"]);
        git(repo.path(), &["tag", "v1"]);
        fs::write(repo.path().join("README.md"), "second\n").unwrap();
        git(repo.path(), &["commit", "--quiet", "-am", "second"]);

        repo
    }

    // Ported from Composer\Test\Util\GitTest::
    // testRunCommandPublicGitHubRepositoryNotInitialClone.
    #[test]
    fn composer_git_uses_configured_protocol_for_public_github_repository() {
        for (protocol, expected) in [
            ("ssh", "git@github.com:acme/repo"),
            ("https", "https://github.com/acme/repo"),
        ] {
            let process = FakeGitProcess::new([GitProcessOutput::success("")]);
            let executor = GitRemoteExecutor::new(process).with_github_protocols([protocol]);
            executor
                .run_command(
                    "https://github.com/acme/repo",
                    &GitAuthentication::default(),
                    remote_command,
                )
                .unwrap();
            assert_eq!(attempted_urls(&executor.process), [expected]);
        }
    }

    // Ported from Composer\Test\Util\GitTest::
    // testRunCommandPrivateGitHubRepositoryNotInitialCloneNotInteractiveWithoutAuthentication.
    #[test]
    fn composer_git_reports_private_github_failure_without_authentication() {
        let process = FakeGitProcess::new([
            GitProcessOutput::failure("repository not found"),
            GitProcessOutput::success("git version 2.50.0"),
        ]);
        let executor = GitRemoteExecutor::new(process).with_github_protocols(["https"]);
        let error = executor
            .run_command(
                "https://github.com/acme/repo",
                &GitAuthentication::default(),
                remote_command,
            )
            .unwrap_err();

        assert!(error.to_string().contains("repository not found"));
        assert_eq!(
            executor.process.commands(),
            [
                remote_command("https://github.com/acme/repo"),
                GitProcessCommand::new("git", ["--version"]),
            ]
        );
    }

    // Ported from Composer\Test\Util\GitTest::
    // testRunCommandPrivateGitHubRepositoryNotInitialCloneNotInteractiveWithAuthentication.
    #[test]
    fn composer_git_retries_private_github_with_configured_authentication() {
        for (git_url, protocol, failures, expected) in [
            (
                "git@github.com:acme/repo.git",
                "ssh",
                1,
                "https://token:MY_GITHUB_TOKEN@github.com/acme/repo.git",
            ),
            (
                "https://github.com/acme/repo",
                "https",
                2,
                "https://token:MY_GITHUB_TOKEN@github.com/acme/repo.git",
            ),
        ] {
            let outputs = std::iter::repeat_with(|| GitProcessOutput::failure("private"))
                .take(failures)
                .chain(std::iter::once(GitProcessOutput::success("")));
            let process = FakeGitProcess::new(outputs);
            let executor = GitRemoteExecutor::new(process).with_github_protocols([protocol]);
            executor
                .run_command(
                    git_url,
                    &GitAuthentication::default().with_github_token("MY_GITHUB_TOKEN"),
                    remote_command,
                )
                .unwrap();

            let urls = attempted_urls(&executor.process);
            assert_eq!(urls.len(), failures + 1);
            assert_eq!(urls.last().map(String::as_str), Some(expected));
        }
    }

    // Ported from Composer\Test\Util\GitTest::
    // testRunCommandPrivateBitbucketRepositoryNotInitialCloneNotInteractiveWithAuthentication.
    #[test]
    fn composer_git_retries_private_bitbucket_with_authentication_or_ssh() {
        let cases = [
            (
                "git@bitbucket.org:acme/repo.git",
                Some(("token", "MY_BITBUCKET_TOKEN")),
                "https://token:MY_BITBUCKET_TOKEN@bitbucket.org/acme/repo.git",
                1,
                0,
            ),
            (
                "https://bitbucket.org/acme/repo",
                Some(("token", "MY_BITBUCKET_TOKEN")),
                "https://token:MY_BITBUCKET_TOKEN@bitbucket.org/acme/repo.git",
                1,
                0,
            ),
            (
                "https://bitbucket.org/acme/repo.git",
                Some(("token", "MY_BITBUCKET_TOKEN")),
                "https://token:MY_BITBUCKET_TOKEN@bitbucket.org/acme/repo.git",
                1,
                0,
            ),
            (
                "git@bitbucket.org:acme/repo.git",
                None,
                "git@bitbucket.org:acme/repo.git",
                0,
                0,
            ),
            (
                "https://bitbucket.org/acme/repo",
                None,
                "git@bitbucket.org:acme/repo.git",
                1,
                1,
            ),
            (
                "https://bitbucket.org/acme/repo.git",
                None,
                "git@bitbucket.org:acme/repo.git",
                1,
                1,
            ),
            (
                "https://bitbucket.org/acme/repo.git",
                Some(("token", "ATAT_BITBUCKET_API_TOKEN")),
                "https://x-bitbucket-api-token-auth:ATAT_BITBUCKET_API_TOKEN@bitbucket.org/acme/repo.git",
                1,
                0,
            ),
        ];

        for (git_url, credentials, expected, failures, config_calls) in cases {
            let outputs = std::iter::repeat_with(|| GitProcessOutput::failure("private"))
                .take(failures)
                .chain(
                    std::iter::repeat_with(|| GitProcessOutput::failure("missing token"))
                        .take(config_calls),
                )
                .chain(std::iter::once(GitProcessOutput::success("")));
            let process = FakeGitProcess::new(outputs);
            let executor = GitRemoteExecutor::new(process);
            let authentication =
                credentials.map_or_else(GitAuthentication::default, |(user, token)| {
                    GitAuthentication::default().with_bitbucket_credentials(user, token)
                });
            executor
                .run_command(git_url, &authentication, remote_command)
                .unwrap();

            let urls = attempted_urls(&executor.process);
            assert_eq!(urls.last().map(String::as_str), Some(expected));
            let git_config_calls = executor
                .process
                .commands()
                .iter()
                .filter(|command| command.args == ["config", "bitbucket.accesstoken"])
                .count();
            assert_eq!(git_config_calls, config_calls);
        }
    }

    // Ported from Composer\Test\Util\GitTest::
    // testRunCommandPrivateBitbucketRepositoryNotInitialCloneInteractiveWithOauth.
    #[test]
    fn composer_git_uses_interactively_acquired_bitbucket_oauth_token() {
        for (git_url, saved_credentials) in [
            ("git@bitbucket.org:acme/repo.git", false),
            ("https://bitbucket.org/acme/repo.git", false),
            ("https://bitbucket.org/acme/repo", false),
            ("git@bitbucket.org:acme/repo.git", true),
        ] {
            let outputs = if saved_credentials {
                vec![
                    GitProcessOutput::failure("private"),
                    GitProcessOutput::failure("expired credentials"),
                    GitProcessOutput::success(""),
                ]
            } else {
                vec![
                    GitProcessOutput::failure("private"),
                    GitProcessOutput::failure("missing git config token"),
                    GitProcessOutput::success(""),
                ]
            };
            let process = FakeGitProcess::new(outputs);
            let executor = GitRemoteExecutor::new(process);
            let mut authentication =
                GitAuthentication::default().with_bitbucket_oauth_token("my-access-token");
            if saved_credentials {
                authentication = authentication
                    .with_bitbucket_credentials("someuseralsoswappedfortoken", "little green men");
            }
            executor
                .run_command(git_url, &authentication, remote_command)
                .unwrap();

            assert_eq!(
                attempted_urls(&executor.process).last().map(String::as_str),
                Some("https://x-token-auth:my-access-token@bitbucket.org/acme/repo.git")
            );
        }
    }

    // Ported from Composer\Test\Util\GitTest::testSyncMirrorSanitizesUrlAfterInitialClone.
    #[test]
    fn composer_git_sync_mirror_strips_credentials_after_initial_clone() {
        let parent = TempDir::new().unwrap();
        let mirror = parent.path().join("mirror.git");
        let process = FakeGitProcess::new([
            GitProcessOutput::success(""),
            GitProcessOutput::success(""),
            GitProcessOutput::success(""),
        ]);
        let executor = GitRemoteExecutor::new(process);

        assert!(executor
            .sync_mirror("https://user:secret@example.com/repo.git", &mirror,)
            .unwrap());
        let commands = executor.process.commands();
        assert_eq!(
            commands.last().unwrap().args,
            [
                "remote",
                "set-url",
                "origin",
                "--",
                "https://example.com/repo.git"
            ]
        );
        assert!(commands[..commands.len() - 1]
            .iter()
            .any(|command| command.args.iter().any(|arg| arg.contains("user:secret"))));
    }

    // Ported from Composer\Test\Util\GitTest::testSyncMirrorSanitizesUrlEvenAfterFailedUpdate.
    #[test]
    fn composer_git_sync_mirror_strips_credentials_after_failed_update() {
        let mirror = TempDir::new().unwrap();
        let process = FakeGitProcess::new([
            GitProcessOutput::success(".\n"),
            GitProcessOutput::success(""),
            GitProcessOutput::success(""),
            GitProcessOutput::failure("update failed"),
            GitProcessOutput::success("git version 2.50.0"),
            GitProcessOutput::success(""),
            GitProcessOutput::success(""),
        ]);
        let executor = GitRemoteExecutor::new(process);

        assert!(!executor
            .sync_mirror("https://user:secret@example.com/repo.git", mirror.path(),)
            .unwrap());
        assert_eq!(
            executor.process.commands().last().unwrap().args,
            [
                "remote",
                "set-url",
                "origin",
                "--",
                "https://example.com/repo.git"
            ]
        );
    }

    #[test]
    fn test_git_downloader_creation() {
        let downloader = GitDownloader::new();
        assert!(downloader.ssh_key.is_none());
        assert!(downloader.use_ssh_agent);
    }

    #[test]
    fn test_git_downloader_with_ssh_key() {
        let downloader = GitDownloader::new().with_ssh_key("/path/to/key");
        assert_eq!(downloader.ssh_key, Some(PathBuf::from("/path/to/key")));
    }

    #[test]
    fn test_is_not_git_repo() {
        let temp_dir = TempDir::new().unwrap();
        assert!(!GitDownloader::is_git_repo(temp_dir.path()));
    }

    #[test]
    fn composer_git_downloader_clones_and_checks_out_reference() {
        let source = create_test_git_repo();
        let parent = TempDir::new().unwrap();
        let destination = parent.path().join("clone");
        let downloader = GitDownloader::new();

        downloader
            .clone(source.path().to_str().unwrap(), &destination, Some("v1"))
            .unwrap();

        assert!(GitDownloader::is_git_repo(&destination));
        assert_eq!(
            fs::read_to_string(destination.join("README.md")).unwrap(),
            "first\n"
        );
        assert_eq!(
            GitDownloader::get_head_commit(&destination).unwrap().len(),
            40
        );
    }

    #[test]
    fn composer_git_downloader_rejects_download_without_source_reference() {
        let source = create_test_git_repo();
        let parent = TempDir::new().unwrap();
        let destination = parent.path().join("clone");
        let error = GitDownloader::new()
            .clone(source.path().to_str().unwrap(), &destination, Some(""))
            .unwrap_err();

        assert!(error.to_string().contains("reference must not be empty"));
        assert!(!destination.exists());
    }

    #[test]
    fn composer_git_downloader_reuses_cached_mirror() {
        let source = create_test_git_repo();
        let source_path = source.path().to_path_buf();
        let parent = TempDir::new().unwrap();
        let cache = parent.path().join("cache.git");
        let first = parent.path().join("first");
        let second = parent.path().join("second");
        let downloader = GitDownloader::new();

        downloader
            .clone_with_cache(source_path.to_str().unwrap(), &first, &cache, Some("v1"))
            .unwrap();
        assert_eq!(
            fs::read_to_string(first.join("README.md")).unwrap(),
            "first\n"
        );
        fs::remove_dir_all(&first).unwrap();
        source.close().unwrap();

        downloader
            .clone_with_cache(source_path.to_str().unwrap(), &second, &cache, Some("v1"))
            .unwrap();
        assert_eq!(
            fs::read_to_string(second.join("README.md")).unwrap(),
            "first\n"
        );
    }

    #[test]
    fn composer_git_downloader_expands_github_protocols_and_sets_push_url() {
        let downloader = GitDownloader::new();
        let (mirror_fetches, _) =
            downloader.github_transport_urls("https://github.com/mirrors/composer");
        let (canonical_fetches, push_url) =
            downloader.github_transport_urls("https://github.com/composer/composer");

        assert_eq!(
            mirror_fetches,
            [
                "https://github.com/mirrors/composer",
                "git@github.com:mirrors/composer",
            ]
        );
        assert_eq!(
            canonical_fetches.first().map(String::as_str),
            Some("https://github.com/composer/composer")
        );
        assert_eq!(
            push_url.as_deref(),
            Some("git@github.com:composer/composer.git")
        );
    }

    #[test]
    fn composer_git_downloader_honors_custom_github_protocols_and_push_url() {
        let cases = [
            (
                vec!["ssh"],
                "git@github.com:composer/composer",
                "git@github.com:composer/composer.git",
            ),
            (
                vec!["https", "ssh", "git"],
                "https://github.com/composer/composer",
                "git@github.com:composer/composer.git",
            ),
            (
                vec!["https"],
                "https://github.com/composer/composer",
                "https://github.com/composer/composer.git",
            ),
        ];

        for (protocols, expected_fetch, expected_push) in cases {
            let downloader = GitDownloader::new().with_github_protocols(protocols);
            let (fetches, push) =
                downloader.github_transport_urls("https://github.com/composer/composer");
            assert_eq!(fetches.first().map(String::as_str), Some(expected_fetch));
            assert_eq!(push.as_deref(), Some(expected_push));
        }
    }

    #[test]
    fn composer_git_downloader_reports_clone_failure() {
        let parent = TempDir::new().unwrap();
        let destination = parent.path().join("clone");
        let missing = parent.path().join("missing.git");
        let error = GitDownloader::new()
            .clone(missing.to_str().unwrap(), &destination, Some("reference"))
            .unwrap_err();

        assert!(error.to_string().contains("failed to clone"));
        assert!(error.to_string().contains("missing.git"));
        assert!(!destination.exists());
    }

    #[test]
    fn composer_git_downloader_rejects_update_without_source_reference() {
        let source = create_test_git_repo();
        let parent = TempDir::new().unwrap();
        let destination = parent.path().join("clone");
        let downloader = GitDownloader::new();
        downloader
            .clone(source.path().to_str().unwrap(), &destination, Some("v1"))
            .unwrap();

        let error = downloader.update(&destination, Some("")).unwrap_err();
        assert!(error.to_string().contains("reference must not be empty"));
    }

    #[test]
    fn composer_git_downloader_fetches_and_checks_out_update() {
        let source = create_test_git_repo();
        let target = git(source.path(), &["rev-parse", "HEAD"]);
        let parent = TempDir::new().unwrap();
        let destination = parent.path().join("clone");
        let downloader = GitDownloader::new();
        downloader
            .clone(source.path().to_str().unwrap(), &destination, Some("v1"))
            .unwrap();

        downloader.update(&destination, Some(&target)).unwrap();
        assert_eq!(
            GitDownloader::get_head_commit(&destination).unwrap(),
            target
        );
        assert_eq!(
            fs::read_to_string(destination.join("README.md")).unwrap(),
            "second\n"
        );
    }

    #[test]
    fn composer_git_downloader_updates_origin_when_repository_url_changes() {
        let source = create_test_git_repo();
        let parent = TempDir::new().unwrap();
        let destination = parent.path().join("clone");
        let replacement = parent.path().join("replacement.git");
        let downloader = GitDownloader::new();
        downloader
            .clone(source.path().to_str().unwrap(), &destination, Some("v1"))
            .unwrap();
        git(
            parent.path(),
            &[
                "clone",
                "--quiet",
                "--bare",
                source.path().to_str().unwrap(),
                replacement.to_str().unwrap(),
            ],
        );

        let replacement_url = replacement.to_str().unwrap();
        downloader
            .update_from_urls(
                &destination,
                &[replacement_url],
                replacement_url,
                Some("v1"),
            )
            .unwrap();
        assert_eq!(
            git(&destination, &["remote", "get-url", "origin"]),
            replacement_url
        );
    }

    #[test]
    fn composer_git_downloader_reports_update_failure_after_all_urls() {
        let source = create_test_git_repo();
        let parent = TempDir::new().unwrap();
        let destination = parent.path().join("clone");
        let first = parent.path().join("missing-one.git");
        let second = parent.path().join("missing-two.git");
        let downloader = GitDownloader::new();
        downloader
            .clone(source.path().to_str().unwrap(), &destination, Some("v1"))
            .unwrap();

        let error = downloader
            .update_from_urls(
                &destination,
                &[first.to_str().unwrap(), second.to_str().unwrap()],
                second.to_str().unwrap(),
                Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            )
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("failed to update from all source URLs"));
        assert!(message.contains("missing-one.git"));
        assert!(message.contains("missing-two.git"));
    }

    #[test]
    fn composer_git_downloader_recovers_with_later_update_url() {
        let source = create_test_git_repo();
        let parent = TempDir::new().unwrap();
        let destination = parent.path().join("clone");
        let missing = parent.path().join("missing.git");
        let downloader = GitDownloader::new();
        downloader
            .clone(source.path().to_str().unwrap(), &destination, Some("v1"))
            .unwrap();
        fs::write(source.path().join("README.md"), "third\n").unwrap();
        git(source.path(), &["commit", "--quiet", "-am", "third"]);
        let target = git(source.path(), &["rev-parse", "HEAD"]);
        let source_url = source.path().to_str().unwrap();

        downloader
            .update_from_urls(
                &destination,
                &[missing.to_str().unwrap(), source_url],
                source_url,
                Some(&target),
            )
            .unwrap();
        assert_eq!(
            GitDownloader::get_head_commit(&destination).unwrap(),
            target
        );
        assert_eq!(
            fs::read_to_string(destination.join("README.md")).unwrap(),
            "third\n"
        );
    }

    #[test]
    fn composer_git_downloader_classifies_semver_downgrade() {
        let direction = GitDownloader::update_direction("1.2.0.0", "1.0.0.0");
        assert_eq!(direction, UpdateDirection::Downgrade);
        assert_eq!(direction.progress_verb(), "Downgrading");
    }

    #[test]
    fn composer_git_downloader_treats_reference_changes_as_upgrade() {
        let direction = GitDownloader::update_direction("dev-ref", "dev-ref2");
        assert_eq!(direction, UpdateDirection::Upgrade);
        assert_eq!(direction.progress_verb(), "Upgrading");
    }

    #[test]
    fn composer_git_downloader_removes_clean_checkout() {
        let source = create_test_git_repo();
        let parent = TempDir::new().unwrap();
        let destination = parent.path().join("clone");
        let downloader = GitDownloader::new();
        downloader
            .clone(source.path().to_str().unwrap(), &destination, Some("v1"))
            .unwrap();

        downloader.remove(&destination).unwrap();
        assert!(!destination.exists());
    }

    #[test]
    fn composer_git_downloader_reports_source_installation() {
        assert_eq!(GitDownloader::new().installation_source(), "source");
    }

    #[test]
    #[ignore] // Requires network access
    fn test_clone_public_repo() {
        let parent = TempDir::new().unwrap();
        let destination = parent.path().join("clone");
        let downloader = GitDownloader::new();

        let result = downloader.clone(
            "https://github.com/octocat/Hello-World.git",
            &destination,
            None,
        );

        assert!(result.is_ok());
        assert!(destination.join(".git").exists());
    }
}
