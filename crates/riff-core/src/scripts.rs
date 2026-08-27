//! Script execution utilities for composer scripts.

use crate::output::{style, Output};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::json::{RiffManifest, ScriptValue};
use crate::plugin::manager::{ObjectScriptAction, ScriptPluginContext};
use crate::plugin::PluginManager;
use crate::process::{escape_argument, redact_command, OutputMode, ProcessError, ProcessExecutor};
use crate::runtime::RuntimeContext;

/// Default process timeout in seconds (same as Riff)
const DEFAULT_PROCESS_TIMEOUT: u64 = 300;

/// Script execution context to track environment variables and timeout settings
pub struct ScriptContext {
    env_vars: HashMap<String, String>,
    bin_dir: PathBuf,
    /// Process timeout in seconds, None means no timeout
    process_timeout: Option<u64>,
}

impl ScriptContext {
    pub fn new(dev_mode: bool, bin_dir: PathBuf) -> Self {
        // Check COMPOSER_PROCESS_TIMEOUT environment variable
        let process_timeout = match std::env::var("COMPOSER_PROCESS_TIMEOUT") {
            Ok(val) => {
                if val == "0" {
                    None // 0 means no timeout
                } else {
                    val.parse::<u64>().ok().or(Some(DEFAULT_PROCESS_TIMEOUT))
                }
            }
            Err(_) => Some(DEFAULT_PROCESS_TIMEOUT),
        };

        Self {
            env_vars: HashMap::from([(
                "COMPOSER_DEV_MODE".to_string(),
                if dev_mode { "1" } else { "0" }.to_string(),
            )]),
            bin_dir,
            process_timeout,
        }
    }

    /// Disable the process timeout
    pub fn disable_timeout(&mut self) {
        self.process_timeout = None;
    }
}

impl Default for ScriptContext {
    fn default() -> Self {
        Self::new(true, PathBuf::from("vendor/bin"))
    }
}

/// Immutable selection of scripts disabled through `COMPOSER_SKIP_SCRIPTS`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptFilter {
    skipped: HashSet<String>,
}

impl ScriptFilter {
    pub fn from_value(value: Option<&str>) -> Self {
        Self {
            skipped: value
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect(),
        }
    }

    pub fn from_process() -> Self {
        Self::from_value(std::env::var("COMPOSER_SKIP_SCRIPTS").ok().as_deref())
    }

    pub fn allows(&self, script_name: &str) -> bool {
        !self.skipped.contains(script_name)
    }
}

pub fn has_script_listeners(
    script_name: &str,
    scripts: &HashMap<&str, Vec<String>>,
    filter: &ScriptFilter,
) -> bool {
    filter.allows(script_name)
        && scripts
            .get(script_name)
            .is_some_and(|commands| !commands.is_empty())
}

pub fn script_command_message(command: &str) -> String {
    format!("> {}", redact_command(command))
}

pub fn script_failure_message(command: &str, event_name: &str, exit_code: i32) -> String {
    format!(
        "Script {} handling the {} event returned with error code {}",
        redact_command(command),
        event_name,
        exit_code
    )
}

/// Collect all scripts from composer.json into a map
pub fn collect_scripts(manifest: &RiffManifest) -> HashMap<&str, Vec<String>> {
    let mut scripts = HashMap::new();

    // Add built-in event scripts
    let events = [
        ("pre-install-cmd", &manifest.scripts.pre_install_cmd),
        ("post-install-cmd", &manifest.scripts.post_install_cmd),
        ("pre-update-cmd", &manifest.scripts.pre_update_cmd),
        ("post-update-cmd", &manifest.scripts.post_update_cmd),
        ("pre-status-cmd", &manifest.scripts.pre_status_cmd),
        ("post-status-cmd", &manifest.scripts.post_status_cmd),
        ("pre-archive-cmd", &manifest.scripts.pre_archive_cmd),
        ("post-archive-cmd", &manifest.scripts.post_archive_cmd),
        ("pre-autoload-dump", &manifest.scripts.pre_autoload_dump),
        ("post-autoload-dump", &manifest.scripts.post_autoload_dump),
        (
            "post-root-package-install",
            &manifest.scripts.post_root_package_install,
        ),
        (
            "post-create-project-cmd",
            &manifest.scripts.post_create_project_cmd,
        ),
        ("pre-operations-exec", &manifest.scripts.pre_operations_exec),
    ];

    for (name, value) in events {
        let cmds: Vec<String> = value.as_vec();
        if !cmds.is_empty() {
            scripts.insert(name, cmds);
        }
    }

    // Add custom scripts
    for (name, value) in &manifest.scripts.custom {
        let cmds: Vec<String> = value.as_vec();
        if !cmds.is_empty() {
            scripts.insert(name.as_str(), cmds);
        }
    }

    scripts
}

fn collect_object_scripts(
    manifest: &RiffManifest,
) -> HashMap<&str, &indexmap::IndexMap<String, serde_json::Value>> {
    manifest
        .scripts
        .custom
        .iter()
        .filter_map(|(name, value)| match value {
            ScriptValue::Object(configuration) => Some((name.as_str(), configuration)),
            _ => None,
        })
        .collect()
}

pub struct ScriptExecutionOptions<'a> {
    pub runtime: &'a RuntimeContext,
    pub dev_mode: bool,
    pub plugins: &'a PluginManager,
    pub bin_dir: PathBuf,
    pub output: &'a Output,
}

struct CommandEnvironment<'a> {
    working_dir: &'a Path,
    scripts: &'a HashMap<&'a str, Vec<String>>,
    object_scripts: &'a HashMap<&'a str, &'a indexmap::IndexMap<String, serde_json::Value>>,
    manifest: &'a RiffManifest,
    runtime: &'a RuntimeContext,
    plugins: &'a PluginManager,
    output: &'a Output,
}

/// Run a specific event script if it exists
/// Returns Ok(0) if script doesn't exist or ran successfully
pub fn run_event_script(
    event_name: &str,
    manifest: &RiffManifest,
    working_dir: &Path,
    quiet: bool,
    options: ScriptExecutionOptions<'_>,
) -> Result<i32> {
    let scripts = collect_scripts(manifest);
    let object_scripts = collect_object_scripts(manifest);
    let filter = ScriptFilter::from_process();

    if !has_script_listeners(event_name, &scripts, &filter) {
        // No script defined for this event, that's fine
        return Ok(0);
    }
    let commands = &scripts[event_name];

    let mut ctx = ScriptContext::new(options.dev_mode, options.bin_dir);
    let mut script_stack = vec![event_name.to_string()];
    let environment = CommandEnvironment {
        working_dir,
        scripts: &scripts,
        object_scripts: &object_scripts,
        manifest,
        runtime: options.runtime,
        plugins: options.plugins,
        output: options.output,
    };

    for cmd in commands {
        if !quiet {
            crate::outln!(options.output, "{}", script_command_message(cmd));
        }

        let exit_code =
            run_command_with_stack(cmd, &[], &mut ctx, &mut script_stack, &environment)?;

        if exit_code != 0 {
            crate::errln!(
                options.output,
                "{}",
                script_failure_message(cmd, event_name, exit_code)
            );
            return Ok(exit_code);
        }
    }

    Ok(0)
}

/// Run a named script with optional arguments
pub fn run_script(
    script_name: &str,
    manifest: &RiffManifest,
    working_dir: &Path,
    args: &[String],
    options: ScriptExecutionOptions<'_>,
) -> Result<i32> {
    let scripts = collect_scripts(manifest);
    let object_scripts = collect_object_scripts(manifest);
    let filter = ScriptFilter::from_process();

    if !filter.allows(script_name) {
        return Ok(0);
    }

    if let Some(configuration) = object_scripts.get(script_name) {
        return run_object_script(
            script_name,
            configuration,
            args,
            &mut ScriptContext::new(options.dev_mode, options.bin_dir),
            &CommandEnvironment {
                working_dir,
                scripts: &scripts,
                object_scripts: &object_scripts,
                manifest,
                runtime: options.runtime,
                plugins: options.plugins,
                output: options.output,
            },
        );
    }

    let Some(commands) = scripts.get(script_name) else {
        crate::errln!(
            options.output,
            "{} Script '{}' is not defined in this package",
            style("Error:").red().bold(),
            script_name
        );
        crate::errln!(options.output);
        crate::errln!(options.output, "Available scripts:");
        for name in scripts.keys() {
            crate::errln!(options.output, "  - {}", name);
        }
        return Ok(1);
    };

    crate::outln!(
        options.output,
        "{} Running {} ({} command(s))",
        style(">").green().bold(),
        style(script_name).cyan(),
        commands.len()
    );

    let mut ctx = ScriptContext::new(options.dev_mode, options.bin_dir);
    let mut script_stack = vec![script_name.to_string()];
    let environment = CommandEnvironment {
        working_dir,
        scripts: &scripts,
        object_scripts: &object_scripts,
        manifest,
        runtime: options.runtime,
        plugins: options.plugins,
        output: options.output,
    };

    for cmd in commands {
        crate::outln!(options.output, "{}", script_command_message(cmd));

        let exit_code =
            run_command_with_stack(cmd, args, &mut ctx, &mut script_stack, &environment)?;

        if exit_code != 0 {
            crate::errln!(
                options.output,
                "{}",
                script_failure_message(cmd, script_name, exit_code)
            );
            return Ok(exit_code);
        }
    }

    Ok(0)
}

/// Run a single command, handling special prefixes
pub fn run_command(
    cmd: &str,
    working_dir: &Path,
    extra_args: &[String],
    scripts: &HashMap<&str, Vec<String>>,
    ctx: &mut ScriptContext,
    runtime: &RuntimeContext,
) -> Result<i32> {
    run_command_with_output(
        cmd,
        working_dir,
        extra_args,
        scripts,
        ctx,
        runtime,
        &Output::silent(),
    )
}

pub fn run_command_with_output(
    cmd: &str,
    working_dir: &Path,
    extra_args: &[String],
    scripts: &HashMap<&str, Vec<String>>,
    ctx: &mut ScriptContext,
    runtime: &RuntimeContext,
    output: &Output,
) -> Result<i32> {
    let object_scripts = HashMap::new();
    let manifest = RiffManifest::default();
    let plugins = PluginManager::builtins(true, crate::config::AllowPlugins::Bool(true))?;
    let environment = CommandEnvironment {
        working_dir,
        scripts,
        object_scripts: &object_scripts,
        manifest: &manifest,
        runtime,
        plugins: &plugins,
        output,
    };
    run_command_with_stack(cmd, extra_args, ctx, &mut Vec::new(), &environment)
}

fn run_command_with_stack(
    cmd: &str,
    extra_args: &[String],
    ctx: &mut ScriptContext,
    script_stack: &mut Vec<String>,
    environment: &CommandEnvironment<'_>,
) -> Result<i32> {
    // Handle @putenv - set environment variable
    if let Some(env_assignment) = cmd.strip_prefix("@putenv ") {
        if let Some((key, value)) = env_assignment.split_once('=') {
            ctx.env_vars.insert(key.to_string(), value.to_string());
            std::env::set_var(key, value);
        }
        return Ok(0);
    }

    // Handle Composer\Config::disableProcessTimeout - disable timeout for subsequent commands
    if cmd.contains("Composer\\Config::disableProcessTimeout") {
        ctx.disable_timeout();
        return Ok(0);
    }

    // Handle @php - execute with current PHP binary
    if let Some(php_cmd) = cmd.strip_prefix("@php ") {
        let php_binary = shell_escape(environment.runtime.php_binary.to_string_lossy().as_ref());
        let full_cmd = append_arguments(&format!("{} {}", php_binary, php_cmd), extra_args);

        return execute_shell_command(&full_cmd, environment.working_dir, ctx, environment.output);
    }

    // Handle @composer - execute composer command via riff
    if let Some(composer_cmd) = cmd.strip_prefix("@composer ") {
        if let Some(exit_code) = environment.plugins.execute_composer_command(
            composer_cmd,
            extra_args,
            &ScriptPluginContext {
                manifest: environment.manifest,
                working_dir: environment.working_dir,
                runtime: environment.runtime,
                output: environment.output,
            },
        )? {
            return Ok(exit_code);
        }
        let riff_binary = shell_escape(environment.runtime.riff_binary.to_string_lossy().as_ref());
        let full_cmd = append_arguments(&format!("{} {}", riff_binary, composer_cmd), extra_args);

        return execute_shell_command(&full_cmd, environment.working_dir, ctx, environment.output);
    }

    // Handle @script-name - reference to another script
    if let Some(script_invocation) = cmd.strip_prefix('@') {
        let (script_ref, inline_args) = script_invocation
            .split_once(char::is_whitespace)
            .unwrap_or((script_invocation, ""));
        // Check if this references another script
        if let Some(ref_commands) = environment.scripts.get(script_ref) {
            if script_stack.iter().any(|active| active == script_ref) {
                anyhow::bail!(
                    "Circular call to script '{}' detected in {}",
                    script_ref,
                    script_stack.join(" -> ")
                );
            }
            crate::outln!(
                environment.output,
                "{} Running referenced script: {}",
                style(">").green(),
                style(script_ref).cyan()
            );

            let mut referenced_args: Vec<String> = inline_args
                .split_whitespace()
                .map(ToString::to_string)
                .collect();
            referenced_args.extend(extra_args.iter().cloned());
            script_stack.push(script_ref.to_string());
            let result = (|| {
                for ref_cmd in ref_commands {
                    crate::outln!(
                        environment.output,
                        "{} {}",
                        style(">").green(),
                        style(redact_command(ref_cmd)).dim()
                    );
                    let exit_code = run_command_with_stack(
                        ref_cmd,
                        &referenced_args,
                        ctx,
                        script_stack,
                        environment,
                    )?;
                    if exit_code != 0 {
                        return Ok(exit_code);
                    }
                }
                Ok(0)
            })();
            script_stack.pop();
            return result;
        } else if let Some(configuration) = environment.object_scripts.get(script_ref) {
            if script_stack.iter().any(|active| active == script_ref) {
                anyhow::bail!(
                    "Circular call to script '{}' detected in {}",
                    script_ref,
                    script_stack.join(" -> ")
                );
            }
            crate::outln!(
                environment.output,
                "{} Running referenced script: {}",
                style(">").green(),
                style(script_ref).cyan()
            );
            let mut referenced_args: Vec<String> = inline_args
                .split_whitespace()
                .map(ToString::to_string)
                .collect();
            referenced_args.extend(extra_args.iter().cloned());
            return run_object_script(
                script_ref,
                configuration,
                &referenced_args,
                ctx,
                environment,
            );
        } else {
            crate::errln!(
                environment.output,
                "{} Referenced script '{}' not found",
                style("Warning:").yellow(),
                script_ref
            );
            return Ok(1);
        }
    }

    // Regular shell command
    let full_cmd = append_arguments(cmd, extra_args);

    execute_shell_command(&full_cmd, environment.working_dir, ctx, environment.output)
}

fn run_object_script(
    script_name: &str,
    configuration: &indexmap::IndexMap<String, serde_json::Value>,
    arguments: &[String],
    ctx: &mut ScriptContext,
    environment: &CommandEnvironment<'_>,
) -> Result<i32> {
    let Some(actions) = environment.plugins.expand_object_script(
        script_name,
        configuration,
        arguments,
        &ScriptPluginContext {
            manifest: environment.manifest,
            working_dir: environment.working_dir,
            runtime: environment.runtime,
            output: environment.output,
        },
    )?
    else {
        anyhow::bail!(
            "Script '{}' is object-valued plugin configuration, but no enabled native plugin handles it",
            script_name
        );
    };

    for action in actions {
        match action {
            ObjectScriptAction::Warning(message) => {
                crate::errln!(
                    environment.output,
                    "{} {message}",
                    style("Warning:").yellow()
                );
            }
            ObjectScriptAction::Execute { display, command } => {
                crate::outln!(environment.output, "Executing script {display}");
                let exit_code = execute_shell_command(
                    &command,
                    environment.working_dir,
                    ctx,
                    environment.output,
                )?;
                if exit_code != 0 {
                    crate::errln!(environment.output, "{}", style("[KO]").red().bold());
                    crate::errln!(
                        environment.output,
                        "Script {display} returned with error code {exit_code}"
                    );
                    return Ok(exit_code);
                }
                crate::outln!(environment.output, "{}", style("[OK]").green().bold());
            }
        }
    }

    Ok(0)
}

fn append_arguments(command: &str, arguments: &[String]) -> String {
    if command.contains("@no_additional_args") {
        return command
            .replace("@no_additional_args", "")
            .trim()
            .to_string();
    }

    let escaped = arguments
        .iter()
        .map(|argument| shell_escape(argument))
        .collect::<Vec<_>>()
        .join(" ");

    if command.contains("@additional_args") {
        return command
            .replace("@additional_args", &escaped)
            .trim()
            .to_string();
    }

    if arguments.is_empty() {
        return command.to_string();
    }

    format!("{} {}", command, escaped)
}

fn shell_escape(argument: &str) -> String {
    escape_argument(Some(argument))
}

/// Execute a shell command with optional timeout
fn execute_shell_command(
    cmd: &str,
    working_dir: &Path,
    ctx: &ScriptContext,
    output: &Output,
) -> Result<i32> {
    // Prepend vendor/bin to PATH so scripts can find vendored binaries
    let vendor_bin = if ctx.bin_dir.is_absolute() {
        ctx.bin_dir.clone()
    } else {
        working_dir.join(&ctx.bin_dir)
    };
    let path_env = if vendor_bin.exists() {
        let current_path = std::env::var("PATH").unwrap_or_default();
        #[cfg(unix)]
        let new_path = format!("{}:{}", vendor_bin.display(), current_path);
        #[cfg(windows)]
        let new_path = format!("{};{}", vendor_bin.display(), current_path);
        Some(new_path)
    } else {
        None
    };

    #[cfg(unix)]
    let mut command = Command::new("sh");
    #[cfg(unix)]
    command.arg("-c").arg(cmd);

    #[cfg(windows)]
    let mut command = Command::new("cmd");
    #[cfg(windows)]
    command.arg("/C").arg(cmd);

    command.current_dir(working_dir);

    // Add vendor/bin to PATH
    if let Some(ref path) = path_env {
        command.env("PATH", path);
    }

    // Add custom environment variables
    for (key, value) in &ctx.env_vars {
        command.env(key, value);
    }

    let timeout = ctx.process_timeout.map(Duration::from_secs);
    match ProcessExecutor::new(timeout).execute(&mut command, OutputMode::Inherit) {
        Ok(output) => Ok(output.exit_code()),
        Err(ProcessError::Timeout(_)) => {
            crate::errln!(
                output,
                "{} Process timed out after {} seconds. Use Composer\\Config::disableProcessTimeout to disable.",
                style("Error:").red().bold(),
                ctx.process_timeout.expect("timeout error requires a timeout")
            );
            Ok(1)
        }
        Err(error) => Err(error)
            .with_context(|| format!("Failed to execute command: {}", redact_command(cmd))),
    }
}

/// List available scripts
pub fn list_scripts(manifest: &RiffManifest) -> Result<i32> {
    list_scripts_with_output(manifest, &Output::silent())
}

pub fn list_scripts_with_output(manifest: &RiffManifest, output: &Output) -> Result<i32> {
    let scripts = collect_scripts(manifest);

    if scripts.is_empty() {
        crate::outln!(
            output,
            "{} No scripts defined in composer.json",
            style("Info:").cyan()
        );
        return Ok(0);
    }

    crate::outln!(output, "{}", style("Available scripts:").cyan().bold());
    crate::outln!(output);

    // Separate custom scripts from event scripts
    let mut custom_scripts: Vec<_> = manifest.scripts.custom.keys().collect();
    custom_scripts.sort();

    let event_scripts = [
        "pre-install-cmd",
        "post-install-cmd",
        "pre-update-cmd",
        "post-update-cmd",
        "pre-status-cmd",
        "post-status-cmd",
        "pre-archive-cmd",
        "post-archive-cmd",
        "pre-autoload-dump",
        "post-autoload-dump",
        "post-root-package-install",
        "post-create-project-cmd",
        "pre-operations-exec",
    ];

    // Print custom scripts first (these are the user-defined ones)
    if !custom_scripts.is_empty() {
        crate::outln!(output, "{}", style("Scripts:").white().bold());
        for name in &custom_scripts {
            if let Some(cmds) = scripts.get(name.as_str()) {
                // Check for description
                let description = manifest.scripts_descriptions.get(*name);

                if let Some(desc) = description {
                    crate::outln!(output, "  {} - {}", style(name).green(), desc);
                } else {
                    crate::outln!(
                        output,
                        "  {} - Runs the {} script as defined in composer.json",
                        style(name).green(),
                        name
                    );
                }

                for cmd in cmds {
                    crate::outln!(output, "    {}", style(cmd).dim());
                }
            }
        }
        crate::outln!(output);
    }

    // Print event scripts (if any are defined)
    let defined_events: Vec<_> = event_scripts
        .iter()
        .filter(|name| scripts.contains_key(*name))
        .collect();

    if !defined_events.is_empty() {
        crate::outln!(output, "{}", style("Event Scripts:").white().bold());
        for name in defined_events {
            if let Some(cmds) = scripts.get(name) {
                crate::outln!(output, "  {}", style(name).yellow());
                for cmd in cmds {
                    crate::outln!(output, "    {}", style(cmd).dim());
                }
            }
        }
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::test_support::{environment_lock, EnvironmentGuard};

    #[cfg(unix)]
    fn executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::write(path, contents).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn append_arguments_preserves_shell_argument_boundaries() {
        assert_eq!(
            append_arguments(
                "printf '%s\\n'",
                &["two words".to_string(), "quote's; value".to_string()]
            ),
            "printf '%s\\n' 'two words' 'quote'\\''s; value'"
        );
        assert_eq!(
            append_arguments("printf fixed @no_additional_args", &["ignored".to_string()]),
            "printf fixed"
        );
        assert_eq!(
            append_arguments("printf @additional_args suffix", &["two words".to_string()]),
            "printf 'two words' suffix"
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_plugin_manager_handles_object_scripts() {
        let directory = tempfile::TempDir::new().unwrap();
        let manifest: RiffManifest = serde_json::from_value(serde_json::json!({
            "require": {"symfony/flex": "^2"},
            "scripts": {
                "auto-scripts": {"printf flex > flex.txt": "script"}
            }
        }))
        .unwrap();
        let plugins =
            PluginManager::builtins(true, crate::config::AllowPlugins::Bool(true)).unwrap();

        let code = run_script(
            "auto-scripts",
            &manifest,
            directory.path(),
            &[],
            ScriptExecutionOptions {
                runtime: &RuntimeContext::default(),
                dev_mode: true,
                plugins: &plugins,
                bin_dir: directory.path().join("vendor/bin"),
                output: &Output::silent(),
            },
        )
        .unwrap();

        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("flex.txt")).unwrap(),
            "flex"
        );
    }

    #[test]
    fn disabled_native_plugin_rejects_its_object_script() {
        let directory = tempfile::TempDir::new().unwrap();
        let manifest: RiffManifest = serde_json::from_value(serde_json::json!({
            "require": {"symfony/flex": "^2"},
            "scripts": {"auto-scripts": {"true": "script"}}
        }))
        .unwrap();
        let plugins =
            PluginManager::builtins(false, crate::config::AllowPlugins::Bool(true)).unwrap();

        let error = run_script(
            "auto-scripts",
            &manifest,
            directory.path(),
            &[],
            ScriptExecutionOptions {
                runtime: &RuntimeContext::default(),
                dev_mode: true,
                plugins: &plugins,
                bin_dir: directory.path().join("vendor/bin"),
                output: &Output::silent(),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("no enabled native plugin"));
    }

    #[test]
    fn native_plugin_manager_intercepts_composer_subcommands() {
        let directory = tempfile::TempDir::new().unwrap();
        std::fs::write(
            directory.path().join("composer.json"),
            r#"{"extra":{"bamarni-bin":{"target-directory":"vendor-bin"}}}"#,
        )
        .unwrap();
        let mut context = ScriptContext::new(true, directory.path().join("vendor/bin"));

        let code = run_command(
            "@composer bin all install --ansi",
            directory.path(),
            &[],
            &HashMap::new(),
            &mut context,
            &RuntimeContext::default(),
        )
        .unwrap();

        assert_eq!(code, 0);
    }

    // Ported from Composer\Test\EventDispatcher\EventDispatcherTest::testDispatcherCanExecuteSingleCommandLineScript.
    #[cfg(unix)]
    #[test]
    fn composer_dispatcher_executes_a_single_shell_command() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut context = ScriptContext::new(true, directory.path().join("vendor/bin"));

        let code = run_command(
            "printf single > single.txt",
            directory.path(),
            &[],
            &HashMap::new(),
            &mut context,
            &RuntimeContext::default(),
        )
        .unwrap();

        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("single.txt")).unwrap(),
            "single"
        );
    }

    // Ported from Composer\Test\EventDispatcher\EventDispatcherTest::testDispatcherCanExecuteCliAndPhpInSameEventScriptStack.
    #[cfg(unix)]
    #[test]
    fn composer_dispatcher_mixes_shell_and_php_commands_in_one_stack() {
        let directory = tempfile::TempDir::new().unwrap();
        let fake_php = directory.path().join("fake-php");
        executable(&fake_php, "#!/bin/sh\nprintf php > \"$1\"\n");
        let runtime = RuntimeContext::new(fake_php, PathBuf::from("riff"));
        let mut context = ScriptContext::new(true, directory.path().join("vendor/bin"));
        let scripts = HashMap::new();

        for command in ["printf cli > cli.txt", "@php php.txt"] {
            assert_eq!(
                run_command(
                    command,
                    directory.path(),
                    &[],
                    &scripts,
                    &mut context,
                    &runtime,
                )
                .unwrap(),
                0
            );
        }

        assert_eq!(
            std::fs::read_to_string(directory.path().join("cli.txt")).unwrap(),
            "cli"
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("php.txt")).unwrap(),
            "php"
        );
    }

    // Ported from Composer\Test\EventDispatcher\EventDispatcherTest::testDispatcherCanPutEnv.
    #[cfg(unix)]
    #[test]
    fn composer_dispatcher_putenv_applies_to_later_commands() {
        let _lock = environment_lock();
        let _environment = EnvironmentGuard::set("RIFF_SCRIPT_TEST_VALUE", None);
        let directory = tempfile::TempDir::new().unwrap();
        let mut context = ScriptContext::new(true, directory.path().join("vendor/bin"));
        let scripts = HashMap::new();
        let runtime = RuntimeContext::default();

        run_command(
            "@putenv RIFF_SCRIPT_TEST_VALUE=123",
            directory.path(),
            &[],
            &scripts,
            &mut context,
            &runtime,
        )
        .unwrap();
        run_command(
            "printf '%s' \"$RIFF_SCRIPT_TEST_VALUE\" > env.txt",
            directory.path(),
            &[],
            &scripts,
            &mut context,
            &runtime,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(directory.path().join("env.txt")).unwrap(),
            "123"
        );
    }

    // Ported from Composer\Test\EventDispatcher\EventDispatcherTest::testDispatcherAppendsDirBinOnPathForEveryListener.
    #[cfg(unix)]
    #[test]
    fn composer_dispatcher_rechecks_bin_dir_before_each_command() {
        let directory = tempfile::TempDir::new().unwrap();
        let bin_dir = directory.path().join("vendor/bin");
        let mut context = ScriptContext::new(true, bin_dir.clone());

        std::fs::create_dir_all(&bin_dir).unwrap();
        executable(
            &bin_dir.join("riff-bin-probe"),
            "#!/bin/sh\nprintf found > bin.txt\n",
        );
        let code = run_command(
            "riff-bin-probe",
            directory.path(),
            &[],
            &HashMap::new(),
            &mut context,
            &RuntimeContext::default(),
        )
        .unwrap();

        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("bin.txt")).unwrap(),
            "found"
        );
    }

    // Ported from Composer\Test\EventDispatcher\EventDispatcherTest::testDispatcherCanExecuteComposerScriptGroups.
    #[cfg(unix)]
    #[test]
    fn composer_dispatcher_executes_nested_script_groups_in_order() {
        let directory = tempfile::TempDir::new().unwrap();
        let scripts = HashMap::from([
            ("root", vec!["@group".to_string()]),
            (
                "group",
                vec![
                    "printf foo >> order.txt".to_string(),
                    "@subgroup".to_string(),
                    "printf bar >> order.txt".to_string(),
                ],
            ),
            ("subgroup", vec!["printf baz >> order.txt".to_string()]),
        ]);
        let mut context = ScriptContext::new(true, directory.path().join("vendor/bin"));

        run_command(
            "@root",
            directory.path(),
            &[],
            &scripts,
            &mut context,
            &RuntimeContext::default(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(directory.path().join("order.txt")).unwrap(),
            "foobazbar"
        );
    }

    // Ported from Composer\Test\EventDispatcher\EventDispatcherTest::testRecursionInScriptsNames.
    #[cfg(unix)]
    #[test]
    fn composer_dispatcher_passes_inline_arguments_to_referenced_scripts() {
        let directory = tempfile::TempDir::new().unwrap();
        let scripts = HashMap::from([
            ("helloWorld", vec!["@hello World".to_string()]),
            ("hello", vec!["printf '%s' > argument.txt".to_string()]),
        ]);
        let mut context = ScriptContext::new(true, directory.path().join("vendor/bin"));

        run_command(
            "@helloWorld",
            directory.path(),
            &[],
            &scripts,
            &mut context,
            &RuntimeContext::default(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(directory.path().join("argument.txt")).unwrap(),
            "World"
        );
    }

    // Ported from Composer\Test\EventDispatcher\EventDispatcherTest::testDispatcherDetectInfiniteRecursion.
    #[test]
    fn composer_dispatcher_rejects_recursive_script_groups() {
        let directory = tempfile::TempDir::new().unwrap();
        let scripts = HashMap::from([
            ("root", vec!["@recurse".to_string()]),
            ("recurse", vec!["@root".to_string()]),
        ]);
        let mut context = ScriptContext::new(true, directory.path().join("vendor/bin"));

        let error = run_command(
            "@root",
            directory.path(),
            &[],
            &scripts,
            &mut context,
            &RuntimeContext::default(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("Circular call"));
    }

    // Ported from Composer\Test\EventDispatcher\EventDispatcherTest::testDispatcherPassDevModeToAutoloadGeneratorForScriptEvents.
    #[cfg(unix)]
    #[test]
    fn composer_dispatcher_exposes_dev_mode_to_script_commands() {
        let directory = tempfile::TempDir::new().unwrap();
        for (dev_mode, expected) in [(true, "1"), (false, "0")] {
            let mut context = ScriptContext::new(dev_mode, directory.path().join("vendor/bin"));
            run_command(
                "printf '%s' \"$COMPOSER_DEV_MODE\" > dev-mode.txt",
                directory.path(),
                &[],
                &HashMap::new(),
                &mut context,
                &RuntimeContext::default(),
            )
            .unwrap();
            assert_eq!(
                std::fs::read_to_string(directory.path().join("dev-mode.txt")).unwrap(),
                expected
            );
        }
    }
}
