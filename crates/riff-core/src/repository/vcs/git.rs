//! Git driver - uses git command-line tools for repository access.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::driver::{VcsDriver, VcsDriverError, VcsInfo};

/// Git driver for local and remote git repositories
pub struct GitDriver {
    /// Repository URL
    url: String,
    /// Local clone path (if cloned)
    repo_path: Option<PathBuf>,
    /// Cached root identifier
    root_identifier: Option<String>,
}

impl GitDriver {
    /// Create a new Git driver for a URL
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();

        // Check if URL is a local path
        let repo_path = if url.starts_with('/')
            || url.starts_with('.')
            || !url.contains("://") && !url.contains('@')
        {
            let path = Path::new(&url);
            if path.exists() && path.join(".git").exists() {
                Some(path.to_path_buf())
            } else if path.exists() && path.is_dir() {
                // Bare repository
                Some(path.to_path_buf())
            } else {
                None
            }
        } else {
            None
        };

        Self {
            url,
            repo_path,
            root_identifier: None,
        }
    }

    /// Create a driver from a local path
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            url: path.to_string_lossy().to_string(),
            repo_path: Some(path),
            root_identifier: None,
        }
    }

    /// Run a git command in the repository
    fn run_git(&self, args: &[&str]) -> Result<String, VcsDriverError> {
        let mut cmd = Command::new("git");

        if let Some(ref path) = self.repo_path {
            cmd.current_dir(path);
        } else {
            // For remote repos, we need to use ls-remote or clone first
            return Err(VcsDriverError::GitError(
                "Remote repository access requires cloning first".to_string(),
            ));
        }

        cmd.args(args);

        let output = cmd
            .output()
            .map_err(|e| VcsDriverError::GitError(format!("Failed to execute git: {}", e)))?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(VcsDriverError::GitError(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ))
        }
    }

    /// Run git ls-remote for remote repositories
    fn run_ls_remote(&self, options: &[&str], refs: &[&str]) -> Result<String, VcsDriverError> {
        let output = Command::new("git")
            .args(["ls-remote", "--quiet"])
            .args(options)
            .arg("--")
            .arg(&self.url)
            .args(refs)
            .output()
            .map_err(|e| VcsDriverError::GitError(format!("Failed to execute git: {}", e)))?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("not found") || stderr.contains("does not exist") {
                Err(VcsDriverError::NotFound(self.url.clone()))
            } else if stderr.contains("Authentication") || stderr.contains("Permission denied") {
                Err(VcsDriverError::AuthRequired(self.url.clone()))
            } else {
                Err(VcsDriverError::GitError(stderr.to_string()))
            }
        }
    }

    /// Check if this is a local repository
    pub fn is_local(&self) -> bool {
        self.repo_path.is_some()
    }

    fn validate_identifier(identifier: &str) -> Result<(), VcsDriverError> {
        if identifier.starts_with('-') {
            return Err(VcsDriverError::InvalidFormat(format!(
                "Git identifier must not start with '-': {identifier}"
            )));
        }
        Ok(())
    }

    /// Return the commit timestamp for an identifier.
    pub fn get_change_date(&self, identifier: &str) -> Result<Option<String>, VcsDriverError> {
        Self::validate_identifier(identifier)?;
        if !self.is_local() {
            return Ok(None);
        }
        let output = self.run_git(&["show", "-s", "--format=%cI", identifier])?;
        Ok(Some(output.trim().to_string()))
    }
}

impl VcsDriver for GitDriver {
    fn get_root_identifier(&self) -> Result<String, VcsDriverError> {
        if let Some(ref cached) = self.root_identifier {
            return Ok(cached.clone());
        }

        if self.is_local() {
            let branch = self.run_git(&["symbolic-ref", "--quiet", "--short", "HEAD"]);
            if let Ok(branch) = branch {
                return Ok(branch.trim().to_string());
            }

            // Detached HEADs have no branch name, so retain the commit as a fallback.
            let commit = self.run_git(&["rev-parse", "HEAD"])?;
            Ok(commit.trim().to_string())
        } else {
            let output = self.run_ls_remote(&["--symref"], &["HEAD"])?;
            for line in output.lines() {
                let mut parts = line.split_whitespace();
                if parts.next() == Some("ref:") {
                    if let Some(branch) = parts
                        .next()
                        .and_then(|reference| reference.strip_prefix("refs/heads/"))
                    {
                        return Ok(branch.to_string());
                    }
                }
            }

            // Servers without symref support still expose the HEAD commit.
            if let Some(line) = output.lines().find(|line| !line.starts_with("ref:")) {
                if let Some(sha) = line.split_whitespace().next() {
                    return Ok(sha.to_string());
                }
            }
            Err(VcsDriverError::NotFound(
                "Could not determine HEAD".to_string(),
            ))
        }
    }

    fn get_tags(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        let mut tags = HashMap::new();

        if self.is_local() {
            // Local repository - use for-each-ref which handles empty case gracefully
            let output = match self.run_git(&[
                "for-each-ref",
                "--format=%(objectname) %(refname:short)",
                "refs/tags/",
            ]) {
                Ok(output) => output,
                Err(_) => return Ok(tags), // No tags
            };

            for line in output.lines() {
                let parts: Vec<&str> = line.splitn(2, ' ').collect();
                if parts.len() == 2 {
                    let sha = parts[0];
                    let tag = parts[1];
                    tags.insert(tag.to_string(), sha.to_string());
                }
            }
        } else {
            // Remote repository
            let output = self.run_ls_remote(&["--tags"], &[])?;

            for line in output.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let sha = parts[0];
                    let ref_name = parts[1];
                    if let Some(tag) = ref_name.strip_prefix("refs/tags/") {
                        // Skip dereferenced entries, prefer the ^{} version
                        if !tag.ends_with("^{}") {
                            tags.insert(tag.to_string(), sha.to_string());
                        } else {
                            // This is the actual commit for annotated tags
                            let tag = tag.trim_end_matches("^{}");
                            tags.insert(tag.to_string(), sha.to_string());
                        }
                    }
                }
            }
        }

        Ok(tags)
    }

    fn get_branches(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        let mut branches = HashMap::new();

        if self.is_local() {
            // Local repository - get all branches
            let output = self.run_git(&[
                "for-each-ref",
                "--format=%(objectname) %(refname:short)",
                "refs/heads/",
            ])?;

            branches.extend(parse_branch_refs(&output, ""));
        } else {
            // Remote repository
            let output = self.run_ls_remote(&["--heads"], &[])?;
            branches.extend(parse_branch_refs(&output, "refs/heads/"));
        }

        Ok(branches)
    }

    fn get_composer_information(&self, identifier: &str) -> Result<VcsInfo, VcsDriverError> {
        let content = self.get_file_content("composer.json", identifier)?;

        let manifest: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| VcsDriverError::InvalidFormat(format!("Invalid JSON: {}", e)))?;

        // Get commit time
        let time = self.get_change_date(identifier)?;

        Ok(VcsInfo {
            manifest: Some(manifest),
            identifier: identifier.to_string(),
            time,
        })
    }

    fn get_file_content(&self, file: &str, identifier: &str) -> Result<String, VcsDriverError> {
        Self::validate_identifier(identifier)?;
        if !self.is_local() {
            return Err(VcsDriverError::GitError(
                "Cannot read file content from remote repository without cloning".to_string(),
            ));
        }

        let output = self.run_git(&["show", &format!("{}:{}", identifier, file)])?;
        Ok(output)
    }

    fn supports(url: &str, deep: bool) -> bool {
        let url_lower = url.to_lowercase();

        // Quick checks
        if url_lower.ends_with(".git") {
            return true;
        }

        if url_lower.starts_with("git://") || url_lower.starts_with("git@") {
            return true;
        }

        // Check for common git hosts
        if url_lower.contains("github.com")
            || url_lower.contains("gitlab.com")
            || url_lower.contains("bitbucket.org")
        {
            return true;
        }

        // Check if it's a local path with .git directory
        if !url.contains("://") && !url.contains('@') {
            let path = Path::new(url);
            if path.exists() && (path.join(".git").exists() || path.join("HEAD").exists()) {
                return true;
            }
        }

        if deep {
            // Try git ls-remote to verify
            let output = Command::new("git")
                .args(["ls-remote", "--quiet", "--exit-code", url])
                .output();

            if let Ok(output) = output {
                return output.status.success();
            }
        }

        false
    }

    fn get_url(&self) -> &str {
        &self.url
    }

    fn get_vcs_type(&self) -> &str {
        "git"
    }
}

fn parse_branch_refs(output: &str, prefix: &str) -> HashMap<String, String> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let sha = parts.next()?;
            let branch = parts.next()?.strip_prefix(prefix)?;
            (!branch.starts_with('-')).then(|| (branch.to_string(), sha.to_string()))
        })
        .collect()
}

/// Get the current git commit hash (HEAD) for a directory.
/// Returns None if the directory is not a git repository or if git is not available.
pub fn get_head_commit(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(path)
        .output()
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_git_repo() -> TempDir {
        let temp = TempDir::new().unwrap();

        // Initialize git repo
        Command::new("git")
            .args(["init"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        // Configure git user
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        Command::new("git")
            .args(["config", "tag.gpgsign", "false"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        // Create composer.json
        let manifest = serde_json::json!({
            "name": "vendor/package",
            "description": "Test package"
        });
        fs::write(
            temp.path().join("composer.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        // Add and commit
        Command::new("git")
            .args(["add", "."])
            .current_dir(temp.path())
            .output()
            .unwrap();

        Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        temp
    }

    #[test]
    fn test_git_driver_local_repo() {
        let temp = create_test_git_repo();
        let driver = GitDriver::from_path(temp.path());

        assert!(driver.is_local());

        let root = driver.get_root_identifier().unwrap();
        assert!(root == "master" || root == "main");

        let branches = driver.get_branches().unwrap();
        assert!(branches.contains_key("master") || branches.contains_key("main"));
    }

    #[test]
    fn test_git_driver_get_file_content() {
        let temp = create_test_git_repo();
        let driver = GitDriver::from_path(temp.path());

        let head = driver.get_root_identifier().unwrap();
        let content = driver.get_file_content("composer.json", &head).unwrap();

        assert!(content.contains("vendor/package"));
    }

    #[test]
    fn test_git_driver_get_composer_info() {
        let temp = create_test_git_repo();
        let driver = GitDriver::from_path(temp.path());

        let head = driver.get_root_identifier().unwrap();
        let info = driver.get_composer_information(&head).unwrap();

        assert!(info.manifest.is_some());
        let json = info.manifest.unwrap();
        assert_eq!(json["name"], "vendor/package");
    }

    #[test]
    fn test_git_driver_tags() {
        let temp = create_test_git_repo();

        // Create a tag (annotated tag with message)
        Command::new("git")
            .args(["tag", "-a", "v1.0.0", "-m", "Version 1.0.0"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        let driver = GitDriver::from_path(temp.path());
        let tags = driver.get_tags().unwrap();

        assert!(tags.contains_key("v1.0.0"));
    }

    #[test]
    fn test_supports_local_path() {
        let temp = create_test_git_repo();
        assert!(GitDriver::supports(temp.path().to_str().unwrap(), false));
    }

    #[test]
    fn test_supports_git_url() {
        assert!(GitDriver::supports(
            "https://github.com/owner/repo.git",
            false
        ));
        assert!(GitDriver::supports("git@github.com:owner/repo.git", false));
        assert!(GitDriver::supports(
            "git://github.com/owner/repo.git",
            false
        ));
    }

    // Ported from Composer\Test\Repository\Vcs\GitDriverTest::
    // testGetRootIdentifierFromLocalWithNetworkDisabled. Local checkout branch
    // discovery never needs the network in Riff.
    #[test]
    fn composer_git_driver_uses_checked_out_branch_as_root_identifier() {
        let temp = create_test_git_repo();
        Command::new("git")
            .args(["branch", "-M", "main"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        let driver = GitDriver::from_path(temp.path());

        assert_eq!(driver.get_root_identifier().unwrap(), "main");
    }

    #[test]
    fn composer_git_driver_uses_remote_head_branch_as_root_identifier() {
        let temp = create_test_git_repo();
        Command::new("git")
            .args(["branch", "-M", "main"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        let remote = format!("file://{}", temp.path().to_string_lossy());
        let driver = GitDriver::new(remote);

        assert!(!driver.is_local());
        assert_eq!(driver.get_root_identifier().unwrap(), "main");
    }

    #[test]
    fn composer_git_driver_filters_invalid_branch_names() {
        let branches = parse_branch_refs(
            "089681446ba44d6d9004350192486f2ceb4eaa06 main\n\
             12681446ba44d6d9004350192486f2ceb4eaa06 2.2\n\
             089681446ba44d6d9004350192486f2ceb4eaa06 -h\n",
            "",
        );

        assert_eq!(branches.len(), 2);
        assert_eq!(
            branches.get("main").map(String::as_str),
            Some("089681446ba44d6d9004350192486f2ceb4eaa06")
        );
        assert_eq!(
            branches.get("2.2").map(String::as_str),
            Some("12681446ba44d6d9004350192486f2ceb4eaa06")
        );
        assert!(!branches.contains_key("-h"));
    }

    #[test]
    fn composer_git_driver_rejects_invalid_file_identifier() {
        let temp = create_test_git_repo();
        let driver = GitDriver::from_path(temp.path());

        assert!(driver.get_file_content("file.txt", "HEAD").is_err());
        assert!(matches!(
            driver.get_file_content("file.txt", "-h"),
            Err(VcsDriverError::InvalidFormat(_))
        ));
    }

    #[test]
    fn composer_git_driver_rejects_invalid_change_date_identifier() {
        let temp = create_test_git_repo();
        let driver = GitDriver::from_path(temp.path());

        assert!(matches!(
            driver.get_change_date("-n1 --format=%at HEAD"),
            Err(VcsDriverError::InvalidFormat(_))
        ));
    }
}
