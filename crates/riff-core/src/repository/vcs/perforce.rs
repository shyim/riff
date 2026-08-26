//! Perforce command and repository driver support.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use super::driver::{VcsDriver, VcsDriverError, VcsInfo};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PerforceConfig {
    pub depot: String,
    pub branch: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub unique_client_name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PerforceCommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait PerforceProcess: Send + Sync {
    fn run(&self, arguments: &[String], input: Option<&str>) -> PerforceCommandOutput;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemPerforceProcess;

impl PerforceProcess for SystemPerforceProcess {
    fn run(&self, arguments: &[String], input: Option<&str>) -> PerforceCommandOutput {
        let Some((program, arguments)) = arguments.split_first() else {
            return PerforceCommandOutput {
                stderr: "empty Perforce command".to_owned(),
                ..PerforceCommandOutput::default()
            };
        };
        let mut command = Command::new(program);
        command.args(arguments);
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let Ok(mut child) = command.spawn() else {
            return PerforceCommandOutput {
                stderr: format!("failed to execute {program}"),
                ..PerforceCommandOutput::default()
            };
        };
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            use std::io::Write;
            if let Err(error) = stdin.write_all(input.as_bytes()) {
                return PerforceCommandOutput {
                    stderr: error.to_string(),
                    ..PerforceCommandOutput::default()
                };
            }
        }
        match child.wait_with_output() {
            Ok(output) => PerforceCommandOutput {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            },
            Err(error) => PerforceCommandOutput {
                stderr: error.to_string(),
                ..PerforceCommandOutput::default()
            },
        }
    }
}

/// A credential update that never requires passing user input through a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PerforceCredentialUpdate {
    Command(Vec<String>),
    Environment { name: String, value: String },
}

/// Testable Perforce utility implementing Composer's command semantics.
pub struct Perforce<P = SystemPerforceProcess> {
    config: PerforceConfig,
    port: String,
    path: PathBuf,
    process: Arc<P>,
    stream: Option<String>,
    stream_depot: bool,
}

impl Perforce<SystemPerforceProcess> {
    pub fn new(config: PerforceConfig, port: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::with_process(config, port, path, Arc::new(SystemPerforceProcess))
    }
}

impl<P: PerforceProcess> Perforce<P> {
    pub fn with_process(
        mut config: PerforceConfig,
        port: impl Into<String>,
        path: impl Into<PathBuf>,
        process: Arc<P>,
    ) -> Self {
        if config
            .unique_client_name
            .as_deref()
            .is_none_or(str::is_empty)
        {
            config.unique_client_name = Some(format!("riff_{}", std::process::id()));
        }
        Self {
            config,
            port: port.into(),
            path: path.into(),
            process,
            stream: None,
            stream_depot: false,
        }
    }

    pub fn depot(&self) -> &str {
        &self.config.depot
    }

    pub fn branch(&self) -> &str {
        self.config.branch.as_deref().unwrap_or("")
    }

    pub fn user(&self) -> Option<&str> {
        self.config.user.as_deref()
    }

    pub fn set_user(&mut self, user: Option<String>) {
        self.config.user = user;
    }

    pub fn set_stream(&mut self, stream: impl Into<String>) {
        let stream = stream.into();
        self.stream_depot = stream.trim_start_matches('/').contains('/');
        self.stream = Some(stream);
    }

    pub fn is_stream(&self) -> bool {
        self.stream_depot
    }

    pub fn stream(&self) -> String {
        self.stream.clone().unwrap_or_else(|| {
            if self.stream_depot && !self.branch().is_empty() {
                format!("//{}/{}", self.depot(), self.branch())
            } else {
                format!("//{}", self.depot())
            }
        })
    }

    pub fn stream_without_label(stream: &str) -> &str {
        stream.split_once('@').map_or(stream, |(stream, _)| stream)
    }

    pub fn client(&self) -> String {
        let stream = self
            .stream()
            .replace("//", "")
            .replace('/', "_")
            .replace('@', "")
            .trim_end_matches('_')
            .to_owned();
        format!(
            "composer_perforce_{}_{}",
            self.config.unique_client_name.as_deref().unwrap_or("riff"),
            stream
        )
    }

    pub fn client_spec_path(&self) -> PathBuf {
        self.path.join(format!("{}.p4.spec", self.client()))
    }

    pub fn generate_command(
        &self,
        arguments: impl IntoIterator<Item = impl Into<String>>,
        use_client: bool,
    ) -> Vec<String> {
        let mut command = vec!["p4".to_owned()];
        if let Some(user) = self.user() {
            command.extend(["-u".to_owned(), user.to_owned()]);
        }
        if use_client {
            command.extend(["-c".to_owned(), self.client()]);
        }
        command.extend(["-p".to_owned(), self.port.clone()]);
        command.extend(arguments.into_iter().map(Into::into));
        command
    }

    pub fn parse_variable(output: &str, name: &str, windows: bool) -> Option<String> {
        if !windows {
            return (!output.trim().is_empty()).then(|| output.trim().to_owned());
        }
        output.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.split_whitespace().next().unwrap_or("").to_owned())
                .filter(|value| !value.is_empty())
        })
    }

    pub fn resolve_user(
        &mut self,
        environment: Option<&str>,
        prompt: impl FnOnce() -> String,
    ) -> &str {
        if self.config.user.as_deref().is_none_or(str::is_empty) {
            self.config.user = environment
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.trim().to_owned())
                .or_else(|| Some(prompt()));
        }
        self.config.user.as_deref().unwrap_or("")
    }

    pub fn resolve_password(
        &mut self,
        environment: Option<&str>,
        prompt: impl FnOnce() -> String,
    ) -> &str {
        if self.config.password.as_deref().is_none_or(str::is_empty) {
            self.config.password = environment
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.trim().to_owned())
                .or_else(|| Some(prompt()));
        }
        self.config.password.as_deref().unwrap_or("")
    }

    pub fn user_credential_update(user: &str, windows: bool) -> PerforceCredentialUpdate {
        if windows {
            PerforceCredentialUpdate::Command(vec![
                "p4".to_owned(),
                "set".to_owned(),
                format!("P4USER={user}"),
            ])
        } else {
            PerforceCredentialUpdate::Environment {
                name: "P4USER".to_owned(),
                value: user.to_owned(),
            }
        }
    }

    pub fn client_spec(&self) -> String {
        let mut spec = format!(
            "Client: {}\n\nUpdate:\n\nAccess:\nOwner:  {}\n\nDescription:\n  Created by {} from composer.\n\nRoot: {}\n\nOptions:  noallwrite noclobber nocompress unlocked modtime rmdir\n\nSubmitOptions:  revertunchanged\n\nLineEnd:  local\n\n",
            self.client(),
            self.user().unwrap_or(""),
            self.user().unwrap_or(""),
            self.path.display(),
        );
        if self.is_stream() {
            spec.push_str(&format!(
                "Stream:\n  {}\n",
                Self::stream_without_label(&self.stream())
            ));
        } else {
            spec.push_str(&format!(
                "View:  {}/...  //{}/... \n",
                self.stream(),
                self.client()
            ));
        }
        spec
    }

    pub fn write_client_spec(&self) -> Result<(), VcsDriverError> {
        std::fs::create_dir_all(&self.path).map_err(process_error)?;
        std::fs::write(self.client_spec_path(), self.client_spec()).map_err(process_error)
    }

    pub fn connect_client(&self) -> Result<(), VcsDriverError> {
        self.run(&["client", "-i"], true, Some(&self.client_spec()))
            .map(|_| ())
    }

    pub fn is_logged_in(&self) -> Result<bool, VcsDriverError> {
        Ok(self.run_output(&["login", "-s"], false, None).success)
    }

    pub fn login(&mut self, password: Option<&str>) -> Result<(), VcsDriverError> {
        if self.is_logged_in()? {
            return Ok(());
        }
        let password = password
            .map(str::to_owned)
            .or_else(|| self.config.password.clone())
            .unwrap_or_default();
        self.run(&["login", "-a"], false, Some(&password))
            .map(|_| ())
    }

    pub fn initialize_client(&mut self) -> Result<(), VcsDriverError> {
        self.login(None)?;
        self.check_stream()?;
        self.write_client_spec()?;
        self.connect_client()
    }

    pub fn check_stream(&mut self) -> Result<bool, VcsDriverError> {
        let output = self.run(&["depots"], false, None)?;
        self.stream_depot = output.lines().any(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            fields.first() == Some(&"Depot")
                && fields.get(1) == Some(&self.depot())
                && fields.get(3).is_some_and(|kind| *kind == "stream")
        });
        if self.stream_depot && !self.branch().is_empty() {
            self.stream = Some(format!("//{}/{}", self.depot(), self.branch()));
        }
        Ok(self.stream_depot)
    }

    pub fn branches(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        if self.is_stream() {
            let _ = self.run(&["streams", &format!("//{}/...", self.depot())], true, None)?;
        }
        let changes = self.run(&["changes", &format!("{}/...", self.stream())], false, None)?;
        let change = parse_change_number(&changes)
            .ok_or_else(|| VcsDriverError::InvalidFormat("missing Perforce change".to_owned()))?;
        Ok(HashMap::from([(
            "master".to_owned(),
            format!("{}@{change}", self.stream()),
        )]))
    }

    pub fn tags(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        let output = self.run(&["labels"], true, None)?;
        Ok(output
            .lines()
            .filter_map(|line| line.strip_prefix("Label "))
            .filter_map(|line| line.split_whitespace().next())
            .map(|label| (label.to_owned(), format!("{}@{label}", self.stream())))
            .collect())
    }

    pub fn file_path(
        &self,
        file: &str,
        identifier: &str,
    ) -> Result<Option<String>, VcsDriverError> {
        let Some((stream, label)) = identifier.split_once('@') else {
            return Ok(Some(format!("{identifier}/{file}")));
        };
        let labeled_path = format!("{stream}/{file}@{label}");
        let output = self.run(&["files", &labeled_path], false, None)?;
        if output.contains("no such file(s).") {
            return Ok(None);
        }
        Ok(parse_change_number(&output).map(|change| format!("{stream}/{file}@{change}")))
    }

    pub fn file_content(
        &self,
        file: &str,
        identifier: &str,
    ) -> Result<Option<String>, VcsDriverError> {
        let Some(path) = self.file_path(file, identifier)? else {
            return Ok(None);
        };
        let output = self.run(&["print", &path], true, None)?;
        Ok((!output.trim().is_empty()).then_some(output))
    }

    pub fn composer_information(
        &self,
        identifier: &str,
    ) -> Result<Option<serde_json::Value>, VcsDriverError> {
        let Some(content) = self.file_content("composer.json", identifier)? else {
            return Ok(None);
        };
        serde_json::from_str(&content)
            .map(Some)
            .map_err(|error| VcsDriverError::InvalidFormat(error.to_string()))
    }

    pub fn sync_code_base(&self, reference: Option<&str>) -> Result<(), VcsDriverError> {
        let mut arguments = vec!["sync".to_owned(), "-f".to_owned()];
        if let Some(reference) = reference {
            arguments.push(format!("@{reference}"));
        }
        let command = self.generate_command(arguments, true);
        self.execute(&command, None).map(|_| ())
    }

    pub fn cleanup_client_spec(&self) -> Result<(), VcsDriverError> {
        self.run(&["client", "-d", &self.client()], false, None)?;
        match std::fs::remove_file(self.client_spec_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(process_error(error)),
        }
    }

    pub fn check_server_exists(port: &str, process: &P) -> bool {
        process
            .run(
                &[
                    "p4".to_owned(),
                    "-p".to_owned(),
                    port.to_owned(),
                    "info".to_owned(),
                    "-s".to_owned(),
                ],
                None,
            )
            .success
    }

    fn run(
        &self,
        arguments: &[&str],
        use_client: bool,
        input: Option<&str>,
    ) -> Result<String, VcsDriverError> {
        let arguments = arguments.iter().map(|value| (*value).to_owned());
        let command = self.generate_command(arguments, use_client);
        self.execute(&command, input)
    }

    fn execute(&self, command: &[String], input: Option<&str>) -> Result<String, VcsDriverError> {
        let output = self.process.run(command, input);
        if output.success {
            Ok(output.stdout)
        } else {
            Err(VcsDriverError::ProcessError(output.stderr))
        }
    }

    fn run_output(
        &self,
        arguments: &[&str],
        use_client: bool,
        input: Option<&str>,
    ) -> PerforceCommandOutput {
        let command = self.generate_command(arguments.iter().copied(), use_client);
        self.process.run(&command, input)
    }
}

pub struct PerforceDriver<P = SystemPerforceProcess> {
    url: String,
    utility: Perforce<P>,
    initialized: bool,
}

impl PerforceDriver<SystemPerforceProcess> {
    pub fn new(url: impl Into<String>, config: PerforceConfig, path: impl Into<PathBuf>) -> Self {
        Self::with_process(url, config, path, Arc::new(SystemPerforceProcess))
    }
}

impl<P: PerforceProcess> PerforceDriver<P> {
    pub fn with_process(
        url: impl Into<String>,
        config: PerforceConfig,
        path: impl Into<PathBuf>,
        process: Arc<P>,
    ) -> Self {
        let url = url.into();
        Self {
            utility: Perforce::with_process(config, url.clone(), path, process),
            url,
            initialized: false,
        }
    }

    pub fn initialize(&mut self) -> Result<(), VcsDriverError> {
        self.utility.initialize_client()?;
        self.initialized = true;
        Ok(())
    }

    pub fn depot(&self) -> &str {
        self.utility.depot()
    }

    pub fn branch(&self) -> &str {
        self.utility.branch()
    }

    pub fn has_composer_file(&self, identifier: &str) -> Result<bool, VcsDriverError> {
        let identifier = format!("//{}/{identifier}", self.depot());
        Ok(self
            .utility
            .composer_information(&identifier)?
            .is_some_and(|manifest| match manifest {
                serde_json::Value::Null => false,
                serde_json::Value::Array(values) => !values.is_empty(),
                serde_json::Value::Object(values) => !values.is_empty(),
                _ => true,
            }))
    }

    pub fn supports_with_process(url: &str, deep: bool, process: &P) -> bool {
        let hinted = url
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .any(|word| word.eq_ignore_ascii_case("perforce") || word.eq_ignore_ascii_case("p4"));
        (deep || hinted) && Perforce::<P>::check_server_exists(url, process)
    }

    pub fn cleanup(&mut self) -> Result<(), VcsDriverError> {
        self.utility.cleanup_client_spec()?;
        self.initialized = false;
        Ok(())
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}

impl<P: PerforceProcess> VcsDriver for PerforceDriver<P> {
    fn get_root_identifier(&self) -> Result<String, VcsDriverError> {
        Ok(self.branch().to_owned())
    }

    fn get_tags(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        self.utility.tags()
    }

    fn get_branches(&self) -> Result<HashMap<String, String>, VcsDriverError> {
        self.utility.branches()
    }

    fn get_composer_information(&self, identifier: &str) -> Result<VcsInfo, VcsDriverError> {
        let manifest = self.utility.composer_information(identifier)?;
        Ok(VcsInfo {
            manifest,
            identifier: identifier.to_owned(),
            time: None,
        })
    }

    fn get_file_content(&self, file: &str, identifier: &str) -> Result<String, VcsDriverError> {
        self.utility
            .file_content(file, identifier)?
            .ok_or_else(|| VcsDriverError::FileNotFound(file.to_owned()))
    }

    fn supports(url: &str, deep: bool) -> bool {
        let hinted = url
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .any(|word| word.eq_ignore_ascii_case("perforce") || word.eq_ignore_ascii_case("p4"));
        (deep || hinted)
            && Perforce::<SystemPerforceProcess>::check_server_exists(url, &SystemPerforceProcess)
    }

    fn get_url(&self) -> &str {
        &self.url
    }

    fn get_vcs_type(&self) -> &str {
        "perforce"
    }
}

fn parse_change_number(output: &str) -> Option<&str> {
    let fields = output.split_whitespace().collect::<Vec<_>>();
    fields
        .windows(2)
        .find(|fields| fields[0].eq_ignore_ascii_case("change"))
        .map(|fields| fields[1])
}

fn process_error(error: std::io::Error) -> VcsDriverError {
    VcsDriverError::ProcessError(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use tempfile::TempDir;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ProcessCall {
        arguments: Vec<String>,
        input: Option<String>,
    }

    #[derive(Default)]
    struct MockProcess {
        outputs: Mutex<VecDeque<PerforceCommandOutput>>,
        calls: Mutex<Vec<ProcessCall>>,
    }

    impl MockProcess {
        fn with_outputs(outputs: impl IntoIterator<Item = PerforceCommandOutput>) -> Arc<Self> {
            Arc::new(Self {
                outputs: Mutex::new(outputs.into_iter().collect()),
                calls: Mutex::default(),
            })
        }

        fn calls(&self) -> Vec<ProcessCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl PerforceProcess for MockProcess {
        fn run(&self, arguments: &[String], input: Option<&str>) -> PerforceCommandOutput {
            self.calls.lock().unwrap().push(ProcessCall {
                arguments: arguments.to_vec(),
                input: input.map(str::to_owned),
            });
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| PerforceCommandOutput {
                    success: true,
                    ..PerforceCommandOutput::default()
                })
        }
    }

    fn success(stdout: impl Into<String>) -> PerforceCommandOutput {
        PerforceCommandOutput {
            success: true,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn failure(stderr: impl Into<String>) -> PerforceCommandOutput {
        PerforceCommandOutput {
            success: false,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    fn config() -> PerforceConfig {
        PerforceConfig {
            depot: "depot".to_owned(),
            branch: Some("branch".to_owned()),
            user: Some("user".to_owned()),
            password: None,
            unique_client_name: Some("TEST".to_owned()),
        }
    }

    fn utility(directory: &TempDir, process: Arc<MockProcess>) -> Perforce<MockProcess> {
        Perforce::with_process(config(), "port", directory.path(), process)
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn composer_perforce_derives_client_stream_and_spec_names() {
        let directory = tempfile::tempdir().unwrap();
        let mut perforce = utility(&directory, MockProcess::with_outputs([]));

        assert_eq!(perforce.stream(), "//depot");
        assert_eq!(perforce.client(), "composer_perforce_TEST_depot");
        assert_eq!(
            perforce.client_spec_path(),
            directory
                .path()
                .join("composer_perforce_TEST_depot.p4.spec")
        );
        assert_eq!(
            Perforce::<MockProcess>::stream_without_label("//depot/branch"),
            "//depot/branch"
        );
        assert_eq!(
            Perforce::<MockProcess>::stream_without_label("//depot/branch@label"),
            "//depot/branch"
        );

        perforce.set_stream("//depot/branch");
        assert!(perforce.is_stream());
        assert_eq!(perforce.stream(), "//depot/branch");
        assert_eq!(perforce.client(), "composer_perforce_TEST_depot_branch");
    }

    #[test]
    fn composer_perforce_generates_typed_commands() {
        let directory = tempfile::tempdir().unwrap();
        let perforce = utility(&directory, MockProcess::with_outputs([]));

        assert_eq!(
            perforce.generate_command(["do", "something"], true),
            strings(&[
                "p4",
                "-u",
                "user",
                "-c",
                "composer_perforce_TEST_depot",
                "-p",
                "port",
                "do",
                "something",
            ])
        );
    }

    #[test]
    fn composer_perforce_resolves_configured_user_first() {
        let directory = tempfile::tempdir().unwrap();
        let mut perforce = utility(&directory, MockProcess::with_outputs([]));
        assert_eq!(
            perforce.resolve_user(Some("environment-user"), || panic!("must not prompt")),
            "user"
        );
    }

    #[test]
    fn composer_perforce_resolves_user_from_platform_environment() {
        assert_eq!(
            Perforce::<MockProcess>::parse_variable("P4USER=windows-user\n", "P4USER", true),
            Some("windows-user".to_owned())
        );
        assert_eq!(
            Perforce::<MockProcess>::parse_variable("unix-user\n", "P4USER", false),
            Some("unix-user".to_owned())
        );
    }

    #[test]
    fn composer_perforce_prompts_for_missing_user() {
        let directory = tempfile::tempdir().unwrap();
        let mut configuration = config();
        configuration.user = None;
        let mut perforce = Perforce::with_process(
            configuration,
            "port",
            directory.path(),
            MockProcess::with_outputs([]),
        );

        assert_eq!(
            perforce.resolve_user(None, || "prompt-user".to_owned()),
            "prompt-user"
        );
        assert_eq!(perforce.user(), Some("prompt-user"));
    }

    #[test]
    fn composer_perforce_credential_updates_are_shell_free() {
        let windows = Perforce::<MockProcess>::user_credential_update("foo && calc.exe", true);
        assert_eq!(
            windows,
            PerforceCredentialUpdate::Command(strings(&["p4", "set", "P4USER=foo && calc.exe"]))
        );
        let unix = Perforce::<MockProcess>::user_credential_update("foo; id", false);
        assert_eq!(
            unix,
            PerforceCredentialUpdate::Environment {
                name: "P4USER".to_owned(),
                value: "foo; id".to_owned(),
            }
        );
    }

    #[test]
    fn composer_perforce_resolves_configured_password_first() {
        let directory = tempfile::tempdir().unwrap();
        let mut configuration = config();
        configuration.password = Some("configured-password".to_owned());
        let mut perforce = Perforce::with_process(
            configuration,
            "port",
            directory.path(),
            MockProcess::with_outputs([]),
        );
        assert_eq!(
            perforce.resolve_password(Some("environment-password"), || panic!("must not prompt")),
            "configured-password"
        );
    }

    #[test]
    fn composer_perforce_resolves_password_from_platform_environment() {
        assert_eq!(
            Perforce::<MockProcess>::parse_variable(
                "P4PASSWD=windows-password (set)\n",
                "P4PASSWD",
                true
            ),
            Some("windows-password".to_owned())
        );
        assert_eq!(
            Perforce::<MockProcess>::parse_variable("unix-password\n", "P4PASSWD", false),
            Some("unix-password".to_owned())
        );
    }

    #[test]
    fn composer_perforce_prompts_for_missing_password() {
        let directory = tempfile::tempdir().unwrap();
        let mut perforce = utility(&directory, MockProcess::with_outputs([]));
        assert_eq!(
            perforce.resolve_password(None, || "prompt-password".to_owned()),
            "prompt-password"
        );
    }

    #[test]
    fn composer_perforce_client_spec_without_stream_matches_composer() {
        let directory = tempfile::tempdir().unwrap();
        let perforce = utility(&directory, MockProcess::with_outputs([]));
        let spec = perforce.client_spec();

        assert!(spec.starts_with("Client: composer_perforce_TEST_depot\n\nUpdate:"));
        assert!(spec.contains("Owner:  user\n\nDescription:\n  Created by user from composer."));
        assert!(spec.contains(&format!("Root: {}", directory.path().display())));
        assert!(spec.ends_with("View:  //depot/...  //composer_perforce_TEST_depot/... \n"));
    }

    #[test]
    fn composer_perforce_client_spec_with_stream_matches_composer() {
        let directory = tempfile::tempdir().unwrap();
        let mut perforce = utility(&directory, MockProcess::with_outputs([]));
        perforce.set_stream("//depot/branch@label");
        let spec = perforce.client_spec();

        assert!(spec.starts_with("Client: composer_perforce_TEST_depot_branchlabel\n\nUpdate:"));
        assert!(spec.ends_with("Stream:\n  //depot/branch\n"));
    }

    #[test]
    fn composer_perforce_checks_login_status() {
        let directory = tempfile::tempdir().unwrap();
        let process = MockProcess::with_outputs([success("")]);
        let perforce = utility(&directory, process.clone());

        assert!(perforce.is_logged_in().unwrap());
        assert_eq!(
            process.calls()[0].arguments,
            strings(&["p4", "-u", "user", "-p", "port", "login", "-s"])
        );
    }

    #[test]
    fn composer_perforce_reads_branches_with_and_without_streams() {
        let directory = tempfile::tempdir().unwrap();
        let process = MockProcess::with_outputs([success(
            "Change 5678 on 2014/03/19 by user@client 'change'",
        )]);
        let perforce = utility(&directory, process.clone());
        assert_eq!(perforce.branches().unwrap()["master"], "//depot@5678");
        assert_eq!(
            process.calls()[0].arguments,
            strings(&["p4", "-u", "user", "-p", "port", "changes", "//depot/..."])
        );

        let process = MockProcess::with_outputs([
            success("Stream //depot/branch mainline none 'branch'"),
            success("Change 1234 on 2014/03/19 by user@client 'change'"),
        ]);
        let mut perforce = utility(&directory, process.clone());
        perforce.set_stream("//depot/branch");
        assert_eq!(
            perforce.branches().unwrap()["master"],
            "//depot/branch@1234"
        );
        assert_eq!(
            process.calls()[0].arguments,
            strings(&[
                "p4",
                "-u",
                "user",
                "-c",
                "composer_perforce_TEST_depot_branch",
                "-p",
                "port",
                "streams",
                "//depot/..."
            ])
        );
    }

    #[test]
    fn composer_perforce_reads_tags_with_and_without_streams() {
        let labels = "Label 0.0.1 2013/07/31 'First'\nLabel 0.0.2 2013/08/01 'Second'\n";
        let directory = tempfile::tempdir().unwrap();
        let perforce = utility(&directory, MockProcess::with_outputs([success(labels)]));
        let tags = perforce.tags().unwrap();
        assert_eq!(tags["0.0.1"], "//depot@0.0.1");
        assert_eq!(tags["0.0.2"], "//depot@0.0.2");

        let mut perforce = utility(&directory, MockProcess::with_outputs([success(labels)]));
        perforce.set_stream("//depot/branch");
        let tags = perforce.tags().unwrap();
        assert_eq!(tags["0.0.1"], "//depot/branch@0.0.1");
        assert_eq!(tags["0.0.2"], "//depot/branch@0.0.2");
    }

    #[test]
    fn composer_perforce_detects_stream_depots() {
        let directory = tempfile::tempdir().unwrap();
        let mut perforce = utility(&directory, MockProcess::with_outputs([success("")]));
        assert!(!perforce.check_stream().unwrap());
        assert!(!perforce.is_stream());

        let process = MockProcess::with_outputs([success(
            "Depot depot 2013/06/25 stream /p4/depots/depot/... 'Created'",
        )]);
        let mut perforce = utility(&directory, process);
        assert!(perforce.check_stream().unwrap());
        assert!(perforce.is_stream());
        assert_eq!(perforce.stream(), "//depot/branch");
    }

    #[test]
    fn composer_perforce_reads_composer_information_without_labels() {
        let manifest = r#"{"name":"test/perforce","minimum-stability":"dev"}"#;
        let directory = tempfile::tempdir().unwrap();
        let process = MockProcess::with_outputs([success(manifest)]);
        let perforce = utility(&directory, process.clone());
        assert_eq!(
            perforce.composer_information("//depot").unwrap().unwrap()["name"],
            "test/perforce"
        );
        assert_eq!(
            process.calls()[0].arguments.last().unwrap(),
            "//depot/composer.json"
        );

        let process = MockProcess::with_outputs([success(manifest)]);
        let mut perforce = utility(&directory, process.clone());
        perforce.set_stream("//depot/branch");
        assert_eq!(
            perforce
                .composer_information("//depot/branch")
                .unwrap()
                .unwrap()["name"],
            "test/perforce"
        );
        assert_eq!(
            process.calls()[0].arguments.last().unwrap(),
            "//depot/branch/composer.json"
        );
    }

    #[test]
    fn composer_perforce_resolves_labeled_composer_information_to_changes() {
        let manifest = r#"{"name":"test/perforce","minimum-stability":"dev"}"#;
        let directory = tempfile::tempdir().unwrap();
        for (identifier, stream) in [
            ("//depot@0.0.1", None),
            ("//depot/branch@0.0.1", Some("//depot/branch")),
        ] {
            let process = MockProcess::with_outputs([
                success("//depot/composer.json#1 - branch change 10001 (text)"),
                success(manifest),
            ]);
            let mut perforce = utility(&directory, process.clone());
            if let Some(stream) = stream {
                perforce.set_stream(stream);
            }
            assert_eq!(
                perforce.composer_information(identifier).unwrap().unwrap()["name"],
                "test/perforce"
            );
            let calls = process.calls();
            assert!(calls[0].arguments.last().unwrap().ends_with("@0.0.1"));
            assert!(calls[1].arguments.last().unwrap().ends_with("@10001"));
        }
    }

    #[test]
    fn composer_perforce_syncs_with_and_without_streams() {
        let directory = tempfile::tempdir().unwrap();
        let process = MockProcess::with_outputs([]);
        let perforce = utility(&directory, process.clone());
        perforce.sync_code_base(Some("label")).unwrap();
        assert_eq!(
            process.calls()[0].arguments,
            strings(&[
                "p4",
                "-u",
                "user",
                "-c",
                "composer_perforce_TEST_depot",
                "-p",
                "port",
                "sync",
                "-f",
                "@label"
            ])
        );

        let process = MockProcess::with_outputs([]);
        let mut perforce = utility(&directory, process.clone());
        perforce.set_stream("//depot/branch");
        perforce.sync_code_base(Some("label")).unwrap();
        assert!(process.calls()[0]
            .arguments
            .contains(&"composer_perforce_TEST_depot_branch".to_owned()));
    }

    #[test]
    fn composer_perforce_checks_server_availability() {
        let process = MockProcess::with_outputs([success("serverDate"), failure("missing p4")]);
        assert!(Perforce::<MockProcess>::check_server_exists(
            "perforce.does.exist:port",
            &*process
        ));
        assert!(!Perforce::<MockProcess>::check_server_exists(
            "perforce.does.exist:port",
            &*process
        ));
        assert_eq!(
            process.calls()[0].arguments,
            strings(&["p4", "-p", "perforce.does.exist:port", "info", "-s"])
        );
    }

    #[test]
    fn composer_perforce_cleanup_deletes_client_and_spec() {
        let directory = tempfile::tempdir().unwrap();
        let process = MockProcess::with_outputs([]);
        let perforce = utility(&directory, process.clone());
        perforce.write_client_spec().unwrap();
        let spec = perforce.client_spec_path();
        assert!(spec.exists());

        perforce.cleanup_client_spec().unwrap();
        assert!(!spec.exists());
        assert_eq!(
            process.calls()[0].arguments,
            strings(&[
                "p4",
                "-u",
                "user",
                "-p",
                "port",
                "client",
                "-d",
                "composer_perforce_TEST_depot"
            ])
        );
    }

    #[test]
    fn composer_perforce_driver_captures_repository_config() {
        let directory = tempfile::tempdir().unwrap();
        let driver = PerforceDriver::with_process(
            "TEST_PERFORCE_URL",
            config(),
            directory.path(),
            MockProcess::with_outputs([]),
        );
        assert_eq!(driver.get_url(), "TEST_PERFORCE_URL");
        assert_eq!(driver.depot(), "depot");
        assert_eq!(driver.branch(), "branch");
    }

    #[test]
    fn composer_perforce_driver_initializes_login_stream_and_client() {
        let directory = tempfile::tempdir().unwrap();
        let process = MockProcess::with_outputs([
            success(""),
            success("Depot depot 2013/06/25 stream /p4/depots/depot/... 'Created'"),
            success(""),
        ]);
        let mut driver =
            PerforceDriver::with_process("port", config(), directory.path(), process.clone());

        driver.initialize().unwrap();
        assert!(driver.is_initialized());
        let calls = process.calls();
        assert!(calls[0].arguments.ends_with(&strings(&["login", "-s"])));
        assert!(calls[1].arguments.ends_with(&strings(&["depots"])));
        assert!(calls[2].arguments.ends_with(&strings(&["client", "-i"])));
        assert!(calls[2].input.as_deref().unwrap().starts_with("Client:"));
    }

    #[test]
    fn composer_perforce_driver_detects_composer_files() {
        let directory = tempfile::tempdir().unwrap();
        let driver = PerforceDriver::with_process(
            "port",
            config(),
            directory.path(),
            MockProcess::with_outputs([success("{}")]),
        );
        assert!(!driver.has_composer_file("branch").unwrap());

        let driver = PerforceDriver::with_process(
            "port",
            config(),
            directory.path(),
            MockProcess::with_outputs([success(r#"{"name":"test/perforce"}"#)]),
        );
        assert!(driver.has_composer_file("branch").unwrap());
    }

    #[test]
    fn composer_perforce_driver_requires_deep_or_hinted_support_checks() {
        let process = MockProcess::with_outputs([success("")]);
        assert!(!PerforceDriver::supports_with_process(
            "TEST_PERFORCE_URL",
            false,
            &*process
        ));
        assert!(process.calls().is_empty());
        assert!(PerforceDriver::supports_with_process(
            "ssl:p4.example.test:1666",
            false,
            &*process
        ));
    }

    #[test]
    fn composer_perforce_driver_cleans_up_client() {
        let directory = tempfile::tempdir().unwrap();
        let process =
            MockProcess::with_outputs([success(""), success(""), success(""), success("")]);
        let mut driver =
            PerforceDriver::with_process("port", config(), directory.path(), process.clone());
        driver.initialize().unwrap();
        driver.cleanup().unwrap();
        assert!(!driver.is_initialized());
        assert!(process.calls()[3].arguments.ends_with(&strings(&[
            "client",
            "-d",
            "composer_perforce_TEST_depot"
        ])));
    }
}
