//! Why command - show which packages depend on a given package.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use riff_core::{
    config::Config,
    is_platform_package,
    json::{LockedPackage, Repository as ManifestRepository, RiffLockfile, RiffManifest},
    query_dependencies_with_candidates, DependencyQuery, DependencyQueryError, DependencyResult,
    Package, Repository,
};

#[derive(usage_rs::Args, Debug)]
pub struct WhyArgs {
    /// Package name to analyze
    #[usage(complete = crate::commands::completion::complete_installed_package)]
    pub package: String,

    /// Version constraint (optional)
    pub constraint: Option<String>,

    /// Show the full dependency tree
    #[usage(short = 't', long)]
    pub tree: bool,

    /// Show recursive dependencies
    #[usage(short = 'r', long)]
    pub recursive: bool,

    /// Read package data from composer.lock instead of the installed repository
    #[usage(long)]
    pub locked: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(usage_rs::Args, Debug)]
pub struct WhyNotArgs {
    /// Package name to analyze
    #[usage(complete = crate::commands::completion::complete_installed_package)]
    pub package: String,

    /// Version constraint to test
    pub version: String,

    /// Show the full dependency tree
    #[usage(short = 't', long)]
    pub tree: bool,

    /// Show recursive dependencies
    #[usage(short = 'r', long)]
    pub recursive: bool,

    /// Read package data from composer.lock instead of the installed repository
    #[usage(long)]
    pub locked: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub async fn execute_why_not(args: WhyNotArgs) -> Result<i32> {
    execute(
        WhyArgs {
            package: args.package,
            constraint: Some(args.version),
            tree: args.tree,
            recursive: args.recursive,
            locked: args.locked,
            working_dir: args.working_dir,
        },
        true,
    )
    .await
}

pub async fn execute(args: WhyArgs, inverted: bool) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;

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

    let config = Config::build(Some(&working_dir), true)?;

    let vendor_dir = working_dir.join(&config.vendor_dir);
    let installed_repo = Arc::new(riff_core::repository::InstalledRepository::new(vendor_dir));
    installed_repo.load().await.ok();
    let mut installed_packages = installed_repo.get_packages().await;
    if args.locked {
        let Some(lock) = &lock else {
            riff_core::errln!(
                "Error: A valid composer.lock file is required to run this command with --locked"
            );
            return Ok(1);
        };
        installed_packages = lock
            .packages
            .iter()
            .chain(lock.packages_dev.iter())
            .map(|package| Arc::new(riff_core::Package::from(package)))
            .collect();
    }

    let has_dependency_packages = !installed_packages.is_empty();

    let root_package = riff_core::Package {
        name: manifest
            .name
            .clone()
            .unwrap_or_else(|| "__root__".to_string()),
        pretty_name: manifest.name.clone(),
        version: manifest
            .version
            .clone()
            .unwrap_or_else(|| "dev-main".to_string())
            .into(),
        pretty_version: manifest.version.clone().map(Into::into),
        package_type: "root-package".into(),
        require: manifest.require.clone().into(),
        require_dev: manifest.require_dev.clone().into(),
        conflict: manifest.conflict.clone().into(),
        replace: manifest.replace.clone().into(),
        provide: manifest.provide.clone().into(),
        ..Default::default()
    };
    if !has_dependency_packages
        && (!manifest.require.is_empty() || !manifest.require_dev.is_empty())
    {
        riff_core::errln!(
            "Warning: No dependencies installed. Try running install or update, or use --locked."
        );
        return Ok(1);
    }

    installed_packages.insert(0, Arc::new(root_package));
    let configured_platform =
        append_configured_platform_packages(&mut installed_packages, &config.platform);
    let repository_candidates = inline_repository_candidates(&manifest)?;

    let needle = &args.package;
    let constraint_str = args.constraint.as_deref().unwrap_or("*");
    let recursive = args.tree || args.recursive;
    let query = match query_dependencies_with_candidates(
        &installed_packages,
        &repository_candidates,
        &DependencyQuery {
            package: needle.clone(),
            constraint: args.constraint.clone(),
            inverted,
            recursive,
        },
    ) {
        Ok(query) => query,
        Err(DependencyQueryError::PackageNotFound(package)) => {
            riff_core::errln!(
                "Error: Could not find package \"{}\" in your project",
                package
            );
            return Ok(1);
        }
        Err(DependencyQueryError::InvalidConstraint {
            constraint,
            message,
        }) => {
            riff_core::errln!("Error: Invalid constraint '{}': {}", constraint, message);
            return Ok(1);
        }
    };

    if inverted && is_platform_package(needle) {
        if let Some(package) = &query.installed_match {
            let configured = configured_platform.contains(&needle.to_ascii_lowercase());
            riff_core::outln!(
                "Package \"{} {}\" found in version \"{}\"{}.",
                needle,
                constraint_str,
                display_version(package),
                if configured {
                    " (version provided by config.platform)"
                } else {
                    ""
                }
            );
        }
    } else if inverted {
        if let Some(package) = &query.installed_match {
            riff_core::outln!(
                "Package \"{}\" {} is already installed! To find out why, run `riff why {}`",
                needle,
                display_version(package),
                needle
            );
            return Ok(0);
        }
        if query.constraint_unavailable {
            riff_core::errln!(
                "Package \"{}\" could not be found with constraint \"{}\", results below will most likely be incomplete.",
                needle,
                constraint_str
            );
        }
    }

    let results = &query.dependents;

    let status = if results.is_empty() {
        let extra = if args.constraint.as_deref().is_some_and(|value| value != "*") {
            format!(
                " in versions {}matching {}",
                if inverted { "not " } else { "" },
                constraint_str
            )
        } else {
            String::new()
        };
        riff_core::outln!(
            "There is no installed package depending on \"{}\"{}",
            needle,
            extra
        );
        if inverted {
            0
        } else {
            1
        }
    } else {
        if args.tree {
            print_tree(results, &query.inspected_packages[0]);
        } else {
            print_table(results);
        }
        if inverted {
            1
        } else {
            0
        }
    };

    if inverted && args.constraint.is_some() && !is_platform_package(needle) {
        let mut command = "update";

        for req in &manifest.require {
            if req.0.to_lowercase() == needle.to_lowercase() {
                command = "require";
                break;
            }
        }

        for req in &manifest.require_dev {
            if req.0.to_lowercase() == needle.to_lowercase() {
                command = "require --dev";
                break;
            }
        }

        riff_core::errln!(
            "Not finding what you were looking for? Try calling `riff {} \"{}:{}\" --dry-run` to get another view on the problem.",
            command, needle, constraint_str
        );
    }

    Ok(status)
}

fn inline_repository_candidates(manifest: &RiffManifest) -> Result<Vec<Arc<Package>>> {
    let mut candidates = Vec::new();
    for repository in manifest.repositories.as_vec() {
        let ManifestRepository::Package { package, .. } = repository else {
            continue;
        };
        let entries = package
            .as_array()
            .map_or_else(|| vec![package.clone()], |packages| packages.to_vec());
        for entry in entries {
            let package: LockedPackage = serde_json::from_value(entry)
                .context("Failed to load inline package repository")?;
            candidates.push(Arc::new(Package::from(&package)));
        }
    }
    Ok(candidates)
}

fn append_configured_platform_packages(
    packages: &mut Vec<Arc<Package>>,
    configured: &std::collections::HashMap<String, serde_json::Value>,
) -> HashSet<String> {
    let mut names = HashSet::new();
    for (name, value) in configured {
        let Some(version) = value.as_str() else {
            continue;
        };
        let name = name.to_ascii_lowercase();
        names.insert(name.clone());
        packages.push(Arc::new(Package::new(name, version)));
    }
    names
}

fn display_version(package: &Package) -> &str {
    if matches!(package.package_type.as_str(), "root-package" | "project") {
        "-"
    } else {
        package
            .pretty_version
            .as_deref()
            .unwrap_or(&package.version)
    }
}

fn print_table(results: &[DependencyResult]) {
    let mut seen = HashSet::new();
    let mut all_results = Vec::new();
    let mut queue: Vec<&DependencyResult> = results.iter().collect();

    while !queue.is_empty() {
        let mut next_queue = Vec::new();
        let mut rows = Vec::new();

        for result in queue {
            let key = format!(
                "{}:{}:{}:{}",
                result.package.name,
                result.link.target,
                result.link.constraint,
                result.link.link_type
            );
            if !seen.insert(key) {
                continue;
            }
            rows.push(result);

            if let Some(ref children) = result.children {
                next_queue.extend(children.iter());
            }
        }

        queue = next_queue;
        rows.extend(all_results);
        all_results = rows;
    }

    let name_width = all_results
        .iter()
        .map(|result| result.package.name.len())
        .max()
        .unwrap_or(0);
    let version_width = all_results
        .iter()
        .map(|result| display_version(&result.package).len())
        .max()
        .unwrap_or(0);

    for result in all_results {
        riff_core::outln!(
            "{:<name_width$} {:<version_width$} {} {} ({})",
            result.package.name,
            display_version(&result.package),
            result.link.link_type.description(),
            result.link.target,
            result.link.pretty_constraint(),
        );
    }
}

fn print_tree(results: &[DependencyResult], root: &Arc<riff_core::Package>) {
    riff_core::outln!("{} {}", root.name, display_version(root));
    print_tree_recursive(results, "");
}

fn print_tree_recursive(results: &[DependencyResult], prefix: &str) {
    let count = results.len();

    for (idx, result) in results.iter().enumerate() {
        let is_last = idx == count - 1;
        let branch = if is_last { "`--" } else { "|--" };
        let package = if matches!(
            result.package.package_type.as_str(),
            "root-package" | "project"
        ) {
            result.package.name.clone()
        } else {
            format!(
                "{} {}",
                result.package.name,
                display_version(&result.package)
            )
        };

        let circular_warn = if result.children.is_none() {
            " (circular dependency aborted here)"
        } else {
            ""
        };

        let link_desc = result.link.link_type.description();

        riff_core::outln!(
            "{}{}{} ({} {} {}){}",
            prefix,
            branch,
            package,
            link_desc,
            result.link.target,
            result.link.constraint,
            circular_warn
        );

        if let Some(ref children) = result.children {
            let new_prefix = format!("{}{}", prefix, if is_last { "   " } else { "|  " });
            print_tree_recursive(children, &new_prefix);
        }
    }
}
