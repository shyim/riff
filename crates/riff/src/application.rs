//! Process-level command resolution shared by the Riff CLI entry point.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use riff_core::json::RiffManifest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandOrigin {
    Builtin,
    Script,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApplicationInvocation {
    pub(crate) arguments: Vec<OsString>,
    pub(crate) command_name: Option<String>,
    pub(crate) origin: CommandOrigin,
    pub(crate) plugins_enabled: bool,
}

impl ApplicationInvocation {
    pub(crate) fn resolve(
        mut arguments: Vec<OsString>,
        current_dir: &Path,
        root: &usage_rs::Command<'_>,
    ) -> Self {
        let plugins_enabled = !has_flag(&arguments, "--no-plugins");
        let Some(command_index) = command_index(&arguments) else {
            return Self {
                arguments,
                command_name: None,
                origin: CommandOrigin::Unknown,
                plugins_enabled,
            };
        };
        let Some(selected_name) = arguments[command_index].to_str().map(str::to_owned) else {
            return Self {
                arguments,
                command_name: None,
                origin: CommandOrigin::Unknown,
                plugins_enabled,
            };
        };

        if let Some(command) = exact_builtin(root, &selected_name) {
            return Self {
                arguments,
                command_name: Some(command.name.to_owned()),
                origin: CommandOrigin::Builtin,
                plugins_enabled,
            };
        }

        let working_dir = working_directory(&arguments, current_dir);
        if is_script_command(&working_dir, &selected_name) {
            arguments[command_index] = OsString::from("run");
            arguments.insert(command_index + 1, OsString::from(&selected_name));
            return Self {
                arguments,
                command_name: Some(selected_name),
                origin: CommandOrigin::Script,
                plugins_enabled,
            };
        }

        Self {
            arguments,
            command_name: Some(selected_name),
            origin: CommandOrigin::Unknown,
            plugins_enabled,
        }
    }

    pub(crate) fn telemetry_command_name(&self) -> Option<&str> {
        match self.origin {
            CommandOrigin::Script => self
                .command_name
                .as_deref()
                .map(|command_name| telemetry_name(command_name, true)),
            CommandOrigin::Builtin => self
                .command_name
                .as_deref()
                .map(|command_name| telemetry_name(command_name, false)),
            CommandOrigin::Unknown => None,
        }
    }

    pub(crate) fn development_warning(&self, warning_at: u64, now: u64) -> Option<String> {
        if now <= warning_at || self.command_name.as_deref() == Some("self-update") {
            return None;
        }
        match self.origin {
            CommandOrigin::Builtin | CommandOrigin::Script => Some(format!(
                "Warning: This development build of Riff is over 60 days old. It is recommended to update it by running \"{} self-update\" to get the latest version.",
                std::env::current_exe()
                    .ok()
                    .as_deref()
                    .unwrap_or_else(|| Path::new("riff"))
                    .display()
            )),
            CommandOrigin::Unknown => None,
        }
    }
}

pub(crate) fn configured_development_warning(invocation: &ApplicationInvocation) -> Option<String> {
    let warning_at = std::env::var("RIFF_DEV_WARNING_TIME")
        .or_else(|_| std::env::var("COMPOSER_DEV_WARNING_TIME"))
        .ok()?
        .parse::<u64>()
        .ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    invocation.development_warning(warning_at, now)
}

pub(crate) fn telemetry_name(command_name: &str, script_alias: bool) -> &str {
    if script_alias {
        "script"
    } else {
        command_name
    }
}

fn exact_builtin<'a>(
    root: &'a usage_rs::Command<'a>,
    selected_name: &str,
) -> Option<&'a usage_rs::Command<'a>> {
    root.subcommands
        .iter()
        .copied()
        .find(|command| command.name == selected_name || command.aliases.contains(&selected_name))
}

fn command_index(arguments: &[OsString]) -> Option<usize> {
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].to_string_lossy();
        if argument == "--" {
            return (index + 1 < arguments.len()).then_some(index + 1);
        }
        if argument == "--php" || argument == "--output" {
            index += 2;
            continue;
        }
        if argument.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(index);
    }
    None
}

fn working_directory(arguments: &[OsString], current_dir: &Path) -> PathBuf {
    for (index, argument) in arguments.iter().enumerate() {
        if argument == OsStr::new("-d") || argument == OsStr::new("--working-dir") {
            if let Some(directory) = arguments.get(index + 1) {
                return absolutize(current_dir, Path::new(directory));
            }
        }
        if let Some(directory) = argument
            .to_str()
            .and_then(|argument| argument.strip_prefix("--working-dir="))
        {
            return absolutize(current_dir, Path::new(directory));
        }
    }
    current_dir.to_owned()
}

fn absolutize(current_dir: &Path, directory: &Path) -> PathBuf {
    if directory.is_absolute() {
        directory.to_owned()
    } else {
        current_dir.join(directory)
    }
}

fn is_script_command(working_dir: &Path, command_name: &str) -> bool {
    std::fs::read(working_dir.join("composer.json"))
        .ok()
        .and_then(|contents| serde_json::from_slice::<RiffManifest>(&contents).ok())
        .is_some_and(|manifest| manifest.scripts.custom.contains_key(command_name))
}

fn has_flag(arguments: &[OsString], flag: &str) -> bool {
    arguments
        .iter()
        .any(|argument| argument == OsStr::new(flag))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation(origin: CommandOrigin, command_name: &str) -> ApplicationInvocation {
        ApplicationInvocation {
            arguments: Vec::new(),
            command_name: Some(command_name.to_owned()),
            origin,
            plugins_enabled: true,
        }
    }

    // Ported from Composer\Test\Console\ApplicationTest::testDevWarning.
    #[test]
    fn composer_application_warns_for_an_expired_development_build() {
        let warning = invocation(CommandOrigin::Builtin, "about")
            .development_warning(99, 100)
            .unwrap();
        assert!(warning.contains("development build of Riff is over 60 days old"));
        assert!(warning.contains("self-update"));
    }

    // Ported from Composer\Test\Console\ApplicationTest::testDevWarningSuppressedForSelfUpdate.
    #[test]
    fn composer_application_suppresses_development_warning_for_self_update() {
        assert_eq!(
            invocation(CommandOrigin::Builtin, "self-update").development_warning(99, 100),
            None
        );
    }

    // Ported from Composer\Test\Console\ApplicationTest::testGetTelemetryCommandName.
    #[test]
    fn composer_application_normalizes_telemetry_command_names() {
        assert_eq!(telemetry_name("about", false), "about");
        assert_eq!(telemetry_name("myscript", true), "script");
        assert_eq!(telemetry_name("help", false), "help");
    }

    // Ported from Composer\Test\Console\ApplicationTest::
    // testNoPluginsDisablesPluginsWhenScriptCommandsExist.
    #[test]
    fn composer_application_keeps_plugins_disabled_while_discovering_scripts() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join("composer.json"),
            r#"{"scripts":{"my-script":"echo hello"}}"#,
        )
        .unwrap();
        let invocation = ApplicationInvocation::resolve(
            [
                "my-script",
                "--no-plugins",
                "-d",
                project.path().to_str().unwrap(),
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
            project.path(),
            crate::Cli::command(),
        );

        assert_eq!(invocation.origin, CommandOrigin::Script);
        assert!(!invocation.plugins_enabled);
        assert_eq!(invocation.arguments[0], "run");
        assert_eq!(invocation.arguments[1], "my-script");
    }
}
