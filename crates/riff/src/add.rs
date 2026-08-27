//! Add command - add and install a package.

use anyhow::{bail, Context, Result};
use riff_core::output::style;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use crate::CommandContext;
use riff_core::{
    config::Config,
    installer::{Installer, UpdateOptions},
    json::{RiffLockfile, RiffManifest},
    package::Package,
    policy_config::PolicyEnvironment,
    solver::{package_matches_platform_requirements, recommended_require_constraint},
    Riff, RiffBuilder,
};
use riff_semver::{Semver, VersionParser};

#[derive(usage_rs::Args, Debug)]
pub struct AddArgs {
    /// Packages to require (e.g., vendor/package:^1.0)
    #[usage(
        value_name = "PACKAGES",
        required,
        complete = crate::commands::completion::complete_available_package
    )]
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

    /// Skip autoloader generation
    #[usage(long)]
    pub no_autoloader: bool,

    /// Skip script execution
    #[usage(long)]
    pub no_scripts: bool,

    /// Disable all plugins
    #[usage(long)]
    pub no_plugins: bool,

    /// Deprecated alias of --no-blocking
    #[usage(long)]
    pub no_security_blocking: bool,

    /// Disable all dependency policy blocking
    #[usage(long)]
    pub no_blocking: bool,

    /// Do not run update after adding
    #[usage(long)]
    pub no_update: bool,

    /// Update the lock file but do not install packages
    #[usage(long)]
    pub no_install: bool,

    /// Record the selected package's exact version
    #[usage(long)]
    pub fixed: bool,

    /// Do not ask any interactive question
    #[usage(short = 'n', long)]
    pub no_interaction: bool,

    /// Increase verbosity (-v, -vv, -vvv)
    #[usage(short = 'v', long, count)]
    pub verbose: u8,

    /// Skip the audit step after update
    #[usage(long)]
    pub no_audit: bool,

    /// Optimize autoloader
    #[usage(short = 'o', long)]
    pub optimize_autoloader: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub async fn execute(args: AddArgs, context: &CommandContext) -> Result<i32> {
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

    validate_package_arguments(&args.packages)?;
    let manifest: RiffManifest = if let Some(content) = &original_json {
        serde_json::from_str(content)?
    } else {
        riff_core::outln!(
            context.output(),
            "{} No composer.json found. Creating one.",
            style("Info:").cyan()
        );
        RiffManifest::default()
    };

    // Load composer.lock
    let lock_path = working_dir.join("composer.lock");
    let original_lock = if lock_path.exists() {
        Some(std::fs::read_to_string(&lock_path).context("Failed to read composer.lock")?)
    } else {
        None
    };
    let lock: Option<RiffLockfile> = original_lock
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .context("Failed to parse composer.lock")?;

    // Load config
    let config = Config::build(Some(&working_dir), true)?;

    // Create Riff using builder
    let mut builder = RiffBuilder::new(working_dir.clone())
        .with_config(config)
        .with_manifest(manifest)
        .with_lockfile(lock)
        .with_platform(context.platform().clone())
        .with_runtime(context.runtime().clone())
        .with_output(context.output().clone())
        .with_policy_environment(PolicyEnvironment::from_process())
        .plugins_enabled(!args.no_plugins)
        .dry_run(args.dry_run);

    builder = crate::install::apply_install_preference(
        builder,
        args.prefer_source,
        args.prefer_dist,
        args.prefer_install.as_deref(),
    )?;

    let mut riff = builder.build()?;
    let plugins = riff.plugins().clone();
    let package_arguments = plugins
        .transform_package_arguments(
            &riff,
            riff_core::plugin::PackageOperation::Require,
            &args.packages,
        )
        .await?;

    riff_core::outln!(
        context.output(),
        "{} Adding packages",
        style("Riff").green().bold()
    );
    if args.dry_run {
        riff_core::outln!(
            context.output(),
            "{} Running in dry-run mode",
            style("Info:").cyan()
        );
    }

    let requested_packages = VersionParser::new().parse_name_version_pairs(&package_arguments);
    let mut resolved_packages = Vec::new();
    for (name, constraint) in requested_packages {
        let resolved =
            resolve_package_pair(&riff, name, constraint, args.fixed, args.verbose).await?;

        if let Some(warning) =
            move_inconsistent_requirement(&mut riff.manifest, &resolved.name, args.dev)
        {
            riff_core::errln!(context.output(), "Warning: {warning}");
            if !args.no_interaction
                && !confirm(
                    "Do you want to move this requirement",
                    true,
                    context.output(),
                )?
            {
                riff_core::errln!(
                    context.output(),
                    "Installation failed, reverting composer.json to its original content."
                );
                return Ok(1);
            }
        }

        if resolved.selected_version.is_some() {
            riff_core::outln!(
                context.output(),
                "Using version {} for {}",
                resolved.constraint,
                resolved.name
            );
        }

        if resolved.feature_branch {
            riff_core::errln!(context.output(),
                "Warning: Version {} looks like it may be a feature branch which is unlikely to keep working in the long run and may be in an unstable state",
                resolved.selected_version.as_deref().unwrap_or(&resolved.constraint)
            );
            if !args.no_interaction
                && !confirm(
                    "Are you sure you want to use this constraint or would you rather abort the whole operation",
                    true,
                    context.output(),
                )?
            {
                riff_core::errln!(context.output(), "Installation failed, reverting composer.json to its original content.");
                return Ok(1);
            }
        }

        riff_core::outln!(
            context.output(),
            "  {} {} {}",
            style("+").green(),
            style(&resolved.name).white().bold(),
            style(&resolved.constraint).yellow()
        );

        if args.dev {
            riff.manifest
                .require_dev
                .insert(resolved.name.clone(), resolved.constraint.clone());
        } else {
            riff.manifest
                .require
                .insert(resolved.name.clone(), resolved.constraint.clone());
        }
        resolved_packages.push((resolved.name, resolved.constraint));
    }

    for message in plugins
        .transform_root_manifest(&mut riff, riff_core::plugin::PackageOperation::Require)
        .await?
    {
        riff_core::outln!(context.output(), "  - {message}");
    }

    // Write updated composer.json
    if !args.dry_run {
        riff_core::json::write_json_value(&json_path, &riff.manifest, true)
            .context("Failed to write composer.json")?;
    } else {
        riff_core::outln!(
            context.output(),
            "{} composer.json would be updated",
            style("Info:").cyan()
        );
    }

    // Run update
    if !args.no_update {
        let new_packages = resolved_packages
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        let update_packages = riff.lockfile.is_some().then_some(new_packages);
        let installer = Installer::new(riff);

        let result = installer
            .update(UpdateOptions {
                optimize_autoloader: args.optimize_autoloader,
                update_packages,
                no_autoloader: args.no_autoloader,
                no_scripts: args.no_scripts,
                no_install: args.no_install,
                no_security_blocking: args.no_security_blocking,
                no_blocking: args.no_blocking,
                ..Default::default()
            })
            .await;

        if !args.dry_run && !matches!(&result, Ok(0)) {
            restore_project_file(&json_path, original_json.as_deref())?;
            restore_project_file(&lock_path, original_lock.as_deref())?;
        }

        result
    } else if args.dry_run {
        riff_core::outln!(
            context.output(),
            "{} Update skipped; no files were changed",
            style("Info:").cyan()
        );
        Ok(0)
    } else {
        riff_core::successln!(
            context.output(),
            "{} Packages added to composer.json",
            style("Success:").green().bold()
        );
        Ok(0)
    }
}

async fn resolve_package_pair(
    riff: &Riff,
    name: String,
    constraint: Option<String>,
    fixed: bool,
    verbose: u8,
) -> Result<ResolvedPackage> {
    if name.is_empty() {
        bail!("Package name cannot be empty");
    }
    if let Some(constraint) = constraint {
        if constraint.is_empty() {
            bail!("Version constraint for {} cannot be empty", name);
        }
        return Ok(ResolvedPackage {
            name,
            constraint,
            selected_version: None,
            feature_branch: false,
        });
    }

    let candidates = riff.repository_manager.find_packages(&name).await;
    if candidates.is_empty() {
        bail!("Could not find a matching version of package {name}");
    }
    let root_constraints = root_constraints_for_requested_package(riff, &name).await;
    let compatible_with_root = candidates
        .iter()
        .filter(|package| package_matches_root_requirements(package, riff, &root_constraints))
        .cloned()
        .collect::<Vec<_>>();
    let selectable = compatible_with_root.as_slice();
    emit_platform_selection_warnings(selectable, &riff.platform_packages, verbose, riff.output());
    let package = select_recommended_package(selectable, &riff.platform_packages)
        .ok_or_else(|| incompatible_platform_error(&name, selectable, &riff.platform_packages))?;
    let php_version = riff
        .platform_packages
        .iter()
        .find(|package| package.name == "php")
        .map(|package| package.version.as_str());
    let selected_version = package.pretty_version().to_owned();
    let constraint = if fixed {
        selected_version.clone()
    } else {
        recommended_require_constraint(package, php_version)
    };
    Ok(ResolvedPackage {
        name,
        feature_branch: looks_like_feature_branch(&selected_version),
        constraint,
        selected_version: Some(selected_version),
    })
}

#[derive(Debug, PartialEq, Eq)]
struct ResolvedPackage {
    name: String,
    constraint: String,
    selected_version: Option<String>,
    feature_branch: bool,
}

fn validate_package_arguments(packages: &[String]) -> Result<()> {
    if packages.iter().any(|package| package == "as") {
        bail!(
            "Cannot use \"as\" as a separate argument. Quote the inline alias as one argument, e.g. \"vendor/package:dev-main as 1.2.x-dev\"."
        );
    }
    Ok(())
}

fn move_inconsistent_requirement(
    manifest: &mut RiffManifest,
    name: &str,
    dev: bool,
) -> Option<String> {
    let moved = if dev {
        manifest.require.shift_remove(name)
    } else {
        manifest.require_dev.shift_remove(name)
    }?;
    if dev {
        manifest.require_dev.insert(name.to_owned(), moved);
        Some(format!(
            "{name} is currently present in the require key and you ran the command with the --dev flag, which will move it to the require-dev key."
        ))
    } else {
        manifest.require.insert(name.to_owned(), moved);
        Some(format!(
            "{name} is currently present in the require-dev key and you ran the command without the --dev flag, which will move it to the require key."
        ))
    }
}

async fn root_constraints_for_requested_package(riff: &Riff, name: &str) -> Vec<String> {
    let mut constraints = Vec::new();
    for (root_name, root_constraint) in riff
        .manifest
        .require
        .iter()
        .chain(riff.manifest.require_dev.iter())
    {
        if root_name.eq_ignore_ascii_case(name) {
            continue;
        }
        let root_candidates = riff.repository_manager.find_packages(root_name).await;
        let root_candidates = root_candidates
            .into_iter()
            .filter(|package| version_satisfies(&package.version, root_constraint))
            .collect::<Vec<_>>();
        if let Some(package) = select_recommended_package(&root_candidates, &riff.platform_packages)
        {
            if let Some(constraint) = package.require.get(name) {
                constraints.push(constraint.to_string());
            }
        }
    }
    constraints
}

fn package_matches_root_requirements(
    package: &Package,
    riff: &Riff,
    incoming_constraints: &[String],
) -> bool {
    if !incoming_constraints
        .iter()
        .all(|constraint| version_satisfies(&package.version, constraint))
    {
        return false;
    }
    package.require.iter().all(|(dependency, constraint)| {
        let root_constraint = riff
            .manifest
            .require
            .get(dependency.as_str())
            .or_else(|| riff.manifest.require_dev.get(dependency.as_str()));
        root_constraint
            .is_none_or(|root_constraint| constraints_intersect(constraint, root_constraint))
    })
}

fn version_satisfies(version: &str, constraint: &str) -> bool {
    Semver::satisfies(version, constraint)
}

fn constraints_intersect(left: &str, right: &str) -> bool {
    fn overlaps(
        left: &(riff_semver::Bound, riff_semver::Bound),
        right: &(riff_semver::Bound, riff_semver::Bound),
    ) -> bool {
        fn lower_after_upper(lower: &riff_semver::Bound, upper: &riff_semver::Bound) -> bool {
            riff_semver::Comparator::greater_than(lower.version(), upper.version())
                || (riff_semver::Comparator::equal_to(lower.version(), upper.version())
                    && !(lower.is_inclusive() && upper.is_inclusive()))
        }
        !lower_after_upper(&left.0, &right.1) && !lower_after_upper(&right.0, &left.1)
    }

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

    let parser = VersionParser::new();
    let (Ok(left), Ok(right)) = (
        parser.parse_constraints(left),
        parser.parse_constraints(right),
    ) else {
        return false;
    };
    let left = ranges(left.as_ref());
    let right = ranges(right.as_ref());
    left.iter()
        .any(|left| right.iter().any(|right| overlaps(left, right)))
}

fn emit_platform_selection_warnings(
    packages: &[Arc<Package>],
    platform_packages: &[Package],
    verbose: u8,
    output: &riff_core::Output,
) {
    let ordered = packages_by_descending_version(packages);
    let mut warned = 0;
    for package in ordered {
        let Some(issue) = first_platform_issue(package, platform_packages) else {
            continue;
        };
        let latest = warned == 0;
        let version_label = if latest {
            format!(
                "{}'s latest version {}",
                package.name,
                package.pretty_version()
            )
        } else {
            format!("{} {}", package.name, package.pretty_version())
        };
        riff_core::errln!(
            output,
            "Warning: Cannot use {version_label} as it {}.",
            issue.warning_description()
        );
        warned += 1;
        if verbose == 0 {
            break;
        }
    }
}

fn incompatible_platform_error(
    name: &str,
    packages: &[Arc<Package>],
    platform_packages: &[Package],
) -> anyhow::Error {
    let details = packages_by_descending_version(packages)
        .into_iter()
        .filter_map(|package| {
            first_platform_issue(package, platform_packages).map(|issue| {
                format!(
                    "  - {} {} {}.",
                    package.name,
                    package.pretty_version(),
                    issue.error_description()
                )
            })
        })
        .collect::<Vec<_>>();
    if details.is_empty() {
        anyhow::anyhow!("Could not find a matching version of package {name}")
    } else {
        anyhow::anyhow!(
            "Package {name} has requirements incompatible with your PHP version, PHP extensions and Composer version:\n{}",
            details.join("\n")
        )
    }
}

fn packages_by_descending_version(packages: &[Arc<Package>]) -> Vec<&Package> {
    let versions = packages
        .iter()
        .map(|package| package.version.as_str())
        .collect::<Vec<_>>();
    Semver::rsort(&versions)
        .into_iter()
        .filter_map(|version| {
            packages
                .iter()
                .find(|package| package.version == version)
                .map(Arc::as_ref)
        })
        .collect()
}

#[derive(Debug)]
struct PlatformIssue<'a> {
    dependency: &'a str,
    constraint: &'a str,
    missing: bool,
}

impl PlatformIssue<'_> {
    fn warning_description(&self) -> String {
        if self.missing {
            format!(
                "requires {} {} which is missing from your platform",
                self.dependency, self.constraint
            )
        } else {
            format!(
                "requires {} {} which is not satisfied by your platform",
                self.dependency, self.constraint
            )
        }
    }

    fn error_description(&self) -> String {
        if self.missing {
            format!(
                "requires {} {} but it is not present",
                self.dependency, self.constraint
            )
        } else {
            format!(
                "requires {} {} but your platform does not satisfy it",
                self.dependency, self.constraint
            )
        }
    }
}

fn first_platform_issue<'a>(
    package: &'a Package,
    platform_packages: &[Package],
) -> Option<PlatformIssue<'a>> {
    package.require.iter().find_map(|(dependency, constraint)| {
        if !riff_core::is_platform_package(dependency) {
            return None;
        }
        let platform = platform_packages
            .iter()
            .find(|package| package.name.eq_ignore_ascii_case(dependency));
        let missing = platform.is_none();
        let satisfied = platform.is_some_and(|platform| {
            VersionParser::new()
                .parse_constraints(constraint)
                .is_ok_and(|constraint| {
                    let normalized = VersionParser::new()
                        .normalize(&platform.version)
                        .unwrap_or_else(|_| platform.version.to_string());
                    constraint.matches_normalized_version(&normalized)
                })
        });
        (!satisfied).then_some(PlatformIssue {
            dependency,
            constraint,
            missing,
        })
    })
}

fn looks_like_feature_branch(version: &str) -> bool {
    version.starts_with("dev-")
        && !matches!(
            version,
            "dev-main" | "dev-master" | "dev-trunk" | "dev-default"
        )
}

fn confirm(question: &str, default: bool, output: &riff_core::Output) -> Result<bool> {
    let default_label = if default { "y" } else { "n" };
    output.write(
        riff_core::OutputLevel::Info,
        riff_core::OutputStream::Stderr,
        format_args!("{question} [{default_label}/n]? "),
    );
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    if answer.is_empty() {
        Ok(default)
    } else {
        Ok(matches!(answer.as_str(), "y" | "yes"))
    }
}

pub(crate) fn select_recommended_package<'a>(
    packages: &'a [std::sync::Arc<Package>],
    platform_packages: &[Package],
) -> Option<&'a Package> {
    let best_stability = packages
        .iter()
        .filter(|package| package_matches_platform_requirements(package, platform_packages))
        .map(|package| package.stability().priority())
        .min()?;
    let eligible: Vec<_> = packages
        .iter()
        .filter(|package| {
            package.stability().priority() == best_stability
                && package_matches_platform_requirements(package, platform_packages)
        })
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
    fn parses_composer_package_version_pairs() {
        let parser = VersionParser::new();
        assert_eq!(
            parser.parse_name_version_pairs(&["vendor/package:^1.2"]),
            vec![("vendor/package".to_string(), Some("^1.2".to_string()))]
        );
        assert_eq!(
            parser.parse_name_version_pairs(&["vendor/package", "^1.2"]),
            vec![("vendor/package".to_string(), Some("^1.2".to_string()))]
        );
    }

    #[test]
    fn recommends_composer_style_constraints() {
        let package = Package::new("vendor/package", "3.1.2.0");
        assert_eq!(recommended_require_constraint(&package, None), "^3.1");

        let package = Package::new("vendor/package", "0.1.3.0");
        assert_eq!(recommended_require_constraint(&package, None), "^0.1.3");

        let mut package = Package::new("vendor/package", "dev-main");
        package.pretty_version = Some("dev-main".into());
        assert_eq!(recommended_require_constraint(&package, None), "dev-main");
    }
}
