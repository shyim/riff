//! Command-line drivers for VCS systems supported by Composer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use super::driver::{VcsDriver, VcsDriverError, VcsInfo};
use super::repository::VcsType;

pub struct CliVcsDriver {
    url: String,
    vcs_type: VcsType,
    checkout: Option<PathBuf>,
    fossil_repository: Option<PathBuf>,
    _temporary: Option<TempDir>,
}

impl CliVcsDriver {
    pub fn new(url: impl Into<String>, vcs_type: VcsType) -> Result<Self, VcsDriverError> {
        let url = url.into();
        let local = Path::new(&url);
        if local.exists() {
            return Ok(Self {
                fossil_repository: (vcs_type == VcsType::Fossil && local.is_file())
                    .then(|| local.to_path_buf()),
                checkout: local.is_dir().then(|| local.to_path_buf()),
                url,
                vcs_type,
                _temporary: None,
            });
        }

        match vcs_type {
            VcsType::Hg => {
                let temporary = tempfile::tempdir().map_err(process_error)?;
                let checkout = temporary.path().join("checkout");
                run(Command::new("hg")
                    .args(["clone", "-U", "--", &url])
                    .arg(&checkout))?;
                Ok(Self {
                    url,
                    vcs_type,
                    checkout: Some(checkout),
                    fossil_repository: None,
                    _temporary: Some(temporary),
                })
            }
            VcsType::Fossil => {
                let temporary = tempfile::tempdir().map_err(process_error)?;
                let repository = temporary.path().join("repository.fossil");
                run(Command::new("fossil")
                    .args(["clone", "--", &url])
                    .arg(&repository))?;
                Ok(Self {
                    url,
                    vcs_type,
                    checkout: None,
                    fossil_repository: Some(repository),
                    _temporary: Some(temporary),
                })
            }
            VcsType::Svn | VcsType::Perforce => Ok(Self {
                url,
                vcs_type,
                checkout: None,
                fossil_repository: None,
                _temporary: None,
            }),
            _ => Err(VcsDriverError::InvalidFormat(
                "command VCS driver requires hg, svn, fossil, or perforce".to_string(),
            )),
        }
    }

    fn hg(&self, args: &[&str]) -> Result<String, VcsDriverError> {
        let checkout = self.checkout.as_ref().ok_or_else(|| {
            VcsDriverError::NotFound(format!("Mercurial checkout for {}", self.url))
        })?;
        output(Command::new("hg").arg("-R").arg(checkout).args(args))
    }

    pub fn supports_hg_url(repository_url: &str) -> bool {
        let local = Path::new(repository_url);
        if local.join(".hg").is_dir()
            || repository_url.starts_with("hg+")
            || repository_url.ends_with(".hg")
        {
            return true;
        }
        let Ok(url) = url::Url::parse(repository_url) else {
            return false;
        };
        matches!(url.scheme(), "ssh" | "https")
            && url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case("bitbucket.org"))
            && url
                .path_segments()
                .is_some_and(|mut segments| segments.next().is_some() && segments.next().is_some())
    }

    /// Recognize the canonical Fossil URL shapes Composer accepts without
    /// invoking the external `fossil` executable.
    pub fn supports_fossil_url(repository_url: &str) -> bool {
        let Ok(url) = url::Url::parse(repository_url) else {
            return Path::new(repository_url)
                .extension()
                .is_some_and(|extension| extension == "fossil");
        };
        if !matches!(url.scheme(), "http" | "https" | "ssh") {
            return false;
        }
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        host.contains("fossil")
            || host == "chiselapp.com"
            || url.path().trim_end_matches('/').ends_with(".fossil")
    }

    pub fn get_hg_file_content(
        &self,
        file: &str,
        identifier: &str,
    ) -> Result<Option<String>, VcsDriverError> {
        validate_hg_identifier(identifier)?;
        match self.hg(&["cat", "-r", identifier, file]) {
            Ok(content) => Ok(Some(content)),
            Err(VcsDriverError::ProcessError(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn get_hg_change_date(&self, identifier: &str) -> Result<String, VcsDriverError> {
        validate_hg_identifier(identifier)?;
        self.hg(&["log", "-r", identifier, "--template", "{date|isodate}"])
            .map(|date| date.trim().to_owned())
    }

    fn fossil(&self, args: &[&str]) -> Result<String, VcsDriverError> {
        if let Some(repository) = &self.fossil_repository {
            output(Command::new("fossil").args(args).arg("-R").arg(repository))
        } else if let Some(checkout) = &self.checkout {
            output(Command::new("fossil").args(args).current_dir(checkout))
        } else {
            Err(VcsDriverError::NotFound(format!(
                "Fossil repository for {}",
                self.url
            )))
        }
    }

    fn svn_list(&self, directory: &str) -> Result<HashMap<String, String>, VcsDriverError> {
        let base = format!("{}/{}", self.url.trim_end_matches('/'), directory);
        let values = output(Command::new("svn").args(["list", "--", &base]))?;
        Ok(values
            .lines()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.trim_end_matches('/'))
            .map(|name| (name.to_string(), format!("{base}/{name}")))
            .collect())
    }

    fn perforce_file(&self, identifier: &str, file: &str) -> String {
        let base = self.url.trim_end_matches('/');
        if identifier.starts_with('@') {
            format!("{base}/{file}{identifier}")
        } else {
            format!("{base}/{file}")
        }
    }
}

impl VcsDriver for CliVcsDriver {
    fn get_root_identifier(&self) -> Result<String, VcsDriverError> {
        match self.vcs_type {
            VcsType::Hg => Ok("default".to_string()),
            VcsType::Svn => Ok(format!("{}/trunk", self.url.trim_end_matches('/'))),
            VcsType::Fossil => Ok("trunk".to_string()),
            VcsType::Perforce => Ok(String::new()),
            _ => unreachable!(),
        }
    }

    fn get_tags(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        match self.vcs_type {
            VcsType::Hg => {
                Ok(
                    parse_pairs(&self.hg(&["tags", "--template", "{tag}\\t{node}\\n"])?)
                        .into_iter()
                        .filter(|(tag, _)| tag != "tip")
                        .collect(),
                )
            }
            VcsType::Svn => self.svn_list("tags"),
            VcsType::Fossil => Ok(self
                .fossil(&["tag", "list"])?
                .lines()
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(|tag| (tag.to_string(), tag.to_string()))
                .collect()),
            VcsType::Perforce => Ok(output(Command::new("p4").arg("labels"))?
                .lines()
                .filter_map(|line| line.strip_prefix("Label "))
                .filter_map(|line| line.split_whitespace().next())
                .map(|label| (label.to_string(), format!("@{label}")))
                .collect()),
            _ => unreachable!(),
        }
    }

    fn get_branches(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        match self.vcs_type {
            VcsType::Hg => {
                let branches = self.hg(&["branches", "--template", "{branch}\\t{node}\\n"])?;
                let bookmarks = self.hg(&["bookmarks", "--template", "{bookmark}\\t{node}\\n"])?;
                Ok(parse_hg_branches(&branches, &bookmarks))
            }
            VcsType::Svn => self.svn_list("branches").or_else(|_| {
                Ok(HashMap::from([(
                    "trunk".to_string(),
                    format!("{}/trunk", self.url.trim_end_matches('/')),
                )]))
            }),
            VcsType::Fossil => Ok(self
                .fossil(&["branch", "list"])?
                .lines()
                .map(|line| line.trim().trim_start_matches('*').trim())
                .filter(|branch| !branch.is_empty())
                .map(|branch| (branch.to_string(), branch.to_string()))
                .collect()),
            VcsType::Perforce => Ok(HashMap::new()),
            _ => unreachable!(),
        }
    }

    fn get_composer_information(&self, identifier: &str) -> Result<VcsInfo, VcsDriverError> {
        let content = self.get_file_content("composer.json", identifier)?;
        let manifest = serde_json::from_str(&content)
            .map_err(|error| VcsDriverError::InvalidFormat(error.to_string()))?;
        let time = if self.vcs_type == VcsType::Hg {
            self.get_hg_change_date(identifier).ok()
        } else {
            None
        };
        Ok(VcsInfo {
            manifest: Some(manifest),
            identifier: identifier.to_string(),
            time,
        })
    }

    fn get_file_content(&self, file: &str, identifier: &str) -> Result<String, VcsDriverError> {
        match self.vcs_type {
            VcsType::Hg => self
                .get_hg_file_content(file, identifier)?
                .ok_or_else(|| VcsDriverError::FileNotFound(file.to_owned())),
            VcsType::Svn => {
                let url = format!("{}/{}", identifier.trim_end_matches('/'), file);
                output(Command::new("svn").args(["cat", "--", &url]))
            }
            VcsType::Fossil => self.fossil(&["cat", file, "-r", identifier]),
            VcsType::Perforce => output(
                Command::new("p4")
                    .args(["print", "-q", "--"])
                    .arg(self.perforce_file(identifier, file)),
            ),
            _ => unreachable!(),
        }
    }

    fn supports(url: &str, _deep: bool) -> bool {
        if Self::supports_hg_url(url) || Self::supports_fossil_url(url) {
            return true;
        }
        !url.trim().is_empty()
    }

    fn get_url(&self) -> &str {
        &self.url
    }

    fn get_vcs_type(&self) -> &str {
        match self.vcs_type {
            VcsType::Hg => "hg",
            VcsType::Svn => "svn",
            VcsType::Fossil => "fossil",
            VcsType::Perforce => "perforce",
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod composer_contract_tests {
    use super::*;

    // Ported from Composer\Test\Repository\Vcs\FossilDriverTest::testSupport.
    #[test]
    fn composer_fossil_driver_recognizes_supported_urls() {
        for url in [
            "http://fossil.kd2.org/kd2fw/",
            "https://chiselapp.com/user/rkeene/repository/flint/index",
            "ssh://fossil.kd2.org/kd2fw.fossil",
        ] {
            assert!(CliVcsDriver::supports_fossil_url(url), "{url}");
        }
        assert!(!CliVcsDriver::supports_fossil_url(
            "https://example.org/vendor/package"
        ));
    }
}

fn parse_pairs(output: &str) -> HashMap<String, String> {
    output
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(name, identifier)| (name.to_string(), identifier.to_string()))
        .collect()
}

fn parse_hg_branches(branches: &str, bookmarks: &str) -> HashMap<String, String> {
    branches
        .lines()
        .chain(bookmarks.lines())
        .filter_map(parse_hg_reference)
        .filter(|(name, _)| !name.starts_with('-'))
        .collect()
}

fn parse_hg_reference(line: &str) -> Option<(String, String)> {
    let line = line.trim().trim_start_matches('*').trim();
    let (name, identifier) = if let Some((name, identifier)) = line.split_once('\t') {
        (name.trim().to_owned(), identifier.trim())
    } else {
        let mut fields = line.split_whitespace().collect::<Vec<_>>();
        let identifier = fields.pop()?;
        (fields.join(" "), identifier)
    };
    if name.is_empty() {
        return None;
    }
    let identifier = identifier
        .split_once(':')
        .map_or(identifier, |(_, identifier)| identifier);
    Some((name, identifier.to_owned()))
}

fn validate_hg_identifier(identifier: &str) -> Result<(), VcsDriverError> {
    if identifier.trim().is_empty() || identifier.starts_with('-') {
        Err(VcsDriverError::InvalidFormat(format!(
            "invalid Mercurial identifier '{identifier}'"
        )))
    } else {
        Ok(())
    }
}

fn run(command: &mut Command) -> Result<(), VcsDriverError> {
    output(command).map(|_| ())
}

fn output(command: &mut Command) -> Result<String, VcsDriverError> {
    let description = format!("{command:?}");
    let output = command
        .output()
        .map_err(|error| VcsDriverError::ProcessError(format!("{description}: {error}")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(VcsDriverError::ProcessError(format!(
            "{description}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn process_error(error: std::io::Error) -> VcsDriverError {
    VcsDriverError::ProcessError(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_hg_driver_supports_bitbucket_mercurial_urls() {
        for url in [
            "ssh://bitbucket.org/user/repo",
            "ssh://hg@bitbucket.org/user/repo",
            "ssh://user@bitbucket.org/user/repo",
            "https://bitbucket.org/user/repo",
            "https://user@bitbucket.org/user/repo",
        ] {
            assert!(CliVcsDriver::supports(url, false), "{url}");
        }
        assert!(!CliVcsDriver::supports_hg_url(
            "https://example.org/user/repo"
        ));
    }

    #[test]
    fn composer_hg_driver_filters_option_like_branch_and_bookmark_names() {
        let branches = "default 1:dbf6c8acb640\n--help  1:dbf6c8acb640";
        let bookmarks = "help    1:dbf6c8acb641\n--help  1:dbf6c8acb641\n";

        assert_eq!(
            parse_hg_branches(branches, bookmarks),
            HashMap::from([
                ("help".to_owned(), "dbf6c8acb641".to_owned()),
                ("default".to_owned(), "dbf6c8acb640".to_owned()),
            ])
        );
    }

    #[test]
    fn composer_hg_driver_rejects_option_like_file_identifiers() {
        let temp = tempfile::tempdir().unwrap();
        let driver = CliVcsDriver::new(temp.path().to_string_lossy(), VcsType::Hg).unwrap();

        assert_eq!(driver.get_hg_file_content("file.txt", "h").unwrap(), None);
        assert!(matches!(
            driver.get_hg_file_content("file.txt", "-h"),
            Err(VcsDriverError::InvalidFormat(_))
        ));
    }

    #[test]
    fn composer_hg_driver_rejects_option_like_change_date_identifiers() {
        let temp = tempfile::tempdir().unwrap();
        let driver = CliVcsDriver::new(temp.path().to_string_lossy(), VcsType::Hg).unwrap();

        assert!(matches!(
            driver.get_hg_change_date("-r foo"),
            Err(VcsDriverError::InvalidFormat(_))
        ));
    }
}
