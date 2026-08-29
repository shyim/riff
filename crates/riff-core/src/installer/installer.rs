use crate::output::style;
use anyhow::{Context, Result};
use compact_str::CompactString;
use foldhash::{HashMap as FastHashMap, HashMapExt, HashSet as FastHashSet};
use indicatif::{ProgressBar, ProgressStyle};
use regex::Regex;
use riff_semver::{Semver, VersionParser};
use std::collections::{
    btree_map::Entry as BTreeEntry, hash_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet, VecDeque,
};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use super::dependency_policy::{PackagePolicy, PolicyPhase, PolicyViolation};
use super::suggestions::SuggestedPackagesReporter;
use crate::autoload::{
    get_head_commit, AutoloadConfig, AutoloadGenerator, ClassMapGenerator, PackageAutoload,
    PlatformCheckRequirements, RootPackageInfo,
};
use crate::config::PlatformCheck;
use crate::event::{
    DependencyOperation, PostAutoloadDumpEvent, PostInstallEvent, PostUpdateEvent,
    PreAutoloadDumpEvent, PreInstallEvent, PreOperationsExecEvent, PreUpdateEvent,
};
use crate::json::{LockAlias, LockedPackage, RiffLockfile, RiffManifest};
use crate::package::{
    detect_root_version_with_non_feature_branches, package_name_matches, parse_branch_aliases,
    parse_inline_alias, validate_package_metadata, AliasPackage, Autoload, Package, RootVersion,
    Stability, DEFAULT_BRANCH_ALIAS,
};
use crate::policy_config::PolicyScope;
use crate::repository::InstalledRepository;
use crate::riff::Riff;
use crate::solver::{PackageId, Policy, Pool, Request, Solver, Transaction};
use crate::util::{canonical_package_name, is_platform_package};
use tokio::sync::Semaphore;

pub struct Installer {
    riff: Riff,
}

#[derive(Default)]
struct PolicyDiagnostics {
    advisories: BTreeMap<String, AdvisoryDiagnostics>,
    other: BTreeSet<String>,
}

#[derive(Default)]
struct AdvisoryDiagnostics {
    versions: BTreeSet<String>,
    identifiers: BTreeSet<String>,
}

impl PolicyDiagnostics {
    fn record(&mut self, package: &Package, violation: PolicyViolation) {
        match violation {
            PolicyViolation::Advisory(advisory) => {
                let summary = self.advisories.entry(package.name.clone()).or_default();
                summary.versions.insert(package.pretty_version().to_owned());
                summary.identifiers.insert(advisory.advisory_id);
            }
            violation => {
                self.other.insert(violation.diagnostic(package));
            }
        }
    }

    fn lines(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.advisories.len() + self.other.len());
        for (package, summary) in &self.advisories {
            let version_count = summary.versions.len();
            let advisory_count = summary.identifiers.len();
            let identifiers = summary
                .identifiers
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>();
            let omitted = advisory_count.saturating_sub(identifiers.len());
            let mut identifier_list = identifiers.join(", ");
            if omitted > 0 {
                identifier_list.push_str(&format!(", and {omitted} more"));
            }
            lines.push(format!(
                "Package {package}: {version_count} candidate {} excluded by {advisory_count} security {} ({identifier_list}).",
                if version_count == 1 { "version was" } else { "versions were" },
                if advisory_count == 1 { "advisory" } else { "advisories" },
            ));
        }
        lines.extend(self.other.iter().cloned());
        lines
    }

    fn has_advisories(&self) -> bool {
        !self.advisories.is_empty()
    }
}

const MAX_CONCURRENT_CLASSMAP_SCANS: usize = 4;
type ClassmapScan = (std::path::PathBuf, HashMap<String, std::path::PathBuf>);
type ClassmapScanTask = tokio::task::JoinHandle<Result<Vec<ClassmapScan>>>;

#[derive(Clone)]
struct AutoloadScanObserver {
    paths: Arc<HashMap<String, Vec<std::path::PathBuf>>>,
    excludes: Arc<Vec<Regex>>,
    semaphore: Arc<Semaphore>,
    tasks: Arc<std::sync::Mutex<Vec<ClassmapScanTask>>>,
}

impl AutoloadScanObserver {
    fn new(paths: HashMap<String, Vec<std::path::PathBuf>>, excludes: Vec<Regex>) -> Self {
        Self {
            paths: Arc::new(paths),
            excludes: Arc::new(excludes),
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_CLASSMAP_SCANS)),
            tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    async fn finish(
        &self,
    ) -> Result<HashMap<std::path::PathBuf, HashMap<String, std::path::PathBuf>>> {
        let tasks = std::mem::take(&mut *self.tasks.lock().expect("autoload scan tasks lock"));
        let mut scans = HashMap::new();
        for task in tasks {
            for (path, classes) in task.await.context("classmap scan task failed")?? {
                scans.insert(path, classes);
            }
        }
        Ok(scans)
    }
}

impl super::PackageInstallObserver for AutoloadScanObserver {
    fn package_ready(&self, package: &Package, _install_path: &std::path::Path) {
        let Some(paths) = self.paths.get(&package.name).cloned() else {
            return;
        };
        let excludes = self.excludes.clone();
        let semaphore = self.semaphore.clone();
        let task = tokio::spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("scan semaphore is open");
            tokio::task::spawn_blocking(move || {
                let generator = ClassMapGenerator::new();
                paths
                    .into_iter()
                    .map(|path| {
                        let classes = generator.generate_with_excludes(&path, &excludes)?;
                        Ok((path, classes))
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .await
            .context("classmap scan worker failed")?
        });
        self.tasks
            .lock()
            .expect("autoload scan tasks lock")
            .push(task);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DumpAutoloadOptions {
    pub optimize: bool,
    pub authoritative: bool,
    pub apcu: bool,
    pub no_dev: bool,
    pub strict_psr: bool,
    pub no_scripts: bool,
    pub dry_run: bool,
}

#[derive(Debug)]
pub struct UpdateResult {
    pub exit_code: i32,
    pub audit_installed_names: Option<FastHashSet<String>>,
    pub updated_package_names: Vec<String>,
    pub updated_package_versions: HashMap<String, String>,
    pub updated_package_branch_aliases: HashMap<String, String>,
}

impl UpdateResult {
    fn exit(exit_code: i32) -> Self {
        Self {
            exit_code,
            audit_installed_names: None,
            updated_package_names: Vec::new(),
            updated_package_versions: HashMap::new(),
            updated_package_branch_aliases: HashMap::new(),
        }
    }

    fn success(
        updated_package_names: Vec<String>,
        updated_package_versions: HashMap<String, String>,
        updated_package_branch_aliases: HashMap<String, String>,
    ) -> Self {
        Self {
            exit_code: 0,
            audit_installed_names: None,
            updated_package_names,
            updated_package_versions,
            updated_package_branch_aliases,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    pub optimize_autoloader: bool,
    pub classmap_authoritative: bool,
    pub apcu_autoloader: bool,
    pub apcu_autoloader_prefix: Option<String>,
    pub update_lock_only: bool,
    /// Refresh source/dist URLs and mirrors without changing the locked package set.
    pub update_mirrors: bool,
    pub update_packages: Option<Vec<String>>,
    pub with_dependencies: bool,
    pub with_all_dependencies: bool,
    pub no_autoloader: bool,
    pub no_scripts: bool,
    pub no_install: bool,
    pub minimal_changes: bool,
    pub root_requirements_only: bool,
    pub temporary_constraints: HashMap<String, String>,
    /// Restrict locked packages to versions sharing their current major/minor series.
    pub patch_only: bool,
    /// Deprecated alias of `no_blocking`; disables every dependency blocker.
    pub no_security_blocking: bool,
    /// Do not exclude candidates because of any dependency policy.
    pub no_blocking: bool,
    pub ignore_platform_requirements: PlatformRequirementFilter,
}

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub optimize_autoloader: bool,
    pub classmap_authoritative: bool,
    pub apcu_autoloader: bool,
    pub apcu_autoloader_prefix: Option<String>,
    pub ignore_platform_requirements: PlatformRequirementFilter,
    pub no_autoloader: bool,
    pub no_scripts: bool,
    /// Deprecated alias of `no_blocking`; disables every dependency blocker.
    pub no_security_blocking: bool,
    /// Do not block locked packages because of any dependency policy.
    pub no_blocking: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PlatformRequirementFilter {
    pub all: bool,
    pub requirements: Vec<String>,
}

impl PlatformRequirementFilter {
    fn ignores(&self, package: &str) -> bool {
        is_platform_package(package)
            && (self.all
                || self.requirements.iter().any(|pattern| {
                    !pattern.ends_with('+') && platform_pattern_matches(pattern, package)
                }))
    }

    fn ignores_upper_bound(&self, package: &str) -> bool {
        is_platform_package(package)
            && (self.all
                || self.requirements.iter().any(|pattern| {
                    platform_pattern_matches(pattern.trim_end_matches('+'), package)
                }))
    }

    fn filter_constraint(&self, package: &str, constraint: &str) -> String {
        if self.ignores(package) {
            return "*".to_string();
        }
        if !self.ignores_upper_bound(package) {
            return constraint.to_string();
        }
        let Ok(parsed) = VersionParser::new().parse_constraints(constraint) else {
            return constraint.to_string();
        };
        let upper_bound = parsed.upper_bound();
        if upper_bound.is_positive_infinity() {
            constraint.to_string()
        } else {
            format!("{constraint} || >= {}", upper_bound.version())
        }
    }
}

fn platform_pattern_matches(pattern: &str, package: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        package
            .get(..prefix.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
    } else {
        package.eq_ignore_ascii_case(pattern)
    }
}

impl Installer {
    pub fn new(riff: Riff) -> Self {
        Self { riff }
    }

    pub async fn update(&self, options: UpdateOptions) -> Result<i32> {
        Ok(self.update_with_result(options).await?.exit_code)
    }

    pub fn lockfile(&self) -> Option<&RiffLockfile> {
        self.riff.lockfile.as_ref()
    }

    pub async fn update_with_result(&self, options: UpdateOptions) -> Result<UpdateResult> {
        let manifest = &self.riff.manifest;
        let working_dir = &self.riff.working_dir;
        let install_config = self.riff.installation_manager.config();
        let dry_run = install_config.dry_run;
        let download_only = install_config.download_only;
        let no_dev = install_config.no_dev;
        let prefer_lowest = install_config.prefer_lowest;
        let prefer_stable =
            install_config.prefer_stable || self.riff.manifest.prefer_stable.unwrap_or(false);
        let platform_packages = &self.riff.platform_packages;

        let mut locked_package_identities = HashSet::new();
        if options.update_mirrors && self.riff.lockfile.is_none() {
            crate::errln!(self.riff.output(),
                "Cannot update lock file information without a lock file present. Run `riff update` to generate a lock file."
            );
            return Ok(UpdateResult::exit(3));
        }
        if options
            .update_packages
            .as_ref()
            .is_some_and(|packages| !packages.is_empty())
            && self.riff.lockfile.is_none()
        {
            crate::errln!(self.riff.output(),
                "Cannot update only a partial set of packages without a lock file present. Run `riff update` to generate a lock file."
            );
            return Ok(UpdateResult::exit(3));
        }

        if let (Some(packages), Some(lock)) = (&options.update_packages, &self.riff.lockfile) {
            let locked_names: HashSet<_> = lock
                .all_packages()
                .map(|package| canonical_package_name(&package.name).into_owned())
                .collect();
            for package in packages {
                let canonical = canonical_package_name(package);
                let matched = locked_names
                    .iter()
                    .any(|locked| package_name_matches(canonical.as_ref(), locked));
                if matched {
                    continue;
                }
                if package.contains('*') {
                    crate::warnln!(
                        self.riff.output(),
                        "Pattern \"{}\" listed for update does not match any locked packages.",
                        package
                    );
                } else {
                    crate::warnln!(
                        self.riff.output(),
                        "Package \"{}\" listed for update is not locked.",
                        package
                    );
                }
            }
        }

        if let Some(root_name) = manifest.name.as_deref() {
            let requires_self = manifest
                .require
                .keys()
                .chain(manifest.require_dev.keys())
                .any(|required| required.eq_ignore_ascii_case(root_name));
            if requires_self {
                crate::errln!(
                    self.riff.output(),
                    "Root package '{}' cannot require itself in its composer.json",
                    root_name
                );
                crate::errln!(
                    self.riff.output(),
                    "Did you accidentally name your root package after an external package?"
                );
                return Ok(UpdateResult::exit(1));
            }
        }

        log::debug!("Reading {}/composer.json", working_dir.display());

        crate::outln!(
            self.riff.output(),
            "{} Updating dependencies",
            style("Riff").green().bold()
        );

        if dry_run {
            crate::outln!(
                self.riff.output(),
                "{} Running in dry-run mode",
                style("Info:").cyan()
            );
        }

        // Dispatch pre-update event
        if !dry_run && !options.no_scripts {
            let exit_code = self.riff.dispatch(&PreUpdateEvent::new(!no_dev)).await?;
            if exit_code != 0 {
                return Ok(UpdateResult::exit(exit_code));
            }
        }

        // Create progress spinner
        let spinner = if self.riff.output().progress_enabled() {
            let spinner = ProgressBar::new_spinner();
            spinner.set_style(
                ProgressStyle::default_spinner()
                    .template("{spinner:.green} {msg}")
                    .unwrap(),
            );
            spinner.enable_steady_tick(Duration::from_millis(100));
            spinner.set_message("Loading repositories...");
            spinner
        } else {
            ProgressBar::hidden()
        };

        // Setup repository manager
        let repo_manager = self.riff.repository_manager.clone();

        spinner.set_message("Resolving dependencies...");

        // Get minimum stability (default to "stable" if not specified)
        let minimum_stability: Stability = manifest
            .minimum_stability
            .as_deref()
            .unwrap_or("stable")
            .parse()
            .unwrap_or(Stability::Stable);

        log::debug!("Minimum stability: {:?}", minimum_stability);

        let stability_flags = root_stability_flags(manifest, minimum_stability);
        let root_references = root_references(manifest);
        let plugin_constraints = self
            .riff
            .plugins()
            .prepare_solver_constraints(&self.riff)
            .await?;

        // Detect root package version
        let root_version = get_root_version(working_dir, manifest);

        // Build package pool
        let mut pool = Pool::with_minimum_stability(minimum_stability);

        // Add root package to pool (for replace/provide/conflict handling)
        // Use add_platform_package to bypass stability filtering (root is always installed)
        let mut root_pkg = create_root_package(manifest, &root_version);
        filter_package_platform_requirements(&mut root_pkg, &options.ignore_platform_requirements);
        let root_id = {
            log::debug!(
                "Root package version: {} (normalized: {})",
                root_pkg.pretty_version.as_deref().unwrap_or("N/A"),
                root_pkg.version
            );
            log::debug!("Root package replaces: {:?}", root_pkg.replace);
            log::debug!("Root package provides: {:?}", root_pkg.provide);
            let root_id = pool.add_platform_package(root_pkg);
            log::debug!("Added root package to pool with id {}", root_id);
            root_id
        };

        // Collect packages that are replaced/provided by root - we don't need to load these
        // from repositories since the root package satisfies them
        let root_replaced: FastHashSet<CompactString> = manifest
            .replace
            .keys()
            .chain(manifest.provide.keys())
            .map(|name| CompactString::new(canonical_package_name(name).as_ref()))
            .collect();

        if !root_replaced.is_empty() {
            log::debug!(
                "Skipping repository lookup for root-replaced packages: {:?}",
                root_replaced
            );
        }

        // Add root stability flags in deterministic order.
        let mut sorted_stability_flags: Vec<_> = stability_flags.iter().collect();
        sorted_stability_flags.sort_by(|a, b| a.0.cmp(b.0));
        for (name, stability) in sorted_stability_flags {
            pool.add_stability_flag(name, *stability);
            log::trace!("Stability flag for {}: {:?}", name, stability);
        }

        // Add platform packages (bypass stability filtering - these are fixed system packages)
        for pkg in platform_packages {
            if options
                .temporary_constraints
                .get(canonical_package_name(&pkg.name).as_ref())
                .is_some_and(|constraint| !Semver::satisfies(&pkg.version, constraint))
            {
                continue;
            }
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
        // Platform-requirement filtering clones the solver projection. Retain
        // the repository-owned Arc so deferred install metadata can still be
        // hydrated after the solver selects the transformed package.
        let mut solver_package_hydration_sources: FastHashMap<usize, Arc<Package>> =
            FastHashMap::new();
        let mut canonical_priority_blocks = BTreeSet::new();

        if options.update_mirrors {
            // A metadata refresh is defined by the lock, not by the current
            // root requirements. This deliberately ignores newly-added root
            // requirements until a normal update is requested.
            for package in self
                .riff
                .lockfile
                .iter()
                .flat_map(RiffLockfile::all_packages)
            {
                pending_packages.insert(
                    CompactString::new(canonical_package_name(&package.name).as_ref()),
                    CompactString::new(&package.version),
                );
            }
        } else {
            // Add root requirements with their constraints - sort for deterministic order
            let mut sorted_require: Vec<_> = manifest.require.iter().collect();
            sorted_require.sort_by(|a, b| a.0.cmp(b.0));
            for (name, constraint) in sorted_require {
                if !is_platform_package(name) {
                    let name = canonical_package_name(name);
                    if !root_replaced.contains(name.as_ref()) {
                        let constraint = parse_inline_alias(constraint)
                            .map(|(actual, _)| actual)
                            .unwrap_or_else(|| constraint.clone());
                        pending_packages.insert(
                            CompactString::new(name.as_ref()),
                            CompactString::new(constraint),
                        );
                    }
                }
            }
            let mut sorted_require_dev: Vec<_> = manifest.require_dev.iter().collect();
            sorted_require_dev.sort_by(|a, b| a.0.cmp(b.0));
            for (name, constraint) in sorted_require_dev {
                if !is_platform_package(name) {
                    let name_lower = canonical_package_name(name);
                    if root_replaced.contains(name_lower.as_ref()) {
                        continue;
                    }
                    let constraint = parse_inline_alias(constraint)
                        .map(|(actual, _)| actual)
                        .unwrap_or_else(|| constraint.clone());
                    merge_pending_constraint(
                        &mut pending_packages,
                        CompactString::new(name_lower.as_ref()),
                        CompactString::new(constraint),
                    );
                }
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
                let constraint = plugin_constraints.rewrite(&name, constraint);
                tasks.spawn(async move {
                    let lookup = repo_manager
                        .find_solver_packages_with_diagnostics(&name, &constraint)
                        .await;
                    (name, lookup)
                });
            }

            // Collect results and process dependencies
            let mut new_deps: Vec<(CompactString, CompactString)> = Vec::new();

            while let Some(result) = tasks.join_next().await {
                if let Ok((name, lookup)) = result {
                    if lookup.blocked_by_higher_priority_repository {
                        canonical_priority_blocks.insert(name.to_string());
                    }
                    let packages = crate::repository::RepositoryUtils::filter_solver_candidates(
                        &name,
                        lookup.packages,
                        |package_name| {
                            loaded_packages.contains(canonical_package_name(package_name).as_ref())
                        },
                    );
                    let packages: Vec<_> = packages
                        .into_iter()
                        .filter(|package| {
                            package_satisfies_temporary_constraints(
                                package,
                                &options.temporary_constraints,
                            )
                        })
                        .filter(|package| {
                            !options.patch_only
                                || patch_update_candidate_allowed(
                                    self.riff.lockfile.as_ref(),
                                    package,
                                )
                        })
                        .map(|package| {
                            if options.ignore_platform_requirements.all
                                || !options.ignore_platform_requirements.requirements.is_empty()
                            {
                                let hydration_source = Arc::clone(&package);
                                let mut package = package.as_ref().clone();
                                filter_package_platform_requirements(
                                    &mut package,
                                    &options.ignore_platform_requirements,
                                );
                                let package = Arc::new(package);
                                solver_package_hydration_sources
                                    .insert(Arc::as_ptr(&package) as usize, hydration_source);
                                package
                            } else {
                                package
                            }
                        })
                        .collect();
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

                    for package in packages {
                        let package_name =
                            CompactString::new(canonical_package_name(&package.name).as_ref());
                        loaded_packages.insert(package_name.clone());
                        match package_batches.entry(package_name) {
                            BTreeEntry::Vacant(entry) => {
                                entry.insert(vec![package]);
                            }
                            BTreeEntry::Occupied(mut entry) => {
                                entry.get_mut().push(package);
                            }
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

        if options.update_mirrors
            || options
                .update_packages
                .as_ref()
                .is_some_and(|packages| !packages.is_empty())
        {
            if let Some(lock) = &self.riff.lockfile {
                let root_requirement_names: HashSet<String> = manifest
                    .require
                    .keys()
                    .chain(manifest.require_dev.keys())
                    .filter(|name| !is_platform_package(name))
                    .map(|name| canonical_package_name(name).into_owned())
                    .collect();
                let lock_injection_exclusions =
                    options
                        .update_packages
                        .as_ref()
                        .map_or_else(HashSet::new, |patterns| {
                            expand_update_allowlist_before_lock_injection(
                                patterns,
                                &package_batches,
                                lock,
                                &root_requirement_names,
                                options.with_dependencies || options.with_all_dependencies,
                                options.with_all_dependencies,
                            )
                        });
                for package in lock.packages.iter().chain(lock.packages_dev.iter()) {
                    if locked_package_is_symlinked_path(package) {
                        continue;
                    }
                    let package_name = canonical_package_name(&package.name);
                    let explicitly_updated =
                        options.update_packages.as_ref().is_some_and(|patterns| {
                            patterns
                                .iter()
                                .any(|pattern| package_name_matches(pattern, package_name.as_ref()))
                        });
                    let repository_candidate_available = package_batches
                        .get(package_name.as_ref())
                        .is_some_and(|packages| !packages.is_empty());
                    if options.update_mirrors && repository_candidate_available {
                        continue;
                    }
                    if !options.update_mirrors
                        && (explicitly_updated
                            || lock_injection_exclusions.contains(package_name.as_ref()))
                    {
                        continue;
                    }
                    let mut package = Package::from(package);
                    if !package_satisfies_temporary_constraints(
                        &package,
                        &options.temporary_constraints,
                    ) {
                        continue;
                    }
                    filter_package_platform_requirements(
                        &mut package,
                        &options.ignore_platform_requirements,
                    );
                    let package = Arc::new(package);
                    locked_package_identities.insert(Arc::as_ptr(&package) as usize);
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
                let package = if let Some(reference) = root_references.get(name.as_str()) {
                    let hydration_source = solver_package_hydration_sources
                        .get(&(Arc::as_ptr(&package) as usize))
                        .unwrap_or(&package);
                    let mut package = repo_manager.hydrate_package(hydration_source);
                    package.set_references(reference);
                    Arc::new(package)
                } else {
                    package
                };
                pool.add_package_arc(package, None);
            }
        }
        add_package_aliases_with_root_exclusions(&mut pool, manifest, &locked_package_identities);

        log::info!(
            "Loaded {} packages ({} HTTP requests) in {:?}",
            pool.len(),
            http_request_count,
            load_start.elapsed()
        );
        log::debug!("Pool has {} packages after loading", pool.len());

        // Solver Request - sort for deterministic order
        let mut request = Request::new();
        if options.update_mirrors {
            for package in self
                .riff
                .lockfile
                .iter()
                .flat_map(RiffLockfile::all_packages)
            {
                request.require(&package.name, &package.version);
            }
        } else {
            let mut sorted_require: Vec<_> = manifest.require.iter().collect();
            sorted_require.sort_by(|a, b| a.0.cmp(b.0));
            for (name, constraint) in sorted_require {
                if is_platform_package(name) {
                    if !options.ignore_platform_requirements.ignores(name) {
                        request.require(
                            name,
                            options
                                .ignore_platform_requirements
                                .filter_constraint(name, constraint),
                        );
                    }
                } else {
                    request.require(name, constraint);
                }
            }
            let mut sorted_require_dev: Vec<_> = manifest.require_dev.iter().collect();
            sorted_require_dev.sort_by(|a, b| a.0.cmp(b.0));
            for (name, constraint) in sorted_require_dev {
                if is_platform_package(name) {
                    if !options.ignore_platform_requirements.ignores(name) {
                        request.require(
                            name,
                            options
                                .ignore_platform_requirements
                                .filter_constraint(name, constraint),
                        );
                    }
                } else {
                    request.require(name, constraint);
                }
            }
        }

        // Add root package as fixed if it has replace/provide
        // This ensures the solver knows the root package is always installed
        // and its replaced/provided packages are available
        let mut root_pkg = create_root_package(manifest, &root_version);
        filter_package_platform_requirements(&mut root_pkg, &options.ignore_platform_requirements);
        request.fix(root_pkg);

        let root_requirement_names: HashSet<String> = manifest
            .require
            .keys()
            .chain(manifest.require_dev.keys())
            .filter(|name| !is_platform_package(name))
            .map(|name| canonical_package_name(name).into_owned())
            .collect();
        let root_requirements_for_update: HashSet<String> = manifest
            .require
            .keys()
            .chain(
                (!no_dev)
                    .then_some(&manifest.require_dev)
                    .into_iter()
                    .flatten()
                    .map(|(name, _)| name),
            )
            .filter(|name| !is_platform_package(name))
            .map(|name| canonical_package_name(name).into_owned())
            .collect();
        let effective_update_packages = if options.root_requirements_only {
            Some(match &options.update_packages {
                Some(packages) if !packages.is_empty() => packages
                    .iter()
                    .filter(|package| {
                        root_requirements_for_update
                            .iter()
                            .any(|requirement| package_name_matches(package, requirement))
                    })
                    .cloned()
                    .collect(),
                _ => root_requirements_for_update
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>(),
            })
        } else {
            options.update_packages.clone()
        };

        let (preferred_versions, locked_packages, solver_update_allowlist) = match (
            &effective_update_packages,
            &self.riff.lockfile,
        ) {
            (Some(packages_to_update), Some(lock)) => {
                let explicit_allowlist = expand_update_patterns(packages_to_update, &pool, lock);
                if options.with_dependencies && !options.with_all_dependencies {
                    for (package, replaced) in skipped_root_update_dependencies(
                        &pool,
                        &explicit_allowlist,
                        &root_requirement_names,
                    ) {
                        let via_replace = replaced
                            .map(|name| format!(" (via replace of {name})"))
                            .unwrap_or_default();
                        crate::warnln!(self.riff.output(),
                                "Dependency {package}{via_replace} is also a root requirement. Package has not been listed as an update argument, so keeping locked at old version. Use --with-all-dependencies (-W) to include root dependencies."
                            );
                    }
                }
                let mut update_allowlist =
                    if options.with_dependencies || options.with_all_dependencies {
                        expand_update_allowlist(
                            &pool,
                            &explicit_allowlist,
                            &root_requirement_names,
                            options.with_all_dependencies,
                        )
                    } else {
                        explicit_allowlist.clone()
                    };
                update_allowlist.extend(
                    lock.packages
                        .iter()
                        .chain(lock.packages_dev.iter())
                        .filter(|package| locked_package_is_symlinked_path(package))
                        .map(|package| canonical_package_name(&package.name).into_owned()),
                );
                log::debug!("Partial update allowlist: {update_allowlist:?}");

                let mut preferred = HashMap::new();
                for pkg in lock.packages.iter().chain(lock.packages_dev.iter()) {
                    let package_name = canonical_package_name(&pkg.name);
                    let should_prefer = if options.minimal_changes {
                        !explicit_allowlist.contains(package_name.as_ref())
                    } else {
                        !update_allowlist.contains(package_name.as_ref())
                    };
                    if should_prefer {
                        preferred.insert(package_name.into_owned(), pkg.version.clone());
                    }
                }
                log::debug!(
                    "Partial update: using {} preferred versions from lock file",
                    preferred.len()
                );
                log::trace!("Partial update preferred versions: {preferred:?}");
                let locked = lock
                    .packages
                    .iter()
                    .chain(lock.packages_dev.iter())
                    .map(Package::from)
                    .collect();
                (preferred, locked, Some(update_allowlist))
            }
            (_, Some(lock)) if options.minimal_changes => {
                log::debug!("Minimal update: preferring every locked package version");
                (
                    lock.packages
                        .iter()
                        .chain(lock.packages_dev.iter())
                        .map(|package| {
                            (
                                canonical_package_name(&package.name).into_owned(),
                                package.version.clone(),
                            )
                        })
                        .collect(),
                    lock.packages
                        .iter()
                        .chain(lock.packages_dev.iter())
                        .map(Package::from)
                        .collect(),
                    None,
                )
            }
            _ => {
                log::debug!("Full update: no preferred versions, updating all packages");
                (HashMap::new(), Vec::new(), None)
            }
        };

        // Composer turns every locked package outside a partial update's allowlist
        // into a solver assertion. A soft version preference is insufficient: an
        // allowed package could otherwise force one of its still-locked providers
        // to update and silently widen the requested update.
        for mut package in locked_packages {
            filter_package_platform_requirements(
                &mut package,
                &options.ignore_platform_requirements,
            );
            request.lock(package);
        }
        if let Some(ref update_allowlist) = solver_update_allowlist {
            request.update(update_allowlist.iter().cloned().collect());
        }

        let policy_package_ids = update_policy_package_ids(&pool, root_id);
        let policy_packages = policy_package_ids
            .iter()
            .copied()
            .filter_map(|package_id| {
                pool.entry(package_id)
                    .and_then(crate::solver::PoolEntry::as_package)
                    .map(AsRef::as_ref)
            })
            .collect::<Vec<_>>();
        let package_policy = PackagePolicy::load_for_update(
            &self.riff,
            &policy_packages,
            options.no_blocking || options.no_security_blocking,
            options.update_mirrors,
        )
        .await?;
        for warning in package_policy.unreachable_repositories() {
            crate::warnln!(self.riff.output(), "Warning: {warning}");
        }
        let mut policy_diagnostics = PolicyDiagnostics::default();
        for package_id in policy_package_ids {
            let Some(package) = pool
                .entry(package_id)
                .and_then(crate::solver::PoolEntry::as_package)
            else {
                continue;
            };
            let apply_advisories = solver_update_allowlist.as_ref().is_none_or(|allowlist| {
                allowlist.contains(canonical_package_name(&package.name).as_ref())
            });
            let apply_install_scope = self.riff.lockfile.as_ref().is_some_and(|lock| {
                lock.packages
                    .iter()
                    .chain(lock.packages_dev.iter())
                    .any(|locked| {
                        locked.name.eq_ignore_ascii_case(&package.name)
                            && (locked.version == package.version.as_str()
                                || locked.version == package.pretty_version())
                    })
            });
            let violations = package_policy.violations(
                package,
                PolicyPhase::Update,
                apply_advisories,
                apply_install_scope,
            );
            if violations.is_empty() {
                continue;
            }
            request.exclude(Arc::clone(package));
            for violation in violations {
                policy_diagnostics.record(package, violation);
            }
        }

        let mut solver_result = match Solver::new(
            &pool,
            &Policy::new()
                .prefer_stable(prefer_stable)
                .prefer_lowest(prefer_lowest)
                .preferred_versions(preferred_versions),
        )
        .solve(&request)
        {
            Ok(result) => result,
            Err(problems) => {
                spinner.finish_and_clear();
                crate::errln!(
                    self.riff.output(),
                    "{} Could not resolve dependencies",
                    style("Error:").red().bold()
                );
                if let Some(root_name) = manifest.name.as_deref() {
                    crate::errln!(self.riff.output(),
                        "  {root_name} is the root package and cannot be modified during dependency resolution."
                    );
                }
                for diagnostic in policy_diagnostics.lines() {
                    crate::errln!(
                        self.riff.output(),
                        "{} {diagnostic}",
                        style("Error:").red().bold()
                    );
                }
                if policy_diagnostics.has_advisories() {
                    crate::errln!(self.riff.output(),
                        "  Run `riff audit` for full advisory details, or use --no-blocking to disable policy blocking."
                    );
                }
                let mut temporary_constraints =
                    options.temporary_constraints.iter().collect::<Vec<_>>();
                temporary_constraints.sort_by(|left, right| left.0.cmp(right.0));
                for (package, constraint) in temporary_constraints {
                    crate::errln!(self.riff.output(),
                        "  Temporary update constraint {package}:{constraint} excluded all compatible candidates."
                    );
                }
                for problem in problems.problems() {
                    crate::errln!(self.riff.output(), "  {}", problem.describe(&pool));
                }
                for name in &canonical_priority_blocks {
                    crate::errln!(self.riff.output(),
                        "  Package {name} is shadowed by a canonical package from a higher repository priority."
                    );
                }
                // Composer reserves exit code 2 for dependency resolution failures.
                return Ok(UpdateResult::exit(2));
            }
        };
        drop(pool);

        let non_dev_roots: HashSet<String> = manifest
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
        let identity_proves_lock_changed = self.riff.lockfile.as_ref().is_none_or(|current| {
            selected_package_identities_changed(current, &selected_prod, &selected_dev)
        });
        let use_dry_run_projection = dry_run && identity_proves_lock_changed;

        solver_result.packages = solver_result
            .packages
            .iter()
            .map(|package| {
                let hydration_source = solver_package_hydration_sources
                    .get(&(Arc::as_ptr(package) as usize))
                    .unwrap_or(package);
                let mut package = if use_dry_run_projection {
                    repo_manager.hydrate_package_for_transaction(hydration_source)
                } else {
                    repo_manager.hydrate_package(hydration_source)
                };
                if options.update_mirrors {
                    if let Some(locked) = self.riff.lockfile.as_ref().and_then(|lock| {
                        lock.all_packages().find(|locked| {
                            locked.name.eq_ignore_ascii_case(&package.name)
                                && locked.version == package.pretty_version()
                        })
                    }) {
                        package = refresh_locked_package_metadata(locked, &package);
                    }
                }
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
        for package in &packages {
            validate_package_metadata(package)?;
            log::trace!(
                "Selected {} {} source={:?} dist={:?}",
                package.name,
                package.version,
                package.source.as_ref().map(|source| &source.reference),
                package
                    .dist
                    .as_ref()
                    .and_then(|dist| dist.reference.as_deref())
            );
        }

        self.riff.plugins().validate(packages.iter())?;

        let lock_summary = lock_transaction.summary();
        let updated_package_names: Vec<_> = lock_transaction
            .operations
            .iter()
            .filter_map(|operation| match operation {
                crate::solver::Operation::Install(package) => Some(package.name.clone()),
                crate::solver::Operation::Update { to, .. } => Some(to.name.clone()),
                _ => None,
            })
            .collect();
        let newly_installed_package_names: HashSet<_> = lock_transaction
            .operations
            .iter()
            .filter_map(|operation| match operation {
                crate::solver::Operation::Install(package) => {
                    Some(canonical_package_name(&package.name).into_owned())
                }
                _ => None,
            })
            .collect();
        let updated_package_versions: HashMap<_, _> = lock_transaction
            .operations
            .iter()
            .filter_map(|operation| match operation {
                crate::solver::Operation::Install(package) => {
                    Some((package.name.clone(), package.pretty_version().to_string()))
                }
                crate::solver::Operation::Update { to, .. } => {
                    Some((to.name.clone(), to.pretty_version().to_string()))
                }
                _ => None,
            })
            .collect();
        let updated_package_branch_aliases: HashMap<_, _> = lock_transaction
            .operations
            .iter()
            .filter_map(|operation| match operation {
                crate::solver::Operation::Install(package) => Some(package),
                crate::solver::Operation::Update { to, .. } => Some(to),
                _ => None,
            })
            .filter_map(|package| {
                parse_branch_aliases(package.extra.as_ref())
                    .into_iter()
                    .find(|(source, _)| {
                        source == package.version() || source == package.pretty_version()
                    })
                    .map(|(_, (_, pretty_alias))| (package.name.clone(), pretty_alias))
            })
            .collect();

        let (prod_packages, dev_packages): (Vec<_>, Vec<_>) =
            packages.iter().partition(|package| {
                non_dev_packages.contains(canonical_package_name(&package.name).as_ref())
            });
        let alias_can_change = |name: &str| {
            solver_update_allowlist.as_ref().is_none_or(|packages| {
                packages.is_empty()
                    || packages
                        .iter()
                        .any(|pattern| package_name_matches(pattern, name))
            })
        };
        let mut lock_aliases: Vec<_> = self
            .riff
            .lockfile
            .iter()
            .flat_map(|lock| &lock.aliases)
            .filter(|alias| !alias_can_change(&alias.package))
            .cloned()
            .collect();
        lock_aliases.extend(
            solver_result
                .aliases
                .iter()
                .filter(|alias| alias.is_root_package_alias() && alias_can_change(alias.name()))
                .map(|alias| LockAlias {
                    package: alias.name().to_string(),
                    version: VersionParser::new()
                        .normalize(alias.alias_of().pretty_version())
                        .map(|version| VersionParser::new().normalize_default_branch(&version))
                        .unwrap_or_else(|_| alias.alias_of().version.to_string()),
                    alias: alias.pretty_version().to_string(),
                    alias_normalized: alias.version().to_string(),
                }),
        );
        lock_aliases.sort_by(|left, right| {
            (&left.package, &left.version, &left.alias).cmp(&(
                &right.package,
                &right.version,
                &right.alias,
            ))
        });
        lock_aliases.dedup();

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
            let platform = manifest
                .require
                .iter()
                .filter(|(name, _)| is_platform_package(name))
                .map(|(name, constraint)| (name.clone(), constraint.clone()))
                .collect();
            let platform_dev = manifest
                .require_dev
                .iter()
                .filter(|(name, _)| is_platform_package(name))
                .map(|(name, constraint)| (name.clone(), constraint.clone()))
                .collect();

            RiffLockfile {
                content_hash: crate::util::compute_content_hash(
                    &serde_json::to_string(manifest).unwrap_or_default(),
                ),
                packages: prod_packages
                    .iter()
                    .map(|package| LockedPackage::from(*package))
                    .collect(),
                packages_dev: dev_packages
                    .iter()
                    .map(|package| LockedPackage::from(*package))
                    .collect(),
                aliases: lock_aliases,
                minimum_stability: manifest
                    .minimum_stability
                    .clone()
                    .unwrap_or_else(|| "stable".to_string()),
                stability_flags: stability_flags
                    .iter()
                    .map(|(name, stability)| (name.clone(), stability.priority()))
                    .collect(),
                prefer_stable: manifest.prefer_stable.unwrap_or(false),
                prefer_lowest,
                platform,
                platform_dev,
                plugin_api_version: "2.9.0".to_string(),
                ..Default::default()
            }
        });

        let lock_file_changed = lock.as_ref().is_none_or(|lock| {
            self.riff
                .lockfile
                .as_ref()
                .is_none_or(|current| !current.equivalent_for_write(lock))
        });

        // Only write lock file if there were changes
        if lock_file_changed && !dry_run && self.riff.config.lock {
            log::debug!("Writing lock file");
            let lock = lock
                .as_ref()
                .expect("non-dry-run updates always build a complete lock");
            crate::json::write_json_value(&working_dir.join("composer.lock"), lock, true)
                .context("Failed to write composer.lock")?;
        }

        if options.update_lock_only {
            spinner.finish_and_clear();
            if lock_file_changed {
                if dry_run {
                    crate::outln!(
                        self.riff.output(),
                        "{} Lock file would be updated",
                        style("Info:").cyan()
                    );
                } else {
                    crate::successln!(
                        self.riff.output(),
                        "{} Lock file updated",
                        style("Success:").green().bold()
                    );
                }
            } else {
                crate::outln!(
                    self.riff.output(),
                    "{} Lock file is up to date",
                    style("Info:").cyan()
                );
            }
            return Ok(UpdateResult::success(
                updated_package_names,
                updated_package_versions,
                updated_package_branch_aliases,
            ));
        }

        if options.no_install {
            spinner.finish_and_clear();
            crate::outln!(
                self.riff.output(),
                "{} Installation skipped; lock file update complete",
                style("Info:").cyan()
            );
            if !dry_run {
                self.report_package_notices(&packages, &newly_installed_package_names);
            }
            if !dry_run && !options.no_scripts {
                let exit_code = self.riff.dispatch(&PostUpdateEvent::new(!no_dev)).await?;
                if exit_code != 0 {
                    return Ok(UpdateResult::exit(exit_code));
                }
            }
            return Ok(UpdateResult::success(
                updated_package_names,
                updated_package_versions,
                updated_package_branch_aliases,
            ));
        }

        log::debug!("Installing dependencies from lock file");
        log::info!(
            "Package operations: {} installs, {} updates, {} removals",
            install_count,
            update_count,
            removal_count
        );

        let manager = &self.riff.installation_manager;
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
        let patch_packages: Vec<_> = packages_to_install
            .iter()
            .map(|package| package.as_ref().clone())
            .collect();
        let present_packages = self.load_actual_installed_packages().await;
        let audit_installed_names = dry_run.then(|| {
            present_packages
                .iter()
                .map(|package| canonical_package_name(&package.name).into_owned())
                .collect()
        });
        let package_hook = if download_only {
            None
        } else {
            crate::patch::prepare(&self.riff, &patch_packages, dry_run).await?
        };
        let desired_patch_fingerprints = package_hook
            .as_ref()
            .map(|hook| hook.fingerprints())
            .unwrap_or_default();
        let changed_patch_packages = if dry_run || download_only {
            Default::default()
        } else {
            let previous_patch_fingerprints =
                crate::patch::read_applied_patch_state(&manager.config().vendor_dir);
            crate::patch::changed_patch_packages(
                &previous_patch_fingerprints,
                &desired_patch_fingerprints,
            )
        };
        let mut transaction = Transaction::from_packages(
            present_packages,
            packages_to_install.clone(),
            solver_result.aliases,
        );
        transaction.skip_same_reference_dev_updates();
        for package in &packages_to_install {
            if changed_patch_packages.contains(canonical_package_name(&package.name).as_ref()) {
                transaction.reinstall(package.clone());
            }
        }
        transaction.sort();
        let plugin_operations = if !dry_run && !download_only && !options.no_scripts {
            Some(
                self.riff
                    .plugins()
                    .prepare_operations(&self.riff, &transaction, &packages_to_install)
                    .await?,
            )
        } else {
            None
        };
        if !dry_run && !download_only && !options.no_scripts {
            let exit_code = self
                .riff
                .dispatch(&PreOperationsExecEvent::with_transaction(
                    !no_dev,
                    true,
                    Arc::new(transaction.clone()),
                ))
                .await?;
            if exit_code != 0 {
                return Ok(UpdateResult::exit(exit_code));
            }
        }
        let layouts = self
            .riff
            .plugins()
            .package_layouts(packages_to_install.iter().map(AsRef::as_ref));
        let autoload_scans = if !dry_run && !download_only && !options.no_autoloader {
            let lock = lock
                .as_ref()
                .expect("non-dry-run updates always build a complete lock");
            let aliases_map: HashMap<String, Vec<String>> = HashMap::new();
            let mut package_autoloads = lock
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
                .collect::<Vec<_>>();
            if !no_dev {
                package_autoloads.extend(lock.packages_dev.iter().map(|package| {
                    locked_package_to_autoload(
                        package,
                        true,
                        &aliases_map,
                        manager.config().prefer_source,
                    )
                }));
            }
            apply_plugin_package_layouts(&self.riff, &mut package_autoloads);
            let scan_generator = AutoloadGenerator::new(AutoloadConfig {
                vendor_dir: manager.config().vendor_dir.clone(),
                base_dir: working_dir.clone(),
                optimize: options.optimize_autoloader || options.classmap_authoritative,
                authoritative: options.classmap_authoritative,
                apcu: options.apcu_autoloader,
                apcu_prefix: options.apcu_autoloader_prefix.clone(),
                suffix: Some(lock.content_hash.clone()),
            });
            let root_autoload = root_autoload(manifest, !no_dev, &[]);
            let (paths, excludes) = scan_generator
                .package_classmap_scan_plan(&package_autoloads, root_autoload.as_ref());
            (!paths.is_empty()).then(|| Arc::new(AutoloadScanObserver::new(paths, excludes)))
        } else {
            None
        };
        let result = manager
            .execute_with_layouts_and_observer(
                &transaction,
                package_hook,
                layouts,
                autoload_scans
                    .as_ref()
                    .map(|observer| observer.clone() as Arc<dyn super::PackageInstallObserver>),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Failed to install packages: {}", e))?;
        if !dry_run && !download_only {
            crate::patch::write_applied_patch_state(
                &manager.config().vendor_dir,
                &desired_patch_fingerprints,
            )?;
        }

        spinner.finish_and_clear();

        let actually_installed: Vec<_> = result
            .installed
            .iter()
            .filter(|p| !is_platform_package(&p.name))
            .collect();

        for pkg in &actually_installed {
            log::debug!("Installed {} ({})", pkg.name, pkg.version);
            crate::outln!(
                self.riff.output(),
                "  {} {} ({})",
                style("-").green(),
                style(&pkg.name).white().bold(),
                style(&pkg.version).yellow()
            );
        }
        for pkg in &result.reinstalled {
            log::debug!("Reinstalled {} ({})", pkg.name, pkg.version);
            crate::outln!(
                self.riff.output(),
                "  {} {} ({}, reinstalled)",
                style("-").green(),
                style(&pkg.name).white().bold(),
                style(&pkg.version).yellow()
            );
        }

        if !dry_run && !download_only && options.no_autoloader {
            let lock = lock
                .as_ref()
                .expect("non-dry-run updates always build a complete lock");
            self.generate_installed_metadata(lock, &root_version, !no_dev)?;
        }

        if !dry_run && !download_only && !options.no_autoloader {
            let lock = lock
                .as_ref()
                .expect("non-dry-run updates always build a complete lock");
            let mut additional_classmap = Vec::new();
            if !options.no_scripts {
                let event = PreAutoloadDumpEvent::new(
                    !no_dev,
                    options.optimize_autoloader || options.classmap_authoritative,
                );
                let exit_code = self.riff.dispatch(&event).await?;
                if exit_code != 0 {
                    return Ok(UpdateResult::exit(exit_code));
                }
                additional_classmap = event.classmap_paths();
            }
            crate::outln!(
                self.riff.output(),
                "{} Generating autoload files",
                style("Info:").cyan()
            );

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
            apply_plugin_package_layouts(&self.riff, &mut package_autoloads);

            let autoload_config = AutoloadConfig {
                vendor_dir: manager.config().vendor_dir.clone(),
                base_dir: working_dir.clone(),
                optimize: options.optimize_autoloader || options.classmap_authoritative,
                authoritative: options.classmap_authoritative,
                apcu: options.apcu_autoloader,
                apcu_prefix: options.apcu_autoloader_prefix.clone(),
                suffix: Some(lock.content_hash.clone()),
            };

            let generator = configure_platform_check(
                AutoloadGenerator::new(autoload_config),
                manifest,
                &package_autoloads,
                &self.riff.config.platform_check,
                &options.ignore_platform_requirements,
            );

            let generator = if let Some(observer) = autoload_scans.as_ref() {
                generator.with_precomputed_classmaps(observer.finish().await?)
            } else {
                generator
            };

            let root_autoload = root_autoload(manifest, dev_mode, &additional_classmap);

            let root_package = create_root_package_info(
                manifest,
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
                )
                .with_operation(DependencyOperation::Update);
                let exit_code = self.riff.dispatch(&event).await?;
                if exit_code != 0 {
                    return Ok(UpdateResult::exit(exit_code));
                }
            }
        }

        let total_changed =
            actually_installed.len() + result.updated.len() + result.reinstalled.len();
        if download_only {
            crate::successln!(
                self.riff.output(),
                "{} {} packages downloaded",
                style("Success:").green().bold(),
                total_changed
            );
        } else if total_changed > 0 || lock_file_changed {
            crate::successln!(
                self.riff.output(),
                "{} {} packages updated",
                style("Success:").green().bold(),
                total_changed
            );
        } else {
            crate::outln!(
                self.riff.output(),
                "{} Nothing to update.",
                style("Info:").cyan()
            );
        }

        if !dry_run {
            self.report_package_notices(&packages, &newly_installed_package_names);
        }

        // Dispatch post-update event
        if !dry_run && !options.no_scripts {
            if let Some(plugin_operations) = plugin_operations {
                plugin_operations.apply(&self.riff)?;
            }
            let exit_code = self.riff.dispatch(&PostUpdateEvent::new(!no_dev)).await?;
            if exit_code != 0 {
                return Ok(UpdateResult::exit(exit_code));
            }
        }

        Ok(UpdateResult {
            exit_code: 0,
            audit_installed_names,
            updated_package_names,
            updated_package_versions,
            updated_package_branch_aliases,
        })
    }

    pub async fn install(&self, options: InstallOptions) -> Result<i32> {
        let manifest = &self.riff.manifest;
        let working_dir = &self.riff.working_dir;
        let install_config = self.riff.installation_manager.config();
        let dry_run = install_config.dry_run;
        let download_only = install_config.download_only;
        let no_dev = install_config.no_dev;
        let lock = self
            .riff
            .lockfile
            .as_ref()
            .context("No composer.lock file found")?;

        let current_content_hash =
            crate::util::compute_content_hash(&serde_json::to_string(manifest).unwrap_or_default());
        if !lock.content_hash.is_empty() && lock.content_hash != current_content_hash {
            crate::warnln!(self.riff.output(),
                "Warning: The lock file is not up to date with the latest changes in composer.json. You may be getting outdated dependencies."
            );
        }

        // Detect root package version
        let root_version = get_root_version(working_dir, manifest);

        // Dispatch pre-install event
        if !dry_run && !options.no_scripts {
            let exit_code = self.riff.dispatch(&PreInstallEvent::new(!no_dev)).await?;
            if exit_code != 0 {
                return Ok(exit_code);
            }
        }

        // Convert locked packages
        let mut packages: Vec<Package> = lock.packages.iter().map(Package::from).collect();
        if !no_dev {
            packages.extend(lock.packages_dev.iter().map(Package::from));
        }
        let policy_packages = packages.iter().collect::<Vec<_>>();
        let package_policy = PackagePolicy::load(
            &self.riff,
            &policy_packages,
            PolicyScope::Install,
            options.no_blocking || options.no_security_blocking,
        )
        .await?;
        for warning in package_policy.unreachable_repositories() {
            crate::warnln!(self.riff.output(), "Warning: {warning}");
        }
        let mut policy_blocked = false;
        for package in &packages {
            validate_package_metadata(package)?;
            for violation in package_policy.violations(package, PolicyPhase::Install, false, false)
            {
                crate::errln!(
                    self.riff.output(),
                    "{} {}",
                    style("Error:").red().bold(),
                    violation.diagnostic(package)
                );
                policy_blocked = true;
            }
        }
        if policy_blocked {
            return Ok(2);
        }
        if let Err(problems) = validate_platform_requirements(
            manifest,
            &packages,
            &self.riff.platform_packages,
            &options.ignore_platform_requirements,
            no_dev,
        ) {
            for problem in problems {
                crate::errln!(
                    self.riff.output(),
                    "{} {problem}",
                    style("Error:").red().bold()
                );
            }
            return Ok(2);
        }
        let relation_problems =
            validate_locked_package_relations(manifest, &root_version, &packages, no_dev);
        for problem in &relation_problems.root_requirements {
            crate::errln!(
                self.riff.output(),
                "{} {problem}",
                style("Error:").red().bold()
            );
        }
        for problem in &relation_problems.solver {
            crate::errln!(
                self.riff.output(),
                "{} {problem}",
                style("Error:").red().bold()
            );
        }
        if !relation_problems.solver.is_empty() {
            return Ok(2);
        }
        if !relation_problems.root_requirements.is_empty()
            && !self.riff.config.allow_missing_requirements
        {
            return Ok(4);
        }
        self.riff.plugins().validate(packages.iter())?;

        crate::outln!(
            self.riff.output(),
            "{} Installing dependencies from lock file",
            style("Riff").green().bold()
        );
        if dry_run {
            crate::outln!(
                self.riff.output(),
                "{} Running in dry-run mode",
                style("Info:").cyan()
            );
        }

        let progress = if self.riff.output().progress_enabled() {
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
            progress
        } else {
            ProgressBar::hidden()
        };

        let manager = &self.riff.installation_manager;
        let present_packages = self.load_actual_installed_packages().await;
        let desired_packages: Vec<_> = packages.iter().cloned().map(Arc::new).collect();
        let package_hook = if download_only {
            None
        } else {
            crate::patch::prepare(&self.riff, &packages, dry_run).await?
        };
        let desired_patch_fingerprints = package_hook
            .as_ref()
            .map(|hook| hook.fingerprints())
            .unwrap_or_default();
        let changed_patch_packages = if dry_run || download_only {
            Default::default()
        } else {
            let previous_patch_fingerprints =
                crate::patch::read_applied_patch_state(&manager.config().vendor_dir);
            crate::patch::changed_patch_packages(
                &previous_patch_fingerprints,
                &desired_patch_fingerprints,
            )
        };
        let mut transaction =
            Transaction::from_packages(present_packages, desired_packages.clone(), Vec::new());
        transaction.skip_same_reference_dev_updates();
        for package in &desired_packages {
            if changed_patch_packages.contains(canonical_package_name(&package.name).as_ref()) {
                transaction.reinstall(package.clone());
            }
        }
        transaction.sort();
        if !dry_run && !download_only && !options.no_scripts {
            let exit_code = self
                .riff
                .dispatch(&PreOperationsExecEvent::with_transaction(
                    !no_dev,
                    true,
                    Arc::new(transaction.clone()),
                ))
                .await?;
            if exit_code != 0 {
                return Ok(exit_code);
            }
        }
        let layouts = self
            .riff
            .plugins()
            .package_layouts(desired_packages.iter().map(AsRef::as_ref));
        let result = manager
            .execute_with_layouts(&transaction, package_hook, layouts)
            .await
            .map_err(|error| anyhow::anyhow!("Failed to install packages: {error}"))?;
        if !dry_run && !download_only {
            crate::patch::write_applied_patch_state(
                &manager.config().vendor_dir,
                &desired_patch_fingerprints,
            )?;
        }

        progress.finish_and_clear();

        if !result.installed.is_empty() {
            for pkg in &result.installed {
                crate::outln!(
                    self.riff.output(),
                    "  {} {} ({})",
                    style("-").green(),
                    style(&pkg.name).white().bold(),
                    style(&pkg.version).yellow()
                );
            }
        }
        for pkg in &result.reinstalled {
            crate::outln!(
                self.riff.output(),
                "  {} {} ({}, reinstalled)",
                style("-").green(),
                style(&pkg.name).white().bold(),
                style(&pkg.version).yellow()
            );
        }

        if !dry_run && !download_only && options.no_autoloader {
            self.generate_installed_metadata(lock, &root_version, !no_dev)?;
        }

        if !dry_run && !download_only && !options.no_autoloader {
            // Dispatch pre-autoload-dump event
            let mut additional_classmap = Vec::new();
            if !options.no_scripts {
                let event = PreAutoloadDumpEvent::new(
                    !no_dev,
                    options.optimize_autoloader || options.classmap_authoritative,
                );
                let exit_code = self.riff.dispatch(&event).await?;
                if exit_code != 0 {
                    return Ok(exit_code);
                }
                additional_classmap = event.classmap_paths();
            }

            crate::outln!(
                self.riff.output(),
                "{} Generating autoload files",
                style("Info:").cyan()
            );

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
            apply_plugin_package_layouts(&self.riff, &mut package_autoloads);

            let autoload_config = AutoloadConfig {
                vendor_dir: manager.config().vendor_dir.clone(),
                base_dir: working_dir.clone(),
                optimize: options.optimize_autoloader || options.classmap_authoritative,
                authoritative: options.classmap_authoritative,
                apcu: options.apcu_autoloader,
                apcu_prefix: options.apcu_autoloader_prefix.clone(),
                suffix: if !lock.content_hash.is_empty() {
                    Some(lock.content_hash.clone())
                } else {
                    None
                },
            };

            let generator = configure_platform_check(
                AutoloadGenerator::new(autoload_config),
                manifest,
                &package_autoloads,
                &self.riff.config.platform_check,
                &options.ignore_platform_requirements,
            );
            // Root autoload from json
            let root_autoload = root_autoload(manifest, dev_mode, &additional_classmap);
            let root_aliases = aliases_map
                .get(&manifest.name.clone().unwrap_or_default())
                .cloned()
                .unwrap_or_default();
            let root_package = create_root_package_info(
                manifest,
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
                )
                .with_operation(DependencyOperation::Install);
                let exit_code = self.riff.dispatch(&event).await?;
                if exit_code != 0 {
                    return Ok(exit_code);
                }
            }
        }

        if download_only {
            crate::successln!(
                self.riff.output(),
                "{} {} packages downloaded",
                style("Success:").green().bold(),
                result.installed.len() + result.updated.len() + result.reinstalled.len()
            );
        } else {
            crate::successln!(
                self.riff.output(),
                "{} {} packages installed, {} reinstalled",
                style("Success:").green().bold(),
                result.installed.len(),
                result.reinstalled.len()
            );
        }

        if !dry_run {
            let newly_installed: HashSet<_> = result
                .installed
                .iter()
                .map(|package| canonical_package_name(&package.name).into_owned())
                .collect();
            self.report_package_notices(&packages, &newly_installed);
        }

        // Dispatch post-install event
        if !options.no_scripts && !dry_run {
            let exit_code = self.riff.dispatch(&PostInstallEvent::new(!no_dev)).await?;
            if exit_code != 0 {
                return Ok(exit_code);
            }
        }

        Ok(0)
    }

    pub async fn dump_autoload(&self, options: DumpAutoloadOptions) -> Result<()> {
        let DumpAutoloadOptions {
            optimize,
            authoritative,
            apcu,
            no_dev,
            strict_psr,
            no_scripts,
            dry_run,
        } = options;
        let manifest = &self.riff.manifest;
        let working_dir = &self.riff.working_dir;
        let manager = &self.riff.installation_manager;

        // Detect root package version
        let root_version = get_root_version(working_dir, manifest);

        if dry_run {
            crate::outln!(
                self.riff.output(),
                "{} Running in dry-run mode",
                style("Info:").cyan()
            );
            crate::outln!(
                self.riff.output(),
                "{} Would generate autoload files",
                style("Info:").cyan()
            );
            return Ok(());
        }

        let mut additional_classmap = Vec::new();
        if !no_scripts {
            let event = PreAutoloadDumpEvent::new(!no_dev, optimize || authoritative);
            let exit_code = self.riff.dispatch(&event).await?;
            if exit_code != 0 {
                anyhow::bail!("pre-autoload-dump script exited with code {}", exit_code);
            }
            additional_classmap = event.classmap_paths();
        }

        let optimized = optimize || authoritative;
        let description = autoload_files_description(optimized, authoritative);
        crate::outln!(
            self.riff.output(),
            "{} Generating {description}",
            style("Info:").cyan()
        );

        let mut aliases_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut package_autoloads: Vec<PackageAutoload> = Vec::new();
        let mut all_installed_packages: Vec<Package> = Vec::new();
        let dev_mode = !no_dev;
        let mut lock_suffix = None;

        if let Some(lock) = &self.riff.lockfile {
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
            apply_plugin_package_layouts(&self.riff, &mut package_autoloads);

            all_installed_packages = lock.packages.iter().map(Package::from).collect();
            if dev_mode {
                all_installed_packages.extend(lock.packages_dev.iter().map(Package::from));
            }

            if !lock.content_hash.is_empty() {
                lock_suffix = Some(lock.content_hash.clone());
            }
        }

        let suffix = self.riff.config.autoloader_suffix.clone().or(lock_suffix);

        let autoload_config = AutoloadConfig {
            vendor_dir: manager.config().vendor_dir.clone(),
            base_dir: working_dir.clone(),
            optimize: optimize || authoritative,
            authoritative,
            apcu,
            apcu_prefix: None,
            suffix,
        };

        let generator = configure_platform_check(
            AutoloadGenerator::new(autoload_config).with_strict_psr(strict_psr),
            manifest,
            &package_autoloads,
            &self.riff.config.platform_check,
            &PlatformRequirementFilter::default(),
        );
        // Root autoload from json
        let root_autoload = root_autoload(manifest, dev_mode, &additional_classmap);
        let root_aliases = aliases_map
            .get(&manifest.name.clone().unwrap_or_default())
            .cloned()
            .unwrap_or_default();
        let root_package =
            create_root_package_info(manifest, &root_version, working_dir, root_aliases, dev_mode);

        let result = match generator.generate_with_result(
            &package_autoloads,
            root_autoload.as_ref(),
            Some(&root_package),
        ) {
            Ok(result) => result,
            Err(error) => {
                crate::errln!(self.riff.output(), "{error}");
                return Err(error).context("Failed to generate autoloader");
            }
        };

        // Dispatch post-autoload-dump event (runs scripts and plugins)
        let arc_packages: Vec<Arc<Package>> = all_installed_packages
            .iter()
            .map(|p| Arc::new(p.clone()))
            .collect();
        if !no_scripts {
            let event =
                PostAutoloadDumpEvent::new(arc_packages, dev_mode, optimize || authoritative);
            self.riff.dispatch(&event).await?;
        }

        if optimized {
            crate::successln!(
                self.riff.output(),
                "{} Generated {description} containing {} classes",
                style("Success:").green().bold(),
                result.class_count
            );
        } else {
            crate::successln!(
                self.riff.output(),
                "{} Generated autoload files",
                style("Success:").green().bold()
            );
        }

        Ok(())
    }

    /// Load package versions recorded in composer.lock.
    fn load_locked_packages(&self, include_dev: bool) -> Vec<Arc<Package>> {
        let Some(lock) = &self.riff.lockfile else {
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
        let repository =
            InstalledRepository::new(self.riff.installation_manager.config().vendor_dir.clone());
        repository.load_transaction_packages().unwrap_or_default()
    }

    fn generate_installed_metadata(
        &self,
        lock: &RiffLockfile,
        root_version: &RootVersion,
        dev_mode: bool,
    ) -> Result<()> {
        let manager = &self.riff.installation_manager;
        let manifest = &self.riff.manifest;
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
        apply_plugin_package_layouts(&self.riff, &mut packages);

        let root_aliases = aliases_map
            .get(&manifest.name.clone().unwrap_or_default())
            .cloned()
            .unwrap_or_default();
        let root_package = create_root_package_info(
            manifest,
            root_version,
            &self.riff.working_dir,
            root_aliases,
            dev_mode,
        );
        let generator = AutoloadGenerator::new(AutoloadConfig {
            vendor_dir: manager.config().vendor_dir.clone(),
            base_dir: self.riff.working_dir.clone(),
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

        crate::errln!(self.riff.output());
        for pkg in abandoned_packages {
            if let Some(ref abandoned) = pkg.abandoned {
                let replacement = match abandoned.replacement() {
                    Some(repl) => format!("Use {} instead", repl),
                    None => "No replacement was suggested".to_string(),
                };
                crate::errln!(
                    self.riff.output(),
                    "{} Package {} is abandoned, you should avoid using it. {}.",
                    style("Warning:").yellow(),
                    pkg.name,
                    replacement
                );
            }
        }
    }

    fn report_package_notices(&self, packages: &[Package], newly_installed: &HashSet<String>) {
        let mut suggestions = SuggestedPackagesReporter::new();
        for package in packages.iter().filter(|package| {
            newly_installed.contains(canonical_package_name(&package.name).as_ref())
        }) {
            suggestions.add_suggestions_from_package(package);
        }
        let suggestion_count = suggestions.filtered(Some(packages)).len();
        if suggestion_count > 0 {
            crate::infoln!(self.riff.output(),
                "{} package suggestion{} were added by new dependencies, use `riff suggest` to see details.",
                suggestion_count,
                if suggestion_count == 1 { "" } else { "s" }
            );
        }

        self.audit_abandoned_packages(packages);

        let show_funding = std::env::var("COMPOSER_FUND")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .is_none_or(|value| value != 0);
        if show_funding {
            let funding_count = packages
                .iter()
                .filter(|package| !package.funding.is_empty())
                .count();
            if funding_count > 0 {
                crate::infoln!(
                    self.riff.output(),
                    "{} package{} you are using {} looking for funding.",
                    funding_count,
                    if funding_count == 1 { "" } else { "s" },
                    if funding_count == 1 { "is" } else { "are" }
                );
                crate::infoln!(
                    self.riff.output(),
                    "Use the `riff fund` command to find out more!"
                );
            }
        }
    }
}

// Helpers

fn autoload_files_description(optimized: bool, authoritative: bool) -> &'static str {
    match (optimized, authoritative) {
        (true, true) => "optimized autoload files (authoritative)",
        (true, false) => "optimized autoload files",
        (false, _) => "autoload files",
    }
}

/// Detects and returns the root package version with logging.
///
/// This handles:
/// 1. COMPOSER_ROOT_VERSION environment variable
/// 2. Explicit version in composer.json
/// 3. Branch alias matching current git branch
/// 4. Git branch name as dev version
fn get_root_version(working_dir: &std::path::Path, manifest: &RiffManifest) -> RootVersion {
    let branch_aliases = manifest.get_branch_aliases();
    let root_version = detect_root_version_with_non_feature_branches(
        working_dir,
        manifest.version.as_deref(),
        &branch_aliases,
        &manifest.non_feature_branches,
    );

    log::info!(
        "Root package version: {} (from {})",
        root_version.pretty_version,
        root_version.source
    );

    root_version
}

fn root_autoload(
    manifest: &RiffManifest,
    dev_mode: bool,
    additional_classmap: &[std::path::PathBuf],
) -> Option<Autoload> {
    let mut autoload: Autoload = manifest.autoload.clone().into();
    if dev_mode {
        autoload.merge(manifest.autoload_dev.clone().into());
    }
    autoload.classmap.extend(
        additional_classmap
            .iter()
            .map(|path| CompactString::new(path.to_string_lossy())),
    );
    Some(autoload)
}

/// Creates a root package that can be added to the solver pool.
///
/// This creates a Package with the root's replace/provide/conflict declarations
/// so the solver knows what virtual packages the root provides.
fn create_root_package(manifest: &RiffManifest, root_version: &RootVersion) -> Package {
    let name = manifest
        .name
        .clone()
        .unwrap_or_else(|| "__root__".to_string());

    let mut pkg = Package::new(&name, &root_version.version);
    pkg.pretty_version = Some(root_version.pretty_version.clone().into());
    pkg.package_type = manifest.package_type.clone().into();

    // Copy replace/provide/conflict from composer.json
    pkg.replace = manifest.replace.clone().into();
    pkg.provide = manifest.provide.clone().into();
    pkg.conflict = manifest.conflict.clone().into();
    if !manifest.extra.is_null() {
        pkg.extra = Some(manifest.extra.clone());
    }

    pkg
}

/// Creates a RootPackageInfo for autoload generation.
fn create_root_package_info(
    manifest: &RiffManifest,
    root_version: &RootVersion,
    working_dir: &std::path::Path,
    aliases: Vec<String>,
    dev_mode: bool,
) -> RootPackageInfo {
    RootPackageInfo {
        name: manifest
            .name
            .clone()
            .unwrap_or_else(|| "__root__".to_string()),
        pretty_version: root_version.pretty_version.clone(),
        version: root_version.version.clone(),
        reference: get_head_commit(working_dir),
        package_type: manifest.package_type.clone(),
        aliases,
        replaces: manifest.replace.clone(),
        provides: manifest.provide.clone(),
        dev_mode,
    }
}

fn add_package_aliases(pool: &mut Pool, manifest: &RiffManifest) {
    add_package_aliases_with_root_exclusions(pool, manifest, &HashSet::new());
}

fn add_package_aliases_with_root_exclusions(
    pool: &mut Pool,
    manifest: &RiffManifest,
    root_alias_exclusions: &HashSet<usize>,
) {
    #[derive(Debug)]
    struct PendingAlias {
        package: Arc<Package>,
        normalized: String,
        pretty: String,
        root: bool,
    }

    let mut root_aliases: Vec<_> = manifest
        .require
        .iter()
        .chain(&manifest.require_dev)
        .filter_map(|(name, constraint)| {
            parse_inline_alias(constraint)
                .map(|(actual, alias)| (canonical_package_name(name).into_owned(), actual, alias))
        })
        .collect();
    root_aliases.sort();

    let packages: Vec<_> = pool
        .all_package_ids()
        .filter_map(|id| pool.package(id).cloned())
        .collect();
    let version_parser = VersionParser::new();
    let mut pending = Vec::new();
    let mut seen = HashSet::new();

    for package in packages {
        let package_identity = Arc::as_ptr(&package) as usize;
        let branch_aliases = parse_branch_aliases(package.extra.as_ref());
        let mut has_explicit_branch_alias = false;
        for (source, (normalized, pretty)) in &branch_aliases {
            if package.version == *source || package.pretty_version() == source {
                has_explicit_branch_alias = true;
                let key = (package_identity, normalized.clone(), false);
                if seen.insert(key) {
                    pending.push(PendingAlias {
                        package: package.clone(),
                        normalized: normalized.clone(),
                        pretty: pretty.clone(),
                        root: false,
                    });
                }
            }
        }

        if package.default_branch == Some(true)
            && package.pretty_version().starts_with("dev-")
            && !has_explicit_branch_alias
        {
            let normalized = DEFAULT_BRANCH_ALIAS.to_string();
            let key = (package_identity, normalized.clone(), false);
            if seen.insert(key) {
                pending.push(PendingAlias {
                    package: package.clone(),
                    normalized: normalized.clone(),
                    pretty: normalized,
                    root: false,
                });
            }
        }

        if root_alias_exclusions.contains(&package_identity) {
            continue;
        }

        for (_, actual, pretty) in root_aliases
            .iter()
            .filter(|(name, _, _)| canonical_package_name(&package.name).as_ref() == name.as_str())
        {
            let actual = actual.split('#').next().unwrap_or(actual);
            let matches_package = Semver::satisfies(&package.version, actual)
                || branch_aliases.values().any(|(normalized, pretty)| {
                    Semver::satisfies(normalized, actual) || Semver::satisfies(pretty, actual)
                });
            if !matches_package {
                continue;
            }
            let normalized = version_parser
                .normalize(pretty)
                .unwrap_or_else(|_| pretty.clone());
            let key = (package_identity, normalized.clone(), true);
            if seen.insert(key) {
                pending.push(PendingAlias {
                    package: package.clone(),
                    normalized,
                    pretty: pretty.clone(),
                    root: true,
                });
            }
        }
    }

    pending.sort_by(|left, right| {
        (
            &left.package.name,
            &left.package.version,
            &left.normalized,
            left.root,
        )
            .cmp(&(
                &right.package.name,
                &right.package.version,
                &right.normalized,
                right.root,
            ))
    });
    for pending in pending {
        let mut alias = AliasPackage::new(pending.package, pending.normalized, pending.pretty);
        alias.set_root_package_alias(pending.root);
        pool.add_alias_package(alias);
    }
}

fn explicit_stability_flag(constraint: &str) -> Option<Stability> {
    let mut explicit = None;
    let mut remainder = constraint;
    while let Some(marker) = remainder.find('@') {
        remainder = &remainder[marker + 1..];
        let stability = [
            ("dev", Stability::Dev),
            ("alpha", Stability::Alpha),
            ("beta", Stability::Beta),
            ("rc", Stability::RC),
            ("stable", Stability::Stable),
        ]
        .into_iter()
        .find_map(|(name, stability)| {
            let candidate = remainder.get(..name.len())?;
            if !candidate.eq_ignore_ascii_case(name) {
                return None;
            }
            let boundary = remainder[name.len()..].chars().next();
            boundary
                .is_none_or(|character| !character.is_alphanumeric() && character != '_')
                .then_some(stability)
        });
        if let Some(stability) = stability {
            if explicit.is_none_or(|current: Stability| stability.priority() > current.priority()) {
                explicit = Some(stability);
            }
        }
    }
    explicit
}

fn extract_stability_flag(constraint: &str, minimum_stability: Stability) -> Option<Stability> {
    if let Some(explicit) = explicit_stability_flag(constraint) {
        return Some(explicit);
    }

    // Composer infers a root stability flag from exact unstable versions and
    // development branch constraints even when no explicit `@dev` suffix is
    // present. Looking at the complete disjunction also selects the most
    // unstable alternative, matching Composer's effective flag.
    let inferred = Stability::from_version(constraint);
    (inferred != Stability::Stable && inferred.priority() >= minimum_stability.priority())
        .then_some(inferred)
}

fn root_stability_flags(
    manifest: &RiffManifest,
    minimum_stability: Stability,
) -> HashMap<String, Stability> {
    let mut flags = HashMap::new();
    for (name, constraint) in manifest.require.iter().chain(&manifest.require_dev) {
        let Some(stability) = extract_stability_flag(constraint, minimum_stability) else {
            continue;
        };
        let name = canonical_package_name(name).into_owned();
        flags
            .entry(name)
            .and_modify(|current: &mut Stability| {
                if stability.priority() > current.priority() {
                    *current = stability;
                }
            })
            .or_insert(stability);
    }
    flags
}

fn root_references(manifest: &RiffManifest) -> HashMap<String, String> {
    manifest
        .require
        .iter()
        .chain(&manifest.require_dev)
        .filter_map(|(name, constraint)| {
            let actual = parse_inline_alias(constraint)
                .map(|(actual, _)| actual)
                .unwrap_or_else(|| constraint.clone());
            let (version, reference) = actual.split_once('#')?;
            if Stability::from_version(version) != Stability::Dev
                || reference.is_empty()
                || !reference
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
            {
                return None;
            }
            Some((
                canonical_package_name(name).into_owned(),
                reference.to_string(),
            ))
        })
        .collect()
}

/// Merge repository URL/mirror metadata into a locked package while keeping
/// every identity-bearing field from the lock file intact.
///
/// This mirrors Composer's `update mirrors` behavior: references, dependency
/// links, release time and versions remain pinned, while source URLs/mirrors
/// and compatible dist URLs/mirrors may be refreshed.
fn refresh_locked_package_metadata(locked: &LockedPackage, remote: &Package) -> Package {
    static KNOWN_DIST_HOST: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^https?://(?:(?:www\.)?bitbucket\.org|(api\.)?github\.com|(?:www\.)?gitlab\.com)/",
        )
        .expect("valid known dist host regex")
    });
    static DIST_REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?P<prefix>/|sha=)[a-f0-9]{40}(?P<suffix>/|$)")
            .expect("valid dist reference regex")
    });

    let mut refreshed = Package::from(locked);
    let (Some(locked_source), Some(remote_source)) = (&mut refreshed.source, &remote.source) else {
        return remote.clone();
    };
    if locked_source.reference.is_empty() || locked_source.source_type != remote_source.source_type
    {
        return remote.clone();
    }

    locked_source.url.clone_from(&remote_source.url);
    locked_source.mirrors.clone_from(&remote_source.mirrors);

    let (Some(locked_dist), Some(remote_dist)) = (&mut refreshed.dist, &remote.dist) else {
        return refreshed;
    };
    if locked_dist.dist_type != remote_dist.dist_type {
        return refreshed;
    }

    if let Some(reference) = locked_dist.reference.as_deref() {
        if KNOWN_DIST_HOST.is_match(&remote_dist.url) {
            let replacement = format!("${{prefix}}{reference}${{suffix}}");
            locked_dist.url = DIST_REFERENCE
                .replace(&remote_dist.url, replacement)
                .into_owned();
        }
    }
    locked_dist.mirrors.clone_from(&remote_dist.mirrors);
    refreshed
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

fn locked_package_is_symlinked_path(package: &LockedPackage) -> bool {
    package
        .dist
        .as_ref()
        .is_some_and(|dist| dist.dist_type == "path")
        && package
            .transport_options
            .as_ref()
            .and_then(|options| options.get("symlink"))
            .and_then(serde_json::Value::as_bool)
            != Some(false)
}

fn expand_update_allowlist_before_lock_injection(
    patterns: &[String],
    package_batches: &BTreeMap<CompactString, Vec<Arc<Package>>>,
    lock: &RiffLockfile,
    root_requirements: &HashSet<String>,
    include_dependencies: bool,
    include_root_requirements: bool,
) -> HashSet<String> {
    let mut known_names: HashSet<String> =
        package_batches.keys().map(ToString::to_string).collect();
    known_names.extend(
        lock.all_packages()
            .map(|package| canonical_package_name(&package.name).into_owned()),
    );

    let mut allowlist = HashSet::new();
    for pattern in patterns {
        if pattern.contains('*') {
            allowlist.extend(
                known_names
                    .iter()
                    .filter(|name| package_name_matches(pattern, name))
                    .cloned(),
            );
        } else {
            allowlist.insert(canonical_package_name(pattern).into_owned());
        }
    }
    if !include_dependencies {
        return allowlist;
    }

    let explicit = allowlist.clone();
    let mut queue: VecDeque<_> = allowlist.iter().cloned().collect();
    while let Some(package_name) = queue.pop_front() {
        let mut dependencies: Vec<String> = Vec::new();
        if let Some(packages) = package_batches.get(package_name.as_str()) {
            for package in packages {
                dependencies.extend(package.require.keys().map(ToString::to_string));
            }
        }
        for package in lock
            .all_packages()
            .filter(|package| package.name.eq_ignore_ascii_case(&package_name))
        {
            dependencies.extend(package.require.keys().cloned());
        }

        for dependency in dependencies {
            if is_platform_package(&dependency) {
                continue;
            }
            let dependency = canonical_package_name(&dependency).into_owned();
            if !include_root_requirements
                && root_requirements.contains(&dependency)
                && !explicit.contains(&dependency)
            {
                continue;
            }
            if allowlist.insert(dependency.clone()) {
                queue.push_back(dependency.clone());
            }

            // Replacers participate in transitive partial updates in the same
            // way as the post-pool allowlist expansion below.
            let mut replacers = Vec::new();
            for packages in package_batches.values() {
                replacers.extend(
                    packages
                        .iter()
                        .filter(|package| {
                            package.replace.keys().any(|name| {
                                canonical_package_name(name).as_ref() == dependency.as_str()
                            })
                        })
                        .map(|package| canonical_package_name(&package.name).into_owned()),
                );
            }
            replacers.extend(
                lock.all_packages()
                    .filter(|package| {
                        package.replace.keys().any(|name| {
                            canonical_package_name(name).as_ref() == dependency.as_str()
                        })
                    })
                    .map(|package| canonical_package_name(&package.name).into_owned()),
            );
            for replacer in replacers {
                if !include_root_requirements
                    && root_requirements.contains(&replacer)
                    && !explicit.contains(&replacer)
                {
                    continue;
                }
                if allowlist.insert(replacer.clone()) {
                    queue.push_back(replacer);
                }
            }
        }
    }
    allowlist
}

fn expand_update_patterns(
    patterns: &[String],
    pool: &Pool,
    lock: &RiffLockfile,
) -> HashSet<String> {
    let mut known_names: HashSet<String> = lock
        .all_packages()
        .map(|package| canonical_package_name(&package.name).into_owned())
        .collect();
    known_names.extend(
        pool.all_package_ids()
            .filter_map(|id| pool.entry(id))
            .map(|entry| canonical_package_name(entry.name()).into_owned()),
    );

    let mut expanded = HashSet::new();
    for pattern in patterns {
        if pattern.contains('*') {
            expanded.extend(
                known_names
                    .iter()
                    .filter(|name| package_name_matches(pattern, name))
                    .cloned(),
            );
        } else {
            expanded.insert(canonical_package_name(pattern).into_owned());
        }
    }
    expanded
}

fn selected_package_identities_changed(
    current: &RiffLockfile,
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
    let mut packages_by_satisfied_name: HashMap<String, Vec<&Package>> = HashMap::new();
    for package in packages {
        for name in package.get_names(true) {
            packages_by_satisfied_name
                .entry(canonical_package_name(&name).into_owned())
                .or_default()
                .push(package);
        }
    }

    let mut result: HashSet<String> = HashSet::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = roots.iter().cloned().collect();

    while let Some(name) = queue.pop_front() {
        if !visited.insert(name.clone()) {
            continue;
        }

        if let Some(providers) = packages_by_satisfied_name.get(&name) {
            for package in providers {
                result.insert(canonical_package_name(&package.name).into_owned());
                for dep_name in package.require.keys() {
                    if !is_platform_package(dep_name) {
                        let dependency = canonical_package_name(dep_name);
                        if !visited.contains(dependency.as_ref()) {
                            queue.push_back(dependency.into_owned());
                        }
                    }
                }
            }
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
                    queue.push_back(dependency.clone());
                }

                // Composer propagates partial updates through `replace` links,
                // but deliberately not through `provide` links. A locked
                // replacer is therefore unlocked when an allowed package gains
                // or changes a requirement on the virtual name it replaces.
                for provider_id in pool.what_provides(&dependency, None) {
                    let Some(provider) = pool.package(provider_id) else {
                        continue;
                    };
                    if !provider.replace.keys().any(|replaced| {
                        canonical_package_name(replaced).as_ref() == dependency.as_str()
                    }) {
                        continue;
                    }
                    let provider_name = canonical_package_name(&provider.name).into_owned();
                    if !include_root_requirements
                        && root_requirements.contains(&provider_name)
                        && !explicit.contains(&provider_name)
                    {
                        continue;
                    }
                    if allowlist.insert(provider_name.clone()) {
                        queue.push_back(provider_name);
                    }
                }
            }
        }
    }

    allowlist
}

fn skipped_root_update_dependencies(
    pool: &Pool,
    explicit: &HashSet<String>,
    root_requirements: &HashSet<String>,
) -> BTreeSet<(String, Option<String>)> {
    let mut skipped = BTreeSet::new();
    let mut visited = HashSet::new();
    let mut queue: VecDeque<_> = explicit.iter().cloned().collect();

    while let Some(package_name) = queue.pop_front() {
        if !visited.insert(package_name.clone()) {
            continue;
        }
        for package_id in pool.packages_by_name(&package_name) {
            let Some(package) = pool.package(package_id) else {
                continue;
            };
            for dependency in package.require.keys() {
                if is_platform_package(dependency) {
                    continue;
                }
                let dependency = canonical_package_name(dependency).into_owned();
                if root_requirements.contains(&dependency) && !explicit.contains(&dependency) {
                    let replacers: BTreeSet<_> = pool
                        .what_provides(&dependency, None)
                        .into_iter()
                        .filter_map(|provider_id| pool.package(provider_id))
                        .filter(|provider| {
                            provider.replace.keys().any(|replaced| {
                                canonical_package_name(replaced).as_ref() == dependency
                            })
                        })
                        .map(|provider| canonical_package_name(&provider.name).into_owned())
                        .collect();
                    if replacers.is_empty() {
                        skipped.insert((dependency, None));
                    } else {
                        skipped.extend(
                            replacers
                                .into_iter()
                                .map(|provider| (provider, Some(dependency.clone()))),
                        );
                    }
                    continue;
                }
                if !visited.contains(&dependency) {
                    queue.push_back(dependency);
                }
            }
        }
    }

    skipped
}

fn configure_platform_check(
    generator: AutoloadGenerator,
    manifest: &RiffManifest,
    packages: &[PackageAutoload],
    mode: &PlatformCheck,
    ignored: &PlatformRequirementFilter,
) -> AutoloadGenerator {
    match platform_check_requirements(manifest, packages, mode, ignored) {
        Some(requirements) => generator.with_platform_check(requirements),
        None => generator,
    }
}

fn platform_check_requirements(
    manifest: &RiffManifest,
    packages: &[PackageAutoload],
    mode: &PlatformCheck,
    ignored: &PlatformRequirementFilter,
) -> Option<PlatformCheckRequirements> {
    if matches!(mode, PlatformCheck::False) || ignored.all {
        return None;
    }

    let parser = VersionParser::new();
    let mut requirements = PlatformCheckRequirements::default();
    let include_requirement = |name: &str| {
        name.eq_ignore_ascii_case("php")
            || name.eq_ignore_ascii_case("php-64bit")
            || (matches!(mode, PlatformCheck::True)
                && name
                    .get(..4)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("ext-")))
    };
    let mut add_requirement = |name: &str, constraint: &str| {
        if !include_requirement(name) || ignored.ignores(name) {
            return;
        }
        let name = name.to_ascii_lowercase();
        let replace = requirements.requires.get(&name).is_none_or(|current| {
            match (
                parser.parse_constraints(constraint),
                parser.parse_constraints(current),
            ) {
                (Ok(candidate), Ok(current)) => candidate
                    .lower_bound()
                    .compare_to(&current.lower_bound(), ">"),
                (Ok(_), Err(_)) => true,
                _ => false,
            }
        });
        if replace {
            requirements.requires.insert(name, constraint.to_owned());
        }
    };

    for (name, constraint) in &manifest.require {
        add_requirement(name, constraint);
    }
    for package in packages.iter().filter(|package| !package.dev_requirement) {
        if let Some(locked) = &package.locked_package {
            for (name, constraint) in &locked.require {
                add_requirement(name, constraint);
            }
        }
    }

    requirements.provides.extend(
        manifest
            .provide
            .iter()
            .filter(|(name, _)| include_requirement(name))
            .map(|(name, constraint)| (name.to_ascii_lowercase(), constraint.clone())),
    );
    requirements.replaces.extend(
        manifest
            .replace
            .iter()
            .filter(|(name, _)| include_requirement(name))
            .map(|(name, constraint)| (name.to_ascii_lowercase(), constraint.clone())),
    );
    for package in packages {
        let Some(locked) = &package.locked_package else {
            continue;
        };
        requirements.provides.extend(
            locked
                .provide
                .iter()
                .filter(|(name, _)| include_requirement(name))
                .map(|(name, constraint)| {
                    (
                        name.to_ascii_lowercase(),
                        resolve_self_version(constraint, &locked.version),
                    )
                }),
        );
        requirements.replaces.extend(
            locked
                .replace
                .iter()
                .filter(|(name, _)| include_requirement(name))
                .map(|(name, constraint)| {
                    (
                        name.to_ascii_lowercase(),
                        resolve_self_version(constraint, &locked.version),
                    )
                }),
        );
    }

    (!requirements.requires.is_empty()).then_some(requirements)
}

fn resolve_self_version(constraint: &str, package_version: &str) -> String {
    if constraint == "self.version" {
        package_version.to_owned()
    } else {
        constraint.to_owned()
    }
}

fn validate_platform_requirements(
    manifest: &RiffManifest,
    packages: &[Package],
    platform_packages: &[Package],
    ignored: &PlatformRequirementFilter,
    no_dev: bool,
) -> std::result::Result<(), Vec<String>> {
    let candidates: HashMap<_, _> = platform_packages
        .iter()
        .map(|package| {
            (
                canonical_package_name(&package.name).into_owned(),
                package.version.to_string(),
            )
        })
        .collect();
    let mut requirements: Vec<(&str, &str)> = manifest
        .require
        .iter()
        .map(|(name, constraint)| (name.as_str(), constraint.as_str()))
        .collect();
    if !no_dev {
        requirements.extend(
            manifest
                .require_dev
                .iter()
                .map(|(name, constraint)| (name.as_str(), constraint.as_str())),
        );
    }
    for package in packages {
        requirements.extend(
            package
                .require
                .iter()
                .map(|(name, constraint)| (name.as_str(), constraint.as_str())),
        );
    }

    let mut problems = Vec::new();
    for (name, constraint) in requirements {
        if !is_platform_package(name) || ignored.ignores(name) {
            continue;
        }
        let constraint = ignored.filter_constraint(name, constraint);
        let canonical = canonical_package_name(name);
        match candidates.get(canonical.as_ref()) {
            Some(version) if Semver::satisfies(version, &constraint) => {}
            Some(version) => problems.push(format!(
                "{name} {version} does not satisfy required constraint {constraint}"
            )),
            None => problems.push(format!("required platform package {name} is missing")),
        }
    }
    problems.sort();
    problems.dedup();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Validate dependency relationships represented by an install's locked set.
///
/// `install` must not resolve new package versions, but it still needs to reject
/// an internally inconsistent lock. In particular, packages sharing an actual
/// or replaced name cannot coexist, and every non-platform requirement must be
/// satisfiable by another locked package, provider, or replacer.
#[derive(Default)]
struct LockedPackageRelationProblems {
    root_requirements: Vec<String>,
    solver: Vec<String>,
}

fn validate_locked_package_relations(
    manifest: &RiffManifest,
    root_version: &RootVersion,
    packages: &[Package],
    no_dev: bool,
) -> LockedPackageRelationProblems {
    let mut problems = LockedPackageRelationProblems::default();
    let mut claimed_names: HashMap<String, (&str, &str)> = HashMap::new();
    let mut pool = Pool::new();

    for package in packages {
        let mut package_claims = HashSet::new();
        package_claims.insert(canonical_package_name(&package.name).into_owned());
        package_claims.extend(
            package
                .replace
                .keys()
                .map(|name| canonical_package_name(name).into_owned()),
        );
        for name in package_claims {
            if let Some((other_name, other_version)) =
                claimed_names.insert(name.clone(), (&package.name, package.pretty_version()))
            {
                if !other_name.eq_ignore_ascii_case(&package.name) {
                    problems.solver.push(format!(
                        "Locked packages {other_name} ({other_version}) and {} ({}) cannot coexist because both claim {name}",
                        package.name,
                        package.pretty_version()
                    ));
                }
            }
        }
        pool.add_platform_package(package.clone());
    }
    let root_package = create_root_package(manifest, root_version);
    if !root_package.replace.is_empty() || !root_package.provide.is_empty() {
        pool.add_platform_package(root_package);
    }
    add_package_aliases(&mut pool, manifest);

    let root_requirements = manifest.require.iter().chain(
        (!no_dev)
            .then_some(&manifest.require_dev)
            .into_iter()
            .flatten(),
    );
    for (name, constraint) in root_requirements {
        if !is_platform_package(name) && pool.what_provides(name, Some(constraint)).is_empty() {
            problems.root_requirements.push(format!(
                "Root composer.json requires {name} {constraint}, but the lock file does not contain a matching package"
            ));
        }
    }

    for package in packages {
        for (name, constraint) in &package.require {
            if !is_platform_package(name) && pool.what_provides(name, Some(constraint)).is_empty() {
                problems.solver.push(format!(
                    "{} {} requires {name} {constraint}, but the lock file does not contain a matching package",
                    package.name,
                    package.pretty_version()
                ));
            }
        }
        for (name, constraint) in &package.conflict {
            let conflicts = pool.what_provides(name, Some(constraint));
            if conflicts.into_iter().any(|id| {
                pool.package(id).is_some_and(|candidate| {
                    !candidate.name.eq_ignore_ascii_case(&package.name)
                        || candidate.version != package.version
                })
            }) {
                problems.solver.push(format!(
                    "{} {} conflicts with locked package {name} {constraint}",
                    package.name,
                    package.pretty_version()
                ));
            }
        }
    }

    problems.root_requirements.sort();
    problems.root_requirements.dedup();
    problems.solver.sort();
    problems.solver.dedup();
    problems
}

fn filter_package_platform_requirements(package: &mut Package, filter: &PlatformRequirementFilter) {
    package
        .require
        .retain(|requirement, _| !is_platform_package(requirement) || !filter.ignores(requirement));
    for (requirement, constraint) in &mut package.require {
        if is_platform_package(requirement) {
            *constraint = filter.filter_constraint(requirement, constraint).into();
        }
    }
    package
        .conflict
        .retain(|requirement, _| !is_platform_package(requirement) || !filter.ignores(requirement));
}

/// Composer never submits the fixed root package or platform packages to its
/// dependency-policy pool filters.
fn update_policy_package_ids(pool: &Pool, root_id: PackageId) -> Vec<PackageId> {
    pool.all_package_ids()
        .filter(|package_id| *package_id != root_id)
        .filter(|package_id| {
            pool.entry(*package_id)
                .and_then(crate::solver::PoolEntry::as_package)
                .is_some_and(|package| !is_platform_package(&package.name))
        })
        .collect()
}

fn patch_update_candidate_allowed(lock: Option<&RiffLockfile>, candidate: &Package) -> bool {
    let Some(locked) = lock.and_then(|lock| {
        lock.all_packages()
            .find(|locked| locked.name.eq_ignore_ascii_case(&candidate.name))
    }) else {
        return true;
    };
    let locked = Package::from(locked);
    match (
        numeric_major_minor(&locked.version),
        numeric_major_minor(&candidate.version),
    ) {
        (Some(locked), Some(candidate)) => locked == candidate,
        _ => locked.version == candidate.version,
    }
}

fn numeric_major_minor(version: &str) -> Option<(u64, u64)> {
    let mut parts = version
        .split_once('-')
        .map_or(version, |(base, _)| base)
        .split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

fn package_satisfies_temporary_constraints(
    package: &Package,
    constraints: &HashMap<String, String>,
) -> bool {
    if constraints
        .get(canonical_package_name(&package.name).as_ref())
        .is_some_and(|constraint| !Semver::satisfies(&package.version, constraint))
    {
        return false;
    }

    let parser = VersionParser::new();
    package
        .replace
        .iter()
        .chain(package.provide.iter())
        .all(|(capability, provided)| {
            let Some(temporary) = constraints.get(canonical_package_name(capability).as_ref())
            else {
                return true;
            };
            if provided == "self.version" {
                return Semver::satisfies(&package.version, temporary);
            }
            parser
                .parse_constraints_cached(provided)
                .is_ok_and(|provided| provided.intersects(temporary).unwrap_or(false))
        })
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
        fileless: false,
        dev_requirement: is_dev,
        aliases,
        replaces: lp.replace.clone(),
        provides: lp.provide.clone(),
        locked_package: Some(lp.clone()),
        installation_source,
        include_paths: Vec::new(),
        target_dir: None,
    }
}

fn apply_plugin_package_layouts(riff: &Riff, packages: &mut [PackageAutoload]) {
    let installed = packages
        .iter()
        .filter_map(|package| package.locked_package.as_ref())
        .map(Package::from)
        .collect::<Vec<_>>();
    let layouts = riff.plugins().package_layouts(&installed);
    for package in packages {
        package.fileless = package
            .locked_package
            .as_ref()
            .map(Package::from)
            .is_some_and(|locked| layouts.is_fileless(&locked));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_policy_candidates_exclude_the_root_and_platform_packages() {
        let mut pool = Pool::new();
        let root_id = pool.add_platform_package(Package::new("root/package", "1.0.0"));
        pool.add_platform_package(Package::new("php", "8.4.0"));
        let dependency_id = pool.add_package(Package::new("vendor/dependency", "1.0.0"));

        assert_eq!(update_policy_package_ids(&pool, root_id), [dependency_id]);
    }

    #[test]
    fn policy_diagnostics_group_candidate_versions_and_cap_advisory_ids() {
        let advisory = |id: &str| {
            PolicyViolation::Advisory(crate::json::SecurityAdvisory {
                advisory_id: id.to_owned(),
                package_name: "vendor/package".to_owned(),
                affected_versions: "*".to_owned(),
                source: None,
                title: None,
                cve: None,
                link: None,
                severity: None,
                reported_at: None,
                sources: None,
            })
        };
        let first = Package::new("vendor/package", "1.0.0");
        let second = Package::new("vendor/package", "1.1.0");
        let mut diagnostics = PolicyDiagnostics::default();
        for id in ["ADV-1", "ADV-2", "ADV-3", "ADV-4", "ADV-5", "ADV-6"] {
            diagnostics.record(&first, advisory(id));
        }
        diagnostics.record(&second, advisory("ADV-1"));

        assert_eq!(
            diagnostics.lines(),
            ["Package vendor/package: 2 candidate versions were excluded by 6 security advisories (ADV-1, ADV-2, ADV-3, ADV-4, ADV-5, and 1 more)."]
        );
    }

    #[test]
    fn platform_check_uses_strongest_production_php_lower_bound() {
        let mut manifest = RiffManifest::default();
        manifest.require.insert("php".into(), ">=8.1".into());
        manifest.require.insert("ext-ctype".into(), "*".into());

        let mut production = LockedPackage {
            name: "vendor/production".into(),
            version: "1.0.0".into(),
            ..Default::default()
        };
        production.require.insert("php".into(), "^8.4.1".into());
        production.require.insert("ext-json".into(), "*".into());
        let mut development = LockedPackage {
            name: "vendor/development".into(),
            version: "1.0.0".into(),
            ..Default::default()
        };
        development.require.insert("php".into(), ">=9".into());

        let packages = [
            locked_package_to_autoload(&production, false, &HashMap::new(), false),
            locked_package_to_autoload(&development, true, &HashMap::new(), false),
        ];
        let requirements = platform_check_requirements(
            &manifest,
            &packages,
            &PlatformCheck::True,
            &PlatformRequirementFilter::default(),
        )
        .unwrap();
        assert_eq!(requirements.requires["php"], "^8.4.1");
        assert_eq!(requirements.requires["ext-ctype"], "*");
        assert_eq!(requirements.requires["ext-json"], "*");

        let php_only = platform_check_requirements(
            &manifest,
            &packages,
            &PlatformCheck::PhpOnly,
            &PlatformRequirementFilter::default(),
        )
        .unwrap();
        assert_eq!(php_only.requires.len(), 1);
        assert_eq!(php_only.requires["php"], "^8.4.1");
    }

    #[test]
    fn platform_check_respects_disabled_and_ignored_requirements() {
        let mut manifest = RiffManifest::default();
        manifest.require.insert("php".into(), ">=8.4".into());
        assert!(platform_check_requirements(
            &manifest,
            &[],
            &PlatformCheck::False,
            &PlatformRequirementFilter::default(),
        )
        .is_none());
        assert!(platform_check_requirements(
            &manifest,
            &[],
            &PlatformCheck::PhpOnly,
            &PlatformRequirementFilter {
                all: false,
                requirements: vec!["php".into()],
            },
        )
        .is_none());
    }

    #[test]
    fn pre_autoload_listener_paths_join_the_root_classmap() {
        let manifest = RiffManifest::default();
        let generated = std::path::PathBuf::from("vendor/composer/GeneratedDiscoveryStrategy.php");
        let autoload = root_autoload(&manifest, true, std::slice::from_ref(&generated))
            .expect("root autoload is always present");

        assert!(autoload
            .classmap
            .contains(&CompactString::new(generated.to_string_lossy())));
    }

    #[test]
    fn changed_package_identity_proves_lock_change() {
        let package = Package::new("vendor/package", "2.0.0.0");
        let current = RiffLockfile {
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
        let current = RiffLockfile {
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
        let current = RiffLockfile {
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
    fn dependency_update_warnings_identify_direct_and_replaced_root_requirements() {
        let mut pool = Pool::new();
        let mut target = Package::new("fixture/target", "2.0.0");
        target
            .require
            .insert("fixture/direct-root".to_string(), "*".to_string());
        target
            .require
            .insert("fixture/virtual-root".to_string(), "*".to_string());
        pool.add_package(target);
        pool.add_package(Package::new("fixture/direct-root", "1.0.0"));
        let mut replacer = Package::new("fixture/replacer", "1.0.0");
        replacer
            .replace
            .insert("fixture/virtual-root".to_string(), "1.0.0".to_string());
        pool.add_package(replacer);

        let skipped = skipped_root_update_dependencies(
            &pool,
            &HashSet::from(["fixture/target".to_string()]),
            &HashSet::from([
                "fixture/direct-root".to_string(),
                "fixture/virtual-root".to_string(),
            ]),
        );

        assert!(skipped.contains(&("fixture/direct-root".to_string(), None)));
        assert!(skipped.contains(&(
            "fixture/replacer".to_string(),
            Some("fixture/virtual-root".to_string())
        )));
    }

    #[test]
    fn update_package_patterns_match_composer_wildcards() {
        assert!(package_name_matches("vendor/*", "vendor/package"));
        assert!(package_name_matches("*/package", "vendor/package"));
        assert!(package_name_matches("*", "vendor/package"));
        assert!(package_name_matches("ven*r/pack*", "vendor/package"));
        assert!(!package_name_matches("other/*", "vendor/package"));
        assert!(!package_name_matches("*/pkg", "vendor/package"));
    }

    #[test]
    fn pending_constraints_merge_in_insertion_order() {
        let mut pending = FastHashMap::new();
        merge_pending_constraint(&mut pending, "vendor/package".into(), "^1".into());
        merge_pending_constraint(&mut pending, "vendor/package".into(), "^2".into());
        merge_pending_constraint(&mut pending, "vendor/package".into(), "^3".into());

        assert_eq!(pending.get("vendor/package").unwrap(), "^1 || ^2 || ^3");
    }

    #[test]
    fn root_stability_flags_include_explicit_and_inferred_unstable_constraints() {
        assert_eq!(
            extract_stability_flag("^1.0@beta", Stability::Stable),
            Some(Stability::Beta)
        );
        assert_eq!(
            extract_stability_flag("dev-main#abcdef", Stability::Stable),
            Some(Stability::Dev)
        );
        assert_eq!(
            extract_stability_flag("^1.0 || 2.0.0-alpha", Stability::Stable),
            Some(Stability::Alpha)
        );
        assert_eq!(
            extract_stability_flag("2.0.0-beta", Stability::Dev),
            None,
            "a more stable constraint needs no override when dev is globally allowed"
        );
    }

    #[test]
    fn explicit_stability_scanner_matches_composer_suffixes_without_partial_words() {
        for (constraint, expected) in [
            ("^1@DEV", Some(Stability::Dev)),
            ("^1@alpha || ^2@RC", Some(Stability::Alpha)),
            ("^1@stable || ^2@beta", Some(Stability::Beta)),
            ("^1@devfoo", None),
            ("^1@rc_1", None),
            ("^1@betaé", None),
        ] {
            assert_eq!(
                explicit_stability_flag(constraint),
                expected,
                "constraint {constraint:?}"
            );
        }
    }

    // Ported from Composer\Test\Package\Loader\RootPackageLoaderTest::
    // testStabilityFlagsParsing.
    #[test]
    fn composer_root_package_loader_parses_stability_flags_across_disjunctions() {
        let mut manifest = RiffManifest {
            minimum_stability: Some("alpha".to_string()),
            ..RiffManifest::default()
        };
        for (name, constraint) in [
            ("foo/bar", "~2.1.0-beta2"),
            ("bar/baz", "1.0.x-dev as 1.2.0"),
            ("qux/quux", "1.0.*@rc"),
            ("zux/complex", "~1.0,>=1.0.2@dev"),
            ("or/op", "^2.0@dev || ^2.0@dev"),
            ("multi/lowest-wins", "^2.0@rc || >=3.0@dev , ~3.5@alpha"),
            ("or/op-without-flags", "dev-master || 2.0 , ~3.5-alpha"),
            ("or/op-without-flags2", "3.0-beta || 2.0 , ~3.5-alpha"),
        ] {
            manifest
                .require
                .insert(name.to_string(), constraint.to_string());
        }

        let flags = root_stability_flags(&manifest, Stability::Alpha);
        assert_eq!(flags.get("foo/bar"), None);
        for (name, stability) in [
            ("bar/baz", Stability::Dev),
            ("qux/quux", Stability::RC),
            ("zux/complex", Stability::Dev),
            ("or/op", Stability::Dev),
            ("multi/lowest-wins", Stability::Dev),
            ("or/op-without-flags", Stability::Dev),
            ("or/op-without-flags2", Stability::Alpha),
        ] {
            assert_eq!(flags.get(name), Some(&stability), "unexpected {name} flag");
        }
    }

    #[test]
    fn root_stability_flags_merge_require_and_require_dev_by_package() {
        let mut manifest = RiffManifest::default();
        manifest
            .require
            .insert("Vendor/Package".to_string(), "2.0.0-beta".to_string());
        manifest
            .require_dev
            .insert("vendor/package".to_string(), "dev-main".to_string());

        let flags = root_stability_flags(&manifest, Stability::Stable);
        assert_eq!(flags.get("vendor/package"), Some(&Stability::Dev));
    }

    #[test]
    fn root_references_extract_dev_constraint_hashes() {
        let mut manifest = RiffManifest::default();
        manifest.require.insert(
            "Vendor/Package".to_string(),
            "dev-main#abcdef as 1.0.0".to_string(),
        );
        manifest
            .require_dev
            .insert("vendor/ignored".to_string(), "1.0.0#abcdef".to_string());

        assert_eq!(
            root_references(&manifest),
            HashMap::from([("vendor/package".to_string(), "abcdef".to_string())])
        );
    }

    #[test]
    fn root_inline_and_default_branch_aliases_are_added_to_the_solver_pool() {
        let mut manifest = RiffManifest::default();
        manifest.require.insert(
            "vendor/package".to_string(),
            "dev-main as 1.0.0".to_string(),
        );
        let mut package = Package::new("vendor/package", "dev-main");
        package.default_branch = Some(true);

        let mut pool = Pool::with_minimum_stability(Stability::Dev);
        pool.add_package(package);
        add_package_aliases(&mut pool, &manifest);

        let aliases: Vec<_> = pool
            .packages_by_name("vendor/package")
            .into_iter()
            .filter_map(|id| match pool.entry(id) {
                Some(crate::solver::PoolEntry::Alias(alias)) => Some(alias),
                _ => None,
            })
            .collect();
        assert!(aliases
            .iter()
            .any(|alias| alias.pretty_version() == "1.0.0" && alias.is_root_package_alias()));
        assert!(aliases.iter().any(|alias| {
            alias.pretty_version() == DEFAULT_BRANCH_ALIAS && !alias.is_root_package_alias()
        }));
    }

    // Ported from Composer\Test\Package\Loader\ArrayLoaderTest::
    // testPackageAliasingWithoutBranchAlias.
    #[test]
    fn implicit_default_branch_alias_is_only_added_for_nonnumeric_dev_branches() {
        for (version, pretty, default_branch, expects_alias) in [
            ("dev-main", "dev-main", true, true),
            (DEFAULT_BRANCH_ALIAS, "dev-main", true, true),
            ("dev-main", "dev-main", false, false),
            ("2.9999999.9999999.9999999-dev", "2.x-dev", true, false),
            ("2.9999999.9999999.9999999-dev", "v2.x-dev", true, false),
        ] {
            let mut package = Package::new("vendor/package", version);
            package.pretty_version = Some(pretty.into());
            package.default_branch = Some(default_branch);
            let mut pool = Pool::with_minimum_stability(Stability::Dev);
            pool.add_package(package);

            add_package_aliases(&mut pool, &RiffManifest::default());

            let has_default_alias = pool
                .packages_by_name("vendor/package")
                .into_iter()
                .filter_map(|id| pool.entry(id).and_then(|entry| entry.as_alias()))
                .any(|alias| alias.pretty_version() == DEFAULT_BRANCH_ALIAS);
            assert_eq!(has_default_alias, expects_alias, "version {pretty}");
        }
    }

    #[test]
    fn root_inline_alias_is_added_alongside_a_branch_alias() {
        let mut manifest = RiffManifest::default();
        manifest.require.insert(
            "vendor/package".to_string(),
            "dev-master as 1.1.2".to_string(),
        );
        let mut package = Package::new("vendor/package", "dev-master");
        package.extra = Some(serde_json::json!({
            "branch-alias": {"dev-master": "2.x-dev"}
        }));

        let mut pool = Pool::with_minimum_stability(Stability::Dev);
        pool.add_package(package);
        add_package_aliases(&mut pool, &manifest);

        let aliases: Vec<_> = pool
            .packages_by_name("vendor/package")
            .into_iter()
            .filter_map(|id| pool.entry(id).and_then(|entry| entry.as_alias()))
            .collect();
        assert!(aliases
            .iter()
            .any(|alias| { alias.pretty_version() == "1.1.2" && alias.is_root_package_alias() }));
        assert!(aliases.iter().any(|alias| {
            alias.pretty_version() == "2.x-dev" && !alias.is_root_package_alias()
        }));
    }

    #[test]
    fn explicit_branch_alias_replaces_the_implicit_default_branch_alias() {
        let manifest = RiffManifest::default();
        let mut package = Package::new("vendor/package", "dev-main");
        package.default_branch = Some(true);
        package.extra = Some(serde_json::json!({
            "branch-alias": {"dev-main": "2.x-dev"}
        }));

        let mut pool = Pool::with_minimum_stability(Stability::Dev);
        pool.add_package(package);
        add_package_aliases(&mut pool, &manifest);

        let alias_versions: Vec<_> = pool
            .packages_by_name("vendor/package")
            .into_iter()
            .filter_map(|id| pool.entry(id).and_then(|entry| entry.as_alias()))
            .map(|alias| alias.pretty_version())
            .collect();
        assert_eq!(alias_versions, ["2.x-dev"]);
    }

    #[test]
    fn named_platform_ignores_distinguish_full_and_upper_bound_modes() {
        let full = PlatformRequirementFilter {
            all: false,
            requirements: vec!["ext-*".to_string()],
        };
        assert!(full.ignores("ext-json"));
        assert!(!full.ignores("php"));

        let upper = PlatformRequirementFilter {
            all: false,
            requirements: vec!["php+".to_string()],
        };
        assert!(!upper.ignores("php"));
        let filtered = upper.filter_constraint("php", "^8.1");
        assert!(Semver::satisfies("9.0.0", &filtered));
        assert!(!Semver::satisfies("7.4.0", &filtered));
    }

    #[test]
    fn composer_ignore_nothing_platform_requirement_filter_data_provider() {
        let filter = PlatformRequirementFilter::default();
        for requirement in ["php", "monolog/monolog"] {
            assert!(!filter.ignores(requirement));
            assert!(!filter.ignores_upper_bound(requirement));
        }
    }

    #[test]
    fn composer_ignore_all_platform_requirement_filter_data_provider() {
        let filter = PlatformRequirementFilter {
            all: true,
            requirements: Vec::new(),
        };
        assert!(filter.ignores("php"));
        assert!(filter.ignores_upper_bound("php"));
        assert!(!filter.ignores("monolog/monolog"));
        assert!(!filter.ignores_upper_bound("monolog/monolog"));
    }

    #[test]
    fn composer_ignore_list_platform_requirement_filter_data_provider() {
        let cases: &[(&[&str], &str, bool)] = &[
            (&["ext-json", "monolog/monolog"], "ext-json", true),
            (&["ext-json", "monolog/monolog"], "php", false),
            (&["ext-json", "monolog/monolog"], "monolog/monolog", false),
            (&["ext-*"], "ext-json", true),
            (&["ext-*", "php*"], "php", true),
            (&["foo", "*"], "ext-json", true),
            (&["*", "foo"], "php", true),
            (&["*", "monolog/*"], "monolog/monolog", false),
            (&[""], "ext-foo", false),
            (&[], "ext-foo", false),
            (&["ext-", "foo"], "ext-foo", false),
        ];

        for (patterns, requirement, expected) in cases {
            let filter = PlatformRequirementFilter {
                all: false,
                requirements: patterns
                    .iter()
                    .map(|pattern| (*pattern).to_string())
                    .collect(),
            };
            assert_eq!(filter.ignores(requirement), *expected);
        }
    }

    #[test]
    fn composer_ignore_list_platform_upper_bound_data_provider() {
        let cases: &[(&[&str], &str, bool)] = &[
            (&["ext-json", "monolog/monolog"], "ext-json", true),
            (&["ext-json+", "monolog/monolog"], "ext-json", true),
            (&["ext-json+", "monolog/monolog"], "php", false),
            (&["monolog/monolog"], "monolog/monolog", false),
            (&["ext-*+"], "ext-json", true),
            (&["ext-*+", "php*+"], "php", true),
            (&["foo", "*+"], "ext-json", true),
            (&["*+", "foo"], "php", true),
            (&["*+", "monolog/*+"], "monolog/monolog", false),
            (&[""], "ext-foo", false),
            (&[], "ext-foo", false),
            (&["ext-", "foo"], "ext-foo", false),
        ];

        for (patterns, requirement, expected) in cases {
            let filter = PlatformRequirementFilter {
                all: false,
                requirements: patterns
                    .iter()
                    .map(|pattern| (*pattern).to_string())
                    .collect(),
            };
            assert_eq!(filter.ignores_upper_bound(requirement), *expected);
        }
    }
}
