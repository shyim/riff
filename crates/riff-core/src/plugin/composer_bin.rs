//! Composer bin plugin - isolate bin dependencies.
//!
//! This is a native Rust port of bamarni/composer-bin-plugin.
//! When forward-command is enabled, install/update commands are
//! automatically forwarded to all bin namespaces in vendor-bin/.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};
use async_trait::async_trait;

use crate::event::{
    DependencyOperation, EventListener, EventType, PostAutoloadDumpEvent, RiffEvent,
};
use crate::installer::{InstallOptions, UpdateOptions};
use crate::json::{RiffLockfile, RiffManifest};
use crate::output::{Output, OutputEvent, OutputLevel, OutputSink, OutputStream};
use crate::process::ProcessRunner;
use crate::riff::Riff;
use crate::runtime::RuntimeContext;
use crate::session::{BatchOptions, ProjectAuditOptions, ProjectInstallRequest};
use crate::Result;

use super::manager::{ComposerCommandHook, PluginDescriptor, PluginRegistrar, ScriptPluginContext};

/// The package name that triggers this plugin.
pub const PACKAGE_NAME: &str = "bamarni/composer-bin-plugin";

/// Configuration for the bin plugin from composer.json extra.bamarni-bin
#[derive(Debug, Clone)]
pub struct BinConfig {
    /// Whether to create bin links in the main vendor/bin directory
    pub bin_links: bool,
    /// Target directory for bin namespaces (default: vendor-bin)
    pub target_directory: String,
    /// Whether to forward install/update commands to all namespaces
    pub forward_command: bool,
}

impl Default for BinConfig {
    fn default() -> Self {
        Self {
            bin_links: false, // Default to false in 2.x behavior
            target_directory: "vendor-bin".to_string(),
            forward_command: false,
        }
    }
}

impl BinConfig {
    /// Parse config from composer.json extra field
    pub fn from_extra(extra: &serde_json::Value) -> Self {
        let bamarni_bin = extra.get("bamarni-bin");

        let mut config = Self::default();

        if let Some(obj) = bamarni_bin.and_then(|v| v.as_object()) {
            if let Some(bin_links) = obj.get("bin-links").and_then(|v| v.as_bool()) {
                config.bin_links = bin_links;
            }
            if let Some(target_dir) = obj.get("target-directory").and_then(|v| v.as_str()) {
                config.target_directory = target_dir.to_string();
            }
            if let Some(forward) = obj.get("forward-command").and_then(|v| v.as_bool()) {
                config.forward_command = forward;
            }
        }

        config
    }
}

/// Composer bin plugin - implements EventListener directly.
pub struct ComposerBinPlugin;

pub(super) fn register(registrar: &mut PluginRegistrar) {
    let plugin = Arc::new(ComposerBinPlugin);
    registrar.descriptor(PluginDescriptor::new(PACKAGE_NAME));
    registrar.event(PACKAGE_NAME, EventType::PostAutoloadDump, plugin.clone());
    registrar.composer_command(PACKAGE_NAME, "bin", plugin);
}

#[async_trait(?Send)]
impl EventListener for ComposerBinPlugin {
    async fn handle(&self, event: &dyn RiffEvent, riff: &Riff) -> anyhow::Result<i32> {
        if event.event_type() != EventType::PostAutoloadDump {
            return Ok(0);
        }

        let Some(e) = event.as_any().downcast_ref::<PostAutoloadDumpEvent>() else {
            return Ok(0);
        };

        // Check if our package is installed
        let is_installed = e.packages.iter().any(|p| p.name == PACKAGE_NAME);
        if !is_installed {
            return Ok(0);
        }

        let config = BinConfig::from_extra(&riff.manifest.extra);
        if !config.forward_command {
            return Ok(0);
        }
        let Some(operation) = e.operation() else {
            return Ok(0);
        };
        if !riff.audit_enabled || riff.session.supports_project_audit() {
            let operation_name = match operation {
                DependencyOperation::Install => "install",
                DependencyOperation::Update => "update",
            };
            let invocation = BinInvocation {
                namespace: "all".to_string(),
                arguments: vec![operation_name.to_string()],
            };
            return run_bin_command_native(
                &invocation,
                NativeBinCommand {
                    operation: match operation {
                        DependencyOperation::Install => NativeOperation::Install,
                        DependencyOperation::Update => NativeOperation::Update,
                    },
                    no_audit: !riff.audit_enabled,
                },
                &ScriptPluginContext {
                    manifest: &riff.manifest,
                    working_dir: &riff.working_dir,
                    runtime: &riff.runtime,
                    output: riff.output(),
                    process_timeout: (riff.config.process_timeout > 0)
                        .then(|| Duration::from_secs(riff.config.process_timeout)),
                    riff: Some(riff),
                },
                riff,
            )
            .await;
        }

        self.post_autoload_dump(riff, operation)
    }

    fn priority(&self) -> i32 {
        -10
    }
}

#[async_trait(?Send)]
impl ComposerCommandHook for ComposerBinPlugin {
    fn execute(
        &self,
        command: &str,
        extra_args: &[String],
        context: &ScriptPluginContext<'_>,
    ) -> anyhow::Result<i32> {
        run_bin_command(
            command,
            context.working_dir,
            extra_args,
            context.runtime,
            context.output,
            context.process_timeout,
        )
    }

    async fn execute_async(
        &self,
        command: &str,
        extra_args: &[String],
        context: &ScriptPluginContext<'_>,
    ) -> anyhow::Result<i32> {
        let invocation = BinInvocation::parse(command)?;
        let Some(native_command) = NativeBinCommand::parse(&invocation.arguments, extra_args)
        else {
            return self.execute(command, extra_args, context);
        };
        let Some(riff) = context.riff else {
            return self.execute(command, extra_args, context);
        };
        if !native_command.no_audit && !riff.session.supports_project_audit() {
            return self.execute(command, extra_args, context);
        }

        run_bin_command_native(&invocation, native_command, context, riff).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeOperation {
    Install,
    Update,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeBinCommand {
    operation: NativeOperation,
    no_audit: bool,
}

impl NativeBinCommand {
    fn parse(arguments: &[String], extra_args: &[String]) -> Option<Self> {
        let (operation, flags) = arguments.split_first()?;
        let operation = match operation.as_str() {
            "install" => NativeOperation::Install,
            "update" => NativeOperation::Update,
            _ => return None,
        };
        let mut no_audit = false;
        for flag in flags.iter().chain(extra_args) {
            match flag.as_str() {
                "--ansi" | "--no-ansi" | "-n" | "--no-interaction" | "--no-progress" => {}
                "--no-audit" => no_audit = true,
                _ => return None,
            }
        }
        Some(Self {
            operation,
            no_audit,
        })
    }
}

#[derive(Default)]
struct BufferedOutput {
    events: Mutex<Vec<OutputEvent>>,
}

impl BufferedOutput {
    fn events(&self) -> Vec<OutputEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl OutputSink for BufferedOutput {
    fn emit(&self, event: OutputEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }
}

struct NativeNamespace {
    directory: PathBuf,
    output: Arc<BufferedOutput>,
    exit_code: Option<i32>,
}

async fn run_bin_command_native(
    invocation: &BinInvocation,
    command: NativeBinCommand,
    context: &ScriptPluginContext<'_>,
    riff: &Riff,
) -> anyhow::Result<i32> {
    let config = BinConfig::from_extra(&context.manifest.extra);
    let namespace_root = context.working_dir.join(&config.target_directory);
    let directories = namespace_directories(&namespace_root, &invocation.namespace)?;
    let mut namespaces = directories
        .into_iter()
        .map(|directory| NativeNamespace {
            directory,
            output: Arc::new(BufferedOutput::default()),
            exit_code: None,
        })
        .collect::<Vec<_>>();
    let mut requests = Vec::new();
    let mut request_indexes = Vec::new();

    for (index, namespace) in namespaces.iter_mut().enumerate() {
        match build_native_request(namespace, command, riff) {
            Ok(request) => {
                request_indexes.push(index);
                requests.push(request);
            }
            Err(error) => {
                namespace.output.emit(OutputEvent {
                    level: OutputLevel::Error,
                    stream: OutputStream::Stderr,
                    message: format!("Error: {error:#}"),
                    newline: true,
                });
                namespace.exit_code = Some(1);
            }
        }
    }

    let concurrency = requests.len().max(1);
    let results = riff
        .session
        .install_projects(
            requests,
            BatchOptions::default().with_max_concurrency(concurrency),
        )
        .await;
    for (index, result) in request_indexes.into_iter().zip(results) {
        namespaces[index].exit_code = Some(match result.into_result() {
            Ok(exit_code) => exit_code,
            Err(error) => {
                namespaces[index].output.emit(OutputEvent {
                    level: OutputLevel::Error,
                    stream: OutputStream::Stderr,
                    message: format!("Error: {error:#}"),
                    newline: true,
                });
                1
            }
        });
    }

    let main_bin_dir = riff.vendor_dir().join("bin");
    let mut aggregate_exit_code = 0i32;
    for namespace in namespaces {
        let namespace_name = namespace
            .directory
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        crate::outln!(
            context.output,
            "Running composer {} in bin namespace {}",
            invocation.arguments.join(" "),
            namespace_name
        );
        replay_buffer(context.output, &namespace.output);

        let mut exit_code = namespace.exit_code.unwrap_or(1);
        if exit_code == 0 && config.bin_links {
            if let Err(error) = create_bin_links(&namespace.directory, &main_bin_dir) {
                crate::errln!(
                    context.output,
                    "Error: Failed to create bin links for namespace {}: {error:#}",
                    namespace_name
                );
                exit_code = 1;
            }
        }
        aggregate_exit_code = aggregate_exit_code.saturating_add(exit_code.clamp(0, 255));
        aggregate_exit_code = aggregate_exit_code.min(255);
    }

    Ok(aggregate_exit_code)
}

fn build_native_request(
    namespace: &NativeNamespace,
    command: NativeBinCommand,
    parent: &Riff,
) -> anyhow::Result<ProjectInstallRequest> {
    let manifest_path = namespace.directory.join("composer.json");
    let manifest: RiffManifest = serde_json::from_slice(
        &std::fs::read(&manifest_path)
            .with_context(|| format!("Failed to read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;
    let lock_path = namespace.directory.join("composer.lock");
    let lockfile: Option<RiffLockfile> = if lock_path.is_file() {
        Some(
            serde_json::from_slice(
                &std::fs::read(&lock_path)
                    .with_context(|| format!("Failed to read {}", lock_path.display()))?,
            )
            .with_context(|| format!("Failed to parse {}", lock_path.display()))?,
        )
    } else {
        None
    };
    let config = crate::config::Config::build(Some(&namespace.directory), true)?;
    let output = Output::from_sink(namespace.output.clone()).with_options(parent.output.options());
    let child = parent
        .session
        .project(namespace.directory.clone())
        .with_config(config)
        .with_manifest(manifest)
        .with_lockfile(lockfile.clone())
        .with_platform(parent.platform.clone())
        .with_runtime(parent.runtime.clone())
        .with_policy_environment(parent.policy_environment.clone())
        .plugins_enabled(parent.plugins_enabled)
        .audit_enabled(parent.audit_enabled)
        .with_output(output)
        .build()?;
    let request = match command.operation {
        NativeOperation::Install if lockfile.is_some() => {
            ProjectInstallRequest::install(child, InstallOptions::default())
        }
        NativeOperation::Install | NativeOperation::Update => {
            ProjectInstallRequest::update(child, UpdateOptions::default())
        }
    };
    Ok(if command.no_audit {
        request
    } else {
        request.with_audit(ProjectAuditOptions { no_dev: false })
    })
}

fn replay_buffer(output: &Output, buffer: &BufferedOutput) {
    for event in buffer.events() {
        if event.newline {
            output.emit(event.level, event.stream, format_args!("{}", event.message));
        } else {
            output.write(event.level, event.stream, format_args!("{}", event.message));
        }
    }
}

impl ComposerBinPlugin {
    fn post_autoload_dump(
        &self,
        riff: &Riff,
        operation: DependencyOperation,
    ) -> anyhow::Result<i32> {
        let config = BinConfig::from_extra(&riff.manifest.extra);

        // Only act if forward-command is enabled
        if !config.forward_command {
            return Ok(0);
        }

        let vendor_bin_root = riff.working_dir.join(&config.target_directory);

        if !vendor_bin_root.exists() {
            return Ok(0);
        }

        // Find all namespace directories
        let mut namespaces: Vec<_> = std::fs::read_dir(&vendor_bin_root)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        namespaces.sort_by_key(std::fs::DirEntry::file_name);

        if namespaces.is_empty() {
            return Ok(0);
        }

        // Get bin directory for bin-links
        let bin_dir = riff.vendor_dir().join("bin");

        let command_name = match operation {
            DependencyOperation::Install => "install",
            DependencyOperation::Update => "update",
        };

        // Forward the parent dependency operation to all namespaces.
        for entry in namespaces {
            let namespace_dir = entry.path();
            let namespace_name = entry.file_name().to_string_lossy().to_string();

            // Ensure composer.json exists
            let namespace_composer = namespace_dir.join("composer.json");
            if !namespace_composer.exists() {
                std::fs::write(&namespace_composer, "{}")?;
            }

            let mut command = riff.runtime.riff_command();
            command.arg(command_name).arg("-d").arg(&namespace_dir);
            let process_output = ProcessRunner::new(riff.output())
                .with_timeout_seconds(riff.config.process_timeout)
                .execute(&mut command)
                .with_context(|| {
                    format!("Failed to run install in bin namespace {namespace_name}")
                })?;
            if !process_output.status.success() {
                return Ok(process_output.exit_code());
            }

            // Create bin links if enabled
            if config.bin_links {
                create_bin_links(&namespace_dir, &bin_dir)?;
            }
        }

        Ok(0)
    }
}

/// Execute a `composer bin <namespace|all> <command>` invocation from a script.
fn run_bin_command(
    command: &str,
    project_dir: &Path,
    extra_args: &[String],
    runtime: &RuntimeContext,
    output: &crate::output::Output,
    process_timeout: Option<Duration>,
) -> anyhow::Result<i32> {
    let invocation = BinInvocation::parse(command)?;
    let manifest: RiffManifest = serde_json::from_str(
        &std::fs::read_to_string(project_dir.join("composer.json"))
            .context("Failed to read composer.json for composer-bin-plugin")?,
    )
    .context("Failed to parse composer.json for composer-bin-plugin")?;
    let config = BinConfig::from_extra(&manifest.extra);
    let namespace_root = project_dir.join(&config.target_directory);
    let namespaces = namespace_directories(&namespace_root, &invocation.namespace)?;

    for namespace_dir in namespaces {
        let namespace = namespace_dir
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        crate::outln!(
            output,
            "Running composer {} in bin namespace {}",
            invocation.arguments.join(" "),
            namespace
        );

        let mut command = runtime.riff_command();
        command
            .args(&invocation.arguments)
            .args(extra_args)
            .arg("-d")
            .arg(&namespace_dir);
        let process_output = ProcessRunner::new(output)
            .with_timeout(process_timeout)
            .execute(&mut command)
            .with_context(|| {
                format!(
                    "Failed to run composer command in bin namespace {}",
                    namespace
                )
            })?;
        if !process_output.status.success() {
            return Ok(process_output.exit_code());
        }
    }

    Ok(0)
}

#[derive(Debug, PartialEq, Eq)]
struct BinInvocation {
    namespace: String,
    arguments: Vec<String>,
}

impl BinInvocation {
    fn parse(command: &str) -> anyhow::Result<Self> {
        let mut parts = command.split_whitespace();
        let Some(namespace) = parts.next() else {
            bail!("composer bin requires a namespace or 'all'");
        };
        let arguments = parts.map(str::to_owned).collect::<Vec<_>>();
        if arguments.is_empty() {
            bail!("composer bin requires a command");
        }
        Ok(Self {
            namespace: namespace.to_owned(),
            arguments,
        })
    }
}

fn namespace_directories(
    namespace_root: &Path,
    namespace: &str,
) -> anyhow::Result<Vec<std::path::PathBuf>> {
    if namespace != "all" {
        let directory = namespace_root.join(namespace);
        if !directory.join("composer.json").is_file() {
            bail!("Composer bin namespace '{namespace}' does not exist");
        }
        return Ok(vec![directory]);
    }

    if !namespace_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut directories = std::fs::read_dir(namespace_root)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("composer.json").is_file())
        .collect::<Vec<_>>();
    directories.sort();
    Ok(directories)
}

/// Create symlinks from namespace vendor/bin to main vendor/bin
fn create_bin_links(namespace_dir: &Path, main_bin_dir: &Path) -> Result<()> {
    let namespace_bin_dir = namespace_dir.join("vendor").join("bin");

    if !namespace_bin_dir.exists() {
        return Ok(());
    }

    // Ensure main bin dir exists
    std::fs::create_dir_all(main_bin_dir)?;

    // Create symlinks for each binary
    for entry in std::fs::read_dir(&namespace_bin_dir)? {
        let entry = entry?;
        let source = entry.path();
        let file_name = entry.file_name();
        let target = main_bin_dir.join(&file_name);

        // Skip if target already exists
        if target.exists() {
            continue;
        }

        // Create symlink
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&source, &target)?;
        }

        #[cfg(windows)]
        {
            // On Windows, copy instead of symlink for simplicity
            std::fs::copy(&source, &target)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{environment_lock, EnvironmentGuard};
    use crate::{ProjectAuditHook, RiffSession};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    #[cfg(unix)]
    fn executable_script(directory: &Path, contents: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join("fake-riff");
        std::fs::write(&path, contents).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    struct CountingAudit(AtomicUsize);

    #[async_trait]
    impl ProjectAuditHook for CountingAudit {
        async fn audit(
            &self,
            _session: &RiffSession,
            _working_dir: &Path,
            _no_dev: bool,
            _output: Output,
        ) -> anyhow::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn test_bin_config_default() {
        let config = BinConfig::default();
        assert!(!config.bin_links);
        assert_eq!(config.target_directory, "vendor-bin");
        assert!(!config.forward_command);
    }

    #[test]
    fn test_bin_config_from_extra() {
        let extra = serde_json::json!({
            "bamarni-bin": {
                "bin-links": true,
                "target-directory": "tools",
                "forward-command": true
            }
        });

        let config = BinConfig::from_extra(&extra);
        assert!(config.bin_links);
        assert_eq!(config.target_directory, "tools");
        assert!(config.forward_command);
    }

    #[test]
    fn test_bin_config_partial_extra() {
        let extra = serde_json::json!({
            "bamarni-bin": {
                "forward-command": true
            }
        });

        let config = BinConfig::from_extra(&extra);
        assert!(!config.bin_links); // default
        assert_eq!(config.target_directory, "vendor-bin"); // default
        assert!(config.forward_command); // overridden
    }

    #[test]
    fn parses_bin_all_update_arguments() {
        assert_eq!(
            BinInvocation::parse("all update --ansi").unwrap(),
            BinInvocation {
                namespace: "all".to_string(),
                arguments: vec!["update".to_string(), "--ansi".to_string()],
            }
        );
    }

    #[test]
    fn native_commands_accept_only_install_update_and_safe_presentation_flags() {
        assert_eq!(
            NativeBinCommand::parse(
                &["install".to_string(), "--ansi".to_string()],
                &["--no-progress".to_string(), "--no-audit".to_string()],
            ),
            Some(NativeBinCommand {
                operation: NativeOperation::Install,
                no_audit: true,
            })
        );
        assert_eq!(
            NativeBinCommand::parse(&["update".to_string(), "-n".to_string()], &[]),
            Some(NativeBinCommand {
                operation: NativeOperation::Update,
                no_audit: false,
            })
        );
        assert_eq!(
            NativeBinCommand::parse(&["install".to_string(), "--no-scripts".to_string()], &[]),
            None
        );
        assert_eq!(NativeBinCommand::parse(&["show".to_string()], &[]), None);
    }

    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn shopware_style_bin_all_uses_one_session_for_every_namespace() {
        let _environment = environment_lock();
        let root = TempDir::new().unwrap();
        let composer_home = root.path().join("composer-home");
        std::fs::create_dir(&composer_home).unwrap();
        let composer_home = composer_home.to_string_lossy().into_owned();
        let _composer_home = EnvironmentGuard::set("COMPOSER_HOME", Some(&composer_home));
        let namespace_root = root.path().join("vendor-bin");
        for namespace in ["rector", "phpstan"] {
            let directory = namespace_root.join(namespace);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("composer.json"),
                r#"{"repositories":[{"packagist.org":false}]}"#,
            )
            .unwrap();
        }

        let audit = Arc::new(CountingAudit(AtomicUsize::new(0)));
        let session = RiffSession::builder()
            .with_cache_dir(root.path().join("cache"))
            .with_project_audit_hook(audit.clone())
            .build()
            .unwrap();
        let manifest: RiffManifest = serde_json::from_value(serde_json::json!({
            "repositories": [{"packagist.org": false}],
            "extra": {"bamarni-bin": {"target-directory": "vendor-bin"}}
        }))
        .unwrap();
        let output = Arc::new(BufferedOutput::default());
        let riff = session
            .project(root.path())
            .with_manifest(manifest)
            .with_platform(crate::Platform::empty())
            .with_output(Output::from_sink(output.clone()))
            .plugins_enabled(false)
            .build()
            .unwrap();

        let exit_code = ComposerBinPlugin
            .execute_async(
                "all install --ansi",
                &[],
                &ScriptPluginContext {
                    manifest: &riff.manifest,
                    working_dir: &riff.working_dir,
                    runtime: &riff.runtime,
                    output: riff.output(),
                    process_timeout: None,
                    riff: Some(&riff),
                },
            )
            .await
            .unwrap();

        assert_eq!(exit_code, 0);
        assert_eq!(audit.0.load(Ordering::SeqCst), 2);
        assert!(namespace_root.join("phpstan/composer.lock").is_file());
        assert!(namespace_root.join("rector/composer.lock").is_file());
        let headers = output
            .events()
            .into_iter()
            .filter(|event| event.message.starts_with("Running composer"))
            .map(|event| event.message)
            .collect::<Vec<_>>();
        assert_eq!(
            headers,
            [
                "Running composer install --ansi in bin namespace phpstan",
                "Running composer install --ansi in bin namespace rector",
            ]
        );
    }

    #[test]
    fn all_selects_composer_namespaces_in_stable_order() {
        let root = TempDir::new().unwrap();
        for namespace in ["rector", "phpstan"] {
            let directory = root.path().join(namespace);
            std::fs::create_dir(&directory).unwrap();
            std::fs::write(directory.join("composer.json"), "{}").unwrap();
        }
        std::fs::create_dir(root.path().join("not-a-namespace")).unwrap();

        assert_eq!(
            namespace_directories(root.path(), "all").unwrap(),
            [root.path().join("phpstan"), root.path().join("rector")]
        );
    }

    #[test]
    fn bin_all_reads_the_root_composer_manifest() {
        let root = TempDir::new().unwrap();
        std::fs::write(
            root.path().join("composer.json"),
            r#"{"extra":{"bamarni-bin":{"target-directory":"vendor-bin"}}}"#,
        )
        .unwrap();

        assert_eq!(
            run_bin_command(
                "all install --ansi",
                root.path(),
                &[],
                &RuntimeContext::default(),
                &crate::output::Output::silent(),
                None,
            )
            .unwrap(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn forwarded_install_uses_the_configured_runtime_and_propagates_failure() {
        let root = TempDir::new().unwrap();
        let namespace = root.path().join("vendor-bin/phpstan");
        std::fs::create_dir_all(&namespace).unwrap();
        std::fs::write(namespace.join("composer.json"), "{}").unwrap();
        let riff_binary = executable_script(root.path(), "#!/bin/sh\nexit 23\n");
        let runtime = RuntimeContext::new(PathBuf::from("custom-php"), riff_binary);
        let manifest: RiffManifest = serde_json::from_value(serde_json::json!({
            "extra": {"bamarni-bin": {"forward-command": true}}
        }))
        .unwrap();

        let riff = Riff::builder(root.path().to_path_buf())
            .with_manifest(manifest)
            .with_platform(crate::Platform::empty())
            .with_runtime(runtime)
            .with_output(Output::silent())
            .build()
            .unwrap();
        let exit_code = ComposerBinPlugin
            .post_autoload_dump(&riff, DependencyOperation::Install)
            .unwrap();

        assert_eq!(exit_code, 23);
    }
}
