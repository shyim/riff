//! Update command - update project dependencies.

use anyhow::{Context, Result};
use riff_core::output::style;
use std::collections::HashMap;
use std::io::BufRead as _;
use std::path::PathBuf;

use riff_core::{
    config::Config,
    installer::{Installer, PlatformRequirementFilter, UpdateOptions},
    json::{RiffLockfile, RiffManifest},
    policy_config::PolicyEnvironment,
    RiffBuilder,
};

use crate::env::composer_env_bool;
use crate::CommandContext;

#[derive(usage_rs::Args, Debug)]
pub struct UpdateArgs {
    /// Packages to update (all if not specified)
    #[usage(
        value_name = "PACKAGES",
        complete = crate::commands::completion::complete_installed_package
    )]
    pub packages: Vec<String>,

    /// Prefer source installation
    #[usage(long)]
    pub prefer_source: bool,

    /// Prefer dist installation
    #[usage(long)]
    pub prefer_dist: bool,

    /// Installation preference: dist, source, or auto
    #[usage(
        long,
        value_name = "PREFERENCE",
        complete = crate::commands::completion::complete_prefer_install
    )]
    pub prefer_install: Option<String>,

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

    /// Update also dependencies of the listed packages
    #[usage(short = 'w', long)]
    pub with_dependencies: bool,

    /// Update all dependencies including root requirements
    #[usage(short = 'W', long)]
    pub with_all_dependencies: bool,

    /// Temporary root requirement constraints (vendor/package:constraint)
    #[usage(long = "with", value_name = "PACKAGE:CONSTRAINT")]
    pub with_constraints: Vec<String>,

    /// Interactively select installed packages with newer versions
    #[usage(long)]
    pub interactive: bool,

    /// Restrict updates of locked packages to their current patch series
    #[usage(long)]
    pub patch_only: bool,

    /// Deprecated alias of --no-blocking
    #[usage(long)]
    pub no_security_blocking: bool,

    /// Disable all dependency policy blocking
    #[usage(long)]
    pub no_blocking: bool,

    /// Prefer stable versions
    #[usage(long)]
    pub prefer_stable: bool,

    /// Prefer lowest versions (for testing)
    #[usage(long)]
    pub prefer_lowest: bool,

    /// Only update the lock file
    #[usage(long)]
    pub lock: bool,

    /// Update the lock file without installing packages
    #[usage(long)]
    pub no_install: bool,

    /// Prefer locked versions unless a change is necessary
    #[usage(short = 'm', long)]
    pub minimal_changes: bool,

    /// Restrict a full update to direct root requirements
    #[usage(long)]
    pub root_reqs: bool,

    /// Bump root constraints after updating; optional mode: all, dev, or no-dev
    #[usage(long)]
    pub bump_after_update: Option<Option<String>>,

    /// Optimize autoloader
    #[usage(short = 'o', long)]
    pub optimize_autoloader: bool,

    /// Use authoritative classmaps
    #[usage(short = 'a', long)]
    pub classmap_authoritative: bool,

    /// Use APCu to cache class lookups
    #[usage(long)]
    pub apcu_autoloader: bool,

    /// Use a custom APCu cache prefix (implicitly enables APCu)
    #[usage(long, value_name = "PREFIX")]
    pub apcu_autoloader_prefix: Option<String>,

    /// Ignore all platform requirements
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

    /// Skip the audit step after update (env: COMPOSER_NO_AUDIT)
    #[usage(long)]
    pub no_audit: bool,

    /// Audit output format (table, plain, json, or summary)
    #[usage(long, default = "summary")]
    pub audit_format: String,
}

pub async fn execute(args: UpdateArgs, context: &CommandContext) -> Result<i32> {
    let skip_audit = args.no_audit || composer_env_bool("COMPOSER_NO_AUDIT")?;
    let no_dev = args.no_dev || composer_env_bool("COMPOSER_NO_DEV")?;
    let prefer_stable = args.prefer_stable || composer_env_bool("COMPOSER_PREFER_STABLE")?;
    let prefer_lowest = args.prefer_lowest || composer_env_bool("COMPOSER_PREFER_LOWEST")?;
    let minimal_changes = args.minimal_changes || composer_env_bool("COMPOSER_MINIMAL_CHANGES")?;
    let with_all_dependencies =
        args.with_all_dependencies || composer_env_bool("COMPOSER_WITH_ALL_DEPENDENCIES")?;
    let with_dependencies = args.with_dependencies
        || with_all_dependencies
        || composer_env_bool("COMPOSER_WITH_DEPENDENCIES")?;
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

    // Check for composer.json
    let json_path = working_dir.join("composer.json");
    if !json_path.exists() {
        riff_core::errln!(
            context.output(),
            "{} No composer.json found in {}",
            style("Error:").red().bold(),
            working_dir.display()
        );
        return Ok(1);
    }

    // Parse composer.json
    let json_content =
        std::fs::read_to_string(&json_path).context("Failed to read composer.json")?;
    let manifest: RiffManifest =
        serde_json::from_str(&json_content).context("Failed to parse composer.json")?;
    let update_mirrors = args.lock
        || args
            .packages
            .iter()
            .any(|package| matches!(package.as_str(), "lock" | "nothing" | "mirrors"));
    let update_lock_only = update_mirrors;
    let mut constraint_specs = args.with_constraints.clone();
    let mut requested_packages = Vec::with_capacity(args.packages.len());
    for package in &args.packages {
        if matches!(package.as_str(), "lock" | "nothing" | "mirrors") {
            continue;
        }
        if let Some((name, _)) = split_package_constraint(package) {
            constraint_specs.push(package.clone());
            requested_packages.push(name.to_string());
        } else {
            requested_packages.push(package.clone());
        }
    }
    let temporary_constraints = parse_temporary_constraints(&manifest, &constraint_specs)?;

    // Load composer.lock if it exists (to determine what's already installed)
    let lock_path = working_dir.join("composer.lock");
    let lock = if lock_path.exists() {
        let lock_content =
            std::fs::read_to_string(&lock_path).context("Failed to read composer.lock")?;
        Some(
            serde_json::from_str::<RiffLockfile>(&lock_content)
                .context("Failed to parse composer.lock")?,
        )
    } else {
        None
    };

    // Load config
    let config = Config::build(Some(&working_dir), true)?;
    let configured_optimize = config.optimize_autoloader;
    let configured_authoritative = config.classmap_authoritative;
    let configured_apcu = config.apcu_autoloader;
    let bump_after_update_mode = match &args.bump_after_update {
        Some(mode) => Some(mode.as_deref().unwrap_or("all").to_string()),
        None => config.bump_after_update.clone(),
    };
    if let Some(mode) = bump_after_update_mode.as_deref() {
        validate_bump_after_update_mode(mode)?;
    }

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
        .no_dev(no_dev)
        .prefer_lowest(prefer_lowest)
        .prefer_stable(prefer_stable);

    // Apply prefer_source/prefer_dist flags
    builder = apply_install_preference(
        builder,
        args.prefer_source,
        args.prefer_dist,
        args.prefer_install.as_deref(),
    )?;

    let mut composer = builder.build()?;
    let plugins = composer.plugins().clone();

    if !args.packages.is_empty() {
        let resolved_arguments = plugins
            .transform_package_arguments(
                &composer,
                riff_core::plugin::PackageOperation::Update,
                &args.packages,
            )
            .await?;
        requested_packages = resolved_arguments
            .iter()
            .filter(|package| !matches!(package.as_str(), "lock" | "nothing" | "mirrors"))
            .map(|package| {
                split_package_constraint(package)
                    .map(|(name, _)| name.to_owned())
                    .unwrap_or_else(|| package.clone())
            })
            .collect();
    }

    let plugin_messages = plugins
        .transform_root_manifest(&mut composer, riff_core::plugin::PackageOperation::Update)
        .await?;
    if !plugin_messages.is_empty() {
        for message in &plugin_messages {
            riff_core::outln!(context.output(), "  - {message}");
        }
        if !args.dry_run {
            riff_core::json::write_json_value(&json_path, &composer.manifest, true)
                .context("Failed to write unpacked Symfony pack requirements")?;
        }
    }

    if args.interactive {
        if !requested_packages.is_empty() {
            anyhow::bail!("--interactive cannot be combined with package arguments");
        }
        requested_packages = select_interactive_packages(&composer).await?;
    }

    // Run Installer
    let installer = Installer::new(composer);

    let update_packages = if requested_packages.is_empty() {
        None
    } else {
        Some(requested_packages)
    };

    let result = installer
        .update_with_result(UpdateOptions {
            optimize_autoloader: args.optimize_autoloader || configured_optimize,
            classmap_authoritative: args.classmap_authoritative || configured_authoritative,
            apcu_autoloader: args.apcu_autoloader
                || args.apcu_autoloader_prefix.is_some()
                || configured_apcu,
            apcu_autoloader_prefix: args.apcu_autoloader_prefix.clone(),
            update_lock_only,
            update_mirrors,
            update_packages,
            with_dependencies,
            with_all_dependencies,
            no_autoloader: args.no_autoloader,
            no_scripts: args.no_scripts,
            no_install: args.no_install,
            minimal_changes,
            root_requirements_only: args.root_reqs,
            temporary_constraints,
            patch_only: args.patch_only,
            no_security_blocking: args.no_security_blocking,
            no_blocking: args.no_blocking,
            ignore_platform_requirements: PlatformRequirementFilter {
                all: ignore_platform_reqs,
                requirements: ignore_platform_req,
            },
        })
        .await;

    if !args.dry_run
        && !plugin_messages.is_empty()
        && !matches!(result.as_ref(), Ok(result) if result.exit_code == 0)
    {
        std::fs::write(&json_path, &json_content)
            .context("Failed to restore composer.json after update failure")?;
    }

    if matches!(result.as_ref(), Ok(result) if result.exit_code == 0)
        && !skip_audit
        && !args.dry_run
    {
        let audit_args = crate::commands::audit::AuditArgs {
            no_dev,
            format: args.audit_format.clone(),
            locked: false,
            abandoned: Some("report".to_string()),
            ignore_severity: Vec::new(),
            ignore_unreachable: false,
            working_dir: working_dir.clone(),
        };

        let update_result = result.as_ref().expect("successful update result");
        let existing_lock = args.dry_run.then(|| installer.lockfile()).flatten();
        let existing_installed_names = args
            .dry_run
            .then_some(update_result.audit_installed_names.as_ref())
            .flatten();
        if let Err(e) = crate::commands::audit::execute_with_context(
            audit_args,
            existing_lock,
            existing_installed_names,
            context,
        )
        .await
        {
            riff_core::warnln!(context.output(), "Warning: Audit failed: {}", e);
        }
    } else if matches!(result.as_ref(), Ok(result) if result.exit_code == 0)
        && args.dry_run
        && !skip_audit
    {
        riff_core::outln!(
            context.output(),
            "{} Skipping audit in dry-run mode",
            style("Info:").cyan()
        );
    }

    let update_result = result?;
    let mut exit_code = update_result.exit_code;
    if exit_code == 0 && !update_lock_only && !update_result.updated_package_versions.is_empty() {
        if let Some(mode) = bump_after_update_mode.as_deref() {
            riff_core::outln!(context.output(), "Bumping dependencies");
            exit_code = bump_after_update(
                &working_dir,
                mode,
                &update_result.updated_package_versions,
                &update_result.updated_package_branch_aliases,
                args.dry_run,
                context.output(),
            )?;
        }
    }
    Ok(exit_code)
}

async fn select_interactive_packages(composer: &riff_core::Riff) -> Result<Vec<String>> {
    let Some(lock) = composer.lockfile.as_ref() else {
        anyhow::bail!("Could not find any package with new versions available");
    };
    let mut installed_names = Vec::new();
    let mut update_available = false;
    for locked in lock.all_packages() {
        installed_names.push(locked.name.to_lowercase());
        let current = riff_core::Package::from(locked);
        let root_constraint = composer
            .manifest
            .require
            .iter()
            .chain(composer.manifest.require_dev.iter())
            .find(|(name, _)| name.eq_ignore_ascii_case(&locked.name))
            .map(|(_, constraint)| constraint.as_str());
        if composer
            .repository_manager
            .find_packages(&locked.name)
            .await
            .iter()
            .any(|package| {
                riff_semver::Comparator::greater_than(&package.version, &current.version)
                    && root_constraint.is_none_or(|constraint| {
                        riff_semver::Semver::satisfies(&package.version, constraint)
                    })
            })
        {
            update_available = true;
        }
    }
    installed_names.sort();
    installed_names.dedup();
    if !update_available {
        anyhow::bail!("Could not find any package with new versions available");
    }

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut selected = Vec::new();
    loop {
        composer.output().write(
            riff_core::OutputLevel::Info,
            riff_core::OutputStream::Stdout,
            format_args!("Package to update (blank to finish): "),
        );
        let mut package = String::new();
        input.read_line(&mut package)?;
        let package = package.trim();
        if package.is_empty() {
            if selected.is_empty() {
                anyhow::bail!("No package named \"\" is installed.");
            }
            break;
        }
        let Some(candidate) = installed_names
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(package))
        else {
            anyhow::bail!("No package named \"{package}\" is installed.");
        };
        if !selected.contains(candidate) {
            selected.push(candidate.clone());
        }
    }

    composer.output().write(
        riff_core::OutputLevel::Info,
        riff_core::OutputStream::Stdout,
        format_args!("Continue with the selected updates? [yes/no]: "),
    );
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        anyhow::bail!("Update aborted");
    }
    Ok(selected)
}

fn validate_bump_after_update_mode(mode: &str) -> Result<()> {
    if matches!(mode, "all" | "dev" | "no-dev") {
        Ok(())
    } else {
        anyhow::bail!("unsupported --bump-after-update mode {mode:?}; expected all, dev, or no-dev")
    }
}

fn apply_install_preference(
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

fn parse_temporary_constraints(
    manifest: &RiffManifest,
    constraints: &[String],
) -> Result<HashMap<String, String>> {
    let parser = riff_semver::VersionParser::new();
    let root_requirements: Vec<_> = manifest
        .require
        .iter()
        .chain(manifest.require_dev.iter())
        .collect();
    let mut result = HashMap::new();
    for specification in constraints {
        let (name, constraint) = split_package_constraint(specification)
            .with_context(|| format!("invalid --with constraint {specification:?}"))?;
        let name = name.trim().to_lowercase();
        let constraint = constraint.trim();
        if name.is_empty() || constraint.is_empty() {
            anyhow::bail!("invalid --with constraint {specification:?}");
        }
        let parsed = parser
            .parse_constraints(constraint)
            .with_context(|| format!("invalid --with constraint {specification:?}"))?;

        if name.contains('*') {
            for (root_name, root_constraint) in &root_requirements {
                if !wildcard_package_match(&name, root_name) {
                    continue;
                }
                let root_parsed = parser.parse_constraints(root_constraint).with_context(|| {
                    format!("invalid root constraint {root_constraint:?} for {root_name}")
                })?;
                if !constraints_intersect(parsed.as_ref(), root_parsed.as_ref()) {
                    anyhow::bail!(
                        "temporary constraint {constraint:?} for {root_name} does not intersect the root constraint {root_constraint:?}"
                    );
                }
                result.insert(root_name.to_lowercase(), constraint.to_string());
            }
        } else {
            if let Some((root_name, root_constraint)) = root_requirements
                .iter()
                .find(|(root_name, _)| root_name.eq_ignore_ascii_case(&name))
            {
                let root_parsed = parser.parse_constraints(root_constraint).with_context(|| {
                    format!("invalid root constraint {root_constraint:?} for {root_name}")
                })?;
                if !constraints_intersect(parsed.as_ref(), root_parsed.as_ref()) {
                    anyhow::bail!(
                        "temporary constraint {constraint:?} for {root_name} does not intersect the root constraint {root_constraint:?}"
                    );
                }
            }
            result.insert(name, constraint.to_string());
        }
    }
    Ok(result)
}

fn split_package_constraint(specification: &str) -> Option<(&str, &str)> {
    let (name, constraint) = specification.split_once([':', '=', ' '])?;
    (!name.trim().is_empty() && !constraint.trim().is_empty()).then_some((name, constraint))
}

fn constraints_intersect(
    left: &dyn riff_semver::ConstraintInterface,
    right: &dyn riff_semver::ConstraintInterface,
) -> bool {
    fn ranges(
        constraint: &dyn riff_semver::ConstraintInterface,
    ) -> Vec<(riff_semver::Bound, riff_semver::Bound)> {
        if let Some((constraints, false)) = constraint.as_multi_constraint() {
            return constraints
                .iter()
                .flat_map(|constraint| ranges(constraint.as_ref()))
                .collect();
        }
        vec![(constraint.lower_bound(), constraint.upper_bound())]
    }

    fn overlaps(
        left: &(riff_semver::Bound, riff_semver::Bound),
        right: &(riff_semver::Bound, riff_semver::Bound),
    ) -> bool {
        fn lower_is_after_upper(lower: &riff_semver::Bound, upper: &riff_semver::Bound) -> bool {
            riff_semver::Comparator::greater_than(lower.version(), upper.version())
                || (riff_semver::Comparator::equal_to(lower.version(), upper.version())
                    && !(lower.is_inclusive() && upper.is_inclusive()))
        }
        !lower_is_after_upper(&left.0, &right.1) && !lower_is_after_upper(&right.0, &left.1)
    }

    let right = ranges(right);
    ranges(left)
        .iter()
        .any(|left| right.iter().any(|right| overlaps(left, right)))
}

fn wildcard_package_match(pattern: &str, package: &str) -> bool {
    let (mut pattern, mut package) = (pattern.bytes().peekable(), package.bytes().peekable());
    let (mut wildcard, mut retry) = (None, None);
    while package.peek().is_some() {
        match (pattern.peek().copied(), package.peek().copied()) {
            (Some(b'*'), _) => {
                pattern.next();
                wildcard = Some(pattern.clone());
                retry = Some(package.clone());
            }
            (Some(expected), Some(actual)) if expected.eq_ignore_ascii_case(&actual) => {
                pattern.next();
                package.next();
            }
            _ if wildcard.is_some() => {
                pattern = wildcard.clone().expect("checked above");
                let mut next = retry.clone().expect("wildcard records retry position");
                next.next();
                retry = Some(next.clone());
                package = next;
            }
            _ => return false,
        }
    }
    pattern.all(|byte| byte == b'*')
}

pub(crate) fn bump_after_update(
    working_dir: &std::path::Path,
    mode: &str,
    updated_package_versions: &HashMap<String, String>,
    updated_package_branch_aliases: &HashMap<String, String>,
    dry_run: bool,
    output: &riff_core::Output,
) -> Result<i32> {
    validate_bump_after_update_mode(mode)?;
    let manifest_path = working_dir.join("composer.json");
    let lock_path = working_dir.join("composer.lock");
    let mut document: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
    let versions: HashMap<_, _> = updated_package_versions
        .iter()
        .map(|(package, version)| (package.to_lowercase(), version.as_str()))
        .collect();
    let branch_aliases: HashMap<_, _> = updated_package_branch_aliases
        .iter()
        .map(|(package, alias)| (package.to_lowercase(), alias.as_str()))
        .collect();
    let sections: &[&str] = match mode {
        "dev" => &["require-dev"],
        "no-dev" => &["require"],
        _ => &["require", "require-dev"],
    };
    let mut changes = Vec::new();
    for section in sections {
        if let Some(requirements) = document
            .get_mut(*section)
            .and_then(|value| value.as_object_mut())
        {
            for (name, constraint_value) in requirements {
                let Some(version) = versions.get(&name.to_lowercase()) else {
                    continue;
                };
                let Some(constraint) = constraint_value.as_str() else {
                    continue;
                };
                let bumped = riff_core::package::version_bumper::bump_requirement_with_branch_alias(
                    constraint,
                    version,
                    branch_aliases.get(&name.to_lowercase()).copied(),
                );
                if bumped != constraint {
                    changes.push(((*section).to_string(), name.clone(), bumped.clone()));
                    *constraint_value = serde_json::Value::String(bumped);
                }
            }
        }
    }
    if changes.is_empty() {
        riff_core::outln!(output, "{} No requirements to bump", style("Info:").cyan());
        return Ok(0);
    }
    if dry_run {
        riff_core::outln!(output, "{} would be updated with:", manifest_path.display());
        for (section, package, constraint) in &changes {
            riff_core::outln!(output, " - {section}.{package}: {constraint}");
        }
        return Ok(1);
    }
    let mut content = serde_json::to_string_pretty(&document)?;
    content.push('\n');
    std::fs::write(&manifest_path, &content)?;
    if lock_path.is_file() {
        let mut lock: RiffLockfile = serde_json::from_slice(&std::fs::read(&lock_path)?)?;
        lock.content_hash = riff_core::util::compute_content_hash(&content);
        let mut lock_content = serde_json::to_string_pretty(&lock)?;
        lock_content.push('\n');
        std::fs::write(lock_path, lock_content)?;
    }
    riff_core::successln!(
        output,
        "{} composer.json constraints bumped ({} changes)",
        style("Success:").green().bold(),
        changes.len()
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use riff_core::json::LockedPackage;

    #[test]
    fn temporary_constraints_are_validated_without_mutating_manifest() {
        let mut manifest = RiffManifest::default();
        manifest
            .require
            .insert("vendor/root".to_string(), "^1.0".to_string());
        manifest
            .require_dev
            .insert("vendor/dev-tool".to_string(), "^2.0".to_string());

        let parsed = parse_temporary_constraints(
            &manifest,
            &[
                "vendor/root:^1.2".to_string(),
                "vendor/transitive:3.*".to_string(),
                "vendor/dev-*:^2.1".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(parsed.get("vendor/root").map(String::as_str), Some("^1.2"));
        assert_eq!(
            parsed.get("vendor/transitive").map(String::as_str),
            Some("3.*")
        );
        assert_eq!(
            parsed.get("vendor/dev-tool").map(String::as_str),
            Some("^2.1")
        );
        assert_eq!(manifest.require["vendor/root"], "^1.0");

        assert!(parse_temporary_constraints(&manifest, &["vendor/root:^3.0".to_string()]).is_err());
    }

    #[test]
    fn bump_after_update_refreshes_the_lock_content_hash() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("composer.json"),
            r#"{"name":"fixture/root","require":{"vendor/package":"^1.0"}}"#,
        )
        .unwrap();
        let lock = RiffLockfile {
            packages: vec![LockedPackage {
                name: "vendor/package".to_string(),
                version: "1.2.3".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        std::fs::write(
            directory.path().join("composer.lock"),
            serde_json::to_vec(&lock).unwrap(),
        )
        .unwrap();

        bump_after_update(
            directory.path(),
            "all",
            &HashMap::from([("vendor/package".to_string(), "1.2.3".to_string())]),
            &HashMap::new(),
            false,
            &riff_core::Output::silent(),
        )
        .unwrap();

        let manifest = std::fs::read_to_string(directory.path().join("composer.json")).unwrap();
        let lock: RiffLockfile =
            serde_json::from_slice(&std::fs::read(directory.path().join("composer.lock")).unwrap())
                .unwrap();
        assert_eq!(
            lock.content_hash,
            riff_core::util::compute_content_hash(&manifest)
        );
        assert!(!manifest.contains(r#""vendor/package": "^1.0""#));
    }

    #[test]
    fn bump_after_update_dry_run_uses_projected_versions_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let original = r#"{"name":"fixture/root","require-dev":{"vendor/package":"^1.0"}}"#;
        std::fs::write(directory.path().join("composer.json"), original).unwrap();

        let exit_code = bump_after_update(
            directory.path(),
            "dev",
            &HashMap::from([("vendor/package".to_string(), "1.2.3".to_string())]),
            &HashMap::new(),
            true,
            &riff_core::Output::silent(),
        )
        .unwrap();

        assert_eq!(exit_code, 1);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("composer.json")).unwrap(),
            original
        );
        assert!(!directory.path().join("composer.lock").exists());
    }

    #[test]
    fn bump_after_update_uses_branch_aliases_for_dev_packages() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("composer.json"),
            r#"{"name":"fixture/root","require":{"vendor/package":"^3.2"}}"#,
        )
        .unwrap();

        bump_after_update(
            directory.path(),
            "all",
            &HashMap::from([("vendor/package".to_string(), "dev-main".to_string())]),
            &HashMap::from([("vendor/package".to_string(), "3.3.x-dev".to_string())]),
            false,
            &riff_core::Output::silent(),
        )
        .unwrap();

        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("composer.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["require"]["vendor/package"], "^3.3");
    }
}
