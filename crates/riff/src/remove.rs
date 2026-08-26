//! Remove command - remove a package from the project.

use anyhow::{Context, Result};
use console::style;
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use crate::CommandContext;
use riff_core::{
    config::Config,
    installer::{Installer, UpdateOptions},
    json::{write_manifest, AllowPlugins, LockedPackage, RiffLockfile, RiffManifest},
    policy_config::PolicyEnvironment,
    RiffBuilder,
};

#[derive(usage_rs::Args, Debug)]
pub struct RemoveArgs {
    /// Packages to remove
    #[usage(
        value_name = "PACKAGES",
        complete = crate::commands::completion::complete_remove_package
    )]
    pub packages: Vec<String>,

    /// Remove packages which are locked but no longer required
    #[usage(long)]
    pub unused: bool,

    /// Remove from development dependencies
    #[usage(long)]
    pub dev: bool,

    /// Run in dry-run mode
    #[usage(long)]
    pub dry_run: bool,

    /// Do not run update after removing
    #[usage(long)]
    pub no_update: bool,

    /// Update the lock file without uninstalling packages
    #[usage(long)]
    pub no_install: bool,

    /// Deprecated; dependency updates are enabled by default
    #[usage(long)]
    pub update_with_dependencies: bool,

    /// Update dependencies including root requirements
    #[usage(long)]
    pub update_with_all_dependencies: bool,

    /// Update dependencies including root requirements
    #[usage(short = 'W', long)]
    pub with_all_dependencies: bool,

    /// Keep inherited dependencies locked where possible
    #[usage(long)]
    pub no_update_with_dependencies: bool,

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

    /// Optimize autoloader
    #[usage(short = 'o', long)]
    pub optimize_autoloader: bool,

    /// Skip the audit step after update
    #[usage(long)]
    pub no_audit: bool,

    /// Do not ask interactive questions
    #[usage(short = 'n', long)]
    pub no_interaction: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DependencyUpdateMode {
    Default,
    All,
    ListedOnly,
}

const DEPRECATED_DEPENDENCY_UPDATE_WARNING: &str = "You are using the deprecated option \"update-with-dependencies\". This is now default behaviour. The --no-update-with-dependencies option can be used to remove a package without its dependencies.";

impl DependencyUpdateMode {
    fn from_args(args: &RemoveArgs) -> Self {
        if args.with_all_dependencies || args.update_with_all_dependencies {
            Self::All
        } else if args.no_update_with_dependencies {
            Self::ListedOnly
        } else {
            Self::Default
        }
    }

    const fn display_flag(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::All => " --with-all-dependencies",
            // Composer's post-remove update subprocess expresses the
            // restricted mode using this compatibility flag.
            Self::ListedOnly => " --with-dependencies",
        }
    }

    const fn installer_flags(self) -> (bool, bool) {
        match self {
            Self::Default => (true, false),
            Self::All => (true, true),
            Self::ListedOnly => (false, false),
        }
    }
}

fn deprecated_dependency_warning(args: &RemoveArgs) -> Option<&'static str> {
    args.update_with_dependencies
        .then_some(DEPRECATED_DEPENDENCY_UPDATE_WARNING)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RemovalPlan {
    removed: Vec<String>,
    update_packages: Vec<String>,
    warnings: Vec<String>,
    no_unused_packages: bool,
}

impl RemovalPlan {
    fn manifest_changed(&self) -> bool {
        !self.removed.is_empty()
    }

    fn should_write_manifest(&self, dry_run: bool) -> bool {
        self.manifest_changed() && !dry_run
    }
}

fn plan_removal(
    manifest: &mut RiffManifest,
    lock: Option<&RiffLockfile>,
    selectors: &[String],
    dev: bool,
    unused: bool,
) -> std::result::Result<RemovalPlan, String> {
    let mut plan = RemovalPlan::default();

    if unused {
        let lock = lock.ok_or_else(|| {
            "A valid composer.lock file is required to run this command with --unused".to_owned()
        })?;
        let unused_packages = unused_package_names(manifest, lock);
        if unused_packages.is_empty() {
            plan.no_unused_packages = true;
        } else {
            for package in unused_packages {
                plan.warnings.push(format!(
                    "{package} is not required in your composer.json and has not been removed"
                ));
                plan.update_packages.push(package);
            }
        }
    }

    for selector in selectors {
        let mut matched = matching_keys(
            if dev {
                &manifest.require_dev
            } else {
                &manifest.require
            },
            selector,
        );
        if !dev {
            matched.extend(matching_keys(&manifest.require_dev, selector));
        }
        matched.sort();
        matched.dedup();

        if matched.is_empty() {
            let opposite = if dev {
                matching_keys(&manifest.require, selector)
            } else {
                Vec::new()
            };
            if opposite.is_empty() {
                plan.warnings.push(format!(
                    "{selector} is not required in your composer.json and has not been removed"
                ));
            } else {
                plan.warnings.extend(opposite.into_iter().map(|package| {
                    format!(
                        "{package} could not be found in require-dev but it is present in require"
                    )
                }));
            }
            continue;
        }

        for package in matched {
            manifest.require.shift_remove(&package);
            manifest.require_dev.shift_remove(&package);
            if !plan.removed.contains(&package) {
                plan.removed.push(package);
            }
        }
        plan.update_packages.push(selector.clone());
    }

    prune_allow_plugins(manifest, &plan.removed);
    plan.update_packages.sort();
    plan.update_packages.dedup();
    Ok(plan)
}

fn matching_keys(requirements: &IndexMap<String, String>, selector: &str) -> Vec<String> {
    requirements
        .keys()
        .filter(|package| package_name_matches(selector, package))
        .cloned()
        .collect()
}

fn package_name_matches(selector: &str, package: &str) -> bool {
    let selector = selector.to_ascii_lowercase();
    let package = package.to_ascii_lowercase();
    match selector.split_once('*') {
        Some((prefix, suffix)) => package.starts_with(prefix) && package.ends_with(suffix),
        None => selector == package,
    }
}

fn prune_allow_plugins(manifest: &mut RiffManifest, removed: &[String]) {
    let Some(AllowPlugins::List(plugins)) = manifest.config.allow_plugins.as_mut() else {
        return;
    };
    plugins.retain(|package, _| {
        !removed
            .iter()
            .any(|removed| package.eq_ignore_ascii_case(removed))
    });
    if plugins.is_empty() {
        manifest.config.allow_plugins = None;
    }
}

fn unused_package_names(manifest: &RiffManifest, lock: &RiffLockfile) -> Vec<String> {
    let packages: HashMap<String, &LockedPackage> = lock
        .all_packages()
        .map(|package| (package.name.to_ascii_lowercase(), package))
        .collect();
    let mut queue: VecDeque<String> = manifest
        .require
        .keys()
        .chain(manifest.require_dev.keys())
        .map(|package| package.to_ascii_lowercase())
        .collect();
    let mut used = HashSet::new();
    while let Some(package_name) = queue.pop_front() {
        if !used.insert(package_name.clone()) {
            continue;
        }
        if let Some(package) = packages.get(&package_name) {
            queue.extend(
                package
                    .require
                    .keys()
                    .map(|dependency| dependency.to_ascii_lowercase()),
            );
        }
    }

    let mut unused = lock
        .all_packages()
        .filter(|package| !used.contains(&package.name.to_ascii_lowercase()))
        .map(|package| package.name.clone())
        .collect::<Vec<_>>();
    unused.sort();
    unused
}

fn still_installed(removed: &[String], installed: &[String]) -> Vec<String> {
    removed
        .iter()
        .filter(|selector| {
            installed
                .iter()
                .any(|package| package_name_matches(selector, package))
        })
        .cloned()
        .collect()
}

fn installed_package_names(vendor_dir: &std::path::Path) -> Result<Vec<String>> {
    let path = vendor_dir.join("composer/installed.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let packages = value
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(packages
        .iter()
        .filter_map(|package| package.get("name").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect())
}

pub async fn execute(args: RemoveArgs, context: &CommandContext) -> Result<i32> {
    if args.packages.is_empty() && !args.unused {
        riff_core::errln!("Not enough arguments (missing: \"packages\").");
        return Ok(1);
    }
    if let Some(warning) = deprecated_dependency_warning(&args) {
        riff_core::warnln!("{warning}");
    }
    // Accepted for Composer CLI compatibility. Remove does not currently run a
    // separate audit or interactive prompt of its own.
    let _compatibility_flags = (args.no_audit, args.no_interaction);

    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;

    let json_path = working_dir.join("composer.json");
    if !json_path.exists() {
        riff_core::errln!(
            "{} No composer.json found in {}",
            style("Error:").red().bold(),
            working_dir.display()
        );
        return Ok(1);
    }

    // Load composer.json
    let original_json = std::fs::read_to_string(&json_path)?;
    let mut manifest: RiffManifest = serde_json::from_str(&original_json)?;

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

    let plan = match plan_removal(
        &mut manifest,
        lock.as_ref(),
        &args.packages,
        args.dev,
        args.unused,
    ) {
        Ok(plan) => plan,
        Err(message) => {
            riff_core::errln!("{message}");
            return Ok(1);
        }
    };
    if plan.no_unused_packages && args.packages.is_empty() {
        riff_core::outln!("No unused packages to remove");
        return Ok(0);
    }
    for warning in &plan.warnings {
        riff_core::warnln!("{warning}");
    }
    if plan.removed.is_empty() && plan.update_packages.is_empty() {
        riff_core::outln!("{} Nothing to remove", style("Info:").cyan());
        return Ok(0);
    }

    // Load config
    let config = Config::build(Some(&working_dir), true)?;
    let vendor_dir = config.get_vendor_dir();

    // Create Riff using builder
    let riff = RiffBuilder::new(working_dir.clone())
        .with_config(config)
        .with_manifest(manifest)
        .with_lockfile(lock)
        .with_platform(context.platform().clone())
        .with_runtime(context.runtime().clone())
        .with_policy_environment(PolicyEnvironment::from_process())
        .plugins_enabled(!args.no_plugins)
        .dry_run(args.dry_run)
        .build()?;

    riff_core::outln!("{} Removing packages", style("Riff").green().bold());
    if args.dry_run {
        riff_core::outln!("{} Running in dry-run mode", style("Info:").cyan());
    }

    for name in &plan.removed {
        riff_core::outln!("  {} {}", style("-").red(), style(name).white().bold());
    }

    // Write updated composer.json
    if plan.should_write_manifest(args.dry_run) {
        write_manifest(&json_path, &riff.manifest).context("Failed to write composer.json")?;
    } else if plan.manifest_changed() {
        riff_core::outln!("{} composer.json would be updated", style("Info:").cyan());
    }

    // Run update
    if !args.no_update {
        let dependency_mode = DependencyUpdateMode::from_args(&args);
        riff_core::outln!(
            "Running riff update {}{}",
            plan.update_packages.join(" "),
            dependency_mode.display_flag()
        );
        let installer = Installer::new(riff);
        let (with_dependencies, with_all_dependencies) = dependency_mode.installer_flags();

        let result = installer
            .update(UpdateOptions {
                optimize_autoloader: args.optimize_autoloader,
                update_packages: Some(plan.update_packages.clone()),
                with_dependencies,
                with_all_dependencies,
                no_autoloader: args.no_autoloader,
                no_scripts: args.no_scripts,
                no_install: args.no_install,
                no_security_blocking: args.no_security_blocking,
                no_blocking: args.no_blocking,
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

        if args.no_install && !args.dry_run && matches!(&result, Ok(0)) {
            let installed = installed_package_names(&vendor_dir)?;
            let remaining = still_installed(&plan.removed, &installed);
            if !remaining.is_empty() {
                for package in remaining {
                    riff_core::errln!(
                        "Removal failed, {package} is still present, it may be required by another package. See `riff why {package}`"
                    );
                }
                return Ok(2);
            }
        }

        result
    } else if args.dry_run {
        riff_core::outln!(
            "{} Update skipped; no files were changed",
            style("Info:").cyan()
        );
        Ok(0)
    } else {
        riff_core::successln!(
            "{} {} packages removed from composer.json",
            style("Success:").green().bold(),
            plan.removed.len()
        );
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(value: serde_json::Value) -> RiffManifest {
        serde_json::from_value(value).unwrap()
    }

    fn lock(packages: serde_json::Value) -> RiffLockfile {
        serde_json::from_value(json!({
            "packages": packages,
            "packages-dev": []
        }))
        .unwrap()
    }

    fn args() -> RemoveArgs {
        RemoveArgs {
            packages: vec!["root/req".to_owned()],
            unused: false,
            dev: false,
            dry_run: false,
            no_update: false,
            no_install: false,
            update_with_dependencies: false,
            update_with_all_dependencies: false,
            with_all_dependencies: false,
            no_update_with_dependencies: false,
            no_autoloader: false,
            no_scripts: false,
            no_plugins: false,
            no_security_blocking: false,
            no_blocking: false,
            optimize_autoloader: false,
            no_audit: false,
            no_interaction: false,
            working_dir: PathBuf::from("."),
        }
    }

    // Ported from Composer\Test\Command\RemoveCommandTest::
    // testExceptionWhenRunningUnusedWithoutLockFile.
    #[test]
    fn composer_remove_unused_requires_lock_file() {
        let mut manifest = manifest(json!({"require": {"root/req": "1.*"}}));
        let error = plan_removal(&mut manifest, None, &[], false, true).unwrap_err();
        assert_eq!(
            error,
            "A valid composer.lock file is required to run this command with --unused"
        );
    }

    // Ported from Composer\Test\Command\RemoveCommandTest::
    // testMessageOutputWhenNoUnusedPackagesToRemove.
    #[test]
    fn composer_remove_unused_reports_when_every_locked_package_is_reachable() {
        let mut manifest = manifest(json!({"require": {"root/req": "1.*"}}));
        let lock = lock(json!([
            {"name": "root/req", "version": "1.0.0", "require": {"nested/req": "^1"}},
            {"name": "nested/req", "version": "1.1.0"}
        ]));
        let plan = plan_removal(&mut manifest, Some(&lock), &[], false, true).unwrap();

        assert!(plan.no_unused_packages);
        assert!(plan.update_packages.is_empty());
        assert!(plan.warnings.is_empty());
    }

    // Ported from Composer\Test\Command\RemoveCommandTest::testRemoveUnusedPackage.
    #[test]
    fn composer_remove_unused_schedules_unreachable_locked_package() {
        let mut manifest = manifest(json!({"require": {"root/req": "1.*"}}));
        let lock = lock(json!([
            {"name": "root/req", "version": "1.0.0"},
            {"name": "not/req", "version": "1.0.0"}
        ]));
        let plan = plan_removal(&mut manifest, Some(&lock), &[], false, true).unwrap();

        assert_eq!(plan.update_packages, ["not/req"]);
        assert_eq!(
            plan.warnings,
            ["not/req is not required in your composer.json and has not been removed"]
        );
        assert!(manifest.require.contains_key("root/req"));
    }

    // Ported from Composer\Test\Command\RemoveCommandTest::
    // testRemoveAllowedPluginPackageWithNoOtherAllowedPlugins.
    #[test]
    fn composer_remove_prunes_only_allow_plugin_entry() {
        let mut manifest = manifest(json!({
            "require": {"root/req": "1.*", "root/another": "1.*"},
            "config": {"allow-plugins": {"root/req": true}}
        }));
        plan_removal(&mut manifest, None, &["root/req".to_owned()], false, false).unwrap();

        assert!(manifest.config.allow_plugins.is_none());
        assert!(manifest.config.is_empty());
    }

    // Ported from Composer\Test\Command\RemoveCommandTest::
    // testRemoveAllowedPluginPackageWithOtherAllowedPlugins.
    #[test]
    fn composer_remove_preserves_other_allowed_plugins() {
        let mut manifest = manifest(json!({
            "require": {"root/req": "1.*", "root/another": "1.*"},
            "config": {"allow-plugins": {"root/another": true, "root/req": true}}
        }));
        plan_removal(&mut manifest, None, &["root/req".to_owned()], false, false).unwrap();

        let Some(AllowPlugins::List(plugins)) = manifest.config.allow_plugins else {
            panic!("expected remaining allow-plugins map");
        };
        assert_eq!(plugins, IndexMap::from([("root/another".to_owned(), true)]));
    }

    // Ported from Composer\Test\Command\RemoveCommandTest::testRemovePackagesByVendor.
    #[test]
    fn composer_remove_expands_vendor_wildcard() {
        let mut manifest = manifest(json!({
            "require": {
                "root/req": "1.*",
                "root/another": "1.*",
                "another/req": "1.*"
            }
        }));
        let plan = plan_removal(&mut manifest, None, &["root/*".to_owned()], false, false).unwrap();

        assert_eq!(plan.removed, ["root/another", "root/req"]);
        assert_eq!(plan.update_packages, ["root/*"]);
        assert_eq!(
            manifest.require,
            IndexMap::from([("another/req".to_owned(), "1.*".to_owned())])
        );
    }

    // Ported from Composer\Test\Command\RemoveCommandTest::testRemovePackagesByVendorWithDryRun.
    #[test]
    fn composer_remove_vendor_wildcard_dry_run_does_not_write_manifest() {
        let original = manifest(json!({
            "require": {"root/req": "1.*", "root/another": "1.*", "another/req": "1.*"}
        }));
        let mut planned = original.clone();
        let plan = plan_removal(&mut planned, None, &["root/*".to_owned()], false, false).unwrap();

        assert!(!plan.should_write_manifest(true));
        assert_eq!(original.require.len(), 3);
        assert_eq!(planned.require.len(), 1);
    }

    // Ported from Composer\Test\Command\RemoveCommandTest::
    // testWarningWhenRemovingPackagesByVendorFromWrongType.
    #[test]
    fn composer_remove_vendor_wildcard_warns_for_wrong_dependency_type() {
        let mut manifest = manifest(json!({
            "require": {"root/req": "1.*", "root/another": "1.*", "another/req": "1.*"}
        }));
        let plan = plan_removal(&mut manifest, None, &["root/*".to_owned()], true, false).unwrap();

        assert!(plan.removed.is_empty());
        assert_eq!(plan.warnings.len(), 2);
        assert!(plan.warnings.iter().all(|warning| warning
            .contains("could not be found in require-dev but it is present in require")));
        assert_eq!(manifest.require.len(), 3);
    }

    // Ported from Composer\Test\Command\RemoveCommandTest::
    // testPackageStillPresentErrorWhenNoInstallFlagUsed.
    #[test]
    fn composer_remove_no_install_detects_still_installed_package() {
        assert_eq!(
            still_installed(
                &["root/req".to_owned()],
                &["root/req".to_owned(), "another/req".to_owned()]
            ),
            ["root/req"]
        );
        assert!(still_installed(&["root/*".to_owned()], &["another/req".to_owned()]).is_empty());
    }

    // Ported from Composer\Test\Command\RemoveCommandTest::
    // testUpdateInheritedDependenciesFlagIsPassedToPostRemoveInstaller.
    #[test]
    fn composer_remove_propagates_dependency_update_modes() {
        let default = args();
        assert_eq!(
            DependencyUpdateMode::from_args(&default).installer_flags(),
            (true, false)
        );

        let mut update_all = args();
        update_all.update_with_all_dependencies = true;
        let mode = DependencyUpdateMode::from_args(&update_all);
        assert_eq!(mode, DependencyUpdateMode::All);
        assert_eq!(mode.display_flag(), " --with-all-dependencies");
        assert_eq!(mode.installer_flags(), (true, true));

        let mut all = args();
        all.with_all_dependencies = true;
        assert_eq!(
            DependencyUpdateMode::from_args(&all),
            DependencyUpdateMode::All
        );

        let mut listed = args();
        listed.no_update_with_dependencies = true;
        let mode = DependencyUpdateMode::from_args(&listed);
        assert_eq!(mode, DependencyUpdateMode::ListedOnly);
        assert_eq!(mode.display_flag(), " --with-dependencies");
        assert_eq!(mode.installer_flags(), (false, false));
    }

    // Ported from Composer\Test\Command\RemoveCommandTest::
    // testWarningWhenRemovingPackageWithDeprecatedDependenciesFlag.
    #[test]
    fn composer_remove_warns_for_deprecated_update_with_dependencies_option() {
        let mut remove = args();
        assert_eq!(deprecated_dependency_warning(&remove), None);
        remove.update_with_dependencies = true;
        assert_eq!(
            deprecated_dependency_warning(&remove),
            Some(DEPRECATED_DEPENDENCY_UPDATE_WARNING)
        );
    }
}
