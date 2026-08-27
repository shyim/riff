use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use riff_core::config::Config;
use riff_core::installer::{
    SuggestedPackagesReporter, MODE_BY_PACKAGE, MODE_BY_SUGGESTION, MODE_LIST,
};
use riff_core::json::{RiffLockfile, RiffManifest};
use riff_core::repository::{InstalledRepository, Repository};
use riff_core::Package;

#[derive(Debug, usage_rs::Args)]
pub struct SuggestsArgs {
    /// Group output by suggesting package (default)
    #[usage(long)]
    pub by_package: bool,

    /// Group output by suggested package
    #[usage(long)]
    pub by_suggestion: bool,

    /// Show suggestions from all dependencies, including transitive ones
    #[usage(short = 'a', long)]
    pub all: bool,

    /// Show only suggested package names
    #[usage(long)]
    pub list: bool,

    /// Exclude suggestions from require-dev packages when a lock file is present
    #[usage(long)]
    pub no_dev: bool,

    /// Packages whose suggestions should be displayed
    #[usage(
        arg,
        name = "PACKAGE",
        complete = crate::commands::completion::complete_installed_package
    )]
    pub packages: Vec<String>,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub async fn execute(args: SuggestsArgs, context: &crate::CommandContext) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let manifest_path = working_dir.join("composer.json");
    let manifest: RiffManifest = serde_json::from_slice(
        &std::fs::read(&manifest_path)
            .with_context(|| format!("Failed to read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;

    let lock_path = working_dir.join("composer.lock");
    let lock = read_lock(&lock_path)?;
    let locked = lock.is_some();
    let mut dependencies = if let Some(lock) = &lock {
        let mut packages: Vec<_> = lock.packages.iter().map(Package::from).collect();
        if !args.no_dev {
            packages.extend(lock.packages_dev.iter().map(Package::from));
        }
        packages
    } else {
        let config = Config::build(Some(&working_dir), true)?;
        let repository = InstalledRepository::new(config.get_vendor_dir());
        repository.load().await.map_err(anyhow::Error::msg)?;
        repository
            .get_packages()
            .await
            .into_iter()
            .map(|package| package.as_ref().clone())
            .collect()
    };

    let root = root_package(&manifest);
    dependencies.push(root);
    let installed = dependencies.clone();

    let requested: HashSet<_> = args
        .packages
        .iter()
        .map(|package| package.to_ascii_lowercase())
        .collect();
    let direct_sources = direct_sources(&manifest, locked && args.no_dev);
    let restrict_to_direct = requested.is_empty() && !args.all;

    let mut all_reporter = SuggestedPackagesReporter::new();
    let mut selected_reporter = SuggestedPackagesReporter::new();
    for package in &dependencies {
        all_reporter.add_suggestions_from_package(package);
        let selected = if requested.is_empty() {
            !restrict_to_direct || direct_sources.contains(&package.name.to_ascii_lowercase())
        } else {
            requested.contains(&package.name.to_ascii_lowercase())
        };
        if selected {
            selected_reporter.add_suggestions_from_package(package);
        }
    }

    let mode = output_mode(&args);
    for line in selected_reporter.render(mode, Some(&installed)) {
        riff_core::outln!(context.output(), "{line}");
    }

    if restrict_to_direct && !args.list {
        let visible = selected_reporter.filtered(Some(&installed)).len();
        let total = all_reporter.filtered(Some(&installed)).len();
        if total > visible {
            riff_core::outln!(
                context.output(),
                "{} additional suggestions by transitive dependencies can be shown with --all",
                total - visible
            );
        }
    }

    Ok(0)
}

fn read_lock(path: &Path) -> Result<Option<RiffLockfile>> {
    if !path.is_file() {
        return Ok(None);
    }
    serde_json::from_slice(&std::fs::read(path)?)
        .with_context(|| format!("Failed to parse {}", path.display()))
        .map(Some)
}

fn root_package(manifest: &RiffManifest) -> Package {
    let mut package = Package::new(
        manifest.name.as_deref().unwrap_or("__root__"),
        manifest.version.as_deref().unwrap_or("dev-main"),
    );
    package.pretty_name.clone_from(&manifest.name);
    package.pretty_version = manifest.version.clone().map(Into::into);
    package.package_type = "root-package".into();
    package.require = manifest.require.clone().into();
    package.require_dev = manifest.require_dev.clone().into();
    package.conflict = manifest.conflict.clone().into();
    package.replace = manifest.replace.clone().into();
    package.provide = manifest.provide.clone().into();
    package.suggest = manifest.suggest.clone().into();
    package
}

fn direct_sources(manifest: &RiffManifest, exclude_dev: bool) -> HashSet<String> {
    let mut sources: HashSet<_> = manifest
        .require
        .keys()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    if !exclude_dev {
        sources.extend(
            manifest
                .require_dev
                .keys()
                .map(|name| name.to_ascii_lowercase()),
        );
    }
    sources.insert(
        manifest
            .name
            .as_deref()
            .unwrap_or("__root__")
            .to_ascii_lowercase(),
    );
    sources
}

fn output_mode(args: &SuggestsArgs) -> u8 {
    let mut mode = MODE_BY_PACKAGE;
    if args.by_suggestion {
        mode = MODE_BY_SUGGESTION;
    }
    if args.by_package {
        mode |= MODE_BY_PACKAGE;
    }
    if args.list {
        mode = MODE_LIST;
    }
    mode
}
