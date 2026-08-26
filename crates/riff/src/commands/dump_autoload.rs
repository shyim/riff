//! Dump-autoload command - regenerate the autoloader.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use crate::CommandContext;
use riff_core::{
    config::Config,
    installer::{DumpAutoloadOptions, Installer},
    json::{RiffLockfile, RiffManifest},
    RiffBuilder,
};

#[derive(usage_rs::Args, Debug)]
pub struct DumpAutoloadArgs {
    /// Show the autoload generation that would run without writing files or running scripts
    #[usage(long)]
    pub dry_run: bool,

    /// Optimize autoloader (convert PSR-4/PSR-0 to classmap)
    #[usage(short = 'o', long)]
    pub optimize: bool,

    /// Use authoritative classmap (only load from classmap)
    #[usage(short = 'a', long)]
    pub classmap_authoritative: bool,

    /// Use APCu to cache found/not-found classes
    #[usage(long)]
    pub apcu: bool,

    /// Skip dev dependencies
    #[usage(long)]
    pub no_dev: bool,

    /// Force development autoload rules to be included
    #[usage(long)]
    pub dev: bool,

    /// Return a failure when optimized PSR mappings contain violations
    #[usage(long)]
    pub strict_psr: bool,

    /// Skip script execution
    #[usage(long)]
    pub no_scripts: bool,

    /// Disable all plugins
    #[usage(long)]
    pub no_plugins: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub async fn execute(args: DumpAutoloadArgs, context: &CommandContext) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;

    // Load composer.json
    let json_path = working_dir.join("composer.json");
    let manifest: RiffManifest = if json_path.exists() {
        let content = std::fs::read_to_string(&json_path)?;
        serde_json::from_str(&content)?
    } else {
        RiffManifest::default() // Or handle strictly? dump-autoload usually works with at least a json or installed.
                                // If generic default, it's fine.
    };

    // Load composer.lock
    let lock_path = working_dir.join("composer.lock");
    let lock: Option<RiffLockfile> = if lock_path.exists() {
        let content =
            std::fs::read_to_string(&lock_path).context("Failed to read composer.lock")?;
        serde_json::from_str(&content).ok()
    } else {
        None
    };

    // Load config
    let config = Config::build(Some(&working_dir), true)?;
    let optimize = args.optimize || config.optimize_autoloader;
    let authoritative = args.classmap_authoritative || config.classmap_authoritative;
    validate_options(&args, optimize, authoritative)?;
    let options = DumpAutoloadOptions {
        optimize,
        authoritative,
        apcu: args.apcu || config.apcu_autoloader,
        no_dev: args.no_dev,
        strict_psr: args.strict_psr,
        no_scripts: args.no_scripts,
        dry_run: args.dry_run,
    };

    // Create Riff using builder
    let riff = RiffBuilder::new(working_dir.clone())
        .with_config(config)
        .with_manifest(manifest)
        .with_lockfile(lock)
        .with_platform(context.platform().clone())
        .with_runtime(context.runtime().clone())
        .plugins_enabled(!args.no_plugins)
        .dry_run(args.dry_run)
        .no_dev(args.no_dev)
        .build()?;

    // Run Installer
    let installer = Installer::new(riff);

    installer.dump_autoload(options)?;

    Ok(0)
}

fn validate_options(
    args: &DumpAutoloadArgs,
    optimize: bool,
    classmap_authoritative: bool,
) -> Result<()> {
    if args.dev && args.no_dev {
        bail!("You can not use both --no-dev and --dev as they conflict with each other.");
    }
    if args.strict_psr && !(optimize || classmap_authoritative) {
        bail!(
            "--strict-psr mode only works with optimized autoloader, use --optimize or --classmap-authoritative if you want a strict return value."
        );
    }
    Ok(())
}
