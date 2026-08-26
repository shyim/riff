use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use riff_core::config::Config;
use riff_core::json::RiffManifest;
use riff_core::package::package_name_matches;
use riff_core::repository::{InstalledRepository, Repository as _};
use riff_core::{Package, RiffBuilder, Transaction};

use crate::CommandContext;

#[derive(Debug, usage_rs::Args)]
pub struct ReinstallArgs {
    /// Installed package names or wildcard patterns to reinstall
    #[usage(
        arg,
        name = "PACKAGES",
        complete = crate::commands::completion::complete_installed_package
    )]
    pub packages: Vec<String>,

    /// Reinstall every installed package of this type; may be repeated
    #[usage(long = "type", value_name = "TYPE")]
    pub package_types: Vec<String>,

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

    /// Disable all plugins
    #[usage(long)]
    pub no_plugins: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub async fn execute(args: ReinstallArgs, context: &CommandContext) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let manifest_path = working_dir.join("composer.json");
    let manifest: RiffManifest = serde_json::from_slice(
        &std::fs::read(&manifest_path)
            .with_context(|| format!("Failed to read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;
    let config = Config::build(Some(&working_dir), true)?;
    let installed = InstalledRepository::new(config.get_vendor_dir());
    installed.load().await.map_err(anyhow::Error::msg)?;
    let packages = installed.get_packages().await;
    let mut selection = select_packages(&packages, &args.packages, &args.package_types)?;

    for pattern in &selection.unmatched_patterns {
        riff_core::warnln!(
            "Pattern \"{pattern}\" does not match any currently installed packages."
        );
    }
    if selection.packages.is_empty() {
        riff_core::warnln!("Found no packages to reinstall, aborting.");
        return Ok(1);
    }

    let mut builder = RiffBuilder::new(working_dir)
        .with_config(config)
        .with_manifest(manifest)
        .with_platform(context.platform().clone())
        .with_runtime(context.runtime().clone())
        .plugins_enabled(!args.no_plugins);
    builder = crate::install::apply_install_preference(
        builder,
        args.prefer_source,
        args.prefer_dist,
        args.prefer_install.as_deref(),
    )?;
    let riff = builder.build()?;

    let mut transaction = Transaction::new();
    selection
        .packages
        .sort_by(|left, right| left.name.cmp(&right.name));
    for package in selection.packages {
        transaction.reinstall(package);
    }
    transaction.sort();
    let install_order = transaction.reinstalls().cloned().collect::<Vec<_>>();

    for package in install_order.iter().rev() {
        riff_core::outln!(
            "  - Removing {} ({})",
            package.name,
            package.pretty_version()
        );
    }
    riff.installation_manager.execute(&transaction).await?;
    for package in &install_order {
        riff_core::outln!(
            "  - Installing {} ({})",
            package.name,
            package.pretty_version()
        );
    }

    Ok(0)
}

struct PackageSelection {
    packages: Vec<Arc<Package>>,
    unmatched_patterns: Vec<String>,
}

fn select_packages(
    installed: &[Arc<Package>],
    patterns: &[String],
    package_types: &[String],
) -> Result<PackageSelection> {
    if !patterns.is_empty() && !package_types.is_empty() {
        bail!("You cannot specify package names and filter by type at the same time.");
    }
    if patterns.is_empty() && package_types.is_empty() {
        bail!("You must pass one or more package names to be reinstalled.");
    }

    let mut installed = installed.to_vec();
    installed.sort_by(|left, right| left.name.cmp(&right.name));
    if !package_types.is_empty() {
        return Ok(PackageSelection {
            packages: installed
                .into_iter()
                .filter(|package| {
                    package_types
                        .iter()
                        .any(|package_type| package.package_type == *package_type)
                })
                .collect(),
            unmatched_patterns: Vec::new(),
        });
    }

    let mut packages = Vec::new();
    let mut selected_names = HashSet::new();
    let mut unmatched_patterns = Vec::new();
    for pattern in patterns {
        let mut matched = false;
        for package in &installed {
            if package_name_matches(pattern, &package.name) {
                matched = true;
                if selected_names.insert(package.name.to_ascii_lowercase()) {
                    packages.push(package.clone());
                }
            }
        }
        if !matched {
            unmatched_patterns.push(pattern.clone());
        }
    }

    Ok(PackageSelection {
        packages,
        unmatched_patterns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str, package_type: &str) -> Arc<Package> {
        let mut package = Package::new(name, "1.0.0.0");
        package.package_type = package_type.into();
        Arc::new(package)
    }

    #[test]
    fn selection_supports_names_wildcards_types_and_unmatched_patterns() {
        let installed = vec![
            package("root/req", "metapackage"),
            package("root/anotherreq", "metapackage"),
            package("root/anotherreq2", "metapackage"),
            package("root/library", "library"),
        ];

        let selected = select_packages(
            &installed,
            &["root/req".into(), "root/anotherreq*".into()],
            &[],
        )
        .unwrap();
        assert_eq!(
            selected
                .packages
                .iter()
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            ["root/req", "root/anotherreq", "root/anotherreq2"]
        );
        assert!(selected.unmatched_patterns.is_empty());

        let selected = select_packages(&installed, &[], &["metapackage".into()]).unwrap();
        assert_eq!(selected.packages.len(), 3);

        let selected = select_packages(&installed, &["root/missing".into()], &[]).unwrap();
        assert!(selected.packages.is_empty());
        assert_eq!(selected.unmatched_patterns, ["root/missing"]);
    }

    #[test]
    fn selection_rejects_missing_or_conflicting_selectors() {
        let installed = vec![package("root/req", "metapackage")];
        assert!(select_packages(&installed, &[], &[]).is_err());
        assert!(
            select_packages(&installed, &["root/req".into()], &["metapackage".into()]).is_err()
        );
    }
}
