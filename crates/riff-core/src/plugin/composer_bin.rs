//! Composer bin plugin - isolate bin dependencies.
//!
//! This is a native Rust port of bamarni/composer-bin-plugin.
//! When forward-command is enabled, install/update commands are
//! automatically forwarded to all bin namespaces in vendor-bin/.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use anyhow::{bail, Context};

use crate::event::{EventListener, EventType, PostAutoloadDumpEvent, RiffEvent};
use crate::json::RiffManifest;
use crate::package::Package;
use crate::riff::Riff;
use crate::runtime::RuntimeContext;
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

impl EventListener for ComposerBinPlugin {
    fn handle(&self, event: &dyn RiffEvent, riff: &Riff) -> anyhow::Result<i32> {
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

        self.post_autoload_dump(
            &riff.vendor_dir(),
            &riff.working_dir,
            &riff.manifest,
            &e.packages,
            riff.output(),
        )?;

        Ok(0)
    }

    fn priority(&self) -> i32 {
        -10
    }
}

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
        )
    }
}

impl ComposerBinPlugin {
    fn post_autoload_dump(
        &self,
        vendor_dir: &Path,
        project_dir: &Path,
        manifest: &RiffManifest,
        _installed_packages: &[Arc<Package>],
        output: &crate::output::Output,
    ) -> Result<()> {
        let config = BinConfig::from_extra(&manifest.extra);

        // Only act if forward-command is enabled
        if !config.forward_command {
            return Ok(());
        }

        let vendor_bin_root = project_dir.join(&config.target_directory);

        if !vendor_bin_root.exists() {
            return Ok(());
        }

        // Find all namespace directories
        let namespaces: Vec<_> = std::fs::read_dir(&vendor_bin_root)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();

        if namespaces.is_empty() {
            return Ok(());
        }

        // Get bin directory for bin-links
        let bin_dir = vendor_dir.join("bin");

        // Forward install command to all namespaces
        for entry in namespaces {
            let namespace_dir = entry.path();
            let namespace_name = entry.file_name().to_string_lossy().to_string();

            // Ensure composer.json exists
            let namespace_composer = namespace_dir.join("composer.json");
            if !namespace_composer.exists() {
                std::fs::write(&namespace_composer, "{}")?;
            }

            // Run riff install in the namespace directory
            if let Ok(current_exe) = std::env::current_exe() {
                let status = Command::new(&current_exe)
                    .arg("install")
                    .arg("-d")
                    .arg(&namespace_dir)
                    .status();

                if let Err(e) = status {
                    crate::errln!(
                        output,
                        "Warning: Failed to run install in namespace {}: {}",
                        namespace_name,
                        e
                    );
                }
            }

            // Create bin links if enabled
            if config.bin_links {
                create_bin_links(&namespace_dir, &bin_dir)?;
            }
        }

        Ok(())
    }
}

/// Execute a `composer bin <namespace|all> <command>` invocation from a script.
fn run_bin_command(
    command: &str,
    project_dir: &Path,
    extra_args: &[String],
    runtime: &RuntimeContext,
    output: &crate::output::Output,
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

        let status = Command::new(&runtime.riff_binary)
            .arg("--php")
            .arg(&runtime.php_binary)
            .args(&invocation.arguments)
            .args(extra_args)
            .arg("-d")
            .arg(&namespace_dir)
            .status()
            .with_context(|| {
                format!(
                    "Failed to run composer command in bin namespace {}",
                    namespace
                )
            })?;
        if !status.success() {
            return Ok(status.code().unwrap_or(1));
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
    use tempfile::TempDir;

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
            )
            .unwrap(),
            0
        );
    }
}
