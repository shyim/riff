use anyhow::{Context, Result};
use compact_str::CompactString;
use composer_rs_semver::VersionParser;
use console::style;
use foldhash::{HashMap as FastHashMap, HashMapExt, HashSet as FastHashSet};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::{
    btree_map::Entry as BTreeEntry, hash_map::Entry, BTreeMap, HashMap, HashSet, VecDeque,
};
use std::sync::Arc;
use std::time::Duration;

use crate::autoload::{
    get_head_commit, AutoloadConfig, AutoloadGenerator, PackageAutoload, RootPackageInfo,
};
use crate::composer::Composer;
use crate::event::{
    PostAutoloadDumpEvent, PostInstallEvent, PostUpdateEvent, PreAutoloadDumpEvent,
    PreInstallEvent, PreUpdateEvent,
};
use crate::json::{ComposerJson, ComposerLock, LockedPackage};
use crate::package::{detect_root_version, Autoload, Package, RootVersion, Stability};
use crate::repository::InstalledRepository;
use crate::solver::{Policy, Pool, Request, Solver, Transaction};
use crate::util::{canonical_package_name, is_platform_package};

pub struct Installer {
    composer: Composer,
}

#[derive(Debug)]
pub struct UpdateResult {
    pub exit_code: i32,
    pub audit_installed_names: Option<FastHashSet<String>>,
}

impl UpdateResult {
    fn exit(exit_code: i32) -> Self {
        Self {
            exit_code,
            audit_installed_names: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    pub optimize_autoloader: bool,
    pub classmap_authoritative: bool,
    pub apcu_autoloader: bool,
    pub update_lock_only: bool,
    pub update_packages: Option<Vec<String>>,
    pub with_dependencies: bool,
    pub with_all_dependencies: bool,
    pub no_autoloader: bool,
    pub no_scripts: bool,
}

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub optimize_autoloader: bool,
    pub classmap_authoritative: bool,
    pub apcu_autoloader: bool,
    pub ignore_platform_reqs: bool,
    pub no_autoloader: bool,
    pub no_scripts: bool,
}

impl Installer {
    pub fn new(composer: Composer) -> Self {
        Self { composer }
    }

    pub async fn update(&self, options: UpdateOptions) -> Result<i32> {
        Ok(self.update_with_result(options).await?.exit_code)
    }

    pub fn composer_lock(&self) -> Option<&ComposerLock> {
        self.composer.composer_lock.as_ref()
    }

    pub async fn update_with_result(&self, options: UpdateOptions) -> Result<UpdateResult> {
        let composer_json = &self.composer.composer_json;
        let working_dir = &self.composer.working_dir;
        let install_config = self.composer.installation_manager.config();
        let dry_run = install_config.dry_run;
        let no_dev = install_config.no_dev;
        let prefer_lowest = install_config.prefer_lowest;
        let platform_packages = &self.composer.platform_packages;

        log::debug!("Reading {}/composer.json", working_dir.display());

        println!("{} Updating dependencies", style("Composer").green().bold());

        if dry_run {
            println!("{} Running in dry-run mode", style("Info:").cyan());
        }

        // Dispatch pre-update event
        if !options.no_scripts {
            let exit_code = self.composer.dispatch(&PreUpdateEvent::new(!no_dev))?;
            if exit_code != 0 {
                return Ok(UpdateResult::exit(exit_code));
            }
        }

        // Create progress spinner
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} {msg}")
                .unwrap(),
        );
        spinner.enable_steady_tick(Duration::from_millis(100));
        spinner.set_message("Loading repositories...");

        // Setup repository manager
        let repo_manager = self.composer.repository_manager.clone();

        spinner.set_message("Resolving dependencies...");

        // Get minimum stability (default to "stable" if not specified)
        let minimum_stability: Stability = composer_json
            .minimum_stability
            .as_deref()
            .unwrap_or("stable")
            .parse()
            .unwrap_or(Stability::Stable);

        log::debug!("Minimum stability: {:?}", minimum_stability);

        // Detect root package version
        let root_version = get_root_version(working_dir, composer_json);

        // Build package pool
        let mut pool = Pool::with_minimum_stability(minimum_stability);

        // Add root package to pool (for replace/provide/conflict handling)
        // Use add_platform_package to bypass stability filtering (root is always installed)
        let root_pkg = create_root_package(composer_json, &root_version);
        if !root_pkg.replace.is_empty() || !root_pkg.provide.is_empty() {
            log::debug!(
                "Root package version: {} (normalized: {})",
                root_pkg.pretty_version.as_deref().unwrap_or("N/A"),
                root_pkg.version
            );
            log::debug!("Root package replaces: {:?}", root_pkg.replace);
            log::debug!("Root package provides: {:?}", root_pkg.provide);
            let root_id = pool.add_platform_package(root_pkg);
            log::debug!("Added root package to pool with id {}", root_id);
        }

        // Collect packages that are replaced/provided by root - we don't need to load these
        // from repositories since the root package satisfies them
        let root_replaced: FastHashSet<CompactString> = composer_json
            .replace
            .keys()
            .chain(composer_json.provide.keys())
            .map(|name| CompactString::new(canonical_package_name(name).as_ref()))
            .collect();

        if !root_replaced.is_empty() {
            log::debug!(
                "Skipping repository lookup for root-replaced packages: {:?}",
                root_replaced
            );
        }

        // Add stability flags - sort for deterministic order
        let mut sorted_require: Vec<_> = composer_json.require.iter().collect();
        sorted_require.sort_by(|a, b| a.0.cmp(b.0));
        for (name, constraint) in sorted_require {
            if let Some(stability) = extract_stability_flag(constraint) {
                pool.add_stability_flag(name, stability);
                log::trace!("Stability flag for {}: {:?}", name, stability);
            }
        }
        let mut sorted_require_dev: Vec<_> = composer_json.require_dev.iter().collect();
        sorted_require_dev.sort_by(|a, b| a.0.cmp(b.0));
        for (name, constraint) in sorted_require_dev {
            if let Some(stability) = extract_stability_flag(constraint) {
                pool.add_stability_flag(name, stability);
                log::trace!("Stability flag for {}: {:?}", name, stability);
            }
        }

        // Add platform packages (bypass stability filtering - these are fixed system packages)
        for pkg in platform_packages {
            log::debug!("Platform package: {} {}", pkg.name, pkg.version);
            pool.add_platform_package(pkg.clone());
        }

        // Load packages with constraint-based filtering
        // This dramatically reduces the pool size by only loading versions that could
        // possibly be selected, similar to PHP Composer's demand-driven loading.
        let load_start = std::time::Instant::now();

        // Track loaded packages and pending packages with their constraints
        // Key = lowercase package name, Value = merged constraint string
        let mut loaded_packages: FastHashSet<CompactString> = root_replaced.clone();
        let mut pending_packages: FastHashMap<CompactString, CompactString> = FastHashMap::new();
        let mut http_request_count = 0usize;

        // Preserve repository name batches so deterministic ordering only has
        // to sort versions within each package name.
        let mut package_batches: BTreeMap<CompactString, Vec<Arc<Package>>> = BTreeMap::new();

        // Add root requirements with their constraints - sort for deterministic order
        let mut sorted_require: Vec<_> = composer_json.require.iter().collect();
        sorted_require.sort_by(|a, b| a.0.cmp(b.0));
        for (name, constraint) in sorted_require {
            if !is_platform_package(name) {
                let name = canonical_package_name(name);
                if !root_replaced.contains(name.as_ref()) {
                    pending_packages.insert(
                        CompactString::new(name.as_ref()),
                        CompactString::new(constraint),
                    );
                }
            }
        }
        let mut sorted_require_dev: Vec<_> = composer_json.require_dev.iter().collect();
        sorted_require_dev.sort_by(|a, b| a.0.cmp(b.0));
        for (name, constraint) in sorted_require_dev {
            if !is_platform_package(name) {
                let name_lower = canonical_package_name(name);
                if root_replaced.contains(name_lower.as_ref()) {
                    continue;
                }
                merge_pending_constraint(
                    &mut pending_packages,
                    CompactString::new(name_lower.as_ref()),
                    CompactString::new(constraint),
                );
            }
        }

        // Process packages in parallel batches for performance
        // Determinism is ensured by:
        // 1. Processing batches in sorted order
        // 2. Sorting packages before adding to pool
        // 3. Sorting HashMap iterations in rule generation
        loop {
            // Get pending packages sorted for deterministic batch processing
            let mut pending_list: Vec<(CompactString, CompactString)> =
                pending_packages.drain().collect();
            if pending_list.is_empty() {
                break;
            }
            pending_list.sort_by(|a, b| a.0.cmp(&b.0));

            // Filter out already loaded packages
            let to_load: Vec<(CompactString, CompactString)> = pending_list
                .into_iter()
                .filter(|(name, _)| !loaded_packages.contains(name))
                .collect();

            if to_load.is_empty() {
                continue;
            }

            // Mark all as loaded before parallel fetch to avoid duplicates
            for (name, _) in &to_load {
                loaded_packages.insert(name.clone());
            }

            spinner.set_message(format!("Loading {} packages...", to_load.len()));
            http_request_count += to_load.len();

            // Load packages in parallel
            let mut tasks = tokio::task::JoinSet::new();
            for (name, constraint) in to_load {
                let repo_manager = repo_manager.clone();
                tasks.spawn(async move {
                    let packages = repo_manager
                        .find_solver_packages_with_constraint(&name, &constraint)
                        .await;
                    (name, packages)
                });
            }

            // Collect results and process dependencies
            let mut new_deps: Vec<(CompactString, CompactString)> = Vec::new();

            while let Some(result) = tasks.join_next().await {
                if let Ok((name, packages)) = result {
                    log::trace!("HTTP: {} ({} versions)", name, packages.len());
                    for pkg in &packages {
                        // Collect dependencies
                        for (dep_name, dep_constraint) in &pkg.require {
                            if !is_platform_package(dep_name) {
                                let dep_name = canonical_package_name(dep_name);
                                if !loaded_packages.contains(dep_name.as_ref()) {
                                    log::trace!(
                                        "Adding dependency {} {} from {} {}",
                                        dep_name,
                                        dep_constraint,
                                        pkg.name,
                                        pkg.version
                                    );
                                    new_deps.push((
                                        CompactString::new(dep_name.as_ref()),
                                        dep_constraint.clone(),
                                    ));
                                }
                            }
                        }
                    }

                    match package_batches.entry(name) {
                        BTreeEntry::Vacant(entry) => {
                            entry.insert(packages);
                        }
                        BTreeEntry::Occupied(mut entry) => {
                            entry.get_mut().extend(packages);
                        }
                    }
                }
            }

            // Merge new dependencies into pending (after parallel fetch completes)
            // Sort first for deterministic merging
            new_deps.sort_by(|a, b| a.0.cmp(&b.0));
            for (dep_name, dep_constraint) in new_deps {
                if !loaded_packages.contains(&dep_name) {
                    merge_pending_constraint(&mut pending_packages, dep_name, dep_constraint);
                }
            }
        }

        if options
            .update_packages
            .as_ref()
            .is_some_and(|packages| !packages.is_empty())
        {
            if let Some(lock) = &self.composer.composer_lock {
                for package in lock.packages.iter().chain(lock.packages_dev.iter()) {
                    let package = Arc::new(Package::from(package));
                    let name = CompactString::new(canonical_package_name(&package.name).as_ref());
                    package_batches.entry(name).or_default().push(package);
                }
            }
        }

        // The map supplies deterministic package-name order, so only versions
        // need comparison within each canonical-name bucket.
        for (name, mut packages) in package_batches {
            debug_assert!(packages
                .iter()
                .all(|package| canonical_package_name(&package.name) == name.as_str()));
            packages.sort_by(|a, b| a.version.cmp(&b.version));
            for package in packages {
                pool.add_package_arc(package, None);
            }
        }

        log::info!(
            "Loaded {} packages ({} HTTP requests) in {:?}",
            pool.len(),
            http_request_count,
            load_start.elapsed()
        );
        log::debug!("Pool has {} packages after loading", pool.len());

        // Solver Request - sort for deterministic order
        let mut request = Request::new();
        let mut sorted_require: Vec<_> = composer_json.require.iter().collect();
        sorted_require.sort_by(|a, b| a.0.cmp(b.0));
        for (name, constraint) in sorted_require {
            if !is_platform_package(name) {
                request.require(name, constraint);
            }
        }
        let mut sorted_require_dev: Vec<_> = composer_json.require_dev.iter().collect();
        sorted_require_dev.sort_by(|a, b| a.0.cmp(b.0));
        for (name, constraint) in sorted_require_dev {
            if !is_platform_package(name) {
                request.require(name, constraint);
            }
        }

        // Add root package as fixed if it has replace/provide
        // This ensures the solver knows the root package is always installed
        // and its replaced/provided packages are available
        let root_pkg = create_root_package(composer_json, &root_version);
        if !root_pkg.replace.is_empty() || !root_pkg.provide.is_empty() {
            request.fix(root_pkg);
        }

        let preferred_versions = match (&options.update_packages, &self.composer.composer_lock) {
            (Some(packages_to_update), Some(lock)) if !packages_to_update.is_empty() => {
                let explicit_allowlist: HashSet<String> = packages_to_update
                    .iter()
                    .map(|name| canonical_package_name(name).into_owned())
                    .collect();
                let update_allowlist = if options.with_dependencies || options.with_all_dependencies
                {
                    let root_requirements: HashSet<_> = composer_json
                        .require
                        .keys()
                        .chain(composer_json.require_dev.keys())
                        .map(|name| canonical_package_name(name).into_owned())
                        .collect();
                    expand_update_allowlist(
                        &pool,
                        &explicit_allowlist,
                        &root_requirements,
                        options.with_all_dependencies,
                    )
                } else {
                    explicit_allowlist
                };

                let mut preferred = HashMap::new();
                for pkg in lock.packages.iter().chain(lock.packages_dev.iter()) {
                    let package_name = canonical_package_name(&pkg.name);
                    if !update_allowlist.contains(package_name.as_ref()) {
                        preferred.insert(package_name.into_owned(), pkg.version.clone());
                    }
                }
                log::debug!(
                    "Partial update: using {} preferred versions from lock file",
                    preferred.len()
                );
                preferred
            }
            _ => {
                log::debug!("Full update: no preferred versions, updating all packages");
                HashMap::new()
            }
        };

        let policy = Policy::new()
            .prefer_lowest(prefer_lowest)
            .preferred_versions(preferred_versions);
        let solver = Solver::new(&pool, &policy).with_optimization(true);

        let mut solver_result = match solver.solve(&request) {
            Ok(result) => result,
            Err(problems) => {
                spinner.finish_and_clear();
                eprintln!(
                    "{} Could not resolve dependencies",
                    style("Error:").red().bold()
                );
                for problem in problems.problems() {
                    eprintln!("  {}", problem.describe(&pool));
                }
                return Ok(UpdateResult::exit(1));
            }
        };

        let non_dev_roots: HashSet<String> = composer_json
            .require
            .keys()
            .filter(|name| !is_platform_package(name))
            .map(|name| canonical_package_name(name).into_owned())
            .collect();
        let selected_packages: Vec<_> = solver_result
            .packages
            .iter()
            .map(Arc::as_ref)
            .filter(|package| !is_platform_package(&package.name))
            .collect();
        let non_dev_packages = find_transitive_dependencies(&selected_packages, &non_dev_roots);
        let (selected_prod, selected_dev): (Vec<_>, Vec<_>) =
            selected_packages.iter().copied().partition(|package| {
                non_dev_packages.contains(canonical_package_name(&package.name).as_ref())
            });
        let identity_proves_lock_changed =
            self.composer.composer_lock.as_ref().is_none_or(|current| {
                selected_package_identities_changed(current, &selected_prod, &selected_dev)
            });
        let use_dry_run_projection = dry_run && identity_proves_lock_changed;

        solver_result.packages = solver_result
            .packages
            .iter()
            .map(|package| {
                let package = if use_dry_run_projection {
                    repo_manager.hydrate_package_for_transaction(package)
                } else {
                    repo_manager.hydrate_package(package)
                };
                Arc::new(package)
            })
            .collect();

        spinner.set_message("Installing packages...");

        let locked_packages = self.load_locked_packages(true);
        let lock_transaction = Transaction::from_packages(
            locked_packages,
            solver_result.packages.clone(),
            solver_result.aliases.clone(),
        );

        let packages: Vec<Package> = solver_result
            .packages
            .iter()
            .map(|p| p.as_ref().clone())
            .filter(|p| !is_platform_package(&p.name))
            .collect();

        self.composer.plugin_policy.validate(packages.iter())?;

        let lock_summary = lock_transaction.summary();

        let (prod_packages, dev_packages): (Vec<_>, Vec<_>) =
            packages.iter().partition(|package| {
                non_dev_packages.contains(canonical_package_name(&package.name).as_ref())
            });

        let install_count = lock_summary.installs;
        let update_count = lock_summary.updates;
        let removal_count = lock_summary.uninstalls;
        log::info!(
            "Lock file operations: {} installs, {} updates, {} removals",
            install_count,
            update_count,
            removal_count
        );

        let lock = (!use_dry_run_projection).then(|| {
            // Extract platform requirements while preserving composer.json order.
            let platform = composer_json
                .require
                .iter()
                .filter(|(name, _)| is_platform_package(name))
                .map(|(name, constraint)| (name.clone(), constraint.clone()))
                .collect();
            let platform_dev = composer_json
                .require_dev
                .iter()
                .filter(|(name, _)| is_platform_package(name))
                .map(|(name, constraint)| (name.clone(), constraint.clone()))
                .collect();

            ComposerLock {
                content_hash: crate::util::compute_content_hash(
                    &serde_json::to_string(composer_json).unwrap_or_default(),
                ),
                packages: prod_packages
                    .iter()
                    .map(|package| LockedPackage::from(*package))
                    .collect(),
                packages_dev: dev_packages
                    .iter()
                    .map(|package| LockedPackage::from(*package))
                    .collect(),
                minimum_stability: composer_json
                    .minimum_stability
                    .clone()
                    .unwrap_or_else(|| "stable".to_string()),
                prefer_stable: composer_json.prefer_stable.unwrap_or(false),
                prefer_lowest,
                platform,
                platform_dev,
                plugin_api_version: "2.9.0".to_string(),
                ..Default::default()
            }
        });

        let lock_file_changed = lock.as_ref().is_none_or(|lock| {
            self.composer
                .composer_lock
                .as_ref()
                .is_none_or(|current| !current.equivalent_for_write(lock))
        });

        // Only write lock file if there were changes
        if lock_file_changed && !dry_run {
            log::debug!("Writing lock file");
            let lock = lock
                .as_ref()
                .expect("non-dry-run updates always build a complete lock");
            let mut lock_content =
                serde_json::to_string_pretty(lock).context("Failed to serialize composer.lock")?;
            // Add trailing newline to match Composer's format
            lock_content.push('\n');
            std::fs::write(working_dir.join("composer.lock"), lock_content)
                .context("Failed to write composer.lock")?;
        }

        if options.update_lock_only {
            spinner.finish_and_clear();
            if lock_file_changed {
                println!("{} Lock file updated", style("Success:").green().bold());
            } else {
                println!("{} Lock file is up to date", style("Info:").cyan());
            }
            return Ok(UpdateResult::exit(0));
        }

        log::debug!("Installing dependencies from lock file");
        log::info!(
            "Package operations: {} installs, {} updates, {} removals",
            install_count,
            update_count,
            removal_count
        );

        let manager = &self.composer.installation_manager;
        let dev_names: HashSet<_> = dev_packages
            .iter()
            .map(|package| canonical_package_name(&package.name).into_owned())
            .collect();
        let packages_to_install: Vec<_> = solver_result
            .packages
            .iter()
            .filter(|package| !is_platform_package(&package.name))
            .filter(|package| {
                !no_dev || !dev_names.contains(canonical_package_name(&package.name).as_ref())
            })
            .cloned()
            .collect();
        let present_packages = self.load_actual_installed_packages().await;
        let audit_installed_names = dry_run.then(|| {
            present_packages
                .iter()
                .map(|package| canonical_package_name(&package.name).into_owned())
                .collect()
        });
        let mut transaction = Transaction::from_packages(
            present_packages,
            packages_to_install,
            solver_result.aliases,
        );
        transaction.sort();
        let result = manager
            .execute(&transaction)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to install packages: {}", e))?;

        spinner.finish_and_clear();

        let actually_installed: Vec<_> = result
            .installed
            .iter()
            .filter(|p| !is_platform_package(&p.name))
            .collect();

        for pkg in &actually_installed {
            log::debug!("Installed {} ({})", pkg.name, pkg.version);
            println!(
                "  {} {} ({})",
                style("-").green(),
                style(&pkg.name).white().bold(),
                style(&pkg.version).yellow()
            );
        }

        if !dry_run && options.no_autoloader {
            let lock = lock
                .as_ref()
                .expect("non-dry-run updates always build a complete lock");
            self.generate_installed_metadata(lock, &root_version, !no_dev)?;
        }

        if !dry_run && !options.no_autoloader {
            let lock = lock
                .as_ref()
                .expect("non-dry-run updates always build a complete lock");
            if !options.no_scripts {
                let exit_code = self.composer.dispatch(&PreAutoloadDumpEvent::new(
                    !no_dev,
                    options.optimize_autoloader || options.classmap_authoritative,
                ))?;
                if exit_code != 0 {
                    return Ok(UpdateResult::exit(exit_code));
                }
            }
            println!("{} Generating autoload files", style("Info:").cyan());

            let aliases_map: HashMap<String, Vec<String>> = HashMap::new();
            let dev_mode = !no_dev;

            let mut package_autoloads: Vec<PackageAutoload> = lock
                .packages
                .iter()
                .map(|lp| {
                    locked_package_to_autoload(
                        lp,
                        false,
                        &aliases_map,
                        manager.config().prefer_source,
                    )
                })
                .collect();
            if dev_mode {
                package_autoloads.extend(lock.packages_dev.iter().map(|lp| {
                    locked_package_to_autoload(
                        lp,
                        true,
                        &aliases_map,
                        manager.config().prefer_source,
                    )
                }));
            }

            let autoload_config = AutoloadConfig {
                vendor_dir: manager.config().vendor_dir.clone(),
                base_dir: working_dir.clone(),
                optimize: options.optimize_autoloader || options.classmap_authoritative,
                authoritative: options.classmap_authoritative,
                apcu: options.apcu_autoloader,
                suffix: Some(lock.content_hash.clone()),
                ..Default::default()
            };

            let generator = AutoloadGenerator::new(autoload_config);

            let root_autoload = root_autoload(composer_json, dev_mode);

            let root_package = create_root_package_info(
                composer_json,
                &root_version,
                working_dir,
                Vec::new(),
                dev_mode,
            );

            generator
                .generate(
                    &package_autoloads,
                    root_autoload.as_ref(),
                    Some(&root_package),
                )
                .context("Failed to generate autoloader")?;

            // Dispatch post-autoload-dump event (runs scripts and plugins)
            let arc_packages: Vec<Arc<Package>> =
                packages.iter().map(|p| Arc::new(p.clone())).collect();
            if !options.no_scripts {
                let event = PostAutoloadDumpEvent::new(
                    arc_packages,
                    !no_dev,
                    options.optimize_autoloader || options.classmap_authoritative,
                );
                let exit_code = self.composer.dispatch(&event)?;
                if exit_code != 0 {
                    return Ok(UpdateResult::exit(exit_code));
                }
            }
        }

        let total_changed = actually_installed.len() + result.updated.len();
        if total_changed > 0 || lock_file_changed {
            println!(
                "{} {} packages updated",
                style("Success:").green().bold(),
                total_changed
            );
        } else {
            println!("{} Nothing to update.", style("Info:").cyan());
        }

        if !dry_run {
            self.audit_abandoned_packages(&packages);
        }

        // Dispatch post-update event
        if !dry_run && !options.no_scripts {
            let exit_code = self.composer.dispatch(&PostUpdateEvent::new(!no_dev))?;
            if exit_code != 0 {
                return Ok(UpdateResult::exit(exit_code));
            }
        }

        Ok(UpdateResult {
            exit_code: 0,
            audit_installed_names,
        })
    }

    pub async fn install(&self, options: InstallOptions) -> Result<i32> {
        let composer_json = &self.composer.composer_json;
        let working_dir = &self.composer.working_dir;
        let install_config = self.composer.installation_manager.config();
        let dry_run = install_config.dry_run;
        let no_dev = install_config.no_dev;
        let lock = self
            .composer
            .composer_lock
            .as_ref()
            .context("No composer.lock file found")?;

        // Detect root package version
        let root_version = get_root_version(working_dir, composer_json);

        // Dispatch pre-install event
        if !options.no_scripts {
            let exit_code = self.composer.dispatch(&PreInstallEvent::new(!no_dev))?;
            if exit_code != 0 {
                return Ok(exit_code);
            }
        }

        // Convert locked packages
        let mut packages: Vec<Package> = lock.packages.iter().map(Package::from).collect();
        if !no_dev {
            packages.extend(lock.packages_dev.iter().map(Package::from));
        }
        self.composer.plugin_policy.validate(packages.iter())?;

        println!(
            "{} Installing dependencies from lock file",
            style("Composer").green().bold()
        );
        if dry_run {
            println!("{} Running in dry-run mode", style("Info:").cyan());
        }

        let progress = ProgressBar::new(packages.len() as u64);
        progress.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
                )
                .unwrap()
                .progress_chars("#>-"),
        );
        progress.enable_steady_tick(Duration::from_millis(100));

        let manager = &self.composer.installation_manager;
        let present_packages = self.load_actual_installed_packages().await;
        let desired_packages: Vec<_> = packages.iter().cloned().map(Arc::new).collect();
        let mut transaction =
            Transaction::from_packages(present_packages, desired_packages, Vec::new());
        transaction.sort();
        let result = manager
            .execute(&transaction)
            .await
            .context("Failed to install packages")?;

        progress.finish_and_clear();

        if !result.installed.is_empty() {
            for pkg in &result.installed {
                println!(
                    "  {} {} ({})",
                    style("-").green(),
                    style(&pkg.name).white().bold(),
                    style(&pkg.version).yellow()
                );
            }
        }

        if !dry_run && options.no_autoloader {
            self.generate_installed_metadata(lock, &root_version, !no_dev)?;
        }

        if !dry_run && !options.no_autoloader {
            // Dispatch pre-autoload-dump event
            if !options.no_scripts {
                let exit_code = self.composer.dispatch(&PreAutoloadDumpEvent::new(
                    !no_dev,
                    options.optimize_autoloader || options.classmap_authoritative,
                ))?;
                if exit_code != 0 {
                    return Ok(exit_code);
                }
            }

            println!("{} Generating autoload files", style("Info:").cyan());

            let mut aliases_map: HashMap<String, Vec<String>> = HashMap::new();
            for alias in &lock.aliases {
                aliases_map
                    .entry(alias.package.clone())
                    .or_default()
                    .push(alias.alias.clone());
            }
            let dev_mode = !no_dev;
            let mut package_autoloads: Vec<PackageAutoload> = lock
                .packages
                .iter()
                .map(|lp| {
                    locked_package_to_autoload(
                        lp,
                        false,
                        &aliases_map,
                        manager.config().prefer_source,
                    )
                })
                .collect();
            if dev_mode {
                package_autoloads.extend(lock.packages_dev.iter().map(|lp| {
                    locked_package_to_autoload(
                        lp,
                        true,
                        &aliases_map,
                        manager.config().prefer_source,
                    )
                }));
            }

            let autoload_config = AutoloadConfig {
                vendor_dir: manager.config().vendor_dir.clone(),
                base_dir: working_dir.clone(),
                optimize: options.optimize_autoloader || options.classmap_authoritative,
                authoritative: options.classmap_authoritative,
                apcu: options.apcu_autoloader,
                suffix: if !lock.content_hash.is_empty() {
                    Some(lock.content_hash.clone())
                } else {
                    None
                },
                ..Default::default()
            };

            let generator = AutoloadGenerator::new(autoload_config);
            // Root autoload from json
            let root_autoload = root_autoload(composer_json, dev_mode);
            let root_aliases = aliases_map
                .get(&composer_json.name.clone().unwrap_or_default())
                .cloned()
                .unwrap_or_default();
            let root_package = create_root_package_info(
                composer_json,
                &root_version,
                working_dir,
                root_aliases,
                dev_mode,
            );

            generator
                .generate(
                    &package_autoloads,
                    root_autoload.as_ref(),
                    Some(&root_package),
                )
                .context("Failed to generate autoloader")?;

            // Dispatch post-autoload-dump event (runs scripts and plugins)
            if !options.no_scripts {
                let arc_packages: Vec<Arc<Package>> =
                    packages.iter().map(|p| Arc::new(p.clone())).collect();
                let event = PostAutoloadDumpEvent::new(
                    arc_packages,
                    dev_mode,
                    options.optimize_autoloader || options.classmap_authoritative,
                );
                let exit_code = self.composer.dispatch(&event)?;
                if exit_code != 0 {
                    return Ok(exit_code);
                }
            }
        }

        println!(
            "{} {} packages installed",
            style("Success:").green().bold(),
            result.installed.len()
        );

        if !dry_run {
            self.audit_abandoned_packages(&packages);
        }

        // Dispatch post-install event
        if !options.no_scripts && !dry_run {
            let exit_code = self.composer.dispatch(&PostInstallEvent::new(!no_dev))?;
            if exit_code != 0 {
                return Ok(exit_code);
            }
        }

        Ok(0)
    }

    pub fn dump_autoload(
        &self,
        optimize: bool,
        authoritative: bool,
        apcu: bool,
        no_dev: bool,
        no_scripts: bool,
    ) -> Result<()> {
        let composer_json = &self.composer.composer_json;
        let working_dir = &self.composer.working_dir;
        let manager = &self.composer.installation_manager;

        // Detect root package version
        let root_version = get_root_version(working_dir, composer_json);

        if !no_scripts {
            let exit_code = self.composer.dispatch(&PreAutoloadDumpEvent::new(
                !no_dev,
                optimize || authoritative,
            ))?;
            if exit_code != 0 {
                anyhow::bail!("pre-autoload-dump script exited with code {}", exit_code);
            }
        }

        println!("{} Generating autoload files", style("Info:").cyan());

        let mut aliases_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut package_autoloads: Vec<PackageAutoload> = Vec::new();
        let mut all_installed_packages: Vec<Package> = Vec::new();
        let dev_mode = !no_dev;
        let mut suffix = None;

        if let Some(lock) = &self.composer.composer_lock {
            for alias in &lock.aliases {
                aliases_map
                    .entry(alias.package.clone())
                    .or_default()
                    .push(alias.alias.clone());
            }

            package_autoloads = lock
                .packages
                .iter()
                .map(|lp| {
                    locked_package_to_autoload(
                        lp,
                        false,
                        &aliases_map,
                        manager.config().prefer_source,
                    )
                })
                .collect();
            if dev_mode {
                package_autoloads.extend(lock.packages_dev.iter().map(|lp| {
                    locked_package_to_autoload(
                        lp,
                        true,
                        &aliases_map,
                        manager.config().prefer_source,
                    )
                }));
            }

            all_installed_packages = lock.packages.iter().map(Package::from).collect();
            if dev_mode {
                all_installed_packages.extend(lock.packages_dev.iter().map(Package::from));
            }

            if !lock.content_hash.is_empty() {
                suffix = Some(lock.content_hash.clone());
            }
        }

        let autoload_config = AutoloadConfig {
            vendor_dir: manager.config().vendor_dir.clone(),
            base_dir: working_dir.clone(),
            optimize: optimize || authoritative,
            authoritative,
            apcu,
            suffix,
            ..Default::default()
        };

        let generator = AutoloadGenerator::new(autoload_config);
        // Root autoload from json
        let root_autoload = root_autoload(composer_json, dev_mode);
        let root_aliases = aliases_map
            .get(&composer_json.name.clone().unwrap_or_default())
            .cloned()
            .unwrap_or_default();
        let root_package = create_root_package_info(
            composer_json,
            &root_version,
            working_dir,
            root_aliases,
            dev_mode,
        );

        generator
            .generate(
                &package_autoloads,
                root_autoload.as_ref(),
                Some(&root_package),
            )
            .context("Failed to generate autoloader")?;

        // Dispatch post-autoload-dump event (runs scripts and plugins)
        let arc_packages: Vec<Arc<Package>> = all_installed_packages
            .iter()
            .map(|p| Arc::new(p.clone()))
            .collect();
        if !no_scripts {
            let event =
                PostAutoloadDumpEvent::new(arc_packages, dev_mode, optimize || authoritative);
            self.composer.dispatch(&event)?;
        }

        if optimize || authoritative {
            println!(
                "{} Generated optimized autoload files",
                style("Success:").green().bold()
            );
        } else {
            println!(
                "{} Generated autoload files",
                style("Success:").green().bold()
            );
        }

        Ok(())
    }

    /// Load package versions recorded in composer.lock.
    fn load_locked_packages(&self, include_dev: bool) -> Vec<Arc<Package>> {
        let Some(lock) = &self.composer.composer_lock else {
            return Vec::new();
        };

        let mut packages: Vec<Arc<Package>> = lock
            .packages
            .iter()
            .map(|lp| Arc::new(Package::from(lp)))
            .collect();

        if include_dev {
            packages.extend(
                lock.packages_dev
                    .iter()
                    .map(|lp| Arc::new(Package::from(lp))),
            );
        }

        packages
    }

    /// Load what is actually present in vendor, rather than assuming the lock is installed.
    async fn load_actual_installed_packages(&self) -> Vec<Arc<Package>> {
        let repository = InstalledRepository::new(
            self.composer
                .installation_manager
                .config()
                .vendor_dir
                .clone(),
        );
        repository.load_transaction_packages().unwrap_or_default()
    }

    fn generate_installed_metadata(
        &self,
        lock: &ComposerLock,
        root_version: &RootVersion,
        dev_mode: bool,
    ) -> Result<()> {
        let manager = &self.composer.installation_manager;
        let composer_json = &self.composer.composer_json;
        let mut aliases_map: HashMap<String, Vec<String>> = HashMap::new();
        for alias in &lock.aliases {
            aliases_map
                .entry(alias.package.clone())
                .or_default()
                .push(alias.alias.clone());
        }

        let mut packages: Vec<PackageAutoload> = lock
            .packages
            .iter()
            .map(|package| {
                locked_package_to_autoload(
                    package,
                    false,
                    &aliases_map,
                    manager.config().prefer_source,
                )
            })
            .collect();
        if dev_mode {
            packages.extend(lock.packages_dev.iter().map(|package| {
                locked_package_to_autoload(
                    package,
                    true,
                    &aliases_map,
                    manager.config().prefer_source,
                )
            }));
        }

        let root_aliases = aliases_map
            .get(&composer_json.name.clone().unwrap_or_default())
            .cloned()
            .unwrap_or_default();
        let root_package = create_root_package_info(
            composer_json,
            root_version,
            &self.composer.working_dir,
            root_aliases,
            dev_mode,
        );
        let generator = AutoloadGenerator::new(AutoloadConfig {
            vendor_dir: manager.config().vendor_dir.clone(),
            base_dir: self.composer.working_dir.clone(),
            ..Default::default()
        });

        generator
            .generate_installed_metadata(&packages, Some(&root_package))
            .context("Failed to generate installed package metadata")
    }

    fn audit_abandoned_packages(&self, packages: &[Package]) {
        let mut abandoned_packages: Vec<_> = packages
            .iter()
            .filter(|p| p.is_abandoned() && !p.is_platform_package())
            .collect();

        if abandoned_packages.is_empty() {
            return;
        }

        abandoned_packages.sort_by(|a, b| a.name.cmp(&b.name));

        eprintln!();
        for pkg in abandoned_packages {
            if let Some(ref abandoned) = pkg.abandoned {
                let replacement = match abandoned.replacement() {
                    Some(repl) => format!("Use {} instead", repl),
                    None => "No replacement was suggested".to_string(),
                };
                eprintln!(
                    "{} Package {} is abandoned, you should avoid using it. {}.",
                    style("Warning:").yellow(),
                    pkg.name,
                    replacement
                );
            }
        }
    }
}

// Helpers

/// Detects and returns the root package version with logging.
///
/// This handles:
/// 1. COMPOSER_ROOT_VERSION environment variable
/// 2. Explicit version in composer.json
/// 3. Branch alias matching current git branch
/// 4. Git branch name as dev version
fn get_root_version(working_dir: &std::path::Path, composer_json: &ComposerJson) -> RootVersion {
    let branch_aliases = composer_json.get_branch_aliases();
    let root_version = detect_root_version(
        working_dir,
        composer_json.version.as_deref(),
        &branch_aliases,
    );

    log::info!(
        "Root package version: {} (from {})",
        root_version.pretty_version,
        root_version.source
    );

    root_version
}

fn root_autoload(composer_json: &ComposerJson, dev_mode: bool) -> Option<Autoload> {
    let mut autoload: Autoload = composer_json.autoload.clone().into();
    if dev_mode {
        autoload.merge(composer_json.autoload_dev.clone().into());
    }
    Some(autoload)
}

/// Creates a root package that can be added to the solver pool.
///
/// This creates a Package with the root's replace/provide/conflict declarations
/// so the solver knows what virtual packages the root provides.
fn create_root_package(composer_json: &ComposerJson, root_version: &RootVersion) -> Package {
    let name = composer_json
        .name
        .clone()
        .unwrap_or_else(|| "__root__".to_string());

    let mut pkg = Package::new(&name, &root_version.version);
    pkg.pretty_version = Some(root_version.pretty_version.clone().into());
    pkg.package_type = composer_json.package_type.clone().into();

    // Copy replace/provide/conflict from composer.json
    pkg.replace = composer_json.replace.clone().into();
    pkg.provide = composer_json.provide.clone().into();
    pkg.conflict = composer_json.conflict.clone().into();

    // Replace self.version with the actual root version
    pkg.replace_self_version();

    pkg
}

/// Creates a RootPackageInfo for autoload generation.
fn create_root_package_info(
    composer_json: &ComposerJson,
    root_version: &RootVersion,
    working_dir: &std::path::Path,
    aliases: Vec<String>,
    dev_mode: bool,
) -> RootPackageInfo {
    RootPackageInfo {
        name: composer_json
            .name
            .clone()
            .unwrap_or_else(|| "__root__".to_string()),
        pretty_version: root_version.pretty_version.clone(),
        version: root_version.version.clone(),
        reference: get_head_commit(working_dir),
        package_type: composer_json.package_type.clone(),
        aliases,
        dev_mode,
    }
}

fn extract_stability_flag(constraint: &str) -> Option<Stability> {
    if let Some(at_pos) = constraint.rfind('@') {
        let stability_str = &constraint[at_pos + 1..];
        let stability: Stability = stability_str.parse().ok()?;
        if stability != Stability::Stable {
            return Some(stability);
        }
    }
    None
}

fn merge_pending_constraint(
    pending: &mut FastHashMap<CompactString, CompactString>,
    name: CompactString,
    constraint: CompactString,
) {
    match pending.entry(name) {
        Entry::Occupied(mut entry) => {
            let existing = entry.get_mut();
            existing.reserve(4 + constraint.len());
            existing.push_str(" || ");
            existing.push_str(&constraint);
        }
        Entry::Vacant(entry) => {
            entry.insert(constraint);
        }
    }
}

fn selected_package_identities_changed(
    current: &ComposerLock,
    prod_packages: &[&Package],
    dev_packages: &[&Package],
) -> bool {
    fn list_changed(packages: &[&Package], locked: &[LockedPackage]) -> bool {
        packages.len() != locked.len()
            || packages.iter().zip(locked).any(|(package, locked)| {
                package.name != locked.name || package.pretty_version() != locked.version
            })
    }

    list_changed(prod_packages, &current.packages)
        || list_changed(dev_packages, &current.packages_dev)
}

fn find_transitive_dependencies(packages: &[&Package], roots: &HashSet<String>) -> HashSet<String> {
    let pkg_map: HashMap<String, &Package> = packages
        .iter()
        .map(|package| (canonical_package_name(&package.name).into_owned(), *package))
        .collect();

    let mut result: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = roots.iter().cloned().collect();

    while let Some(name) = queue.pop_front() {
        if result.contains(&name) {
            continue;
        }

        if let Some(pkg) = pkg_map.get(&name) {
            result.insert(name.clone());
            for (dep_name, _) in &pkg.require {
                if !is_platform_package(dep_name) {
                    let dependency = canonical_package_name(dep_name);
                    if !result.contains(dependency.as_ref()) {
                        queue.push_back(dependency.into_owned());
                    }
                }
            }
        } else {
            result.insert(name);
        }
    }
    result
}

fn expand_update_allowlist(
    pool: &Pool,
    explicit: &HashSet<String>,
    root_requirements: &HashSet<String>,
    include_root_requirements: bool,
) -> HashSet<String> {
    let mut allowlist = explicit.clone();
    let mut queue: VecDeque<_> = explicit.iter().cloned().collect();

    while let Some(package_name) = queue.pop_front() {
        for package_id in pool.packages_by_name(&package_name) {
            let Some(package) = pool.package(package_id) else {
                continue;
            };
            for dependency in package.require.keys() {
                if is_platform_package(dependency) {
                    continue;
                }
                let dependency = canonical_package_name(dependency);
                if !include_root_requirements
                    && root_requirements.contains(dependency.as_ref())
                    && !explicit.contains(dependency.as_ref())
                {
                    continue;
                }
                let dependency = dependency.into_owned();
                if allowlist.insert(dependency.clone()) {
                    queue.push_back(dependency);
                }
            }
        }
    }

    allowlist
}

fn locked_package_to_autoload(
    lp: &LockedPackage,
    is_dev: bool,
    aliases_map: &HashMap<String, Vec<String>>,
    prefer_source: bool,
) -> PackageAutoload {
    let autoload = Autoload::from(&lp.autoload);
    let requires: Vec<String> = lp
        .require
        .keys()
        .filter(|k| !is_platform_package(k))
        .cloned()
        .collect();
    let reference = lp
        .source
        .as_ref()
        .map(|s| s.reference.clone())
        .or_else(|| lp.dist.as_ref().and_then(|d| d.reference.clone()));
    let aliases = aliases_map.get(&lp.name).cloned().unwrap_or_default();

    let normalized_version = VersionParser::new()
        .normalize(&lp.version)
        .unwrap_or_else(|_| lp.version.clone());
    let package = Package::from(lp);
    let installation_source =
        if lp.source.is_some() && (package.is_dev() || prefer_source || lp.dist.is_none()) {
            Some("source".to_string())
        } else if lp.dist.is_some() {
            Some("dist".to_string())
        } else if lp.source.is_some() {
            Some("source".to_string())
        } else {
            None
        };

    PackageAutoload {
        name: lp.name.clone(),
        autoload,
        install_path: lp.name.clone(),
        requires,
        pretty_version: Some(lp.version.clone()),
        version: Some(normalized_version),
        reference,
        package_type: lp.package_type.clone(),
        dev_requirement: is_dev,
        aliases,
        replaces: lp.replace.clone(),
        provides: lp.provide.clone(),
        locked_package: Some(lp.clone()),
        installation_source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_package_identity_proves_lock_change() {
        let package = Package::new("vendor/package", "2.0.0.0");
        let current = ComposerLock {
            packages: vec![LockedPackage {
                name: "vendor/package".into(),
                version: "1.0.0".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        assert!(selected_package_identities_changed(
            &current,
            &[&package],
            &[]
        ));
    }

    #[test]
    fn matching_identity_requires_full_lock_comparison() {
        let mut package = Package::new("vendor/package", "1.0.0.0");
        package.pretty_version = Some("1.0.0".into());
        package.description = Some("metadata may still differ".into());
        let current = ComposerLock {
            packages: vec![LockedPackage {
                name: "vendor/package".into(),
                version: "1.0.0".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        assert!(!selected_package_identities_changed(
            &current,
            &[&package],
            &[]
        ));
    }

    #[test]
    fn production_and_development_identity_lists_are_distinct() {
        let package = Package::new("vendor/package", "1.0.0.0");
        let current = ComposerLock {
            packages_dev: vec![LockedPackage {
                name: "vendor/package".into(),
                version: "1.0.0.0".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        assert!(selected_package_identities_changed(
            &current,
            &[&package],
            &[]
        ));
    }

    #[test]
    fn update_dependency_allowlist_respects_root_requirement_mode() {
        let mut pool = Pool::new();
        let mut target = Package::new("fixture/target", "2.0.0");
        target
            .require
            .insert("fixture/dependency".to_string(), "^2.0".to_string());
        target
            .require
            .insert("fixture/root-dependency".to_string(), "^2.0".to_string());
        pool.add_package(target);

        let mut dependency = Package::new("fixture/dependency", "2.0.0");
        dependency
            .require
            .insert("fixture/transitive".to_string(), "^1.0".to_string());
        pool.add_package(dependency);

        let explicit = HashSet::from(["fixture/target".to_string()]);
        let root_requirements = HashSet::from(["fixture/root-dependency".to_string()]);
        let dependencies_only =
            expand_update_allowlist(&pool, &explicit, &root_requirements, false);
        assert!(dependencies_only.contains("fixture/dependency"));
        assert!(dependencies_only.contains("fixture/transitive"));
        assert!(!dependencies_only.contains("fixture/root-dependency"));

        let all_dependencies = expand_update_allowlist(&pool, &explicit, &root_requirements, true);
        assert!(all_dependencies.contains("fixture/root-dependency"));
    }

    #[test]
    fn pending_constraints_merge_in_insertion_order() {
        let mut pending = FastHashMap::new();
        merge_pending_constraint(&mut pending, "vendor/package".into(), "^1".into());
        merge_pending_constraint(&mut pending, "vendor/package".into(), "^2".into());
        merge_pending_constraint(&mut pending, "vendor/package".into(), "^3".into());

        assert_eq!(pending.get("vendor/package").unwrap(), "^1 || ^2 || ^3");
    }
}
