//! Install command - install project dependencies.

use anyhow::{Context, Result};
use console::style;
use std::path::PathBuf;

use composer_rs_core::{
    config::Config,
    installer::{InstallOptions, Installer, UpdateOptions},
    json::{ComposerJson, ComposerLock},
    ComposerBuilder,
};

#[derive(usage_rs::Args, Debug)]
pub struct InstallArgs {
    /// Prefer source installation (git clone)
    #[usage(long)]
    pub prefer_source: bool,

    /// Prefer dist installation (zip download)
    #[usage(long)]
    pub prefer_dist: bool,

    /// Run in dry-run mode (no actual changes)
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

    /// Optimize autoloader (convert PSR-4/PSR-0 to classmap)
    #[usage(short = 'o', long)]
    pub optimize_autoloader: bool,

    /// Use authoritative classmap (only load from classmap)
    #[usage(short = 'a', long)]
    pub classmap_authoritative: bool,

    /// Use APCu to cache found/not-found classes
    #[usage(long)]
    pub apcu_autoloader: bool,

    /// Ignore platform requirements
    #[usage(long)]
    pub ignore_platform_reqs: bool,

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

    /// Skip the audit step after installation (env: COMPOSER_NO_AUDIT)
    #[usage(long)]
    pub no_audit: bool,

    /// Audit output format (table, plain, json, or summary)
    #[usage(long, default = "summary")]
    pub audit_format: String,
}

use crate::platform::AppContext;

pub async fn execute(args: InstallArgs, context: &AppContext) -> Result<i32> {
    let skip_audit = args.no_audit || std::env::var("COMPOSER_NO_AUDIT").unwrap_or_default() == "1";

    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;

    // Load composer.json
    let json_path = working_dir.join("composer.json");
    let composer_json: ComposerJson = if json_path.exists() {
        let content = std::fs::read_to_string(&json_path)?;
        serde_json::from_str(&content)?
    } else {
        ComposerJson::default()
    };

    // Check for composer.lock
    let lock_path = working_dir.join("composer.lock");
    let (lock, run_update) = if lock_path.exists() {
        let content =
            std::fs::read_to_string(&lock_path).context("Failed to read composer.lock")?;
        (
            Some(
                serde_json::from_str::<ComposerLock>(&content)
                    .context("Failed to parse composer.lock")?,
            ),
            false,
        )
    } else {
        println!(
            "{} No composer.lock file found. Running update to generate one.",
            style("Info:").cyan()
        );
        (None, true)
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
        .no_dev(args.no_dev);

    // Apply prefer_source/prefer_dist flags
    if args.prefer_source {
        builder = builder.prefer_source(true);
    } else if args.prefer_dist {
        builder = builder.prefer_dist(true);
    }

    let composer = builder.build()?;

    // Run Installer
    let installer = Installer::new(composer);

    let result = if run_update {
        installer
            .update(UpdateOptions {
                optimize_autoloader: args.optimize_autoloader,
                classmap_authoritative: args.classmap_authoritative,
                apcu_autoloader: args.apcu_autoloader,
                no_autoloader: args.no_autoloader,
                no_scripts: args.no_scripts,
                ..Default::default()
            })
            .await
    } else {
        installer
            .install(InstallOptions {
                optimize_autoloader: args.optimize_autoloader,
                classmap_authoritative: args.classmap_authoritative,
                apcu_autoloader: args.apcu_autoloader,
                ignore_platform_reqs: args.ignore_platform_reqs,
                no_autoloader: args.no_autoloader,
                no_scripts: args.no_scripts,
            })
            .await
    };

    if matches!(result.as_ref(), Ok(&0)) && !skip_audit {
        let audit_args = crate::commands::audit::AuditArgs {
            no_dev: args.no_dev,
            format: args.audit_format.clone(),
            locked: false,
            abandoned: Some("report".to_string()),
            working_dir: working_dir.clone(),
        };

        if let Err(e) = crate::commands::audit::execute(audit_args).await {
            eprintln!("Warning: Audit failed: {}", e);
        }
    }

    result
}
