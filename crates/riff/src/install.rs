//! Install command - install project dependencies.

use anyhow::{Context, Result};
use riff_core::output::style;
use std::path::PathBuf;

use riff_core::{
    config::Config,
    installer::{InstallOptions, Installer, PlatformRequirementFilter, UpdateOptions},
    json::{RiffLockfile, RiffManifest},
    policy_config::PolicyEnvironment,
    RiffBuilder,
};

#[derive(usage_rs::Args, Debug)]
pub struct InstallArgs {
    /// Packages are not accepted by install; use `riff require` instead
    #[usage(arg, name = "PACKAGES")]
    pub packages: Vec<String>,

    /// Deprecated compatibility option with no effect
    #[usage(long)]
    pub dev: bool,

    /// Deprecated compatibility option with no effect
    #[usage(long)]
    pub no_suggest: bool,

    /// Invalid on install; use `riff update --no-install` instead
    #[usage(long)]
    pub no_install: bool,

    /// Prefer source installation (git clone)
    #[usage(long)]
    pub prefer_source: bool,

    /// Prefer dist installation (zip download)
    #[usage(long)]
    pub prefer_dist: bool,

    /// Installation preference: dist, source, or auto
    #[usage(
        long,
        value_name = "PREFERENCE",
        complete = crate::commands::completion::complete_prefer_install
    )]
    pub prefer_install: Option<String>,

    /// Run in dry-run mode (no actual changes)
    #[usage(long)]
    pub dry_run: bool,

    /// Download packages into Riff's cache without changing vendor
    #[usage(long)]
    pub download_only: bool,

    /// Skip dev dependencies
    #[usage(long)]
    pub no_dev: bool,

    /// Skip autoloader generation
    #[usage(long)]
    pub no_autoloader: bool,

    /// Skip script execution
    #[usage(long)]
    pub no_scripts: bool,

    /// Deprecated alias of --no-blocking
    #[usage(long)]
    pub no_security_blocking: bool,

    /// Disable all dependency policy blocking
    #[usage(long)]
    pub no_blocking: bool,

    /// Disable all plugins
    #[usage(long)]
    pub no_plugins: bool,

    /// Optimize autoloader (convert PSR-4/PSR-0 to classmap)
    #[usage(short = 'o', long)]
    pub optimize_autoloader: bool,

    /// Use authoritative classmap (only load from classmap)
    #[usage(short = 'a', long)]
    pub classmap_authoritative: bool,

    /// Use APCu to cache found/not-found classes
    #[usage(long)]
    pub apcu_autoloader: bool,

    /// Use a custom APCu cache prefix (implicitly enables APCu)
    #[usage(long, value_name = "PREFIX")]
    pub apcu_autoloader_prefix: Option<String>,

    /// Ignore platform requirements
    #[usage(long)]
    pub ignore_platform_reqs: bool,

    /// Ignore a specific platform requirement; may be repeated
    #[usage(long, value_name = "REQ")]
    pub ignore_platform_req: Vec<String>,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,

    // Common Composer flags (for compatibility)
    /// Do not ask any interactive question
    #[usage(short = 'n', long)]
    pub no_interaction: bool,

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

use crate::{env::composer_env_bool, CommandContext};

pub async fn execute(args: InstallArgs, context: &CommandContext) -> Result<i32> {
    if args.dev {
        riff_core::warnln!(context.output(),
            "You are using the deprecated option \"--dev\". It has no effect and will break in Composer 3."
        );
    }
    if args.no_suggest {
        riff_core::warnln!(context.output(),
            "You are using the deprecated option \"--no-suggest\". It has no effect and will break in Composer 3."
        );
    }
    if let Some(package) = args.packages.first() {
        riff_core::errln!(context.output(),
            "Invalid argument {package}. Use \"riff require {package}\" instead to add packages to your composer.json."
        );
        return Ok(1);
    }
    if args.no_install {
        riff_core::errln!(context.output(),
            "Invalid option \"--no-install\". Use \"riff update --no-install\" instead if you are trying to update composer.lock."
        );
        return Ok(1);
    }

    let skip_audit = args.no_audit || composer_env_bool("COMPOSER_NO_AUDIT")?;
    let no_dev = args.no_dev || composer_env_bool("COMPOSER_NO_DEV")?;
    let ignore_platform_reqs =
        args.ignore_platform_reqs || composer_env_bool("COMPOSER_IGNORE_PLATFORM_REQS")?;
    let ignore_platform_req = if args.ignore_platform_req.is_empty() && !ignore_platform_reqs {
        std::env::var("COMPOSER_IGNORE_PLATFORM_REQ")
            .ok()
            .filter(|value| !value.is_empty())
            .map(|value| value.split(',').map(str::to_string).collect())
            .unwrap_or_default()
    } else {
        args.ignore_platform_req.clone()
    };

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
        RiffManifest::default()
    };

    // Check for composer.lock
    let lock_path = working_dir.join("composer.lock");
    let (lock, run_update) = if lock_path.exists() {
        let content =
            std::fs::read_to_string(&lock_path).context("Failed to read composer.lock")?;
        (
            Some(
                serde_json::from_str::<RiffLockfile>(&content)
                    .context("Failed to parse composer.lock")?,
            ),
            false,
        )
    } else {
        riff_core::outln!(
            context.output(),
            "{} No composer.lock file found. Running update to generate one.",
            style("Info:").cyan()
        );
        (None, true)
    };

    // Load config
    let config = Config::build(Some(&working_dir), true)?;
    let configured_optimize = config.optimize_autoloader;
    let configured_authoritative = config.classmap_authoritative;
    let configured_apcu = config.apcu_autoloader;

    // A lock file already determines the complete audit package set. Fetch its
    // advisory data while dependencies are downloaded and extracted, but defer
    // evaluation and output until installation has succeeded.
    let audit_prefetch = if !skip_audit && !run_update && !args.dry_run && !args.download_only {
        let audit_lock = lock
            .clone()
            .expect("a lock file is present when install does not run update");
        Some(tokio::spawn(crate::commands::audit::prefetch_for_install(
            working_dir.clone(),
            manifest.clone(),
            config.clone(),
            audit_lock,
            no_dev,
            context.output().clone(),
        )))
    } else {
        None
    };

    // Create Riff using a session that nested vendor-bin projects can reuse.
    let session = crate::commands::audit::project_session(context)?;
    let mut builder = RiffBuilder::new(working_dir.clone())
        .with_session(session)
        .with_config(config)
        .with_manifest(manifest)
        .with_lockfile(lock)
        .with_platform(context.platform().clone())
        .with_runtime(context.runtime().clone())
        .with_output(context.output().clone())
        .with_policy_environment(PolicyEnvironment::from_process())
        .plugins_enabled(!args.no_plugins)
        .audit_enabled(!skip_audit)
        .dry_run(args.dry_run)
        .download_only(args.download_only)
        .no_dev(no_dev);

    // Apply prefer_source/prefer_dist flags
    builder = apply_install_preference(
        builder,
        args.prefer_source,
        args.prefer_dist,
        args.prefer_install.as_deref(),
    )?;

    let composer = builder.build()?;

    // Run Installer
    let installer = Installer::new(composer);

    let result = if run_update {
        installer
            .update(UpdateOptions {
                optimize_autoloader: args.optimize_autoloader || configured_optimize,
                classmap_authoritative: args.classmap_authoritative || configured_authoritative,
                apcu_autoloader: args.apcu_autoloader
                    || args.apcu_autoloader_prefix.is_some()
                    || configured_apcu,
                apcu_autoloader_prefix: args.apcu_autoloader_prefix.clone(),
                no_autoloader: args.no_autoloader,
                no_scripts: args.no_scripts,
                no_security_blocking: args.no_security_blocking,
                no_blocking: args.no_blocking,
                ignore_platform_requirements: PlatformRequirementFilter {
                    all: ignore_platform_reqs,
                    requirements: ignore_platform_req.clone(),
                },
                ..Default::default()
            })
            .await
    } else {
        installer
            .install(InstallOptions {
                optimize_autoloader: args.optimize_autoloader || configured_optimize,
                classmap_authoritative: args.classmap_authoritative || configured_authoritative,
                apcu_autoloader: args.apcu_autoloader
                    || args.apcu_autoloader_prefix.is_some()
                    || configured_apcu,
                apcu_autoloader_prefix: args.apcu_autoloader_prefix.clone(),
                ignore_platform_requirements: PlatformRequirementFilter {
                    all: ignore_platform_reqs,
                    requirements: ignore_platform_req,
                },
                no_autoloader: args.no_autoloader,
                no_scripts: args.no_scripts,
                no_security_blocking: args.no_security_blocking,
                no_blocking: args.no_blocking,
            })
            .await
    };

    if matches!(result.as_ref(), Ok(&0)) && !skip_audit && !args.dry_run && !args.download_only {
        let audit_args = crate::commands::audit::AuditArgs {
            no_dev,
            format: args.audit_format.clone(),
            locked: false,
            abandoned: Some("report".to_string()),
            ignore_severity: Vec::new(),
            ignore_unreachable: false,
            working_dir: working_dir.clone(),
        };

        let audit_result = match audit_prefetch {
            Some(audit_prefetch) => match audit_prefetch.await {
                Ok(Ok(prefetched)) => {
                    crate::commands::audit::render_prefetched_install(
                        prefetched, audit_args, context,
                    )
                    .await
                }
                Ok(Err(error)) => Err(error),
                Err(error) => Err(anyhow::Error::new(error)),
            },
            None => crate::commands::audit::execute(audit_args, context).await,
        };
        if let Err(error) = audit_result {
            riff_core::warnln!(context.output(), "Warning: Audit failed: {error}");
        }
    } else if matches!(result.as_ref(), Ok(&0)) && args.dry_run && !skip_audit {
        riff_core::outln!(
            context.output(),
            "{} Skipping audit in dry-run mode",
            style("Info:").cyan()
        );
    } else if let Some(audit_prefetch) = audit_prefetch {
        audit_prefetch.abort();
    }

    result
}

pub(crate) fn apply_install_preference(
    mut builder: RiffBuilder,
    prefer_source: bool,
    prefer_dist: bool,
    prefer_install: Option<&str>,
) -> Result<RiffBuilder> {
    if prefer_source && prefer_dist {
        anyhow::bail!("--prefer-source and --prefer-dist cannot be combined");
    }
    if prefer_install.is_some() && (prefer_source || prefer_dist) {
        anyhow::bail!("--prefer-install cannot be combined with --prefer-source or --prefer-dist");
    }
    builder = match prefer_install {
        Some("source") => builder.prefer_source(true),
        Some("dist") => builder.prefer_dist(true),
        Some("auto") => builder.prefer_auto(),
        Some(value) => anyhow::bail!(
            "unsupported --prefer-install value {value:?}; expected dist, source, or auto"
        ),
        None if prefer_source => builder.prefer_source(true),
        None if prefer_dist => builder.prefer_dist(true),
        None => builder,
    };
    Ok(builder)
}

pub(crate) async fn reconcile_after_patch(
    working_dir: PathBuf,
    context: &CommandContext,
) -> Result<i32> {
    let config = Config::build(Some(&working_dir), true)?;
    let installed_path = working_dir
        .join(&config.vendor_dir)
        .join("composer/installed.json");
    let no_dev = std::fs::read(&installed_path)
        .ok()
        .and_then(|content| serde_json::from_slice::<serde_json::Value>(&content).ok())
        .and_then(|document| document.get("dev").and_then(serde_json::Value::as_bool))
        .is_some_and(|dev| !dev);
    execute(
        InstallArgs {
            packages: Vec::new(),
            dev: false,
            no_suggest: false,
            no_install: false,
            prefer_source: false,
            prefer_dist: true,
            dry_run: false,
            download_only: false,
            no_dev,
            no_autoloader: false,
            no_scripts: false,
            no_plugins: false,
            no_security_blocking: false,
            no_blocking: false,
            optimize_autoloader: false,
            classmap_authoritative: false,
            apcu_autoloader: false,
            apcu_autoloader_prefix: None,
            ignore_platform_reqs: false,
            ignore_platform_req: Vec::new(),
            prefer_install: None,
            working_dir,
            no_interaction: true,
            verbose: 0,
            no_audit: true,
            audit_format: "summary".to_string(),
        },
        context,
    )
    .await
}
