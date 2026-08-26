//! Source downloaders backed by Composer-supported VCS command-line tools.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Result, RiffError};

pub struct VcsDownloader;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcsCommandSpec {
    pub program: &'static str,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerforceCheckout {
    pub repository_url: String,
    pub user: Option<String>,
    pub destination: PathBuf,
    pub reference: String,
}

impl PerforceCheckout {
    pub fn new(
        repository_url: impl Into<String>,
        user: Option<String>,
        destination: impl Into<PathBuf>,
        reference: impl Into<String>,
    ) -> Self {
        Self {
            repository_url: repository_url.into(),
            user,
            destination: destination.into(),
            reference: reference.into(),
        }
    }
}

#[derive(Debug, Default)]
pub struct PerforceSession {
    checkout: Option<PerforceCheckout>,
}

impl PerforceSession {
    pub fn initialize(&mut self, checkout: PerforceCheckout) -> &PerforceCheckout {
        self.checkout.get_or_insert(checkout)
    }

    pub fn checkout(&self) -> Option<&PerforceCheckout> {
        self.checkout.as_ref()
    }

    pub fn install_plan(&self) -> Result<PerforceInstallPlan> {
        let checkout = self
            .checkout
            .as_ref()
            .ok_or_else(|| RiffError::DownloadFailed {
                package: "source".to_owned(),
                reason: "Perforce session has not been initialized".to_owned(),
            })?;
        VcsDownloader::perforce_install_plan(
            &checkout.repository_url,
            &checkout.destination,
            Some(&checkout.reference),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerforceInstallPlan {
    pub reference: String,
    pub label: Option<u64>,
    pub commands: Vec<VcsCommandSpec>,
}

impl VcsCommandSpec {
    fn new(program: &'static str, args: impl IntoIterator<Item = OsString>) -> Self {
        Self {
            program,
            args: args.into_iter().collect(),
            current_dir: None,
        }
    }

    fn in_directory(mut self, directory: &Path) -> Self {
        self.current_dir = Some(directory.to_path_buf());
        self
    }

    fn command(&self) -> Command {
        let mut command = Command::new(self.program);
        command.args(&self.args);
        if let Some(directory) = &self.current_dir {
            command.current_dir(directory);
        }
        command
    }
}

impl VcsDownloader {
    pub fn clone(
        vcs_type: &str,
        url: &str,
        destination: &Path,
        reference: Option<&str>,
    ) -> Result<()> {
        match vcs_type {
            "hg" | "mercurial" => {
                Self::ensure_new_checkout(destination, None)?;
                let plan = Self::install_plan(vcs_type, url, destination, reference)?;
                if let Err(error) = Self::execute_plan(vcs_type, &plan) {
                    let _ = Self::remove_checkout_files(vcs_type, destination);
                    return Err(error);
                }
                Ok(())
            }
            "svn" => {
                let mut command = Command::new("svn");
                command.arg("checkout");
                let checkout_url = reference
                    .filter(|value| value.contains("://"))
                    .unwrap_or(url);
                if checkout_url == url {
                    if let Some(reference) = reference.filter(|value| {
                        !value.is_empty()
                            && value.chars().all(|character| character.is_ascii_digit())
                    }) {
                        command.args(["--revision", reference]);
                    }
                }
                command.arg("--").arg(checkout_url).arg(destination);
                run(vcs_type, &mut command).map(|_| ())
            }
            "fossil" => {
                let repository = destination.with_extension("fossil");
                Self::ensure_new_checkout(destination, Some(&repository))?;
                let plan = Self::install_plan(vcs_type, url, destination, reference)?;
                std::fs::create_dir_all(destination)?;
                if let Err(error) = Self::execute_plan(vcs_type, &plan) {
                    let _ = Self::remove_checkout_files(vcs_type, destination);
                    return Err(error);
                }
                Ok(())
            }
            "perforce" | "p4" => {
                let plan = Self::perforce_install_plan(url, destination, reference)?;
                std::fs::create_dir_all(destination)?;
                Self::execute_plan(vcs_type, &plan.commands)
            }
            other => Err(RiffError::DownloadFailed {
                package: "source".to_string(),
                reason: format!("Unsupported source type: {other}"),
            }),
        }
    }

    pub fn install_plan(
        vcs_type: &str,
        url: &str,
        destination: &Path,
        reference: Option<&str>,
    ) -> Result<Vec<VcsCommandSpec>> {
        let reference = required_reference(vcs_type, reference)?;
        match vcs_type {
            "hg" | "mercurial" => Ok(vec![
                VcsCommandSpec::new(
                    "hg",
                    [
                        OsString::from("clone"),
                        OsString::from("--"),
                        OsString::from(url),
                        destination.as_os_str().to_owned(),
                    ],
                ),
                VcsCommandSpec::new(
                    "hg",
                    [
                        OsString::from("up"),
                        OsString::from("--"),
                        OsString::from(reference),
                    ],
                )
                .in_directory(destination),
            ]),
            "fossil" => {
                let repository = destination.with_extension("fossil");
                Ok(vec![
                    VcsCommandSpec::new(
                        "fossil",
                        [
                            OsString::from("clone"),
                            OsString::from("--"),
                            OsString::from(url),
                            repository.as_os_str().to_owned(),
                        ],
                    ),
                    VcsCommandSpec::new(
                        "fossil",
                        [
                            OsString::from("open"),
                            OsString::from("--nested"),
                            OsString::from("--"),
                            repository.as_os_str().to_owned(),
                        ],
                    )
                    .in_directory(destination),
                    VcsCommandSpec::new(
                        "fossil",
                        [
                            OsString::from("update"),
                            OsString::from("--"),
                            OsString::from(reference),
                        ],
                    )
                    .in_directory(destination),
                ])
            }
            other => Err(unsupported_vcs(other)),
        }
    }

    pub fn update_plan(
        vcs_type: &str,
        url: &str,
        destination: &Path,
        reference: Option<&str>,
    ) -> Result<Vec<VcsCommandSpec>> {
        let reference = required_reference(vcs_type, reference)?;
        match vcs_type {
            "hg" | "mercurial" => Ok(vec![
                VcsCommandSpec::new("hg", [OsString::from("status")]).in_directory(destination),
                VcsCommandSpec::new(
                    "hg",
                    [
                        OsString::from("pull"),
                        OsString::from("--"),
                        OsString::from(url),
                    ],
                )
                .in_directory(destination),
                VcsCommandSpec::new(
                    "hg",
                    [
                        OsString::from("up"),
                        OsString::from("--"),
                        OsString::from(reference),
                    ],
                )
                .in_directory(destination),
            ]),
            "fossil" => Ok(vec![
                VcsCommandSpec::new("fossil", [OsString::from("changes")])
                    .in_directory(destination),
                VcsCommandSpec::new("fossil", [OsString::from("pull")]).in_directory(destination),
                VcsCommandSpec::new(
                    "fossil",
                    [
                        OsString::from("up"),
                        OsString::from("--"),
                        OsString::from(reference),
                    ],
                )
                .in_directory(destination),
            ]),
            other => Err(unsupported_vcs(other)),
        }
    }

    pub fn update(
        vcs_type: &str,
        url: &str,
        destination: &Path,
        reference: Option<&str>,
    ) -> Result<()> {
        let plan = Self::update_plan(vcs_type, url, destination, reference)?;
        Self::execute_clean_worktree_plan(vcs_type, &plan)
    }

    pub fn removal_plan(vcs_type: &str, destination: &Path) -> Result<Vec<VcsCommandSpec>> {
        match vcs_type {
            "hg" | "mercurial" => Ok(vec![
                VcsCommandSpec::new("hg", [OsString::from("status")]).in_directory(destination)
            ]),
            "fossil" => Ok(vec![VcsCommandSpec::new(
                "fossil",
                [OsString::from("changes")],
            )
            .in_directory(destination)]),
            other => Err(unsupported_vcs(other)),
        }
    }

    pub fn removal_paths(vcs_type: &str, destination: &Path) -> Result<Vec<PathBuf>> {
        match vcs_type {
            "hg" | "mercurial" => Ok(vec![destination.to_path_buf()]),
            "fossil" => Ok(vec![
                destination.to_path_buf(),
                destination.with_extension("fossil"),
            ]),
            other => Err(unsupported_vcs(other)),
        }
    }

    pub fn remove(vcs_type: &str, destination: &Path) -> Result<()> {
        let plan = Self::removal_plan(vcs_type, destination)?;
        Self::execute_clean_worktree_plan(vcs_type, &plan)?;
        Self::remove_checkout_files(vcs_type, destination)
    }

    pub const fn installation_source() -> &'static str {
        "source"
    }

    pub fn perforce_install_plan(
        url: &str,
        destination: &Path,
        reference: Option<&str>,
    ) -> Result<PerforceInstallPlan> {
        let reference = required_reference("perforce", reference)?;
        let label = reference
            .rsplit_once('@')
            .and_then(|(_, label)| label.parse::<u64>().ok());
        let mut spec = url.trim_end_matches('/').to_owned();
        if !spec.ends_with("...") {
            spec.push_str("/...");
        }
        if let Some(label) = label {
            spec.push('@');
            spec.push_str(&label.to_string());
        }

        Ok(PerforceInstallPlan {
            reference: reference.to_owned(),
            label,
            commands: vec![VcsCommandSpec::new(
                "p4",
                [
                    OsString::from("-d"),
                    destination.as_os_str().to_owned(),
                    OsString::from("sync"),
                    OsString::from(spec),
                ],
            )],
        })
    }

    fn ensure_new_checkout(destination: &Path, repository: Option<&Path>) -> Result<()> {
        if destination.exists() || repository.is_some_and(Path::exists) {
            return Err(RiffError::DownloadFailed {
                package: "source".to_owned(),
                reason: format!(
                    "VCS checkout destination '{}' already exists",
                    destination.display()
                ),
            });
        }
        Ok(())
    }

    fn execute_plan(vcs_type: &str, plan: &[VcsCommandSpec]) -> Result<()> {
        for spec in plan {
            run_spec(vcs_type, spec)?;
        }
        Ok(())
    }

    fn execute_clean_worktree_plan(vcs_type: &str, plan: &[VcsCommandSpec]) -> Result<()> {
        let Some((check, operations)) = plan.split_first() else {
            return Ok(());
        };
        let changes = run_spec(vcs_type, check)?;
        if !changes.trim().is_empty() {
            return Err(RiffError::DownloadFailed {
                package: "source".to_owned(),
                reason: format!("{vcs_type} checkout has local changes"),
            });
        }
        Self::execute_plan(vcs_type, operations)
    }

    fn remove_checkout_files(vcs_type: &str, destination: &Path) -> Result<()> {
        for path in Self::removal_paths(vcs_type, destination)? {
            if path.is_dir() {
                std::fs::remove_dir_all(path)?;
            } else if path.exists() {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}

fn required_reference<'a>(vcs_type: &str, reference: Option<&'a str>) -> Result<&'a str> {
    reference
        .filter(|reference| !reference.trim().is_empty())
        .ok_or_else(|| RiffError::DownloadFailed {
            package: "source".to_owned(),
            reason: format!("{vcs_type} source reference must not be empty"),
        })
}

fn unsupported_vcs(vcs_type: &str) -> RiffError {
    RiffError::DownloadFailed {
        package: "source".to_string(),
        reason: format!("Unsupported source type: {vcs_type}"),
    }
}

fn run_spec(vcs_type: &str, spec: &VcsCommandSpec) -> Result<String> {
    let mut command = spec.command();
    run(vcs_type, &mut command)
}

fn run(vcs_type: &str, command: &mut Command) -> Result<String> {
    let description = format!("{command:?}");
    let output = command
        .output()
        .map_err(|error| RiffError::DownloadFailed {
            package: "source".to_string(),
            reason: format!("failed to execute {vcs_type}: {error}"),
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(RiffError::DownloadFailed {
            package: "source".to_string(),
            reason: format!(
                "{description}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(spec: &VcsCommandSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn composer_hg_downloader_rejects_install_without_source_reference() {
        let temp = tempfile::tempdir().unwrap();
        let error = VcsDownloader::install_plan(
            "hg",
            "https://example.test/repository",
            &temp.path().join("checkout"),
            None,
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("source reference must not be empty"));
    }

    #[test]
    fn composer_hg_downloader_plans_clone_and_reference_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("checkout");
        let plan = VcsDownloader::install_plan(
            "hg",
            "https://mercurial.example/repository",
            &destination,
            Some("ref"),
        )
        .unwrap();

        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].program, "hg");
        assert_eq!(
            args(&plan[0]),
            [
                "clone".to_owned(),
                "--".to_owned(),
                "https://mercurial.example/repository".to_owned(),
                destination.to_string_lossy().into_owned(),
            ]
        );
        assert_eq!(plan[0].current_dir, None);
        assert_eq!(args(&plan[1]), ["up", "--", "ref"]);
        assert_eq!(plan[1].current_dir.as_deref(), Some(destination.as_path()));
    }

    #[test]
    fn composer_hg_downloader_rejects_update_without_source_reference() {
        let temp = tempfile::tempdir().unwrap();
        let error = VcsDownloader::update_plan(
            "hg",
            "https://example.test/repository",
            temp.path(),
            Some(""),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("source reference must not be empty"));
    }

    #[test]
    fn composer_hg_downloader_plans_clean_pull_and_update() {
        let temp = tempfile::tempdir().unwrap();
        let plan = VcsDownloader::update_plan(
            "hg",
            "https://mercurial.example/repository",
            temp.path(),
            Some("ref"),
        )
        .unwrap();

        assert_eq!(plan.len(), 3);
        assert_eq!(args(&plan[0]), ["status"]);
        assert_eq!(
            args(&plan[1]),
            ["pull", "--", "https://mercurial.example/repository"]
        );
        assert_eq!(args(&plan[2]), ["up", "--", "ref"]);
        assert!(plan
            .iter()
            .all(|command| command.current_dir.as_deref() == Some(temp.path())));
    }

    #[test]
    fn composer_hg_downloader_plans_clean_check_and_checkout_removal() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("checkout");
        let plan = VcsDownloader::removal_plan("hg", &destination).unwrap();

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].program, "hg");
        assert_eq!(args(&plan[0]), ["status"]);
        assert_eq!(plan[0].current_dir.as_deref(), Some(destination.as_path()));
        assert_eq!(
            VcsDownloader::removal_paths("hg", &destination).unwrap(),
            [destination]
        );
    }

    #[test]
    fn composer_hg_downloader_reports_source_installation() {
        assert_eq!(VcsDownloader::installation_source(), "source");
    }

    #[test]
    fn composer_fossil_downloader_rejects_install_without_source_reference() {
        let temp = tempfile::tempdir().unwrap();
        let error = VcsDownloader::install_plan(
            "fossil",
            "https://example.test/repository",
            &temp.path().join("checkout"),
            None,
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("source reference must not be empty"));
    }

    #[test]
    fn composer_fossil_downloader_plans_clone_open_and_reference_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("checkout");
        let repository = destination.with_extension("fossil");
        let plan = VcsDownloader::install_plan(
            "fossil",
            "http://fossil.example/repository",
            &destination,
            Some("trunk"),
        )
        .unwrap();

        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].program, "fossil");
        assert_eq!(
            args(&plan[0]),
            [
                "clone".to_owned(),
                "--".to_owned(),
                "http://fossil.example/repository".to_owned(),
                repository.to_string_lossy().into_owned(),
            ]
        );
        assert_eq!(
            args(&plan[1]),
            [
                "open".to_owned(),
                "--nested".to_owned(),
                "--".to_owned(),
                repository.to_string_lossy().into_owned(),
            ]
        );
        assert_eq!(args(&plan[2]), ["update", "--", "trunk"]);
        assert!(plan[1..]
            .iter()
            .all(|command| command.current_dir.as_deref() == Some(destination.as_path())));
    }

    #[test]
    fn composer_fossil_downloader_rejects_update_without_source_reference() {
        let temp = tempfile::tempdir().unwrap();
        let error = VcsDownloader::update_plan(
            "fossil",
            "https://example.test/repository",
            temp.path(),
            Some(""),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("source reference must not be empty"));
    }

    #[test]
    fn composer_fossil_downloader_plans_clean_pull_and_update() {
        let temp = tempfile::tempdir().unwrap();
        let plan = VcsDownloader::update_plan(
            "fossil",
            "http://fossil.example/repository",
            temp.path(),
            Some("trunk"),
        )
        .unwrap();

        assert_eq!(plan.len(), 3);
        assert_eq!(args(&plan[0]), ["changes"]);
        assert_eq!(args(&plan[1]), ["pull"]);
        assert_eq!(args(&plan[2]), ["up", "--", "trunk"]);
        assert!(plan
            .iter()
            .all(|command| command.current_dir.as_deref() == Some(temp.path())));
    }

    #[test]
    fn composer_fossil_downloader_plans_clean_check_and_companion_removal() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("checkout");
        let plan = VcsDownloader::removal_plan("fossil", &destination).unwrap();

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].program, "fossil");
        assert_eq!(args(&plan[0]), ["changes"]);
        assert_eq!(plan[0].current_dir.as_deref(), Some(destination.as_path()));
        assert_eq!(
            VcsDownloader::removal_paths("fossil", &destination).unwrap(),
            [destination.clone(), destination.with_extension("fossil")]
        );
    }

    #[test]
    fn composer_fossil_downloader_reports_source_installation() {
        assert_eq!(VcsDownloader::installation_source(), "source");
    }

    #[test]
    fn composer_perforce_downloader_initializes_session_from_repository_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let checkout = PerforceCheckout::new(
            "p4://example.test/depot/project",
            Some("test-user".to_owned()),
            temp.path(),
            "//depot/project/main",
        );
        let mut session = PerforceSession::default();

        assert_eq!(session.initialize(checkout.clone()), &checkout);
        assert_eq!(session.checkout(), Some(&checkout));
        assert_eq!(
            session.install_plan().unwrap().reference,
            checkout.reference
        );
    }

    #[test]
    fn composer_perforce_downloader_preserves_existing_session() {
        let temp = tempfile::tempdir().unwrap();
        let first = PerforceCheckout::new(
            "p4://example.test/depot/first",
            Some("first-user".to_owned()),
            temp.path().join("first"),
            "//depot/first/main",
        );
        let replacement = PerforceCheckout::new(
            "p4://example.test/depot/replacement",
            Some("replacement-user".to_owned()),
            temp.path().join("replacement"),
            "//depot/replacement/main",
        );
        let mut session = PerforceSession::default();
        session.initialize(first.clone());

        assert_eq!(session.initialize(replacement), &first);
        assert_eq!(session.checkout(), Some(&first));
        assert_eq!(session.install_plan().unwrap().reference, first.reference);
    }

    #[test]
    fn composer_perforce_downloader_plans_tagged_sync_workflow() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("checkout");
        let plan = VcsDownloader::perforce_install_plan(
            "//depot/project",
            &destination,
            Some("SOURCE_REF@123"),
        )
        .unwrap();

        assert_eq!(plan.reference, "SOURCE_REF@123");
        assert_eq!(plan.label, Some(123));
        assert_eq!(plan.commands.len(), 1);
        assert_eq!(plan.commands[0].program, "p4");
        assert_eq!(
            args(&plan.commands[0]),
            [
                "-d".to_owned(),
                destination.to_string_lossy().into_owned(),
                "sync".to_owned(),
                "//depot/project/...@123".to_owned(),
            ]
        );
    }

    #[test]
    fn composer_perforce_downloader_plans_untagged_sync_workflow() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("checkout");
        let plan = VcsDownloader::perforce_install_plan(
            "//depot/project/",
            &destination,
            Some("SOURCE_REF"),
        )
        .unwrap();

        assert_eq!(plan.reference, "SOURCE_REF");
        assert_eq!(plan.label, None);
        assert_eq!(
            args(&plan.commands[0]),
            [
                "-d".to_owned(),
                destination.to_string_lossy().into_owned(),
                "sync".to_owned(),
                "//depot/project/...".to_owned(),
            ]
        );
    }
}
