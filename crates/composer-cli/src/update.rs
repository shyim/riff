//! Update command - update project dependencies.

use anyhow::{Context, Result};
use console::style;
use std::path::PathBuf;

use sonata_core::{
    config::Config,
    installer::{Installer, UpdateOptions},
    json::{ComposerJson, ComposerLock},
    ComposerBuilder,
};

use crate::platform::AppContext;

#[derive(usage_rs::Args, Debug)]
pub struct UpdateArgs {
    /// Packages to update (all if not specified)
    #[usage(value_name = "PACKAGES")]
    pub packages: Vec<String>,

    /// Prefer source installation
    #[usage(long)]
    pub prefer_source: bool,

    /// Prefer dist installation
    #[usage(long)]
    pub prefer_dist: bool,

    /// Run in dry-run mode
    #[usage(long)]
    pub dry_run: bool,

    /// Skip dev dependencies
    #[usage(long)]
    pub no_dev: bool,

    /// Skip autoloader generation
    #[usage(long)]
    pub no_autoloader: bool,

    /// Skip script execution
    #[usage(long)]
    pub no_scripts: bool,

    /// Disable all plugins
    #[usage(long)]
    pub no_plugins: bool,

    /// Disable progress output
    #[usage(long)]
    pub no_progress: bool,

    /// Update also dependencies of the listed packages
    #[usage(short = 'w', long)]
    pub with_dependencies: bool,

    /// Update all dependencies including root requirements
    #[usage(short = 'W', long)]
    pub with_all_dependencies: bool,

    /// Prefer stable versions
    #[usage(long)]
    pub prefer_stable: bool,

    /// Prefer lowest versions (for testing)
    #[usage(long)]
    pub prefer_lowest: bool,

    /// Only update the lock file
    #[usage(long)]
    pub lock: bool,

    /// Optimize autoloader
    #[usage(short = 'o', long)]
    pub optimize_autoloader: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,

    // Common Composer flags (for compatibility)
    /// Force ANSI output
    #[usage(long)]
    pub ansi: bool,

    /// Disable ANSI output
    #[usage(long)]
    pub no_ansi: bool,

    /// Do not ask any interactive question
    #[usage(short = 'n', long)]
    pub no_interaction: bool,

    /// Do not output any message
    #[usage(short = 'q', long)]
    pub quiet: bool,

    /// Increase verbosity (-v, -vv, -vvv)
    #[usage(short = 'v', long, count)]
    pub verbose: u8,

    /// Skip the audit step after update (env: COMPOSER_NO_AUDIT)
    #[usage(long)]
    pub no_audit: bool,

    /// Audit output format (table, plain, json, or summary)
    #[usage(long, default = "summary")]
    pub audit_format: String,
}

pub async fn execute(args: UpdateArgs, context: &AppContext) -> Result<i32> {
    let skip_audit = args.no_audit || std::env::var("COMPOSER_NO_AUDIT").unwrap_or_default() == "1";

    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;

    // Check for composer.json
    let json_path = working_dir.join("composer.json");
    if !json_path.exists() {
        eprintln!(
            "{} No composer.json found in {}",
            style("Error:").red().bold(),
            working_dir.display()
        );
        return Ok(1);
    }

    // Parse composer.json
    let json_content =
        std::fs::read_to_string(&json_path).context("Failed to read composer.json")?;
    let composer_json: ComposerJson =
        serde_json::from_str(&json_content).context("Failed to parse composer.json")?;

    // Load composer.lock if it exists (to determine what's already installed)
    let lock_path = working_dir.join("composer.lock");
    let lock = if lock_path.exists() {
        let lock_content =
            std::fs::read_to_string(&lock_path).context("Failed to read composer.lock")?;
        Some(
            serde_json::from_str::<ComposerLock>(&lock_content)
                .context("Failed to parse composer.lock")?,
        )
    } else {
        None
    };

    // Load config
    let config = Config::build(Some(&working_dir), true)?;

    // Detect platform
    let platform_packages = context.packages(&config)?;

    // Create Composer using builder
    let mut builder = ComposerBuilder::new(working_dir.clone())
        .with_config(config)
        .with_composer_json(composer_json)
        .with_composer_lock(lock)
        .with_platform_packages(platform_packages)
        .with_runtime(context.runtime().clone())
        .plugins_enabled(!args.no_plugins)
        .dry_run(args.dry_run)
        .no_dev(args.no_dev)
        .prefer_lowest(args.prefer_lowest);

    // Apply prefer_source/prefer_dist flags
    if args.prefer_source {
        builder = builder.prefer_source(true);
    } else if args.prefer_dist {
        builder = builder.prefer_dist(true);
    }

    let composer = builder.build()?;

    // Run Installer
    let installer = Installer::new(composer);

    let update_packages = if args.packages.is_empty() {
        None
    } else {
        Some(args.packages.clone())
    };

    let result = installer
        .update_with_result(UpdateOptions {
            optimize_autoloader: args.optimize_autoloader,
            update_lock_only: args.lock,
            update_packages,
            with_dependencies: args.with_dependencies || args.with_all_dependencies,
            with_all_dependencies: args.with_all_dependencies,
            no_autoloader: args.no_autoloader,
            no_scripts: args.no_scripts,
            ..Default::default()
        })
        .await;

    if matches!(result.as_ref(), Ok(result) if result.exit_code == 0) && !skip_audit {
        let audit_args = crate::commands::audit::AuditArgs {
            no_dev: args.no_dev,
            format: args.audit_format.clone(),
            locked: false,
            abandoned: Some("report".to_string()),
            working_dir: working_dir.clone(),
        };

        let update_result = result.as_ref().expect("successful update result");
        let existing_lock = args.dry_run.then(|| installer.composer_lock()).flatten();
        let existing_installed_names = args
            .dry_run
            .then(|| update_result.audit_installed_names.as_ref())
            .flatten();
        if let Err(e) = crate::commands::audit::execute_with_context(
            audit_args,
            existing_lock,
            existing_installed_names,
        )
        .await
        {
            eprintln!("Warning: Audit failed: {}", e);
        }
    }

    result.map(|result| result.exit_code)
}
