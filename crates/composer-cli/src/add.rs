//! Add command - add and install a package.

use anyhow::{Context, Result};
use console::style;
use std::path::PathBuf;

use crate::platform::AppContext;
use composer_rs_core::{
    config::Config,
    installer::{Installer, UpdateOptions},
    json::{ComposerJson, ComposerLock},
    package::{Package, Stability},
    Composer, ComposerBuilder,
};
use composer_rs_semver::{Semver, VersionParser};

#[derive(usage_rs::Args, Debug)]
pub struct AddArgs {
    /// Packages to require (e.g., vendor/package:^1.0)
    #[usage(value_name = "PACKAGES", required)]
    pub packages: Vec<String>,

    /// Add as development dependency
    #[usage(long)]
    pub dev: bool,

    /// Prefer source installation
    #[usage(long)]
    pub prefer_source: bool,

    /// Prefer dist installation
    #[usage(long)]
    pub prefer_dist: bool,

    /// Run in dry-run mode
    #[usage(long)]
    pub dry_run: bool,

    /// Skip autoloader generation
    #[usage(long)]
    pub no_autoloader: bool,

    /// Skip script execution
    #[usage(long)]
    pub no_scripts: bool,

    /// Disable all plugins
    #[usage(long)]
    pub no_plugins: bool,

    /// Do not run update after adding
    #[usage(long)]
    pub no_update: bool,

    /// Optimize autoloader
    #[usage(short = 'o', long)]
    pub optimize_autoloader: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub async fn execute(args: AddArgs, context: &AppContext) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;

    // Load composer.json
    let json_path = working_dir.join("composer.json");
    let original_json = if json_path.exists() {
        Some(std::fs::read_to_string(&json_path)?)
    } else {
        None
    };
    let composer_json: ComposerJson = if let Some(content) = &original_json {
        serde_json::from_str(content)?
    } else {
        println!(
            "{} No composer.json found. Creating one.",
            style("Info:").cyan()
        );
        ComposerJson::default()
    };

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
    let mut builder = ComposerBuilder::new(working_dir.clone())
        .with_config(config)
        .with_composer_json(composer_json)
        .with_composer_lock(lock)
        .with_platform_packages(platform_packages)
        .with_runtime(context.runtime().clone())
        .plugins_enabled(!args.no_plugins)
        .dry_run(args.dry_run);

    // Apply prefer_source/prefer_dist flags
    if args.prefer_source {
        builder = builder.prefer_source(true);
    } else if args.prefer_dist {
        builder = builder.prefer_dist(true);
    }

    let mut composer = builder.build()?;

    println!("{} Adding packages", style("Composer").green().bold());
    if args.dry_run {
        println!("{} Running in dry-run mode", style("Info:").cyan());
    }

    let mut resolved_packages = Vec::new();
    for spec in &args.packages {
        let (name, constraint) = resolve_package_spec(&composer, spec).await?;

        println!(
            "  {} {} {}",
            style("+").green(),
            style(&name).white().bold(),
            style(&constraint).yellow()
        );

        if args.dev {
            composer
                .composer_json
                .require_dev
                .insert(name.clone(), constraint.clone());
        } else {
            composer
                .composer_json
                .require
                .insert(name.clone(), constraint.clone());
        }
        resolved_packages.push((name, constraint));
    }

    // Write updated composer.json
    if !args.dry_run {
        let content = serde_json::to_string_pretty(&composer.composer_json)
            .context("Failed to serialize composer.json")?;
        std::fs::write(&json_path, content).context("Failed to write composer.json")?;
    }

    // Run update
    if !args.no_update {
        // Run Installer
        let installer = Installer::new(composer);

        let new_packages = resolved_packages
            .into_iter()
            .map(|(name, _)| name)
            .collect();

        let result = installer
            .update(UpdateOptions {
                optimize_autoloader: args.optimize_autoloader,
                update_packages: Some(new_packages),
                no_autoloader: args.no_autoloader,
                no_scripts: args.no_scripts,
                ..Default::default()
            })
            .await;

        if !args.dry_run && !matches!(&result, Ok(0)) {
            restore_project_file(&json_path, original_json.as_deref())?;
            restore_project_file(&lock_path, original_lock.as_deref())?;
        }

        result
    } else {
        println!(
            "{} Packages added to composer.json",
            style("Success:").green().bold()
        );
        Ok(0)
    }
}

/// Parse a package specification (vendor/package:^1.0 or vendor/package)
fn parse_package_spec(spec: &str) -> (String, Option<String>) {
    if let Some(pos) = spec.find(':') {
        let name = spec[..pos].to_string();
        let constraint = spec[pos + 1..].to_string();
        (name, Some(constraint))
    } else {
        (spec.to_string(), None)
    }
}

async fn resolve_package_spec(composer: &Composer, spec: &str) -> Result<(String, String)> {
    let (name, constraint) = parse_package_spec(spec);
    if name.is_empty() {
        anyhow::bail!("Package name cannot be empty");
    }
    if let Some(constraint) = constraint {
        if constraint.is_empty() {
            anyhow::bail!("Version constraint for {} cannot be empty", name);
        }
        return Ok((name, constraint));
    }

    let candidates = composer.repository_manager.find_packages(&name).await;
    let package = select_recommended_package(&candidates)
        .with_context(|| format!("Could not find a matching version of package {}", name))?;
    Ok((name, recommended_constraint(package)))
}

fn select_recommended_package(packages: &[std::sync::Arc<Package>]) -> Option<&Package> {
    let best_stability = packages
        .iter()
        .map(|package| package.stability().priority())
        .min()?;
    let eligible: Vec<_> = packages
        .iter()
        .filter(|package| package.stability().priority() == best_stability)
        .collect();
    let versions: Vec<_> = eligible
        .iter()
        .map(|package| package.version.as_str())
        .collect();
    let best_version = Semver::rsort(&versions).into_iter().next()?;
    eligible
        .into_iter()
        .find(|package| package.version == best_version)
        .map(|package| package.as_ref())
}

fn recommended_constraint(package: &Package) -> String {
    if package.is_dev() {
        return package.pretty_version().to_string();
    }

    let normalized = VersionParser::new()
        .normalize(&package.version)
        .unwrap_or_else(|_| package.version.to_string());
    let mut parts: Vec<_> = normalized.split('.').collect();
    if parts.len() != 4
        || !parts[0].chars().all(|character| character.is_ascii_digit())
        || !parts[3]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
    {
        return package.pretty_version().to_string();
    }

    if parts[0] == "0" {
        parts.truncate(3);
    } else {
        parts.truncate(2);
    }
    let mut constraint = format!("^{}", parts.join("."));
    if package.stability() != Stability::Stable {
        constraint.push('@');
        constraint.push_str(&package.stability().to_string());
    }
    constraint
}

fn restore_project_file(path: &std::path::Path, original: Option<&str>) -> Result<()> {
    if let Some(content) = original {
        std::fs::write(path, content)
            .with_context(|| format!("Failed to restore {}", path.display()))?;
    } else if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("Failed to remove {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_optional_constraint() {
        assert_eq!(
            parse_package_spec("vendor/package:^1.2"),
            ("vendor/package".to_string(), Some("^1.2".to_string()))
        );
        assert_eq!(
            parse_package_spec("vendor/package"),
            ("vendor/package".to_string(), None)
        );
    }

    #[test]
    fn recommends_composer_style_constraints() {
        let package = Package::new("vendor/package", "3.1.2.0");
        assert_eq!(recommended_constraint(&package), "^3.1");

        let package = Package::new("vendor/package", "0.1.3.0");
        assert_eq!(recommended_constraint(&package), "^0.1.3");

        let mut package = Package::new("vendor/package", "dev-main");
        package.pretty_version = Some("dev-main".into());
        package.stability = Some(Stability::Dev);
        assert_eq!(recommended_constraint(&package), "dev-main");
    }
}
