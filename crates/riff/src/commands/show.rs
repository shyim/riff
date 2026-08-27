//! Show command - display package information.

use anyhow::{Context, Result};
use riff_core::output::style;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::CommandContext;
use riff_core::{
    config::Config,
    is_platform_package,
    json::{RiffLockfile, RiffManifest},
    Repository, RepositoryManager, RiffBuilder,
};
use riff_semver::VersionParser;

#[derive(Debug, Clone, Copy, PartialEq)]
enum UpdateType {
    UpToDate,
    Patch,
    Minor,
    Major,
}

fn determine_update_type(current: &str, latest: &str) -> UpdateType {
    let parser = VersionParser::new();
    let current_normalized = parser
        .normalize(current)
        .unwrap_or_else(|_| current.to_string());
    let latest_normalized = parser
        .normalize(latest)
        .unwrap_or_else(|_| latest.to_string());

    if current_normalized == latest_normalized {
        return UpdateType::UpToDate;
    }

    let current_parts: Vec<u64> = current_normalized
        .split('.')
        .filter_map(|s| s.split('-').next())
        .filter_map(|s| s.parse().ok())
        .collect();
    let latest_parts: Vec<u64> = latest_normalized
        .split('.')
        .filter_map(|s| s.split('-').next())
        .filter_map(|s| s.parse().ok())
        .collect();

    let current_major = current_parts.first().copied().unwrap_or(0);
    let current_minor = current_parts.get(1).copied().unwrap_or(0);
    let latest_major = latest_parts.first().copied().unwrap_or(0);
    let latest_minor = latest_parts.get(1).copied().unwrap_or(0);

    let current_patch = current_parts.get(2).copied().unwrap_or(0);
    let latest_patch = latest_parts.get(2).copied().unwrap_or(0);

    if latest_major > current_major
        || (current_major == 0 && latest_minor > current_minor)
        || (current_major == 0 && current_minor == 0 && latest_patch > current_patch)
    {
        UpdateType::Major
    } else if latest_minor > current_minor || (current_major == 0 && latest_patch > current_patch) {
        UpdateType::Minor
    } else {
        UpdateType::Patch
    }
}

struct PackageWithLatest {
    package: Arc<riff_core::Package>,
    latest_version: Option<String>,
    update_type: UpdateType,
}

#[derive(usage_rs::Args, Debug)]
pub struct ShowArgs {
    /// Package to inspect (or wildcard pattern)
    #[usage(complete = crate::commands::completion::complete_show_package)]
    pub package: Option<String>,

    /// Version or version constraint to inspect
    pub version: Option<String>,

    /// List all packages
    #[usage(long)]
    pub all: bool,

    /// List all locked packages
    #[usage(long)]
    pub locked: bool,

    /// List installed packages (deprecated, installed packages are the default)
    #[usage(long)]
    pub installed: bool,

    /// List platform packages only
    #[usage(short = 'p', long)]
    pub platform: bool,

    /// List available packages only
    #[usage(short = 'a', long)]
    pub available: bool,

    /// Show the root package information
    #[usage(short = 's', long = "self")]
    pub self_package: bool,

    /// List package names only
    #[usage(short = 'N', long)]
    pub name_only: bool,

    /// Show package paths
    #[usage(short = 'P', long)]
    pub path: bool,

    /// List dependencies as a tree
    #[usage(short = 't', long)]
    pub tree: bool,

    /// Show the latest version
    #[usage(short = 'l', long)]
    pub latest: bool,

    /// Show only outdated packages
    #[usage(short = 'o', long)]
    pub outdated: bool,

    /// Show only packages directly required by root
    #[usage(short = 'D', long)]
    pub direct: bool,

    /// Output format: text or json
    #[usage(
        short = 'f',
        long,
        default = "text",
        complete = crate::commands::completion::complete_output_format
    )]
    pub format: String,

    /// Disables search in require-dev packages
    #[usage(long)]
    pub no_dev: bool,

    /// Increase diagnostics for rejected package versions
    #[usage(skip)]
    pub verbose: u8,

    /// Sort outdated packages by release age
    #[usage(skip)]
    pub sort_by_age: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,

    #[usage(skip)]
    pub strict: bool,

    #[usage(skip)]
    pub update_filter: Option<String>,

    #[usage(long)]
    pub ignore: Vec<String>,
}

pub async fn execute(args: ShowArgs, context: &CommandContext) -> Result<i32> {
    let output = context.output();
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;

    if args.format != "text" && args.format != "json" {
        riff_core::errln!(
            output,
            "Error: Unsupported format '{}'. Use 'text' or 'json'.",
            args.format
        );
        return Ok(1);
    }

    if args.direct && (args.all || args.available || args.platform) {
        riff_core::errln!(
            output,
            "Error: --direct is not usable with --all, --platform, or --available"
        );
        return Ok(1);
    }

    if args.tree && (args.all || args.available) {
        riff_core::errln!(
            output,
            "Error: --tree is not usable with --all or --available"
        );
        return Ok(1);
    }

    if args.tree && args.latest {
        riff_core::errln!(output, "Error: --tree is not usable with --latest");
        return Ok(1);
    }

    if args.tree && args.path {
        riff_core::errln!(output, "Error: --tree is not usable with --path");
        return Ok(1);
    }

    if args.outdated {
        // --outdated implies --latest
    }

    if args.installed {
        riff_core::warnln!(output, "You are using the deprecated option \"installed\".");
    }
    if !args.ignore.is_empty() && !args.outdated {
        riff_core::warnln!(output,
            "You are using the option \"ignore\" without --outdated; it only filters outdated results."
        );
    }

    let json_path = working_dir.join("composer.json");
    let manifest: RiffManifest = if json_path.exists() {
        let content = std::fs::read_to_string(&json_path)?;
        serde_json::from_str(&content)?
    } else {
        RiffManifest::default()
    };

    let lock: Option<RiffLockfile> = {
        let lock_path = working_dir.join("composer.lock");
        if lock_path.exists() {
            let content = std::fs::read_to_string(&lock_path).ok();
            content.and_then(|c| serde_json::from_str(&c).ok())
        } else {
            None
        }
    };
    if args.locked && lock.is_none() {
        riff_core::errln!(
            output,
            "Error: A valid composer.json and composer.lock is required for --locked"
        );
        return Ok(1);
    }

    let config = Config::build(Some(&working_dir), true)?;

    if args.platform {
        let mut packages = context.packages(&config)?;
        if let Some(pattern) = &args.package {
            packages.retain(|package| wildcard_matches(pattern, &package.name));
            if packages.is_empty() && !pattern.contains('*') {
                riff_core::errln!(
                    output,
                    "Error: {}",
                    package_not_found_message(pattern, &args)
                );
                return Ok(1);
            }
        }
        if args.format == "json" {
            let rows: Vec<_> = packages
                .iter()
                .map(|package| {
                    serde_json::json!({
                        "name": package.name,
                        "direct-dependency": false,
                        "homepage": null,
                        "source": null,
                        "version": package.pretty_version(),
                        "description": "Platform package provided by riff",
                        "abandoned": false,
                    })
                })
                .collect();
            riff_core::outln!(
                output,
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "platform": rows }))?
            );
        } else {
            for package in packages {
                riff_core::outln!(output, "{:<30} {}", package.name, package.pretty_version());
            }
        }
        return Ok(0);
    }

    let vendor_dir = working_dir.join(&config.vendor_dir);
    let platform_versions: HashMap<String, String> = context
        .packages(&config)?
        .into_iter()
        .map(|package| (package.name.to_lowercase(), package.version.to_string()))
        .collect();
    let installed_repo = Arc::new(riff_core::repository::InstalledRepository::new(
        vendor_dir.clone(),
    ));
    installed_repo.load().await.ok();
    let mut installed_packages = installed_repo.get_packages().await;

    if args.locked {
        let locked = lock.as_ref().expect("lock was checked above");
        installed_packages = locked
            .packages
            .iter()
            .chain(if args.no_dev {
                [].iter()
            } else {
                locked.packages_dev.iter()
            })
            .map(|package| Arc::new(riff_core::Package::from(package)))
            .collect();
    } else if args.no_dev {
        let dev_packages: HashSet<_> = lock
            .as_ref()
            .map(|lock| {
                lock.packages_dev
                    .iter()
                    .map(|package| package.name.to_lowercase())
                    .collect()
            })
            .unwrap_or_default();
        installed_packages.retain(|package| !dev_packages.contains(&package.name.to_lowercase()));
    }

    let riff = RiffBuilder::new(working_dir.clone())
        .with_config(config.clone())
        .with_manifest(manifest.clone())
        .with_lockfile(lock.clone())
        .with_platform(context.platform().clone())
        .with_runtime(context.runtime().clone())
        .with_output(output.clone())
        .build()?;
    let repository_manager = riff.repository_manager;
    let package_list_context = PackageListContext {
        repository_manager: &repository_manager,
        platform_versions: &platform_versions,
        output,
    };

    if args.all && args.package.is_none() {
        show_all_sections(
            context,
            &config,
            &manifest,
            lock.as_ref(),
            &installed_packages,
            &working_dir,
        )
        .await?;
        return Ok(0);
    }

    if args.available {
        let displayed =
            show_available_packages(&manifest, &working_dir, args.package.as_deref(), output)
                .await?;
        if displayed == 0 {
            if let Some(package) = args.package.as_deref().filter(|name| !name.contains('*')) {
                riff_core::errln!(
                    output,
                    "Error: {}",
                    package_not_found_message(package, &args)
                );
                return Ok(1);
            }
        }
        return Ok(0);
    }

    let list_self = args.self_package && (args.installed || args.locked);
    if list_self {
        if let Some(name) = &manifest.name {
            let version = manifest.version.as_deref().unwrap_or("dev-main");
            let normalized = VersionParser::new()
                .normalize(version)
                .unwrap_or_else(|_| version.to_owned());
            let mut root = riff_core::Package::new(name, normalized);
            root.pretty_version = Some(version.into());
            root.description = manifest.description.clone();
            installed_packages.push(Arc::new(root));
        }
    }

    if args.self_package && !list_self {
        if args.name_only {
            if let Some(name) = &manifest.name {
                riff_core::outln!(output, "{}", name);
            }
            return Ok(0);
        }

        if args.package.is_some() {
            riff_core::errln!(
                output,
                "Error: Cannot use --self together with a package name"
            );
            return Ok(1);
        }

        print_root_package_info(&manifest, &args.format, output)?;
        return Ok(0);
    }

    if installed_packages.is_empty()
        && (!manifest.require.is_empty() || !manifest.require_dev.is_empty())
    {
        riff_core::warnln!(
            output,
            "Warning: No dependencies installed. Try running install or update."
        );
    }

    let show_latest = args.latest || args.outdated;

    let displayed = if let Some(package_name) = &args.package {
        if !package_name.contains('*') && !show_latest {
            let is_installed = installed_packages
                .iter()
                .any(|package| package.name.eq_ignore_ascii_case(package_name));
            if !is_installed {
                riff_core::errln!(
                    output,
                    "Error: {}",
                    package_not_found_message(package_name, &args)
                );
                return Ok(1);
            }
            if args.direct
                && !manifest
                    .require
                    .keys()
                    .chain(manifest.require_dev.keys())
                    .any(|name| name.eq_ignore_ascii_case(package_name))
            {
                riff_core::errln!(output,
                    "Error: Package '{}' is installed but is not a direct dependency of the root package",
                    package_name
                );
                return Ok(1);
            }
            show_single_package(
                &installed_packages,
                package_name,
                args.version.as_deref(),
                &args,
                &vendor_dir,
                output,
            )?;
            1
        } else {
            list_packages_with_latest(
                &installed_packages,
                Some(package_name),
                &manifest,
                &args,
                show_latest,
                &package_list_context,
            )
            .await?
        }
    } else {
        if args.tree {
            show_tree_all(&installed_packages, &manifest, output)?;
            installed_packages.len()
        } else {
            list_packages_with_latest(
                &installed_packages,
                None,
                &manifest,
                &args,
                show_latest,
                &package_list_context,
            )
            .await?
        }
    };

    Ok(if args.strict && args.outdated && displayed > 0 {
        1
    } else {
        0
    })
}

fn print_root_package_info(
    manifest: &RiffManifest,
    format: &str,
    output: &riff_core::Output,
) -> Result<()> {
    if format == "json" {
        let json = serde_json::json!({
            "name": manifest.name,
            "version": manifest.version,
            "description": manifest.description,
            "type": manifest.package_type,
            "license": manifest.license,
            "require": manifest.require,
            "require-dev": manifest.require_dev,
        });
        riff_core::outln!(output, "{}", serde_json::to_string_pretty(&json)?);
    } else {
        if let Some(name) = &manifest.name {
            riff_core::outln!(output, "name     : {}", name);
        }
        if let Some(desc) = &manifest.description {
            riff_core::outln!(output, "descrip. : {}", desc);
        }
        if let Some(version) = &manifest.version {
            riff_core::outln!(output, "version  : {}", version);
        }
        riff_core::outln!(output, "type     : {}", manifest.package_type);

        if !manifest.require.is_empty() {
            riff_core::outln!(output, "\nrequires");
            for (name, constraint) in &manifest.require {
                riff_core::outln!(output, "{} {}", name, constraint);
            }
        }

        if !manifest.require_dev.is_empty() {
            riff_core::outln!(output, "\nrequires (dev)");
            for (name, constraint) in &manifest.require_dev {
                riff_core::outln!(output, "{} {}", name, constraint);
            }
        }
    }
    Ok(())
}

fn show_single_package(
    packages: &[Arc<riff_core::Package>],
    name: &str,
    _version: Option<&str>,
    args: &ShowArgs,
    vendor_dir: &Path,
    output: &riff_core::Output,
) -> Result<()> {
    let name_lower = name.to_lowercase();
    let package = packages
        .iter()
        .find(|p| p.name.to_lowercase() == name_lower);

    let package = match package {
        Some(p) => p,
        None => {
            riff_core::errln!(output, "Error: Package '{}' not found", name);
            return Ok(());
        }
    };

    if args.path {
        let install_path = vendor_dir.join(&package.name);
        if install_path.exists() {
            riff_core::outln!(output, "{} {}", package.name, install_path.display());
        } else {
            riff_core::outln!(output, "{} null", package.name);
        }
        return Ok(());
    }

    if args.tree && args.format == "json" {
        riff_core::outln!(
            output,
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "installed": [{
                    "name": package.name,
                    "version": package.pretty_version.as_deref().unwrap_or(&package.version),
                    "description": package.description,
                }]
            }))?
        );
        return Ok(());
    }

    if args.tree {
        show_tree_single(package, packages, output)?;
        return Ok(());
    }

    if args.format == "json" {
        print_package_json(package, output)?;
    } else {
        print_package_info(package, output)?;
    }

    Ok(())
}

fn print_package_info(package: &riff_core::Package, output: &riff_core::Output) -> Result<()> {
    riff_core::outln!(output, "name     : {}", package.name);
    if let Some(desc) = &package.description {
        riff_core::outln!(output, "descrip. : {}", desc);
    }
    riff_core::outln!(
        output,
        "versions : {}",
        package
            .pretty_version
            .as_deref()
            .unwrap_or(&package.version)
    );
    riff_core::outln!(output, "type     : {}", package.package_type);

    if let Some(abandoned) = &package.abandoned {
        let replacement = match abandoned.replacement() {
            Some(pkg) => format!("Use {} instead", pkg),
            None => "No replacement was suggested".to_string(),
        };
        riff_core::errln!(
            output,
            "\nPackage {} is abandoned, you should avoid using it. {}.",
            package.name,
            replacement
        );
    }

    if !package.require.is_empty() {
        riff_core::outln!(output, "\nrequires");
        for (name, constraint) in &package.require {
            riff_core::outln!(output, "{} {}", name, constraint);
        }
    }

    if !package.require_dev.is_empty() {
        riff_core::outln!(output, "\nrequires (dev)");
        for (name, constraint) in &package.require_dev {
            riff_core::outln!(output, "{} {}", name, constraint);
        }
    }

    if !package.provide.is_empty() {
        riff_core::outln!(output, "\nprovide");
        for (name, constraint) in &package.provide {
            riff_core::outln!(output, "{} {}", name, constraint);
        }
    }

    if !package.conflict.is_empty() {
        riff_core::outln!(output, "\nconflict");
        for (name, constraint) in &package.conflict {
            riff_core::outln!(output, "{} {}", name, constraint);
        }
    }

    if !package.replace.is_empty() {
        riff_core::outln!(output, "\nreplace");
        for (name, constraint) in &package.replace {
            riff_core::outln!(output, "{} {}", name, constraint);
        }
    }

    Ok(())
}

fn print_package_json(package: &riff_core::Package, output: &riff_core::Output) -> Result<()> {
    let abandoned_value = package.abandoned.as_ref().map(|a| match a.replacement() {
        Some(pkg) => serde_json::json!(pkg),
        None => serde_json::json!(true),
    });

    let json = serde_json::json!({
        "name": package.name,
        "version": package.pretty_version.as_deref().unwrap_or(&package.version),
        "description": package.description,
        "type": package.package_type,
        "abandoned": abandoned_value,
        "require": package.require,
        "require-dev": package.require_dev,
        "provide": package.provide,
        "conflict": package.conflict,
        "replace": package.replace,
    });
    riff_core::outln!(output, "{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

fn package_not_found_message(package: &str, args: &ShowArgs) -> String {
    if args.all {
        return format!("Package \"{package}\" not found.");
    }
    if args.locked {
        return format!(
            "Package \"{package}\" not found in lock file, try using --available (-a) to show all available packages."
        );
    }
    if is_platform_package(package) && !args.platform {
        return format!(
            "Package \"{package}\" not found, try using --platform (-p) to show platform packages, try using --available (-a) to show all available packages."
        );
    }
    if args.working_dir != Path::new(".") && !args.platform {
        return format!(
            "Package \"{package}\" not found in {}/composer.json, try using --available (-a) to show all available packages.",
            args.working_dir.display()
        );
    }
    format!(
        "Package \"{package}\" not found, try using --available (-a) to show all available packages."
    )
}

async fn show_available_packages(
    manifest: &RiffManifest,
    working_dir: &Path,
    filter: Option<&str>,
    output: &riff_core::Output,
) -> Result<usize> {
    let mut packages = available_packages(manifest, working_dir, output).await;
    if let Some(filter) = filter {
        packages.retain(|package| wildcard_matches(filter, &package.name));
    }
    let count = packages.len();
    for package in packages {
        let description = package.description.as_deref().unwrap_or_default();
        if description.is_empty() {
            riff_core::outln!(output, "{}", package.name);
        } else {
            riff_core::outln!(output, "{} {}", package.name, description);
        }
    }
    Ok(count)
}

async fn available_packages(
    manifest: &RiffManifest,
    working_dir: &Path,
    output: &riff_core::Output,
) -> Vec<Arc<riff_core::Package>> {
    let mut manager = RepositoryManager::new().with_output(output.clone());
    for repository in manifest.repositories.as_vec() {
        manager.add_from_json_repository_at(&repository, working_dir);
    }
    let mut latest: HashMap<String, Arc<riff_core::Package>> = HashMap::new();
    for package in manager.get_packages().await {
        let key = package.name.to_lowercase();
        let replace = latest
            .get(&key)
            .is_none_or(|current| compare_versions(&package.version, &current.version).is_gt());
        if replace {
            latest.insert(key, package);
        }
    }
    let mut packages: Vec<_> = latest.into_values().collect();
    packages.sort_by(|left, right| left.name.cmp(&right.name));
    packages
}

async fn show_all_sections(
    context: &CommandContext,
    config: &Config,
    manifest: &RiffManifest,
    lock: Option<&RiffLockfile>,
    installed: &[Arc<riff_core::Package>],
    working_dir: &Path,
) -> Result<()> {
    let output = context.output();
    let mut platform = context.packages(config)?;
    platform.sort_by(|left, right| left.name.cmp(&right.name));
    riff_core::outln!(output, "platform:");
    for package in platform {
        riff_core::outln!(output, "  {} {}", package.name, package.pretty_version());
    }

    if let Some(lock) = lock {
        let mut locked: Vec<_> = lock
            .packages
            .iter()
            .chain(lock.packages_dev.iter())
            .map(riff_core::Package::from)
            .collect();
        locked.sort_by(|left, right| left.name.cmp(&right.name));
        if !locked.is_empty() {
            riff_core::outln!(output, "\nlocked:");
            for package in locked {
                print_section_package(&package, output);
            }
        }
    }

    let available = available_packages(manifest, working_dir, output).await;
    if !available.is_empty() {
        riff_core::outln!(output, "\navailable:");
        for package in available {
            let description = package.description.as_deref().unwrap_or_default();
            if description.is_empty() {
                riff_core::outln!(output, "  {}", package.name);
            } else {
                riff_core::outln!(output, "  {} {}", package.name, description);
            }
        }
    }

    if !installed.is_empty() {
        let mut installed = installed.to_vec();
        installed.sort_by(|left, right| left.name.cmp(&right.name));
        riff_core::outln!(output, "\ninstalled:");
        for package in installed {
            print_section_package(&package, output);
        }
    }
    Ok(())
}

fn print_section_package(package: &riff_core::Package, output: &riff_core::Output) {
    let description = package.description.as_deref().unwrap_or_default();
    if description.is_empty() {
        riff_core::outln!(output, "  {} {}", package.name, package.pretty_version());
    } else {
        riff_core::outln!(
            output,
            "  {} {} {}",
            package.name,
            package.pretty_version(),
            description
        );
    }
}

async fn fetch_latest_versions(
    packages: &[Arc<riff_core::Package>],
    repository_manager: &RepositoryManager,
    platform_versions: &HashMap<String, String>,
    verbose: u8,
    output: &riff_core::Output,
) -> HashMap<String, String> {
    let mut latest_versions = HashMap::new();

    for pkg in packages {
        if is_platform_package(&pkg.name) {
            continue;
        }

        let versions = repository_manager.find_packages(&pkg.name).await;
        let (latest, rejected) =
            find_latest_platform_compatible_version(&versions, platform_versions);
        let current = pkg.pretty_version.as_deref().unwrap_or(&pkg.version);
        for rejected in rejected
            .into_iter()
            .filter(|rejected| verbose > 0 || versions_equal(&rejected.version, current))
        {
            let subject = if verbose > 0 && rejected.latest {
                format!("{}'s latest version {}", pkg.name, rejected.version)
            } else {
                format!("{} {}", pkg.name, rejected.version)
            };
            riff_core::warnln!(
                output,
                "Cannot use {} as it requires {} {} which is missing from your platform.",
                subject,
                rejected.requirement,
                rejected.constraint
            );
        }
        if let Some(latest) = latest {
            latest_versions.insert(pkg.name.to_lowercase(), latest);
        }
    }

    latest_versions
}

#[derive(Debug)]
struct RejectedPlatformVersion {
    version: String,
    requirement: String,
    constraint: String,
    latest: bool,
}

fn find_latest_platform_compatible_version(
    packages: &[Arc<riff_core::Package>],
    platform_versions: &HashMap<String, String>,
) -> (Option<String>, Vec<RejectedPlatformVersion>) {
    let parser = VersionParser::new();
    let mut stable_versions: Vec<_> = packages
        .iter()
        .filter(|p| {
            let version = p.pretty_version.as_deref().unwrap_or(&p.version);
            !version.contains("dev")
                && !version.contains("alpha")
                && !version.contains("beta")
                && !version.contains("RC")
        })
        .collect();

    stable_versions.sort_by(|a, b| {
        let v_a = a.pretty_version.as_deref().unwrap_or(&a.version);
        let v_b = b.pretty_version.as_deref().unwrap_or(&b.version);

        let norm_a = parser.normalize(v_a).unwrap_or_else(|_| v_a.to_string());
        let norm_b = parser.normalize(v_b).unwrap_or_else(|_| v_b.to_string());

        compare_versions(&norm_b, &norm_a)
    });

    let mut rejected = Vec::new();
    let mut latest = None;
    for (index, package) in stable_versions.into_iter().enumerate() {
        let version = package
            .pretty_version
            .as_deref()
            .unwrap_or(&package.version)
            .to_owned();
        let mut compatible = true;
        for (requirement, constraint) in &package.require {
            if !is_platform_package(requirement) {
                continue;
            }
            let Some(installed) = platform_versions.get(requirement.as_str()) else {
                rejected.push(RejectedPlatformVersion {
                    version: version.clone(),
                    requirement: requirement.to_string(),
                    constraint: constraint.to_string(),
                    latest: index == 0,
                });
                compatible = false;
                continue;
            };
            if parser
                .parse_constraints_cached(constraint)
                .is_ok_and(|required| !required.satisfies(installed))
            {
                compatible = false;
            }
        }
        if compatible && latest.is_none() {
            latest = Some(version);
        }
    }
    (latest, rejected)
}

fn versions_equal(left: &str, right: &str) -> bool {
    let parser = VersionParser::new();
    parser.normalize(left).unwrap_or_else(|_| left.to_owned())
        == parser.normalize(right).unwrap_or_else(|_| right.to_owned())
}

fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let a_parts: Vec<u64> = a
        .split('.')
        .filter_map(|s| s.split('-').next())
        .filter_map(|s| s.parse().ok())
        .collect();
    let b_parts: Vec<u64> = b
        .split('.')
        .filter_map(|s| s.split('-').next())
        .filter_map(|s| s.parse().ok())
        .collect();

    for i in 0..std::cmp::max(a_parts.len(), b_parts.len()) {
        let a_part = a_parts.get(i).copied().unwrap_or(0);
        let b_part = b_parts.get(i).copied().unwrap_or(0);
        match a_part.cmp(&b_part) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

fn strip_version_prefix(version: &str) -> &str {
    version
        .strip_prefix('v')
        .or_else(|| version.strip_prefix('V'))
        .unwrap_or(version)
}

async fn list_packages_with_latest(
    packages: &[Arc<riff_core::Package>],
    filter: Option<&str>,
    manifest: &RiffManifest,
    args: &ShowArgs,
    show_latest: bool,
    context: &PackageListContext<'_>,
) -> Result<usize> {
    let output = context.output;
    let mut filtered: Vec<_> = packages
        .iter()
        .filter(|p| {
            if let Some(pattern) = filter {
                wildcard_matches(pattern, &p.name)
            } else {
                true
            }
        })
        .cloned()
        .collect();

    let root_requires: HashSet<String> =
        manifest.require.keys().map(|s| s.to_lowercase()).collect();

    let root_requires_dev: HashSet<String> = manifest
        .require_dev
        .keys()
        .map(|s| s.to_lowercase())
        .collect();

    if args.direct {
        filtered.retain(|p| {
            let name = p.name.to_lowercase();
            root_requires.contains(&name) || root_requires_dev.contains(&name)
        });
    }

    filtered.sort_by(|a, b| a.name.cmp(&b.name));

    let latest_versions = if show_latest {
        fetch_latest_versions(
            &filtered,
            context.repository_manager,
            context.platform_versions,
            args.verbose,
            output,
        )
        .await
    } else {
        HashMap::new()
    };

    let mut packages_with_latest: Vec<PackageWithLatest> = filtered
        .into_iter()
        .map(|p| {
            let current = p.pretty_version.as_deref().unwrap_or(&p.version);
            let latest = latest_versions.get(&p.name.to_lowercase()).cloned();
            let update_type = if let Some(ref lat) = latest {
                determine_update_type(current, lat)
            } else {
                UpdateType::UpToDate
            };
            PackageWithLatest {
                package: p,
                latest_version: latest,
                update_type,
            }
        })
        .collect();

    if args.sort_by_age {
        packages_with_latest.sort_by(|left, right| {
            left.package
                .time
                .cmp(&right.package.time)
                .then_with(|| left.package.name.cmp(&right.package.name))
        });
    }

    if args.outdated {
        packages_with_latest.retain(|p| p.update_type != UpdateType::UpToDate);
    }

    if let Some(filter) = &args.update_filter {
        packages_with_latest.retain(|package| {
            matches!(
                (filter.as_str(), package.update_type),
                ("major", UpdateType::Major)
                    | ("minor", UpdateType::Minor)
                    | ("patch", UpdateType::Patch)
            )
        });
    }

    if !args.ignore.is_empty() {
        packages_with_latest.retain(|package| {
            !args
                .ignore
                .iter()
                .any(|pattern| wildcard_matches(pattern, &package.package.name))
        });
    }

    if packages_with_latest.is_empty() {
        return Ok(0);
    }

    let package_count = packages_with_latest.len();

    if args.format == "json" {
        let json: Vec<_> = packages_with_latest
            .iter()
            .map(|p| {
                let abandoned_value = p
                    .package
                    .abandoned
                    .as_ref()
                    .map(|a| match a.replacement() {
                        Some(pkg) => serde_json::json!(pkg),
                        None => serde_json::json!(true),
                    })
                    .unwrap_or_else(|| serde_json::json!(false));

                let mut obj = serde_json::json!({
                    "name": p.package.name,
                    "direct-dependency": root_requires.contains(&p.package.name.to_lowercase())
                        || root_requires_dev.contains(&p.package.name.to_lowercase()),
                    "version": p.package.pretty_version.as_deref().unwrap_or(&p.package.version),
                    "description": p.package.description,
                    "abandoned": abandoned_value,
                });

                if let Some(ref latest) = p.latest_version {
                    obj["latest"] = serde_json::json!(latest);
                    obj["latest-status"] = serde_json::json!(match p.update_type {
                        UpdateType::UpToDate => "up-to-date",
                        UpdateType::Patch | UpdateType::Minor => "semver-safe-update",
                        UpdateType::Major => "update-possible",
                    });
                }

                obj
            })
            .collect();
        riff_core::outln!(
            output,
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"installed": json}))?
        );
    } else {
        if show_latest && !args.name_only {
            riff_core::errln!(output, "{}", style("Color legend:").green());
            riff_core::errln!(
                output,
                "- {} release available - update recommended",
                style("patch or minor").red()
            );
            riff_core::errln!(
                output,
                "- {} release available - update possible",
                style("major").yellow()
            );
            riff_core::errln!(output);

            let direct: Vec<_> = packages_with_latest
                .iter()
                .filter(|p| {
                    root_requires.contains(&p.package.name.to_lowercase())
                        || root_requires_dev.contains(&p.package.name.to_lowercase())
                })
                .collect();

            let transitive: Vec<_> = packages_with_latest
                .iter()
                .filter(|p| {
                    !root_requires.contains(&p.package.name.to_lowercase())
                        && !root_requires_dev.contains(&p.package.name.to_lowercase())
                })
                .collect();

            if !direct.is_empty() {
                riff_core::errln!(
                    output,
                    "{}",
                    style("Direct dependencies required in composer.json:").green()
                );
                print_packages_list(&direct, args, output);
            }

            if !transitive.is_empty() && !args.direct {
                if !direct.is_empty() {
                    riff_core::outln!(output);
                }
                riff_core::errln!(
                    output,
                    "{}",
                    style("Transitive dependencies not required in composer.json:").green()
                );
                print_packages_list(&transitive, args, output);
            }
        } else {
            print_packages_list(
                &packages_with_latest.iter().collect::<Vec<_>>(),
                args,
                output,
            );
        }
    }

    Ok(package_count)
}

struct PackageListContext<'a> {
    repository_manager: &'a RepositoryManager,
    platform_versions: &'a HashMap<String, String>,
    output: &'a riff_core::Output,
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let regex = regex::escape(pattern).replace(r"\*", ".*");
    regex::Regex::new(&format!("(?i)^{regex}$")).is_ok_and(|regex| regex.is_match(value))
}

fn make_packagist_link(name: &str) -> String {
    format!("https://packagist.org/packages/{}", name)
}

fn terminal_link(text: &str, url: &str) -> String {
    use console::Term;
    let term = Term::stdout();
    if term.is_term() {
        format!("\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\", url, text)
    } else {
        text.to_string()
    }
}

fn print_packages_list(
    packages: &[&PackageWithLatest],
    args: &ShowArgs,
    output: &riff_core::Output,
) {
    let name_width = packages
        .iter()
        .map(|p| p.package.name.len())
        .max()
        .unwrap_or(30)
        .max(30);

    for pwl in packages {
        let package = &pwl.package;
        if args.name_only {
            riff_core::outln!(output, "{}", package.name);
        } else {
            let raw_version = package
                .pretty_version
                .as_deref()
                .unwrap_or(&package.version);
            let version = strip_version_prefix(raw_version);
            let desc = package
                .description
                .as_deref()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("");

            let link_url = make_packagist_link(&package.name);
            let linked_name = terminal_link(&package.name, &link_url);
            let padding = " ".repeat(name_width.saturating_sub(package.name.len()));

            if let Some(ref latest) = pwl.latest_version {
                let latest_display = strip_version_prefix(latest);
                let truncated_desc = if desc.len() > 30 {
                    format!("{}...", &desc[..27])
                } else {
                    desc.to_string()
                };

                let (colored_version, indicator, colored_latest) = match pwl.update_type {
                    UpdateType::UpToDate => (
                        style(version).green().to_string(),
                        style("=").green().to_string(),
                        style(latest_display).green().to_string(),
                    ),
                    UpdateType::Patch | UpdateType::Minor => (
                        style(version).red().to_string(),
                        style("!").red().to_string(),
                        style(latest_display).red().to_string(),
                    ),
                    UpdateType::Major => (
                        style(version).yellow().to_string(),
                        style("~").yellow().to_string(),
                        style(latest_display).yellow().to_string(),
                    ),
                };

                riff_core::outln!(
                    output,
                    "{}{} {:<7} {} {:<7} {}",
                    linked_name,
                    padding,
                    colored_version,
                    indicator,
                    colored_latest,
                    truncated_desc
                );
            } else {
                let abandoned_marker = if package.abandoned.is_some() {
                    format!(" {}", style("[abandoned]").red())
                } else {
                    String::new()
                };
                riff_core::outln!(
                    output,
                    "{}{} {:<15} {}{}",
                    linked_name,
                    padding,
                    version,
                    desc,
                    abandoned_marker
                );
            }
        }
    }
}

fn show_tree_single(
    package: &Arc<riff_core::Package>,
    all_packages: &[Arc<riff_core::Package>],
    output: &riff_core::Output,
) -> Result<()> {
    let version = package
        .pretty_version
        .as_deref()
        .unwrap_or(&package.version);
    let desc = package.description.as_deref().unwrap_or("");
    riff_core::outln!(output, "{} {} {}", package.name, version, desc);

    let mut visited = HashSet::new();
    visited.insert(package.name.to_lowercase());

    print_dependencies_tree(&package.require, all_packages, "", &mut visited, output);

    Ok(())
}

fn show_tree_all(
    packages: &[Arc<riff_core::Package>],
    manifest: &RiffManifest,
    output: &riff_core::Output,
) -> Result<()> {
    let root_requires: HashSet<String> = manifest
        .require
        .keys()
        .chain(manifest.require_dev.keys())
        .map(|s| s.to_lowercase())
        .collect();

    let mut root_packages: Vec<_> = packages
        .iter()
        .filter(|p| root_requires.contains(&p.name.to_lowercase()))
        .collect();

    root_packages.sort_by(|a, b| a.name.cmp(&b.name));

    for package in root_packages {
        let version = package
            .pretty_version
            .as_deref()
            .unwrap_or(&package.version);
        riff_core::outln!(output, "{} {}", package.name, version);

        let mut visited = HashSet::new();
        visited.insert(package.name.to_lowercase());

        print_dependencies_tree(&package.require, packages, "", &mut visited, output);
    }

    Ok(())
}

fn print_dependencies_tree(
    requires: &riff_core::package::DependencyMap,
    all_packages: &[Arc<riff_core::Package>],
    prefix: &str,
    visited: &mut HashSet<String>,
    output: &riff_core::Output,
) {
    let mut deps: Vec<_> = requires.iter().collect();
    deps.sort_by(|a, b| a.0.cmp(b.0));

    let count = deps.len();
    for (idx, (dep_name, constraint)) in deps.iter().enumerate() {
        let is_last = idx == count - 1;
        let branch = if is_last { "└──" } else { "├──" };

        let dep_lower = dep_name.as_str().to_lowercase();
        let package = all_packages
            .iter()
            .find(|p| p.name.to_lowercase() == dep_lower);

        if let Some(pkg) = package {
            let version = pkg.pretty_version.as_deref().unwrap_or(&pkg.version);

            if visited.contains(&dep_lower) {
                riff_core::outln!(
                    output,
                    "{}{} {} {} (circular dependency aborted here)",
                    prefix,
                    branch,
                    dep_name,
                    version
                );
            } else {
                riff_core::outln!(
                    output,
                    "{}{} {} {} ({})",
                    prefix,
                    branch,
                    dep_name,
                    version,
                    constraint
                );

                visited.insert(dep_lower.clone());

                let new_prefix = format!("{}{}   ", prefix, if is_last { " " } else { "│" });
                print_dependencies_tree(&pkg.require, all_packages, &new_prefix, visited, output);

                visited.remove(&dep_lower);
            }
        } else {
            riff_core::outln!(output, "{}{} {} ({})", prefix, branch, dep_name, constraint);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_determine_update_type_up_to_date() {
        assert_eq!(
            determine_update_type("1.0.0", "1.0.0"),
            UpdateType::UpToDate
        );
        assert_eq!(
            determine_update_type("v2.3.4", "2.3.4"),
            UpdateType::UpToDate
        );
    }

    #[test]
    fn test_determine_update_type_patch() {
        assert_eq!(determine_update_type("1.0.0", "1.0.1"), UpdateType::Patch);
        assert_eq!(determine_update_type("1.0.0", "1.0.5"), UpdateType::Patch);
    }

    #[test]
    fn test_determine_update_type_minor() {
        assert_eq!(determine_update_type("1.0.0", "1.1.0"), UpdateType::Minor);
        assert_eq!(determine_update_type("1.0.0", "1.5.3"), UpdateType::Minor);
    }

    #[test]
    fn test_determine_update_type_major() {
        assert_eq!(determine_update_type("1.0.0", "2.0.0"), UpdateType::Major);
        assert_eq!(determine_update_type("1.5.3", "3.0.0"), UpdateType::Major);
        assert_eq!(determine_update_type("0.1.3", "0.2.0"), UpdateType::Major);
        assert_eq!(determine_update_type("0.0.3", "0.0.4"), UpdateType::Major);
    }

    #[test]
    fn composer_show_command_zero_major_updates_follow_semver_rules() {
        let cases = [
            ("0.1.2", "0.1.2.1", UpdateType::Patch),
            ("0.1.0", "0.1.2", UpdateType::Minor),
            ("0.1.0", "0.2.0", UpdateType::Major),
            ("0.0.1", "0.0.2", UpdateType::Major),
        ];

        for (current, latest, expected) in cases {
            assert_eq!(determine_update_type(current, latest), expected);
        }
    }

    #[test]
    fn test_compare_versions() {
        assert_eq!(
            compare_versions("1.0.0", "1.0.0"),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            compare_versions("1.0.1", "1.0.0"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(compare_versions("1.0.0", "1.0.1"), std::cmp::Ordering::Less);
        assert_eq!(
            compare_versions("2.0.0", "1.9.9"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_versions("1.10.0", "1.9.0"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn test_compare_versions_with_prefix() {
        assert_eq!(
            compare_versions("1.0.0-beta", "1.0.0"),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn test_strip_version_prefix() {
        assert_eq!(strip_version_prefix("v1.0.0"), "1.0.0");
        assert_eq!(strip_version_prefix("V2.3.4"), "2.3.4");
        assert_eq!(strip_version_prefix("1.0.0"), "1.0.0");
        assert_eq!(strip_version_prefix("v7.3.8"), "7.3.8");
    }

    #[test]
    fn test_wildcard_matches_package_names() {
        assert!(wildcard_matches("fixture/*", "fixture/tool"));
        assert!(!wildcard_matches("fixture/tool", "fixture/toolkit"));
        assert!(wildcard_matches("FIXTURE/*", "fixture/tool"));
    }
}
