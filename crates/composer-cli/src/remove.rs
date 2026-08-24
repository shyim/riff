//! Remove command - remove a package from the project.

use anyhow::{Context, Result};
use console::style;
use std::path::PathBuf;

use crate::platform::AppContext;
use composer_rs_core::{
    config::Config,
    installer::{Installer, UpdateOptions},
    json::{ComposerJson, ComposerLock},
    ComposerBuilder,
};

#[derive(usage_rs::Args, Debug)]
pub struct RemoveArgs {
    /// Packages to remove
    #[usage(value_name = "PACKAGES", required)]
    pub packages: Vec<String>,

    /// Remove from development dependencies
    #[usage(long)]
    pub dev: bool,

    /// Run in dry-run mode
    #[usage(long)]
    pub dry_run: bool,

    /// Do not run update after removing
    #[usage(long)]
    pub no_update: bool,

    /// Skip autoloader generation
    #[usage(long)]
    pub no_autoloader: bool,

    /// Skip script execution
    #[usage(long)]
    pub no_scripts: bool,

    /// Disable all plugins
    #[usage(long)]
    pub no_plugins: bool,

    /// Optimize autoloader
    #[usage(short = 'o', long)]
    pub optimize_autoloader: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub async fn execute(args: RemoveArgs, context: &AppContext) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;

    let json_path = working_dir.join("composer.json");
    if !json_path.exists() {
        eprintln!(
            "{} No composer.json found in {}",
            style("Error:").red().bold(),
            working_dir.display()
        );
        return Ok(1);
    }

    // Load composer.json
    let original_json = std::fs::read_to_string(&json_path)?;
    let composer_json: ComposerJson = serde_json::from_str(&original_json)?;

    // Load composer.lock
    let lock_path = working_dir.join("composer.lock");
    let original_lock = if lock_path.exists() {
        Some(std::fs::read_to_string(&lock_path).context("Failed to read composer.lock")?)
    } else {
        None
    };
    let lock: Option<ComposerLock> = original_lock
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .context("Failed to parse composer.lock")?;

    // Load config
    let config = Config::build(Some(&working_dir), true)?;

    // Detect platform
    let platform_packages = context.packages(&config)?;

    // Create Composer using builder
    let mut composer = ComposerBuilder::new(working_dir.clone())
        .with_config(config)
        .with_composer_json(composer_json)
        .with_composer_lock(lock)
        .with_platform_packages(platform_packages)
        .with_runtime(context.runtime().clone())
        .plugins_enabled(!args.no_plugins)
        .dry_run(args.dry_run)
        .build()?;

    println!("{} Removing packages", style("Composer").green().bold());
    if args.dry_run {
        println!("{} Running in dry-run mode", style("Info:").cyan());
    }

    let mut removed = Vec::new();

    for name in &args.packages {
        // Try to remove from require or require-dev
        let was_in_require =
            !args.dev && composer.composer_json.require.shift_remove(name).is_some();
        let was_in_dev = composer
            .composer_json
            .require_dev
            .shift_remove(name)
            .is_some();

        if was_in_require || was_in_dev {
            println!("  {} {}", style("-").red(), style(name).white().bold());
            removed.push(name.clone());
        } else {
            println!(
                "  {} {} is not installed",
                style("!").yellow(),
                style(name).white()
            );
        }
    }

    if removed.is_empty() {
        println!("{} Nothing to remove", style("Info:").cyan());
        return Ok(0);
    }

    // Write updated composer.json
    if !args.dry_run {
        let content = serde_json::to_string_pretty(&composer.composer_json)
            .context("Failed to serialize composer.json")?;
        std::fs::write(&json_path, content).context("Failed to write composer.json")?;
    }

    // Run update
    if !args.no_update {
        let installer = Installer::new(composer);

        let result = installer
            .update(UpdateOptions {
                optimize_autoloader: args.optimize_autoloader,
                no_autoloader: args.no_autoloader,
                no_scripts: args.no_scripts,
                ..Default::default()
            })
            .await;

        if !args.dry_run && !matches!(&result, Ok(0)) {
            std::fs::write(&json_path, &original_json)
                .context("Failed to restore composer.json")?;
            if let Some(content) = original_lock {
                std::fs::write(&lock_path, content).context("Failed to restore composer.lock")?;
            } else if lock_path.exists() {
                std::fs::remove_file(&lock_path).context("Failed to remove composer.lock")?;
            }
        }

        result
    } else {
        println!(
            "{} {} packages removed from composer.json",
            style("Success:").green().bold(),
            removed.len()
        );
        Ok(0)
    }
}
