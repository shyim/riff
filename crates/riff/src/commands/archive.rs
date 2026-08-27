use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use riff_core::archive::create_package_archive;
use riff_core::config::Config;
use riff_core::json::RiffManifest;
use riff_core::package::load_package_config;
use riff_core::repository::InstalledPackage;
use riff_core::scripts::{run_event_script, ScriptExecutionOptions};
use riff_core::Package;

use crate::CommandContext;

#[derive(Debug, usage_rs::Args)]
pub struct ArchiveArgs {
    /// Package to archive instead of the current project
    #[usage(complete = crate::commands::completion::complete_available_package)]
    pub package: Option<String>,

    /// Version of the package to archive
    pub version: Option<String>,

    /// Resulting archive format: tar or zip
    #[usage(
        short = 'f',
        long,
        complete = crate::commands::completion::complete_archive_format
    )]
    pub format: Option<String>,

    /// Directory receiving the archive
    #[usage(long)]
    pub dir: Option<PathBuf>,

    /// Archive file name without the format extension
    #[usage(long)]
    pub file: Option<String>,

    /// Ignore archive.exclude and export-ignore filters
    #[usage(long)]
    pub ignore_filters: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
struct ArchiveSettings {
    format: String,
    directory: PathBuf,
}

impl ArchiveSettings {
    fn resolve(args: &ArchiveArgs, config: &Config, working_dir: &Path) -> Self {
        let directory = args
            .dir
            .clone()
            .unwrap_or_else(|| config.archive_dir.clone());
        let directory = if directory.is_absolute() {
            directory
        } else {
            working_dir.join(directory)
        };
        Self {
            format: args
                .format
                .clone()
                .unwrap_or_else(|| config.archive_format.clone()),
            directory,
        }
    }
}

pub fn execute(args: ArchiveArgs, context: &CommandContext) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let manifest_path = working_dir.join("composer.json");
    let manifest_value: serde_json::Value = serde_json::from_slice(
        &fs::read(&manifest_path)
            .with_context(|| format!("Failed to read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;
    let manifest: RiffManifest = serde_json::from_value(manifest_value.clone())?;
    let config = Config::build(Some(&working_dir), true)?;
    let plugins = riff_core::plugin::PluginManager::builtins(true, config.allow_plugins.clone())?;
    let settings = ArchiveSettings::resolve(&args, &config, &working_dir);

    let code = run_event_script(
        "pre-archive-cmd",
        &manifest,
        &working_dir,
        false,
        ScriptExecutionOptions {
            runtime: context.runtime(),
            dev_mode: true,
            plugins: &plugins,
            bin_dir: config.get_bin_dir(),
            output: context.output(),
        },
    )?;
    if code != 0 {
        return Ok(code);
    }

    let (package, source) = if let Some(name) = args.package.as_deref() {
        load_installed_package(&config.get_vendor_dir(), name, args.version.as_deref())?
    } else {
        (load_root_package(manifest_value)?, working_dir.clone())
    };

    riff_core::errln!(
        context.output(),
        "Creating the archive into \"{}\".",
        settings.directory.display()
    );
    let archive = create_package_archive(
        &package,
        &source,
        &settings.directory,
        &settings.format,
        args.file.as_deref(),
        args.ignore_filters,
    )?;
    riff_core::outln!(context.output(), "Created: {}", archive.display());

    let code = run_event_script(
        "post-archive-cmd",
        &manifest,
        &working_dir,
        false,
        ScriptExecutionOptions {
            runtime: context.runtime(),
            dev_mode: true,
            plugins: &plugins,
            bin_dir: config.get_bin_dir(),
            output: context.output(),
        },
    )?;
    Ok(code)
}

fn load_root_package(mut value: serde_json::Value) -> Result<Package> {
    let object = value
        .as_object_mut()
        .context("composer.json must contain a JSON object")?;
    object
        .entry("version")
        .or_insert_with(|| serde_json::Value::String("dev-main".to_owned()));
    load_package_config(&value).map_err(anyhow::Error::msg)
}

fn load_installed_package(
    vendor_dir: &Path,
    requested_name: &str,
    requested_version: Option<&str>,
) -> Result<(Package, PathBuf)> {
    let installed_path = vendor_dir.join("composer/installed.json");
    let value: serde_json::Value = serde_json::from_slice(
        &fs::read(&installed_path)
            .with_context(|| format!("Failed to read {}", installed_path.display()))?,
    )
    .with_context(|| format!("Failed to parse {}", installed_path.display()))?;
    let packages_value = if value.is_array() {
        value
    } else {
        value
            .get("packages")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()))
    };
    let packages: Vec<InstalledPackage> = serde_json::from_value(packages_value)?;
    let requested_name = requested_name.to_lowercase();
    let Some(installed) = packages.into_iter().find(|package| {
        package.name.eq_ignore_ascii_case(&requested_name)
            && requested_version.is_none_or(|version| package.version == version)
    }) else {
        bail!("Could not find a package matching {requested_name}.");
    };
    let install_path = installed
        .install_path
        .as_deref()
        .context("Installed package has no install-path")?;
    let package = Package::from_installed_json(&installed);
    let source = normalize_path(&vendor_dir.join("composer").join(install_path));
    if !source.is_dir() {
        bail!("Installed package path {} does not exist", source.display());
    }
    Ok((package, source))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from Composer\Test\Command\ArchiveCommandTest::
    // testUsesConfigFromFactoryWhenComposerIsNotDefined.
    #[test]
    fn composer_archive_command_uses_factory_defaults() {
        let working_dir = Path::new("/work/project");
        let args = ArchiveArgs {
            package: None,
            version: None,
            format: None,
            dir: None,
            file: None,
            ignore_filters: false,
            working_dir: working_dir.to_owned(),
        };
        let settings = ArchiveSettings::resolve(&args, &Config::default(), working_dir);
        assert_eq!(settings.format, "tar");
        assert_eq!(settings.directory, working_dir);
    }
}
