//! Autoload generator - creates PHP autoloader files.

use indexmap::IndexMap;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use regex::Regex;
use serde::Serialize;

use crate::json::LockedPackage;
use crate::package::Autoload;
use crate::Result;
use riff_semver::VersionParser;

use super::classmap::ClassMapGenerator;

/// Sort packages by dependency weight (topological sort).
/// Packages that are dependencies come first, alphabetical by name as tie-breaker.
fn sort_packages_by_dependency(packages: &[PackageAutoload]) -> Vec<PackageAutoload> {
    sort_packages_by_dependency_with_weights(packages, &HashMap::new())
}

fn sort_packages_by_dependency_with_weights(
    packages: &[PackageAutoload],
    preset_weights: &HashMap<String, isize>,
) -> Vec<PackageAutoload> {
    let package_names: HashSet<_> = packages
        .iter()
        .map(|package| package.name.as_str())
        .collect();
    let mut users: HashMap<String, Vec<String>> = HashMap::new();
    for package in packages {
        for dependency in &package.requires {
            if package_names.contains(dependency.as_str()) {
                users
                    .entry(dependency.clone())
                    .or_default()
                    .push(package.name.clone());
            }
        }
    }

    fn importance(
        name: &str,
        users: &HashMap<String, Vec<String>>,
        preset_weights: &HashMap<String, isize>,
        computing: &mut HashSet<String>,
        computed: &mut HashMap<String, isize>,
    ) -> isize {
        if let Some(weight) = computed.get(name) {
            return *weight;
        }
        if !computing.insert(name.to_string()) {
            return 0;
        }

        let mut weight = preset_weights.get(name).copied().unwrap_or_default();
        if let Some(package_users) = users.get(name) {
            for user in package_users {
                weight -= 1 - importance(user, users, preset_weights, computing, computed);
            }
        }
        computing.remove(name);
        computed.insert(name.to_string(), weight);
        weight
    }

    fn natural_name_cmp(left: &str, right: &str) -> Ordering {
        let left = left.as_bytes();
        let right = right.as_bytes();
        let (mut left_index, mut right_index) = (0, 0);
        while left_index < left.len() && right_index < right.len() {
            if left[left_index].is_ascii_digit() && right[right_index].is_ascii_digit() {
                let left_end = (left_index..left.len())
                    .find(|index| !left[*index].is_ascii_digit())
                    .unwrap_or(left.len());
                let right_end = (right_index..right.len())
                    .find(|index| !right[*index].is_ascii_digit())
                    .unwrap_or(right.len());
                let left_number = &left[left_index..left_end];
                let right_number = &right[right_index..right_end];
                match left_number
                    .len()
                    .cmp(&right_number.len())
                    .then_with(|| left_number.cmp(right_number))
                {
                    Ordering::Equal => {
                        left_index = left_end;
                        right_index = right_end;
                    }
                    ordering => return ordering,
                }
            } else {
                match left[left_index]
                    .to_ascii_lowercase()
                    .cmp(&right[right_index].to_ascii_lowercase())
                {
                    Ordering::Equal => {
                        left_index += 1;
                        right_index += 1;
                    }
                    ordering => return ordering,
                }
            }
        }
        left.len().cmp(&right.len())
    }

    let mut computing = HashSet::new();
    let mut computed = HashMap::new();
    for package in packages {
        importance(
            &package.name,
            &users,
            preset_weights,
            &mut computing,
            &mut computed,
        );
    }

    let mut sorted: Vec<_> = packages.to_vec();
    sorted.sort_by(|a, b| {
        computed[&a.name]
            .cmp(&computed[&b.name])
            .then_with(|| natural_name_cmp(&a.name, &b.name))
    });
    sorted
}

/// Configuration for autoload generation
#[derive(Debug, Clone)]
pub struct AutoloadConfig {
    /// Vendor directory
    pub vendor_dir: PathBuf,
    /// Base directory (project root)
    pub base_dir: PathBuf,
    /// Whether to optimize autoloader (authoritative classmap)
    pub optimize: bool,
    /// Whether to use APCu for caching
    pub apcu: bool,
    /// Optional custom APCu cache key prefix
    pub apcu_prefix: Option<String>,
    /// Whether to generate authoritative classmap
    pub authoritative: bool,
    /// Suffix for class names (content-hash from lock file)
    pub suffix: Option<String>,
}

impl Default for AutoloadConfig {
    fn default() -> Self {
        Self {
            vendor_dir: PathBuf::from("vendor"),
            base_dir: PathBuf::from("."),
            optimize: false,
            apcu: false,
            apcu_prefix: None,
            authoritative: false,
            suffix: None,
        }
    }
}

/// Package with autoload information for generation
#[derive(Debug, Clone)]
pub struct PackageAutoload {
    /// Package name
    pub name: String,
    /// Autoload configuration
    pub autoload: Autoload,
    /// Install path relative to vendor dir
    pub install_path: String,
    /// Package dependencies (required packages) - used for sorting
    pub requires: Vec<String>,
    /// Pretty version string (e.g., "1.2.3", "dev-main")
    pub pretty_version: Option<String>,
    /// Normalized version string (e.g., "1.2.3.0")
    pub version: Option<String>,
    /// VCS reference (commit hash, tag)
    pub reference: Option<String>,
    /// Package type (library, project, etc.)
    pub package_type: String,
    /// Whether an installer plugin declares this package has no installed files.
    pub fileless: bool,
    /// Whether this is a dev requirement
    pub dev_requirement: bool,
    /// Version aliases
    pub aliases: Vec<String>,
    /// Packages that this package replaces (name -> version constraint)
    pub replaces: IndexMap<String, String>,
    /// Packages that this package provides (name -> version constraint)
    pub provides: IndexMap<String, String>,
    /// Original lock entry used to generate Composer's installed repository
    pub locked_package: Option<LockedPackage>,
    /// Whether the package was installed from dist or source
    pub installation_source: Option<String>,
    /// Legacy include-path entries contributed by this package.
    pub include_paths: Vec<String>,
    /// Legacy target directory relative to the package install path.
    pub target_dir: Option<String>,
}

impl PackageAutoload {
    /// Returns true if this is a metapackage (no files, only dependencies)
    pub fn is_metapackage(&self) -> bool {
        self.fileless || self.package_type == crate::package::package_type::METAPACKAGE
    }
}

impl Default for PackageAutoload {
    fn default() -> Self {
        Self {
            name: String::new(),
            autoload: Autoload::default(),
            install_path: String::new(),
            requires: Vec::new(),
            pretty_version: None,
            version: None,
            reference: None,
            package_type: "library".to_string(),
            fileless: false,
            dev_requirement: false,
            aliases: Vec::new(),
            replaces: IndexMap::new(),
            provides: IndexMap::new(),
            locked_package: None,
            installation_source: None,
            include_paths: Vec::new(),
            target_dir: None,
        }
    }
}

/// Inputs used to build Composer's runtime platform check.
#[derive(Debug, Clone, Default)]
pub struct PlatformCheckRequirements {
    pub requires: IndexMap<String, String>,
    pub provides: IndexMap<String, String>,
    pub replaces: IndexMap<String, String>,
    pub ignored: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoloadGenerationEvent {
    PreGenerate,
    PostGenerate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoloadGenerationResult {
    pub class_count: usize,
}

#[derive(Debug, Clone, Default)]
struct GenerationOptions {
    root_include_paths: Vec<String>,
    root_target_dir: Option<String>,
    root_requires: Vec<String>,
    use_global_include_path: bool,
    platform: Option<PlatformCheckRequirements>,
    strict_psr: bool,
}

/// Root package information for installed.php
#[derive(Debug, Clone, Default)]
pub struct RootPackageInfo {
    /// Package name (vendor/package format)
    pub name: String,
    /// Pretty version string
    pub pretty_version: String,
    /// Normalized version string
    pub version: String,
    /// VCS reference
    pub reference: Option<String>,
    /// Package type
    pub package_type: String,
    /// Version aliases
    pub aliases: Vec<String>,
    /// Packages replaced by the root package.
    pub replaces: IndexMap<String, String>,
    /// Virtual packages provided by the root package.
    pub provides: IndexMap<String, String>,
    /// Whether dev dependencies are installed
    pub dev_mode: bool,
}

/// Autoload generator
pub struct AutoloadGenerator {
    config: AutoloadConfig,
    classmap_generator: ClassMapGenerator,
    options: GenerationOptions,
    event_handler:
        Option<Box<dyn Fn(AutoloadGenerationEvent) -> Result<()> + Send + Sync + 'static>>,
    precomputed_classmaps: HashMap<PathBuf, HashMap<String, PathBuf>>,
}

impl AutoloadGenerator {
    /// Create a new autoload generator
    pub fn new(config: AutoloadConfig) -> Self {
        Self {
            config,
            classmap_generator: ClassMapGenerator::new(),
            options: GenerationOptions::default(),
            event_handler: None,
            precomputed_classmaps: HashMap::new(),
        }
    }

    pub fn with_root_include_paths(mut self, paths: impl IntoIterator<Item = String>) -> Self {
        self.options.root_include_paths = paths.into_iter().collect();
        self
    }

    pub fn with_root_target_dir(mut self, target_dir: impl Into<String>) -> Self {
        self.options.root_target_dir = Some(target_dir.into());
        self
    }

    pub fn with_root_requires(mut self, requirements: impl IntoIterator<Item = String>) -> Self {
        self.options.root_requires = requirements.into_iter().collect();
        self
    }

    pub fn with_global_include_path(mut self, enabled: bool) -> Self {
        self.options.use_global_include_path = enabled;
        self
    }

    pub fn with_platform_check(mut self, requirements: PlatformCheckRequirements) -> Self {
        self.options.platform = Some(requirements);
        self
    }

    pub fn with_strict_psr(mut self, enabled: bool) -> Self {
        self.options.strict_psr = enabled;
        self
    }

    pub fn with_event_handler(
        mut self,
        handler: impl Fn(AutoloadGenerationEvent) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        self.event_handler = Some(Box::new(handler));
        self
    }

    /// Reuse class scans collected while packages were extracted. Paths not
    /// present here are scanned synchronously, which covers root paths added by
    /// lifecycle hooks.
    pub fn with_precomputed_classmaps(
        mut self,
        classmaps: HashMap<PathBuf, HashMap<String, PathBuf>>,
    ) -> Self {
        self.precomputed_classmaps = classmaps;
        self
    }

    pub(crate) fn package_classmap_scan_plan(
        &self,
        packages: &[PackageAutoload],
        root_autoload: Option<&Autoload>,
    ) -> (HashMap<String, Vec<PathBuf>>, Vec<Regex>) {
        let excludes = self.collect_exclude_patterns(packages, root_autoload);
        let mut plans = HashMap::new();
        for package in packages {
            if package.is_metapackage() {
                continue;
            }
            let mut paths = package
                .autoload
                .classmap
                .iter()
                .map(|path| self.package_autoload_path(package, path))
                .collect::<Vec<_>>();
            if self.config.optimize || self.config.authoritative {
                paths.extend(
                    package
                        .autoload
                        .psr4
                        .values()
                        .flat_map(|paths| paths.as_vec())
                        .map(|path| self.package_autoload_path(package, &path)),
                );
                paths.extend(
                    package
                        .autoload
                        .psr0
                        .values()
                        .flat_map(|paths| paths.as_vec())
                        .map(|path| self.package_autoload_path(package, &path)),
                );
            }
            paths.sort();
            paths.dedup();
            if !paths.is_empty() {
                plans.insert(package.name.clone(), paths);
            }
        }
        (plans, excludes)
    }

    fn package_autoload_path(&self, package: &PackageAutoload, path: &str) -> PathBuf {
        let path = self.adjust_target_path(path, package.target_dir.as_deref(), false);
        self.config
            .vendor_dir
            .join(&package.install_path)
            .join(path)
    }

    pub fn duplicate_file_autoload_paths(
        &self,
        packages: &[PackageAutoload],
        root_autoload: Option<&Autoload>,
    ) -> Vec<String> {
        let mut paths = Vec::new();
        for package in sort_packages_by_dependency(packages) {
            if package.is_metapackage() {
                continue;
            }
            paths.extend(package.autoload.files.iter().map(|path| {
                let path = self.adjust_target_path(path, package.target_dir.as_deref(), false);
                self.get_path_code(&package.install_path, &path, false)
            }));
        }
        if let Some(root) = root_autoload {
            paths.extend(root.files.iter().map(|path| {
                let path =
                    self.adjust_target_path(path, self.options.root_target_dir.as_deref(), true);
                self.get_path_code("", &path, true)
            }));
        }
        let mut seen = HashSet::new();
        let mut reported = HashSet::new();
        paths
            .into_iter()
            .filter(|path| !seen.insert(path.clone()) && reported.insert(path.clone()))
            .collect()
    }

    /// Get the suffix for class names
    fn get_suffix(&self) -> String {
        self.config.suffix.clone().unwrap_or_else(|| {
            // Generate a random suffix if none provided
            let mut hasher = Md5::new();
            hasher.update(format!("{:?}", std::time::SystemTime::now()).as_bytes());
            format!("{:x}", hasher.finalize())[..16].to_string()
        })
    }

    /// Collect and compile exclude-from-classmap patterns from all packages
    fn collect_exclude_patterns(
        &self,
        packages: &[PackageAutoload],
        root_autoload: Option<&Autoload>,
    ) -> Vec<Regex> {
        let mut patterns = Vec::new();

        // Collect patterns from packages
        for pkg in packages {
            for pattern in &pkg.autoload.exclude_from_classmap {
                if let Some(regex) = self.compile_exclude_pattern(pattern, &pkg.install_path, false)
                {
                    patterns.push(regex);
                }
            }
        }

        // Collect patterns from root autoload
        if let Some(autoload) = root_autoload {
            for pattern in &autoload.exclude_from_classmap {
                if let Some(regex) = self.compile_exclude_pattern(pattern, "", true) {
                    patterns.push(regex);
                }
            }
        }

        patterns
    }

    /// Compile an exclude-from-classmap pattern to a regex
    /// Handles wildcards (* and **) similar to Composer
    fn compile_exclude_pattern(
        &self,
        pattern: &str,
        install_path: &str,
        is_root: bool,
    ) -> Option<Regex> {
        // Normalize path separators
        let pattern = pattern.replace('\\', "/").trim_matches('/').to_string();

        // Build the full path pattern
        let full_pattern = if is_root {
            // For root package, pattern is relative to base_dir
            let base = self.config.base_dir.to_string_lossy().replace('\\', "/");
            format!("{}/{}", base.trim_end_matches('/'), pattern)
        } else {
            // For packages, pattern is relative to the package install path
            let vendor = self.config.vendor_dir.to_string_lossy().replace('\\', "/");
            format!(
                "{}/{}/{}",
                vendor.trim_end_matches('/'),
                install_path,
                pattern
            )
        };

        // Escape regex special characters, but preserve * and **
        let escaped = regex::escape(&full_pattern);

        // Convert wildcards:
        // ** matches any characters including /
        // * matches any characters except /
        let regex_pattern = escaped
            .replace(r"\*\*", ".*") // ** -> match anything
            .replace(r"\*", "[^/]*"); // * -> match anything except /

        // Composer treats exclusions as path segments. A rule for `tests`, for
        // example, must not accidentally exclude a sibling named `testsuite`.
        let regex_pattern = format!(r"{regex_pattern}(?:$|/)");

        // Compile the regex
        Regex::new(&regex_pattern).ok()
    }

    /// Generate autoloader for installed packages
    pub fn generate(
        &self,
        packages: &[PackageAutoload],
        root_autoload: Option<&Autoload>,
        root_package: Option<&RootPackageInfo>,
    ) -> Result<()> {
        self.generate_with_result(packages, root_autoload, root_package)
            .map(|_| ())
    }

    /// Generate an autoloader and return the observable generation summary.
    pub fn generate_with_result(
        &self,
        packages: &[PackageAutoload],
        root_autoload: Option<&Autoload>,
        root_package: Option<&RootPackageInfo>,
    ) -> Result<AutoloadGenerationResult> {
        if let Some(handler) = &self.event_handler {
            handler(AutoloadGenerationEvent::PreGenerate)?;
        }
        let composer_dir = self.config.vendor_dir.join("composer");
        std::fs::create_dir_all(&composer_dir)?;

        let suffix = self.get_suffix();

        let selected_packages = if self.options.root_requires.is_empty() {
            packages.to_vec()
        } else {
            select_reachable_packages(packages, &self.options.root_requires)
        };
        // Sort packages by dependency weight for reproducible output
        let sorted_packages = sort_packages_by_dependency(&selected_packages);

        // Collect exclude-from-classmap patterns from all packages
        let exclude_patterns = self.collect_exclude_patterns(&sorted_packages, root_autoload);

        // Collect autoload data from all packages
        // Use BTreeMap for sorted output
        let mut psr4: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut psr0: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut classmap: BTreeMap<String, String> = BTreeMap::new();
        // Files are stored as (identifier, path) pairs - order matters!
        let mut files: Vec<(String, String)> = Vec::new();

        // Process package autoloads in sorted order (dependencies first)
        // Skip metapackages as they have no files to autoload
        for pkg in &sorted_packages {
            if pkg.is_metapackage() {
                continue;
            }
            self.process_autoload(
                &pkg.autoload,
                &pkg.install_path,
                pkg.target_dir.as_deref(),
                &pkg.name,
                &mut psr4,
                &mut psr0,
                &mut classmap,
                &mut files,
                &exclude_patterns,
            )?;
        }

        // Process root autoload last (root overrides)
        if let Some(autoload) = root_autoload {
            self.process_autoload(
                autoload,
                "",
                self.options.root_target_dir.as_deref(),
                "__root__",
                &mut psr4,
                &mut psr0,
                &mut classmap,
                &mut files,
                &exclude_patterns,
            )?;
        }

        let mut seen_files = HashSet::new();
        files.retain(|(_, path)| seen_files.insert(path.clone()));

        // Generate authoritative classmap if optimizing
        if self.config.optimize || self.config.authoritative {
            self.generate_optimized_classmap(&psr4, &psr0, &mut classmap, &exclude_patterns)?;
        }

        // Add Composer\InstalledVersions to classmap
        classmap.insert(
            "Composer\\InstalledVersions".to_string(),
            "$vendorDir . '/composer/InstalledVersions.php'".to_string(),
        );

        // Generate files
        let include_paths = self.collect_include_paths(&sorted_packages);
        let has_platform_check = self.generate_platform_check(&composer_dir)?;
        self.generate_autoload_php(&composer_dir, &suffix)?;
        self.generate_autoload_real(
            &composer_dir,
            &suffix,
            !files.is_empty(),
            !include_paths.is_empty(),
            has_platform_check,
        )?;
        self.generate_autoload_static(&composer_dir, &suffix, &psr4, &psr0, &classmap, &files)?;
        self.generate_autoload_psr4(&composer_dir, &psr4)?;
        self.generate_autoload_namespaces(&composer_dir, &psr0)?;
        self.generate_autoload_classmap(&composer_dir, &classmap)?;
        if !files.is_empty() {
            self.generate_autoload_files(&composer_dir, &files)?;
        } else {
            remove_generated_file(&composer_dir.join("autoload_files.php"))?;
        }
        if !include_paths.is_empty() {
            self.generate_include_paths(&composer_dir, &include_paths)?;
        } else {
            remove_generated_file(&composer_dir.join("include_paths.php"))?;
        }
        self.generate_class_loader(&composer_dir)?;
        self.generate_installed_metadata(&sorted_packages, root_package)?;

        if let Some(handler) = &self.event_handler {
            handler(AutoloadGenerationEvent::PostGenerate)?;
        }

        Ok(AutoloadGenerationResult {
            class_count: classmap.len(),
        })
    }

    /// Generate package-state metadata independently of the autoloader.
    pub fn generate_installed_metadata(
        &self,
        packages: &[PackageAutoload],
        root_package: Option<&RootPackageInfo>,
    ) -> Result<()> {
        let composer_dir = self.config.vendor_dir.join("composer");
        std::fs::create_dir_all(&composer_dir)?;
        let sorted_packages = sort_packages_by_dependency(packages);

        self.generate_installed_versions(&composer_dir)?;
        self.generate_installed_json(&composer_dir, &sorted_packages, root_package)?;
        self.generate_installed_php(&composer_dir, &sorted_packages, root_package)?;

        Ok(())
    }

    /// Process a package's autoload configuration
    #[allow(clippy::too_many_arguments)]
    fn process_autoload(
        &self,
        autoload: &Autoload,
        install_path: &str,
        target_dir: Option<&str>,
        package_name: &str,
        psr4: &mut BTreeMap<String, Vec<String>>,
        psr0: &mut BTreeMap<String, Vec<String>>,
        classmap: &mut BTreeMap<String, String>,
        files: &mut Vec<(String, String)>,
        exclude_patterns: &[Regex],
    ) -> Result<()> {
        let is_root = install_path.is_empty();

        // PSR-4
        for (namespace, paths) in &autoload.psr4 {
            // Normalize namespace - strip leading backslash
            let ns = namespace.trim_start_matches('\\').to_string();
            let entry = psr4.entry(ns).or_default();
            let paths = paths
                .as_vec()
                .into_iter()
                .map(|path| self.adjust_target_path(&path, target_dir, is_root))
                .map(|path| self.get_path_code(install_path, &path, is_root))
                .collect::<Vec<_>>();
            // Composer traverses dependency-sorted packages in reverse for
            // PSR mappings so the root and dependents take precedence while
            // preserving path order within each package.
            entry.splice(0..0, paths);
        }

        // PSR-0
        for (namespace, paths) in &autoload.psr0 {
            let ns = namespace.trim_start_matches('\\').to_string();
            let entry = psr0.entry(ns).or_default();
            let paths = paths
                .as_vec()
                .into_iter()
                .map(|path| self.adjust_target_path(&path, target_dir, is_root))
                .map(|path| self.get_path_code(install_path, &path, is_root))
                .collect::<Vec<_>>();
            entry.splice(0..0, paths);
        }

        // Classmap
        for path in &autoload.classmap {
            let path = self.adjust_target_path(path, target_dir, is_root);
            let full_path = if is_root {
                self.config.base_dir.join(&path)
            } else {
                self.config.vendor_dir.join(install_path).join(&path)
            };
            let classes = self.scan_classes(&full_path, exclude_patterns)?;
            for (class_name, file_path) in classes {
                let path_code = self.path_to_code(&file_path);
                classmap.insert(class_name, path_code);
            }
        }

        // Files - compute identifier as md5(package_name:path)
        for path in &autoload.files {
            let file_identifier = Self::compute_file_identifier(package_name, path);
            let path = self.adjust_target_path(path, target_dir, is_root);
            let full_path = self.get_path_code(install_path, &path, is_root);
            files.push((file_identifier, full_path));
        }

        Ok(())
    }

    /// Convert a path to PHP code reference ($vendorDir or $baseDir)
    /// This format is used for autoload_psr4.php, autoload_namespaces.php, etc.
    fn get_path_code(&self, install_path: &str, path: &str, is_root: bool) -> String {
        if Self::is_absolute_install_path(path) {
            return self.path_to_code(Path::new(path));
        }
        let normalized = normalize_relative_path(path);
        let path = normalized.trim_end_matches('/');
        if is_root {
            if path.is_empty() || path == "." {
                "$baseDir . '/'".to_string()
            } else {
                format!("$baseDir . '/{}'", path)
            }
        } else {
            let full_path = if path.is_empty() {
                install_path.to_string()
            } else {
                format!("{}/{}", install_path, path)
            };
            format!("$vendorDir . '/{}'", full_path)
        }
    }

    fn adjust_target_path(&self, path: &str, target_dir: Option<&str>, is_root: bool) -> String {
        let path = normalize_relative_path(path);
        let Some(target_dir) = target_dir else {
            return path;
        };
        let target_dir = normalize_relative_path(target_dir)
            .trim_matches('/')
            .to_owned();
        let stripped = path
            .strip_prefix(&format!("{target_dir}/"))
            .or_else(|| (path == target_dir).then_some(""))
            .unwrap_or(&path);
        if is_root {
            stripped.to_owned()
        } else if stripped.is_empty() {
            target_dir
        } else {
            format!("{target_dir}/{stripped}")
        }
    }

    fn collect_include_paths(&self, packages: &[PackageAutoload]) -> Vec<String> {
        let mut paths = self
            .options
            .root_include_paths
            .iter()
            .map(|path| path.trim_start_matches(['/', '\\']))
            .map(|path| {
                self.adjust_target_path(path, self.options.root_target_dir.as_deref(), true)
            })
            .map(|path| self.get_path_code("", &path, true))
            .collect::<Vec<_>>();
        for package in packages {
            paths.extend(package.include_paths.iter().map(|path| {
                let path = self.adjust_target_path(
                    path.trim_start_matches(['/', '\\']),
                    package.target_dir.as_deref(),
                    false,
                );
                self.get_path_code(&package.install_path, &path, false)
            }));
        }
        paths
    }

    /// Convert an absolute PathBuf to PHP code reference
    fn path_to_code(&self, path: &Path) -> String {
        let path_str = path.to_string_lossy();

        // Check if path is under vendor dir
        let vendor_path = self
            .config
            .vendor_dir
            .canonicalize()
            .unwrap_or_else(|_| self.config.vendor_dir.clone());
        let base_path = self
            .config
            .base_dir
            .canonicalize()
            .unwrap_or_else(|_| self.config.base_dir.clone());

        if let Ok(canonical) = path.canonicalize() {
            if let Ok(rel) = canonical.strip_prefix(&vendor_path) {
                return format!(
                    "$vendorDir . '/{}'",
                    rel.to_string_lossy().replace('\\', "/")
                );
            }
            if let Ok(rel) = canonical.strip_prefix(&base_path) {
                return format!("$baseDir . '/{}'", rel.to_string_lossy().replace('\\', "/"));
            }
        }

        // Fallback - try without canonicalize
        if let Ok(rel) = path.strip_prefix(&self.config.vendor_dir) {
            return format!(
                "$vendorDir . '/{}'",
                rel.to_string_lossy().replace('\\', "/")
            );
        }
        if let Ok(rel) = path.strip_prefix(&self.config.base_dir) {
            return format!("$baseDir . '/{}'", rel.to_string_lossy().replace('\\', "/"));
        }

        // Last resort - use $baseDir with the path
        format!("$baseDir . '/{}'", path_str.replace('\\', "/"))
    }

    /// Generate optimized classmap from PSR-4/PSR-0 directories
    fn generate_optimized_classmap(
        &self,
        psr4: &BTreeMap<String, Vec<String>>,
        psr0: &BTreeMap<String, Vec<String>>,
        classmap: &mut BTreeMap<String, String>,
        exclude_patterns: &[Regex],
    ) -> Result<()> {
        let mut violations = Vec::new();
        // Scan PSR-4 directories
        for (namespace, paths) in psr4 {
            for path_code in paths {
                // Extract actual path from code like "$vendorDir . '/symfony/console'"
                if let Some(path) = self.extract_path_from_code(path_code) {
                    let classes = self.scan_classes(Path::new(&path), exclude_patterns)?;
                    for (class_name, file_path) in classes {
                        if !class_matches_psr4(namespace, &path, &class_name, &file_path) {
                            if self.options.strict_psr {
                                violations.push(psr_violation(
                                    "psr-4",
                                    namespace,
                                    &path,
                                    &class_name,
                                    &file_path,
                                    &self.config.base_dir,
                                ));
                            }
                            continue;
                        }
                        let code = self.path_to_code(&file_path);
                        classmap.entry(class_name).or_insert(code);
                    }
                }
            }
        }

        // Scan PSR-0 directories
        for (namespace, paths) in psr0 {
            for path_code in paths {
                if let Some(path) = self.extract_path_from_code(path_code) {
                    let classes = self.scan_classes(Path::new(&path), exclude_patterns)?;
                    for (class_name, file_path) in classes {
                        if !class_matches_psr0(namespace, &path, &class_name, &file_path) {
                            if self.options.strict_psr {
                                violations.push(psr_violation(
                                    "psr-0",
                                    namespace,
                                    &path,
                                    &class_name,
                                    &file_path,
                                    &self.config.base_dir,
                                ));
                            }
                            continue;
                        }
                        let code = self.path_to_code(&file_path);
                        classmap.entry(class_name).or_insert(code);
                    }
                }
            }
        }

        if !violations.is_empty() {
            return Err(crate::RiffError::InstallationFailed(violations.join("\n")));
        }
        Ok(())
    }

    /// Extract actual filesystem path from PHP code like "$vendorDir . '/path'"
    fn extract_path_from_code(&self, code: &str) -> Option<String> {
        if code.starts_with("$vendorDir") {
            // Extract path after "$vendorDir . '"
            let parts: Vec<&str> = code.splitn(2, "'").collect();
            if parts.len() >= 2 {
                let rel_path = parts[1]
                    .trim_end_matches('\'')
                    .trim_start_matches(['/', '\\']);
                return Some(
                    self.config
                        .vendor_dir
                        .join(rel_path)
                        .to_string_lossy()
                        .to_string(),
                );
            }
        } else if code.starts_with("$baseDir") {
            let parts: Vec<&str> = code.splitn(2, "'").collect();
            if parts.len() >= 2 {
                let rel_path = parts[1]
                    .trim_end_matches('\'')
                    .trim_start_matches(['/', '\\']);
                return Some(
                    self.config
                        .base_dir
                        .join(rel_path)
                        .to_string_lossy()
                        .to_string(),
                );
            }
        }
        None
    }

    fn scan_classes(&self, path: &Path, excludes: &[Regex]) -> Result<HashMap<String, PathBuf>> {
        self.precomputed_classmaps
            .get(path)
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| {
                self.classmap_generator
                    .generate_with_excludes(path, excludes)
            })
    }

    /// Generate vendor/autoload.php
    fn generate_autoload_php(&self, _composer_dir: &Path, suffix: &str) -> Result<()> {
        let content = format!(
            r#"<?php

// autoload.php @generated by Composer

if (PHP_VERSION_ID < 50600) {{
    if (!headers_sent()) {{
        header('HTTP/1.1 500 Internal Server Error');
    }}
    $err = 'Composer 2.3.0 dropped support for autoloading on PHP <5.6 and you are running '.PHP_VERSION.', please upgrade PHP or use Composer 2.2 LTS via "composer self-update --2.2". Aborting.'.PHP_EOL;
    if (!ini_get('display_errors')) {{
        if (PHP_SAPI === 'cli' || PHP_SAPI === 'phpdbg') {{
            fwrite(STDERR, $err);
        }} elseif (!headers_sent()) {{
            echo $err;
        }}
    }}
    throw new RuntimeException($err);
}}

require_once __DIR__ . '/composer/autoload_real.php';

return ComposerAutoloaderInit{suffix}::getLoader();
"#
        );

        let autoload_path = self.config.vendor_dir.join("autoload.php");
        std::fs::write(autoload_path, content)?;
        Ok(())
    }

    /// Generate vendor/composer/autoload_real.php
    fn generate_autoload_real(
        &self,
        composer_dir: &Path,
        suffix: &str,
        has_files: bool,
        has_include_paths: bool,
        has_platform_check: bool,
    ) -> Result<()> {
        let apcu_prefix = if let Some(prefix) = &self.config.apcu_prefix {
            format!(
                "        $loader->setApcuPrefix({});\n",
                Self::php_string(prefix)
            )
        } else if self.config.apcu {
            format!(
                "        $loader->setApcuPrefix('ComposerAutoloader{}');\n",
                suffix
            )
        } else {
            String::new()
        };

        let authoritative = if self.config.authoritative {
            "        $loader->setClassMapAuthoritative(true);\n".to_string()
        } else {
            String::new()
        };

        let platform_loader = if has_platform_check {
            "        require __DIR__ . '/platform_check.php';\n\n"
        } else {
            ""
        };
        let include_path_loader = if has_include_paths {
            "        $includePaths = require __DIR__ . '/include_paths.php';\n        $includePaths[] = get_include_path();\n        set_include_path(implode(PATH_SEPARATOR, $includePaths));\n\n"
        } else {
            ""
        };
        let global_include_path = if self.options.use_global_include_path {
            "        $loader->setUseIncludePath(true);\n"
        } else {
            ""
        };

        let files_loader = if has_files {
            format!(
                r#"
        $filesToLoad = \Composer\Autoload\ComposerStaticInit{suffix}::$files;
        $requireFile = \Closure::bind(static function ($fileIdentifier, $file) {{
            if (empty($GLOBALS['__composer_autoload_files'][$fileIdentifier])) {{
                $GLOBALS['__composer_autoload_files'][$fileIdentifier] = true;

                require $file;
            }}
        }}, null, null);
        foreach ($filesToLoad as $fileIdentifier => $file) {{
            $requireFile($fileIdentifier, $file);
        }}
"#
            )
        } else {
            String::new()
        };

        let content = format!(
            r#"<?php

// autoload_real.php @generated by Composer

class ComposerAutoloaderInit{suffix}
{{
    private static $loader;

    public static function loadClassLoader($class)
    {{
        if ('Composer\Autoload\ClassLoader' === $class) {{
            require __DIR__ . '/ClassLoader.php';
        }}
    }}

    /**
     * @return \Composer\Autoload\ClassLoader
     */
    public static function getLoader()
    {{
        if (null !== self::$loader) {{
            return self::$loader;
        }}

{platform_loader}        spl_autoload_register(array('ComposerAutoloaderInit{suffix}', 'loadClassLoader'), true, true);
        self::$loader = $loader = new \Composer\Autoload\ClassLoader(\dirname(__DIR__));
        spl_autoload_unregister(array('ComposerAutoloaderInit{suffix}', 'loadClassLoader'));

{include_path_loader}        require __DIR__ . '/autoload_static.php';
        call_user_func(\Composer\Autoload\ComposerStaticInit{suffix}::getInitializer($loader));

{global_include_path}        $loader->register(true);
{apcu_prefix}{authoritative}{files_loader}
        return $loader;
    }}
}}
"#
        );

        std::fs::write(composer_dir.join("autoload_real.php"), content)?;
        Ok(())
    }

    /// Convert $vendorDir/$baseDir paths to __DIR__ format for static file
    fn to_static_path(&self, path: &str) -> String {
        if path.starts_with("$vendorDir") {
            // $vendorDir . '/x' => __DIR__ . '/..' . '/x'
            path.replace("$vendorDir", "__DIR__ . '/..'")
        } else if path.starts_with("$baseDir") {
            path.replacen("$baseDir", &self.base_dir_static_expression(), 1)
        } else {
            path.to_string()
        }
    }

    /// Generate vendor/composer/autoload_static.php
    fn generate_autoload_static(
        &self,
        composer_dir: &Path,
        suffix: &str,
        psr4: &BTreeMap<String, Vec<String>>,
        psr0: &BTreeMap<String, Vec<String>>,
        classmap: &BTreeMap<String, String>,
        files: &[(String, String)],
    ) -> Result<()> {
        let mut content = format!(
            r#"<?php

// autoload_static.php @generated by Composer

namespace Composer\Autoload;

class ComposerStaticInit{suffix}
{{
"#
        );

        // Generate files array if present
        if !files.is_empty() {
            content.push_str("    public static $files = array (\n");
            for (identifier, path) in files {
                content.push_str(&format!(
                    "        '{}' => {},\n",
                    identifier,
                    self.to_static_path(path)
                ));
            }
            content.push_str("    );\n\n");
        }

        // Generate PSR-4 prefix lengths grouped by first character
        // Sorted in descending order by namespace (krsort equivalent)
        let mut psr4_vec: Vec<_> = psr4.iter().collect();
        psr4_vec.sort_by(|a, b| b.0.cmp(a.0)); // Reverse sort

        if !psr4.is_empty() {
            // Group by first character
            let mut by_first_char: BTreeMap<char, Vec<(&String, usize)>> = BTreeMap::new();
            for (namespace, _) in &psr4_vec {
                let first_char = namespace.chars().next().unwrap_or('_');
                by_first_char
                    .entry(first_char)
                    .or_default()
                    .push((namespace, namespace.len()));
            }

            content.push_str("    public static $prefixLengthsPsr4 = array (\n");
            // Sort by first char descending
            let mut char_entries: Vec<_> = by_first_char.iter().collect();
            char_entries.sort_by(|a, b| b.0.cmp(a.0));

            for (first_char, namespaces) in char_entries {
                content.push_str(&format!("        '{}' =>\n        array (\n", first_char));
                for (ns, len) in namespaces {
                    let ns_escaped = ns.replace('\\', "\\\\");
                    content.push_str(&format!("            '{}' => {},\n", ns_escaped, len));
                }
                content.push_str("        ),\n");
            }
            content.push_str("    );\n\n");

            // Generate PSR-4 prefix directories
            content.push_str("    public static $prefixDirsPsr4 = array (\n");
            for (namespace, paths) in &psr4_vec {
                let ns_escaped = namespace.replace('\\', "\\\\");
                content.push_str(&format!("        '{}' =>\n        array (\n", ns_escaped));
                for (i, path) in paths.iter().enumerate() {
                    content.push_str(&format!(
                        "            {} => {},\n",
                        i,
                        self.to_static_path(path)
                    ));
                }
                content.push_str("        ),\n");
            }
            content.push_str("    );\n\n");
        }

        // Generate PSR-0 prefixes if present
        if !psr0.is_empty() {
            let mut psr0_vec: Vec<_> = psr0.iter().collect();
            psr0_vec.sort_by(|a, b| b.0.cmp(a.0));

            // Group by first character
            let mut by_first_char: BTreeMap<char, Vec<(&String, &Vec<String>)>> = BTreeMap::new();
            for (namespace, paths) in &psr0_vec {
                let first_char = namespace.chars().next().unwrap_or('_');
                by_first_char
                    .entry(first_char)
                    .or_default()
                    .push((namespace, paths));
            }

            content.push_str("    public static $prefixesPsr0 = array (\n");
            let mut char_entries: Vec<_> = by_first_char.iter().collect();
            char_entries.sort_by(|a, b| b.0.cmp(a.0));

            for (first_char, namespaces) in char_entries {
                content.push_str(&format!("        '{}' =>\n        array (\n", first_char));
                for (ns, paths) in namespaces {
                    let ns_escaped = ns.replace('\\', "\\\\");
                    content.push_str(&format!(
                        "            '{}' =>\n            array (\n",
                        ns_escaped
                    ));
                    for (i, path) in paths.iter().enumerate() {
                        content.push_str(&format!(
                            "                {} => {},\n",
                            i,
                            self.to_static_path(path)
                        ));
                    }
                    content.push_str("            ),\n");
                }
                content.push_str("        ),\n");
            }
            content.push_str("    );\n\n");
        }

        // Generate classmap
        content.push_str("    public static $classMap = array (\n");
        for (class, path) in classmap {
            let class_escaped = class.replace('\\', "\\\\");
            content.push_str(&format!(
                "        '{}' => {},\n",
                class_escaped,
                self.to_static_path(path)
            ));
        }
        content.push_str("    );\n\n");

        // Generate initializer
        let mut initializer_content = String::new();
        if !psr4.is_empty() {
            initializer_content.push_str(&format!(
                "            $loader->prefixLengthsPsr4 = ComposerStaticInit{}::$prefixLengthsPsr4;\n",
                suffix
            ));
            initializer_content.push_str(&format!(
                "            $loader->prefixDirsPsr4 = ComposerStaticInit{}::$prefixDirsPsr4;\n",
                suffix
            ));
        }
        if !psr0.is_empty() {
            initializer_content.push_str(&format!(
                "            $loader->prefixesPsr0 = ComposerStaticInit{}::$prefixesPsr0;\n",
                suffix
            ));
        }
        initializer_content.push_str(&format!(
            "            $loader->classMap = ComposerStaticInit{}::$classMap;\n",
            suffix
        ));

        content.push_str(&format!(
            r#"    public static function getInitializer(ClassLoader $loader)
    {{
        return \Closure::bind(function () use ($loader) {{
{}
        }}, null, ClassLoader::class);
    }}
}}
"#,
            initializer_content
        ));

        std::fs::write(
            composer_dir.join("autoload_static.php"),
            php_source_bytes(&content),
        )?;
        Ok(())
    }

    /// Generate vendor/composer/autoload_psr4.php
    fn generate_autoload_psr4(
        &self,
        composer_dir: &Path,
        psr4: &BTreeMap<String, Vec<String>>,
    ) -> Result<()> {
        // Sort in descending order like Composer does (krsort)
        let mut psr4_vec: Vec<_> = psr4.iter().collect();
        psr4_vec.sort_by(|a, b| b.0.cmp(a.0));

        let mut entries = Vec::new();
        for (namespace, paths) in psr4_vec {
            let ns_escaped = namespace.replace('\\', "\\\\");
            let paths_str = paths.to_vec();

            entries.push(format!(
                "    '{}' => array({})",
                ns_escaped,
                paths_str.join(", ")
            ));
        }

        let base_dir = self.base_dir_php_expression();
        let content = format!(
            r#"<?php

// autoload_psr4.php @generated by Composer

$vendorDir = dirname(__DIR__);
$baseDir = {base_dir};

return array(
{},
);
"#,
            entries.join(",\n")
        );

        std::fs::write(composer_dir.join("autoload_psr4.php"), content)?;
        Ok(())
    }

    /// Generate vendor/composer/autoload_namespaces.php (PSR-0)
    fn generate_autoload_namespaces(
        &self,
        composer_dir: &Path,
        psr0: &BTreeMap<String, Vec<String>>,
    ) -> Result<()> {
        let mut psr0_vec: Vec<_> = psr0.iter().collect();
        psr0_vec.sort_by(|a, b| b.0.cmp(a.0));

        let mut entries = Vec::new();
        for (namespace, paths) in psr0_vec {
            let ns_escaped = namespace.replace('\\', "\\\\");
            let paths_str = paths.to_vec();

            entries.push(format!(
                "    '{}' => array({})",
                ns_escaped,
                paths_str.join(", ")
            ));
        }

        let entries_str = if entries.is_empty() {
            String::new()
        } else {
            format!("{},\n", entries.join(",\n"))
        };

        let base_dir = self.base_dir_php_expression();
        let content = format!(
            r#"<?php

// autoload_namespaces.php @generated by Composer

$vendorDir = dirname(__DIR__);
$baseDir = {base_dir};

return array(
{});
"#,
            entries_str
        );

        std::fs::write(composer_dir.join("autoload_namespaces.php"), content)?;
        Ok(())
    }

    /// Generate vendor/composer/autoload_classmap.php
    fn generate_autoload_classmap(
        &self,
        composer_dir: &Path,
        classmap: &BTreeMap<String, String>,
    ) -> Result<()> {
        let entries: Vec<String> = classmap
            .iter()
            .map(|(class, path)| format!("    '{}' => {}", class.replace('\\', "\\\\"), path))
            .collect();

        let entries_str = if entries.is_empty() {
            String::new()
        } else {
            format!("{},\n", entries.join(",\n"))
        };

        let base_dir = self.base_dir_php_expression();
        let content = format!(
            r#"<?php

// autoload_classmap.php @generated by Composer

$vendorDir = dirname(__DIR__);
$baseDir = {base_dir};

return array(
{});
"#,
            entries_str
        );

        std::fs::write(
            composer_dir.join("autoload_classmap.php"),
            php_source_bytes(&content),
        )?;
        Ok(())
    }

    /// Generate vendor/composer/autoload_files.php
    fn generate_autoload_files(
        &self,
        composer_dir: &Path,
        files: &[(String, String)],
    ) -> Result<()> {
        let entries: Vec<String> = files
            .iter()
            .map(|(identifier, path)| format!("    '{}' => {}", identifier, path))
            .collect();

        let entries_str = if entries.is_empty() {
            String::new()
        } else {
            format!("{},\n", entries.join(",\n"))
        };

        let base_dir = self.base_dir_php_expression();
        let content = format!(
            r#"<?php

// autoload_files.php @generated by Composer

$vendorDir = dirname(__DIR__);
$baseDir = {base_dir};

return array(
{});
"#,
            entries_str
        );

        std::fs::write(composer_dir.join("autoload_files.php"), content)?;
        Ok(())
    }

    fn generate_include_paths(&self, composer_dir: &Path, paths: &[String]) -> Result<()> {
        let entries = paths
            .iter()
            .map(|path| format!("    {path}"))
            .collect::<Vec<_>>()
            .join(",\n");
        let base_dir = self.base_dir_php_expression();
        let content = format!(
            r#"<?php

// include_paths.php @generated by Composer

$vendorDir = dirname(__DIR__);
$baseDir = {base_dir};

return array(
{entries},
);
"#
        );
        std::fs::write(composer_dir.join("include_paths.php"), content)?;
        Ok(())
    }

    fn base_dir_php_expression(&self) -> String {
        let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let vendor = absolute_lexical_path(&self.config.vendor_dir, &current_dir);
        let base = absolute_lexical_path(&self.config.base_dir, &current_dir);
        let relative = pathdiff::diff_paths(base, vendor).unwrap_or_else(|| PathBuf::from(".."));
        php_relative_expression("$vendorDir", &relative)
    }

    fn base_dir_static_expression(&self) -> String {
        let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let vendor = absolute_lexical_path(&self.config.vendor_dir, &current_dir);
        let base = absolute_lexical_path(&self.config.base_dir, &current_dir);
        let relative = pathdiff::diff_paths(base, vendor).unwrap_or_else(|| PathBuf::from(".."));
        let from_composer =
            normalize_relative_path(&PathBuf::from("..").join(relative).to_string_lossy());
        format!("__DIR__ . '/{from_composer}'")
    }

    /// Generate vendor/composer/platform_check.php
    fn generate_platform_check(&self, composer_dir: &Path) -> Result<bool> {
        let Some(platform) = &self.options.platform else {
            remove_generated_file(&composer_dir.join("platform_check.php"))?;
            return Ok(false);
        };

        let parser = VersionParser::new();
        let mut php_check = None;
        let mut require_64bit = false;
        let mut extensions = Vec::new();
        for (name, constraint) in &platform.requires {
            if platform_requirement_ignored(name, &platform.ignored)
                || platform_requirement_satisfied(
                    name,
                    constraint,
                    &platform.provides,
                    &platform.replaces,
                    &parser,
                )
            {
                continue;
            }
            if name.eq_ignore_ascii_case("php") || name.eq_ignore_ascii_case("php-64bit") {
                if let Ok(parsed) = parser.parse_constraints(constraint) {
                    let lower = parsed.lower_bound();
                    if !lower.is_zero() {
                        php_check = php_version_check(lower.version(), lower.is_inclusive());
                    }
                }
                require_64bit |= name.eq_ignore_ascii_case("php-64bit");
            } else if let Some(extension) = name
                .get(..4)
                .filter(|prefix| prefix.eq_ignore_ascii_case("ext-"))
                .map(|_| name[4..].to_ascii_lowercase())
            {
                extensions.push(extension);
            }
        }
        extensions.sort();
        extensions.dedup();
        if php_check.is_none() && !require_64bit && extensions.is_empty() {
            remove_generated_file(&composer_dir.join("platform_check.php"))?;
            return Ok(false);
        }

        let mut checks = String::new();
        if let Some((condition, requirement)) = php_check {
            checks.push_str(&format!(
                "if (!({condition})) {{\n    $issues[] = 'Your Composer dependencies require a PHP version \"{requirement}\". You are running ' . PHP_VERSION . '.';\n}}\n\n"
            ));
        }
        if require_64bit {
            checks.push_str("if (PHP_INT_SIZE !== 8) {\n    $issues[] = 'Your Composer dependencies require a 64-bit build of PHP.';\n}\n\n");
        }
        if !extensions.is_empty() {
            checks.push_str("$missingExtensions = array();\n\n");
            for extension in extensions {
                checks.push_str(&format!(
                    "extension_loaded({}) || $missingExtensions[] = {};\n",
                    Self::php_string(&extension),
                    Self::php_string(&extension)
                ));
            }
            checks.push_str("\nif ($missingExtensions) {\n    $issues[] = 'Your Composer dependencies require the following PHP extensions to be installed: ' . implode(', ', $missingExtensions) . '.';\n}\n\n");
        }

        let content = r#"<?php

// platform_check.php @generated by Composer

$issues = array();

__RIFF_PLATFORM_CHECKS__if ($issues) {
    if (!headers_sent()) {
        header('HTTP/1.1 500 Internal Server Error');
    }
    if (!ini_get('display_errors')) {
        if (PHP_SAPI === 'cli' || PHP_SAPI === 'phpdbg') {
            fwrite(STDERR, 'Composer detected issues in your platform:' . PHP_EOL.PHP_EOL . implode(PHP_EOL, $issues) . PHP_EOL.PHP_EOL);
        } elseif (!headers_sent()) {
            echo 'Composer detected issues in your platform:' . PHP_EOL.PHP_EOL . str_replace('You are running '.PHP_VERSION.'.', '', implode(PHP_EOL, $issues)) . PHP_EOL.PHP_EOL;
        }
    }
    throw new \RuntimeException(
        'Composer detected issues in your platform: ' . implode(' ', $issues)
    );
}
"#
        .replace("__RIFF_PLATFORM_CHECKS__", &checks);

        std::fs::write(composer_dir.join("platform_check.php"), content)?;
        Ok(true)
    }

    /// Generate vendor/composer/InstalledVersions.php
    fn generate_installed_versions(&self, composer_dir: &Path) -> Result<()> {
        // Copy the InstalledVersions.php template
        let content = include_str!("InstalledVersions.php.template");
        std::fs::write(composer_dir.join("InstalledVersions.php"), content)?;
        Ok(())
    }

    /// Generate vendor/composer/installed.json for Composer-compatible tooling.
    fn generate_installed_json(
        &self,
        composer_dir: &Path,
        packages: &[PackageAutoload],
        root_package: Option<&RootPackageInfo>,
    ) -> Result<()> {
        let parser = VersionParser::new();
        let mut installed_packages = Vec::new();
        let mut dev_package_names = Vec::new();

        for package in packages {
            let mut value = package
                .locked_package
                .as_ref()
                .map(serde_json::to_value)
                .transpose()?
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "name": package.name,
                        "version": package.pretty_version.as_deref().unwrap_or("dev-main"),
                        "type": package.package_type,
                        "replace": package.replaces,
                        "provide": package.provides,
                    })
                });
            let object = value
                .as_object_mut()
                .expect("serialized locked package must be an object");
            let pretty_version = package
                .pretty_version
                .as_deref()
                .or(package.version.as_deref())
                .unwrap_or("dev-main");
            let normalized_version = package
                .pretty_version
                .as_deref()
                .and_then(|version| parser.normalize(version).ok())
                .or_else(|| package.version.clone())
                .unwrap_or_else(|| pretty_version.to_string());
            object.insert(
                "version_normalized".to_string(),
                serde_json::Value::String(normalized_version),
            );
            if let Some(installation_source) = &package.installation_source {
                object.insert(
                    "installation-source".to_string(),
                    serde_json::Value::String(installation_source.clone()),
                );
            }
            object.insert(
                "install-path".to_string(),
                if package.is_metapackage() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(Self::installed_json_path(
                        composer_dir,
                        &package.install_path,
                    ))
                },
            );
            reorder_installed_package_fields(object);

            if package.dev_requirement {
                dev_package_names.push(package.name.clone());
            }
            installed_packages.push(value);
        }

        installed_packages.sort_by(|left, right| {
            let left = left
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let right = right
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            left.cmp(right)
        });
        dev_package_names.sort();

        let dev_mode = root_package
            .map(|root| root.dev_mode)
            .unwrap_or_else(|| !dev_package_names.is_empty());
        let value = serde_json::json!({
            "packages": installed_packages,
            "dev": dev_mode,
            "dev-package-names": dev_package_names,
        });
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
        let mut serializer = serde_json::Serializer::with_formatter(Vec::new(), formatter);
        value.serialize(&mut serializer)?;
        let mut content = serializer.into_inner();
        content.push(b'\n');
        std::fs::write(composer_dir.join("installed.json"), content)?;
        Ok(())
    }

    /// Generate vendor/composer/ClassLoader.php
    fn generate_class_loader(&self, composer_dir: &Path) -> Result<()> {
        let content = include_str!("ClassLoader.php.template");
        std::fs::write(composer_dir.join("ClassLoader.php"), content)?;
        std::fs::write(
            composer_dir.join("LICENSE"),
            include_str!("Composer.LICENSE"),
        )?;
        Ok(())
    }

    /// Compute MD5 hash for file identifier (package_name:path)
    /// This matches Composer's behavior
    fn compute_file_identifier(package_name: &str, path: &str) -> String {
        let mut hasher = Md5::new();
        hasher.update(format!("{}:{}", package_name, path).as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Generate vendor/composer/installed.php
    fn generate_installed_php(
        &self,
        composer_dir: &Path,
        packages: &[PackageAutoload],
        root_package: Option<&RootPackageInfo>,
    ) -> Result<()> {
        let mut versions: BTreeMap<String, PackageVersionEntry> = BTreeMap::new();

        // Add all installed packages to versions
        for pkg in packages {
            // Metapackages have no install path (they have no files)
            let install_path = if pkg.is_metapackage() {
                None
            } else {
                Some(Self::installed_php_path(&pkg.install_path))
            };

            let entry = PackageVersionEntry {
                pretty_version: pkg.pretty_version.clone(),
                version: pkg.version.clone(),
                reference: pkg.reference.clone(),
                package_type: Some(pkg.package_type.clone()),
                install_path,
                real_package: true,
                aliases: pkg.aliases.clone(),
                dev_requirement: pkg.dev_requirement,
                replaced: Vec::new(),
                provided: Vec::new(),
            };
            versions.insert(pkg.name.clone(), entry);
        }

        // Process replaced and provided packages
        for pkg in packages {
            let is_dev = pkg.dev_requirement;

            // Handle replaced packages
            for (replaced_name, version_constraint) in &pkg.replaces {
                // Skip platform packages
                if Self::is_platform_package(replaced_name) {
                    continue;
                }

                let replaced_version = if version_constraint == "self.version" {
                    pkg.pretty_version.clone().unwrap_or_default()
                } else {
                    version_constraint.clone()
                };

                if let Some(entry) = versions.get_mut(replaced_name) {
                    // Package exists, add to its replaced list
                    if !entry.replaced.contains(&replaced_version) {
                        entry.replaced.push(replaced_version);
                    }
                    // Only mark as non-dev if this package is non-dev
                    if !is_dev {
                        entry.dev_requirement = false;
                    }
                } else {
                    // Virtual package - create entry with just replaced info
                    versions.insert(
                        replaced_name.clone(),
                        PackageVersionEntry {
                            pretty_version: None,
                            version: None,
                            reference: None,
                            package_type: None,
                            install_path: None,
                            real_package: false,
                            aliases: Vec::new(),
                            dev_requirement: is_dev,
                            replaced: vec![replaced_version],
                            provided: Vec::new(),
                        },
                    );
                }
            }

            // Handle provided packages
            for (provided_name, version_constraint) in &pkg.provides {
                // Skip platform packages
                if Self::is_platform_package(provided_name) {
                    continue;
                }

                let provided_version = if version_constraint == "self.version" {
                    pkg.pretty_version.clone().unwrap_or_default()
                } else {
                    version_constraint.clone()
                };

                if let Some(entry) = versions.get_mut(provided_name) {
                    if !entry.provided.contains(&provided_version) {
                        entry.provided.push(provided_version);
                    }
                    if !is_dev {
                        entry.dev_requirement = false;
                    }
                } else {
                    versions.insert(
                        provided_name.clone(),
                        PackageVersionEntry {
                            pretty_version: None,
                            version: None,
                            reference: None,
                            package_type: None,
                            install_path: None,
                            real_package: false,
                            aliases: Vec::new(),
                            dev_requirement: is_dev,
                            replaced: Vec::new(),
                            provided: vec![provided_version],
                        },
                    );
                }
            }
        }

        if let Some(root) = root_package {
            for (replaced_name, constraint) in &root.replaces {
                if Self::is_platform_package(replaced_name) {
                    continue;
                }
                let constraint = if constraint == "self.version" {
                    root.pretty_version.clone()
                } else {
                    constraint.clone()
                };
                if let Some(entry) = versions.get_mut(replaced_name) {
                    if !entry.replaced.contains(&constraint) {
                        entry.replaced.push(constraint);
                    }
                    entry.dev_requirement = false;
                } else {
                    versions.insert(
                        replaced_name.clone(),
                        PackageVersionEntry {
                            pretty_version: None,
                            version: None,
                            reference: None,
                            package_type: None,
                            install_path: None,
                            real_package: false,
                            aliases: Vec::new(),
                            dev_requirement: false,
                            replaced: vec![constraint],
                            provided: Vec::new(),
                        },
                    );
                }
            }
            for (provided_name, constraint) in &root.provides {
                if Self::is_platform_package(provided_name) {
                    continue;
                }
                let constraint = if constraint == "self.version" {
                    root.pretty_version.clone()
                } else {
                    constraint.clone()
                };
                if let Some(entry) = versions.get_mut(provided_name) {
                    if !entry.provided.contains(&constraint) {
                        entry.provided.push(constraint);
                    }
                    entry.dev_requirement = false;
                } else {
                    versions.insert(
                        provided_name.clone(),
                        PackageVersionEntry {
                            pretty_version: None,
                            version: None,
                            reference: None,
                            package_type: None,
                            install_path: None,
                            real_package: false,
                            aliases: Vec::new(),
                            dev_requirement: false,
                            replaced: Vec::new(),
                            provided: vec![constraint],
                        },
                    );
                }
            }
        }

        // Sort replaced/provided arrays
        for entry in versions.values_mut() {
            entry.replaced.sort();
            entry.provided.sort();
            entry.aliases.sort();
        }

        // Build root package entry
        let (
            root_name,
            root_pretty_version,
            root_version,
            root_reference,
            root_type,
            root_aliases,
            root_dev,
        ) = if let Some(root) = root_package {
            (
                root.name.clone(),
                root.pretty_version.clone(),
                root.version.clone(),
                root.reference.clone(),
                root.package_type.clone(),
                root.aliases.clone(),
                root.dev_mode,
            )
        } else {
            (
                "__root__".to_string(),
                "dev-main".to_string(),
                "dev-main".to_string(),
                None,
                "library".to_string(),
                Vec::new(),
                true,
            )
        };

        // Also add root package to versions (Composer does this)
        versions.insert(
            root_name.clone(),
            PackageVersionEntry {
                pretty_version: Some(root_pretty_version.clone()),
                version: Some(root_version.clone()),
                reference: root_reference.clone(),
                package_type: Some(root_type.clone()),
                install_path: Some("__DIR__ . '/../../'".to_string()),
                real_package: true,
                aliases: root_aliases.clone(),
                dev_requirement: false,
                replaced: Vec::new(),
                provided: Vec::new(),
            },
        );

        // Generate the PHP code
        let mut content = String::from("<?php return array(\n");

        // Root section
        content.push_str("    'root' => array(\n");
        content.push_str(&format!(
            "        'name' => {},\n",
            Self::php_string(&root_name)
        ));
        content.push_str(&format!(
            "        'pretty_version' => {},\n",
            Self::php_string(&root_pretty_version)
        ));
        content.push_str(&format!(
            "        'version' => {},\n",
            Self::php_string(&root_version)
        ));
        content.push_str(&format!(
            "        'reference' => {},\n",
            Self::php_value_or_null(&root_reference)
        ));
        content.push_str(&format!(
            "        'type' => {},\n",
            Self::php_string(&root_type)
        ));
        content.push_str("        'install_path' => __DIR__ . '/../../',\n");
        content.push_str(&format!(
            "        'aliases' => {},\n",
            Self::php_string_array(&root_aliases, 8)
        ));
        content.push_str(&format!(
            "        'dev' => {},\n",
            if root_dev { "true" } else { "false" }
        ));
        content.push_str("    ),\n");

        // Versions section
        content.push_str("    'versions' => array(\n");
        for (name, entry) in &versions {
            content.push_str(&format!("        {} => array(\n", Self::php_string(name)));

            if let Some(ref pv) = entry.pretty_version {
                content.push_str(&format!(
                    "            'pretty_version' => {},\n",
                    Self::php_string(pv)
                ));
            }
            if let Some(ref v) = entry.version {
                content.push_str(&format!(
                    "            'version' => {},\n",
                    Self::php_string(v)
                ));
            }
            if entry.pretty_version.is_some() || entry.version.is_some() {
                content.push_str(&format!(
                    "            'reference' => {},\n",
                    Self::php_value_or_null(&entry.reference)
                ));
            }
            if let Some(ref t) = entry.package_type {
                content.push_str(&format!("            'type' => {},\n", Self::php_string(t)));
            }
            if let Some(ref ip) = entry.install_path {
                content.push_str(&format!("            'install_path' => {},\n", ip));
            } else if entry.real_package {
                content.push_str("            'install_path' => null,\n");
            }
            if !entry.aliases.is_empty() || entry.pretty_version.is_some() {
                content.push_str(&format!(
                    "            'aliases' => {},\n",
                    Self::php_string_array(&entry.aliases, 12)
                ));
            }
            content.push_str(&format!(
                "            'dev_requirement' => {},\n",
                if entry.dev_requirement {
                    "true"
                } else {
                    "false"
                }
            ));
            if !entry.replaced.is_empty() {
                content.push_str(&format!(
                    "            'replaced' => {},\n",
                    Self::php_string_array(&entry.replaced, 12)
                ));
            }
            if !entry.provided.is_empty() {
                content.push_str(&format!(
                    "            'provided' => {},\n",
                    Self::php_string_array(&entry.provided, 12)
                ));
            }
            content.push_str("        ),\n");
        }
        content.push_str("    ),\n");
        content.push_str(");\n");

        std::fs::write(composer_dir.join("installed.php"), content)?;
        Ok(())
    }

    /// Check if a package name is a platform package (php, ext-*, lib-*)
    fn is_platform_package(name: &str) -> bool {
        name == "php"
            || name == "php-64bit"
            || name == "hhvm"
            || name.starts_with("ext-")
            || name.starts_with("lib-")
            || name.starts_with("composer-")
    }

    /// Convert a string to PHP string literal
    fn php_string(s: &str) -> String {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }

    fn installed_php_path(path: &str) -> String {
        if Self::is_absolute_install_path(path) {
            return Self::php_string(path);
        }

        let path = normalize_relative_path(&path.replace('\\', "/"));
        let path = path.trim_start_matches('/');
        let relative = path
            .strip_prefix("composer/")
            .map(|path| format!("/./{path}"))
            .unwrap_or_else(|| format!("/../{path}"));
        format!("__DIR__ . {}", Self::php_string(&relative))
    }

    fn installed_json_path(composer_dir: &Path, path: &str) -> String {
        let is_absolute = Self::is_absolute_install_path(path);
        let path = Path::new(path);
        let install_path = if is_absolute {
            pathdiff::diff_paths(path, composer_dir).unwrap_or_else(|| path.to_path_buf())
        } else {
            let normalized = normalize_relative_path(&path.to_string_lossy().replace('\\', "/"));
            if let Some(path) = normalized.strip_prefix("composer/") {
                PathBuf::from(".").join(path)
            } else {
                PathBuf::from("..").join(normalized)
            }
        };
        let install_path = install_path.to_string_lossy().replace('\\', "/");
        if !is_absolute && !install_path.starts_with('.') {
            format!("./{install_path}")
        } else {
            install_path
        }
    }

    fn is_absolute_install_path(path: &str) -> bool {
        Path::new(path).is_absolute()
            || path.starts_with('/')
            || path
                .as_bytes()
                .get(1)
                .is_some_and(|separator| *separator == b':')
            || path.starts_with("\\\\")
    }

    /// Convert an Option<String> to PHP value or null
    fn php_value_or_null(opt: &Option<String>) -> String {
        match opt {
            Some(s) => Self::php_string(s),
            None => "NULL".to_string(),
        }
    }

    /// Convert a Vec<String> to PHP array
    fn php_string_array(arr: &[String], closing_indent: usize) -> String {
        if arr.is_empty() {
            "array()".to_string()
        } else {
            let item_indent = " ".repeat(closing_indent + 4);
            let closing_indent = " ".repeat(closing_indent);
            let items = arr
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    format!("{item_indent}{index} => {},", Self::php_string(value))
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("array(\n{items}\n{closing_indent})")
        }
    }
}

fn php_source_bytes(source: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(source.len());
    for character in source.chars() {
        let codepoint = u32::from(character);
        if (0xE000..=0xE0FF).contains(&codepoint) {
            bytes.push((codepoint - 0xE000) as u8);
        } else {
            let mut encoded = [0; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        }
    }
    bytes
}

fn reorder_installed_package_fields(object: &mut serde_json::Map<String, serde_json::Value>) {
    const COMPOSER_FIELD_ORDER: &[&str] = &[
        "name",
        "version",
        "version_normalized",
        "target-dir",
        "source",
        "dist",
        "require",
        "conflict",
        "replace",
        "provide",
        "require-dev",
        "suggest",
        "time",
        "default-branch",
        "bin",
        "type",
        "extra",
        "installation-source",
        "autoload",
        "autoload-dev",
        "notification-url",
        "include-path",
        "php-ext",
        "archive",
        "scripts",
        "license",
        "authors",
        "description",
        "homepage",
        "keywords",
        "repositories",
        "support",
        "funding",
        "abandoned",
        "transport-options",
        "install-path",
    ];
    for field in ["autoload", "autoload-dev"] {
        let Some(autoload) = object
            .get_mut(field)
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        let mut original = std::mem::take(autoload);
        for key in [
            "files",
            "psr-4",
            "psr-0",
            "classmap",
            "exclude-from-classmap",
        ] {
            if let Some(value) = original.remove(key) {
                autoload.insert(key.to_owned(), value);
            }
        }
        autoload.extend(original);
    }

    let mut original = std::mem::take(object);
    for key in COMPOSER_FIELD_ORDER {
        if let Some(value) = original.remove(*key) {
            object.insert((*key).to_owned(), value);
        }
    }
    object.extend(original);
}

fn select_reachable_packages(
    packages: &[PackageAutoload],
    root_requires: &[String],
) -> Vec<PackageAutoload> {
    let mut selected = HashSet::new();
    let mut pending = root_requires.to_vec();
    while let Some(requirement) = pending.pop() {
        let package = packages
            .iter()
            .find(|package| {
                package
                    .replaces
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case(&requirement))
            })
            .or_else(|| {
                packages
                    .iter()
                    .find(|package| package.name.eq_ignore_ascii_case(&requirement))
            });
        let Some(package) = package else {
            continue;
        };
        if selected.insert(package.name.clone()) {
            pending.extend(package.requires.iter().cloned());
        }
    }
    packages
        .iter()
        .filter(|package| selected.contains(&package.name))
        .cloned()
        .collect()
}

fn remove_generated_file(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn normalize_relative_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|part| *part != "..") => {
                parts.pop();
            }
            ".." if !absolute => parts.push(part),
            ".." => {}
            _ => parts.push(part),
        }
    }
    let normalized = parts.join("/");
    if absolute {
        format!("/{normalized}")
    } else {
        normalized
    }
}

fn absolute_lexical_path(path: &Path, current_dir: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    };
    PathBuf::from(normalize_relative_path(&path.to_string_lossy()))
}

fn php_relative_expression(anchor: &str, relative: &Path) -> String {
    let mut parents = 0;
    let mut tail = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::ParentDir if tail.is_empty() => parents += 1,
            std::path::Component::Normal(value) => {
                tail.push(value.to_string_lossy().replace('\'', "\\'"));
            }
            _ => {}
        }
    }
    let mut expression = match parents {
        0 => anchor.to_owned(),
        1 => format!("dirname({anchor})"),
        count => format!("dirname({anchor}, {count})"),
    };
    if !tail.is_empty() {
        expression.push_str(&format!(" . '/{}'", tail.join("/")));
    }
    expression
}

fn platform_requirement_ignored(name: &str, ignored: &[String]) -> bool {
    ignored.iter().any(|pattern| {
        let pattern = pattern.to_ascii_lowercase();
        let name = name.to_ascii_lowercase();
        if let Some(prefix) = pattern.strip_suffix('*') {
            name.starts_with(prefix)
        } else {
            name == pattern
        }
    })
}

fn platform_requirement_satisfied(
    name: &str,
    constraint: &str,
    provides: &IndexMap<String, String>,
    replaces: &IndexMap<String, String>,
    parser: &VersionParser,
) -> bool {
    provides.iter().chain(replaces).any(|(provided, version)| {
        if !provided.eq_ignore_ascii_case(name) {
            return false;
        }
        match (
            parser.parse_constraints(constraint),
            parser.parse_constraints(version),
        ) {
            (Ok(required), Ok(provided)) => required.matches(provided.as_ref()),
            _ => false,
        }
    })
}

fn php_version_check(version: &str, inclusive: bool) -> Option<(String, String)> {
    let numbers = version
        .split(['.', '-'])
        .take(3)
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    let major = *numbers.first()?;
    let minor = numbers.get(1).copied().unwrap_or_default();
    let patch = numbers.get(2).copied().unwrap_or_default();
    let version_id = major * 10_000 + minor * 100 + patch;
    let operator = if inclusive { ">=" } else { ">" };
    Some((
        format!("PHP_VERSION_ID {operator} {version_id}"),
        format!("{operator} {major}.{minor}.{patch}"),
    ))
}

fn class_relative_php_path(scan_root: &str, file: &Path) -> Option<String> {
    let root = Path::new(scan_root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(scan_root));
    let file = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    Some(
        file.strip_prefix(root)
            .ok()?
            .to_string_lossy()
            .replace('\\', "/"),
    )
}

fn class_matches_psr4(namespace: &str, scan_root: &str, class: &str, file: &Path) -> bool {
    let Some(relative) = class_relative_php_path(scan_root, file) else {
        return false;
    };
    let Some(class_suffix) = class.strip_prefix(namespace) else {
        return false;
    };
    format!("{}.php", class_suffix.replace('\\', "/")) == relative
}

fn class_matches_psr0(namespace: &str, scan_root: &str, class: &str, file: &Path) -> bool {
    if !class.starts_with(namespace) {
        return false;
    }
    let Some(relative) = class_relative_php_path(scan_root, file) else {
        return false;
    };
    format!("{}.php", class.replace(['\\', '_'], "/")) == relative
}

fn psr_violation(
    standard: &str,
    namespace: &str,
    scan_root: &str,
    class: &str,
    file: &Path,
    base_dir: &Path,
) -> String {
    let scan_root = Path::new(scan_root);
    let rule_path = scan_root
        .strip_prefix(base_dir)
        .map(|path| format!("./{}", path.to_string_lossy().replace('\\', "/")))
        .unwrap_or_else(|_| scan_root.to_string_lossy().to_string());
    format!(
        "Class {class} located in {} does not comply with {standard} autoloading standard (rule: {namespace} => {rule_path}). Skipping.",
        file.display()
    )
}

/// Internal structure for building installed.php version entries
#[derive(Debug, Clone)]
struct PackageVersionEntry {
    pretty_version: Option<String>,
    version: Option<String>,
    reference: Option<String>,
    package_type: Option<String>,
    install_path: Option<String>,
    real_package: bool,
    aliases: Vec<String>,
    dev_requirement: bool,
    replaced: Vec<String>,
    provided: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn generator_for(temp_dir: &TempDir) -> AutoloadGenerator {
        AutoloadGenerator::new(AutoloadConfig {
            vendor_dir: temp_dir.path().join("vendor"),
            base_dir: temp_dir.path().to_path_buf(),
            suffix: Some("ComposerPort".to_string()),
            ..Default::default()
        })
    }

    fn generated(temp_dir: &TempDir, filename: &str) -> String {
        std::fs::read_to_string(temp_dir.path().join("vendor/composer").join(filename)).unwrap()
    }

    fn package(name: &str, autoload: Autoload) -> PackageAutoload {
        PackageAutoload {
            name: name.to_string(),
            install_path: name.to_string(),
            autoload,
            ..Default::default()
        }
    }

    fn dependency_package(name: &str, requires: &[&str]) -> PackageAutoload {
        PackageAutoload {
            name: name.to_string(),
            requires: requires.iter().map(|name| (*name).to_string()).collect(),
            ..Default::default()
        }
    }

    fn assert_dependency_order(
        packages: Vec<PackageAutoload>,
        expected: &[&str],
        weights: HashMap<String, isize>,
    ) {
        let actual = sort_packages_by_dependency_with_weights(&packages, &weights)
            .into_iter()
            .map(|package| package.name)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    fn write_php(path: impl AsRef<Path>, contents: &str) {
        let path = path.as_ref();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn precomputed_package_scans_preserve_explicit_and_optimized_classmaps() {
        let temp_dir = TempDir::new().unwrap();
        let config = AutoloadConfig {
            vendor_dir: temp_dir.path().join("vendor"),
            base_dir: temp_dir.path().to_path_buf(),
            optimize: true,
            suffix: Some("ComposerPort".to_string()),
            ..Default::default()
        };
        write_php(
            config.vendor_dir.join("vendor/package/src/Example.php"),
            "<?php namespace Example; class Example {}",
        );
        let packages = vec![package(
            "vendor/package",
            Autoload::new()
                .add_classmap("src/")
                .add_psr4("Example\\", "src/"),
        )];

        AutoloadGenerator::new(config.clone())
            .generate(&packages, None, None)
            .unwrap();
        let expected_classmap = generated(&temp_dir, "autoload_classmap.php");
        let expected_static = generated(&temp_dir, "autoload_static.php");

        std::fs::remove_dir_all(config.vendor_dir.join("composer")).unwrap();
        let planner = AutoloadGenerator::new(config.clone());
        let (plans, excludes) = planner.package_classmap_scan_plan(&packages, None);
        let scanner = ClassMapGenerator::new();
        let scans = plans
            .into_values()
            .flatten()
            .map(|path| {
                let classes = scanner.generate_with_excludes(&path, &excludes).unwrap();
                (path, classes)
            })
            .collect();
        AutoloadGenerator::new(config)
            .with_precomputed_classmaps(scans)
            .generate(&packages, None, None)
            .unwrap();

        assert_eq!(
            generated(&temp_dir, "autoload_classmap.php"),
            expected_classmap
        );
        assert_eq!(generated(&temp_dir, "autoload_static.php"), expected_static);
    }

    #[test]
    fn test_autoload_config_default() {
        let config = AutoloadConfig::default();
        assert_eq!(config.vendor_dir, PathBuf::from("vendor"));
        assert!(!config.optimize);
        assert!(!config.apcu);
    }

    // Ported from Composer\Test\Util\PackageSorterTest::
    // testSortingDoesNothingWithNoDependencies.
    #[test]
    fn composer_package_sorter_preserves_natural_order_without_dependencies() {
        let packages = ["foo/bar1", "foo/bar2", "foo/bar3", "foo/bar4"]
            .map(|name| dependency_package(name, &[]))
            .to_vec();

        assert_dependency_order(
            packages,
            &["foo/bar1", "foo/bar2", "foo/bar3", "foo/bar4"],
            HashMap::new(),
        );
    }

    // Ported from Composer\Test\Util\PackageSorterTest::
    // testSortingOrdersDependenciesHigherThanPackage and its data provider.
    #[test]
    fn composer_package_sorter_orders_transitive_dependencies_and_weighted_packages() {
        assert_dependency_order(
            vec![
                dependency_package("foo/bar1", &["foo/bar4"]),
                dependency_package("foo/bar2", &["foo/bar4"]),
                dependency_package("foo/bar3", &["foo/bar4"]),
                dependency_package("foo/bar4", &[]),
            ],
            &["foo/bar4", "foo/bar1", "foo/bar2", "foo/bar3"],
            HashMap::new(),
        );
        assert_dependency_order(
            vec![
                dependency_package("foo/bar1", &["foo/bar2"]),
                dependency_package("foo/bar2", &["foo/bar4"]),
                dependency_package("foo/bar3", &["foo/bar4"]),
                dependency_package("foo/bar4", &[]),
            ],
            &["foo/bar4", "foo/bar2", "foo/bar1", "foo/bar3"],
            HashMap::new(),
        );
        assert_dependency_order(
            vec![
                dependency_package("foo/bar1", &["foo/bar3"]),
                dependency_package("foo/bar2", &["foo/bar3"]),
                dependency_package("foo/bar3", &["foo/bar4"]),
                dependency_package("foo/bar4", &[]),
                dependency_package("foo/bar5", &["foo/bar3"]),
                dependency_package("foo/bar6", &["foo/bar3"]),
            ],
            &[
                "foo/bar4", "foo/bar3", "foo/bar1", "foo/bar2", "foo/bar5", "foo/bar6",
            ],
            HashMap::new(),
        );
        assert_dependency_order(
            vec![
                dependency_package("foo/bar1", &["foo/bar2"]),
                dependency_package("foo/bar2", &[]),
                dependency_package("foo/bar3", &["foo/bar4"]),
                dependency_package("foo/bar4", &[]),
                dependency_package("foo/bar5", &["foo/bar2"]),
                dependency_package("foo/bar6", &["foo/bar2"]),
            ],
            &[
                "foo/bar2", "foo/bar4", "foo/bar1", "foo/bar3", "foo/bar5", "foo/bar6",
            ],
            HashMap::new(),
        );
        assert_dependency_order(
            vec![
                dependency_package("foo/bar1", &["circular/part1"]),
                dependency_package("foo/bar2", &["circular/part2"]),
                dependency_package("circular/part1", &["circular/part2"]),
                dependency_package("circular/part2", &["circular/part1"]),
            ],
            &["circular/part1", "circular/part2", "foo/bar1", "foo/bar2"],
            HashMap::new(),
        );
        assert_dependency_order(
            vec![
                dependency_package("foo/bar10", &["foo/dep"]),
                dependency_package("foo/bar2", &["foo/dep"]),
                dependency_package("foo/baz", &["foo/dep"]),
                dependency_package("foo/dep", &[]),
            ],
            &["foo/dep", "foo/bar2", "foo/bar10", "foo/baz"],
            HashMap::new(),
        );
        assert_dependency_order(
            vec![
                dependency_package("foo/bar", &["foo/dep"]),
                dependency_package("foo/bar2", &["foo/dep2"]),
                dependency_package("foo/dep", &[]),
                dependency_package("foo/dep2", &[]),
            ],
            &["foo/dep", "foo/bar", "foo/dep2", "foo/bar2"],
            HashMap::from([("foo/bar".to_string(), -1000)]),
        );
    }

    #[test]
    fn test_generate_empty() {
        let temp_dir = TempDir::new().unwrap();
        let config = AutoloadConfig {
            vendor_dir: temp_dir.path().join("vendor"),
            ..Default::default()
        };

        let generator = AutoloadGenerator::new(config);
        let result = generator.generate(&[], None, None);

        assert!(result.is_ok());
        assert!(temp_dir.path().join("vendor/autoload.php").exists());
        assert!(temp_dir
            .path()
            .join("vendor/composer/autoload_real.php")
            .exists());
        assert!(temp_dir
            .path()
            .join("vendor/composer/installed.json")
            .exists());
    }

    // Ported from Composer\\Test\\Autoload\\AutoloadGeneratorTest::testRootPackageAutoloading.
    #[test]
    fn root_package_autoloading_generates_psr_maps_and_classmap() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::create_dir_all(temp_dir.path().join("composersrc")).unwrap();
        std::fs::write(
            temp_dir.path().join("composersrc/ClassMapFoo.php"),
            "<?php class ClassMapFoo {}",
        )
        .unwrap();

        let root_autoload = Autoload::new()
            .add_psr0("Main", "src/")
            .add_psr0("Lala", vec!["src/".to_string(), "lib/".to_string()])
            .add_psr4("Acme\\Fruit\\", "src-fruit/")
            .add_psr4(
                "Acme\\Cake\\",
                vec!["src-cake/".to_string(), "lib-cake/".to_string()],
            )
            .add_classmap("composersrc/");

        generator_for(&temp_dir)
            .generate(&[], Some(&root_autoload), None)
            .unwrap();

        let psr4 = generated(&temp_dir, "autoload_psr4.php");
        assert!(psr4.contains("'Acme\\\\Fruit\\\\' => array($baseDir . '/src-fruit')"));
        assert!(psr4.contains(
            "'Acme\\\\Cake\\\\' => array($baseDir . '/src-cake', $baseDir . '/lib-cake')"
        ));

        let psr0 = generated(&temp_dir, "autoload_namespaces.php");
        assert!(psr0.contains("'Main' => array($baseDir . '/src')"));
        assert!(psr0.contains("'Lala' => array($baseDir . '/src', $baseDir . '/lib')"));

        let classmap = generated(&temp_dir, "autoload_classmap.php");
        assert!(classmap.contains("'ClassMapFoo' => $baseDir . '/composersrc/ClassMapFoo.php'"));
    }

    // Ported from Composer\\Test\\Autoload\\AutoloadGeneratorTest::testRootPackageDevAutoloading.
    #[test]
    fn root_dev_autoloading_is_emitted_when_merged_into_root_autoload() {
        let temp_dir = TempDir::new().unwrap();
        let mut root_autoload = Autoload::new().add_psr0("Main", "src/");
        root_autoload.merge(
            Autoload::new()
                .add_psr0("Main", "tests/")
                .add_file("devfiles/foo.php"),
        );

        generator_for(&temp_dir)
            .generate(&[], Some(&root_autoload), None)
            .unwrap();

        let psr0 = generated(&temp_dir, "autoload_namespaces.php");
        assert!(psr0.contains("'Main' => array($baseDir . '/src', $baseDir . '/tests')"));
        let files = generated(&temp_dir, "autoload_files.php");
        assert!(files.contains("$baseDir . '/devfiles/foo.php'"));
    }

    // Ported from Composer\\Test\\Autoload\\AutoloadGeneratorTest::testRootPackageDevAutoloadingDisabledByDefault.
    #[test]
    fn root_dev_autoloading_is_not_emitted_without_dev_merge() {
        let temp_dir = TempDir::new().unwrap();
        let root_autoload = Autoload::new().add_psr0("Main", "src/");

        generator_for(&temp_dir)
            .generate(&[], Some(&root_autoload), None)
            .unwrap();

        let psr0 = generated(&temp_dir, "autoload_namespaces.php");
        assert!(psr0.contains("'Main' => array($baseDir . '/src')"));
        assert!(!psr0.contains("/tests'"));
        assert!(!temp_dir
            .path()
            .join("vendor/composer/autoload_files.php")
            .exists());
    }

    // Ported from Composer\\Test\\Autoload\\AutoloadGeneratorTest::testFilesAutoloadOrderByDependencies.
    #[test]
    fn dependency_file_autoloads_are_emitted_before_dependents_and_root() {
        let temp_dir = TempDir::new().unwrap();
        let packages = vec![
            PackageAutoload {
                name: "z/foo".to_string(),
                install_path: "z/foo".to_string(),
                autoload: Autoload::new().add_file("testA.php"),
                requires: vec!["c/lorem".to_string()],
                ..Default::default()
            },
            PackageAutoload {
                name: "b/bar".to_string(),
                install_path: "b/bar".to_string(),
                autoload: Autoload::new().add_file("testB.php"),
                requires: vec!["c/lorem".to_string(), "d/d".to_string()],
                ..Default::default()
            },
            PackageAutoload {
                name: "c/lorem".to_string(),
                install_path: "c/lorem".to_string(),
                autoload: Autoload::new().add_file("testC.php"),
                ..Default::default()
            },
            PackageAutoload {
                name: "d/d".to_string(),
                install_path: "d/d".to_string(),
                autoload: Autoload::new().add_file("testD.php"),
                requires: vec!["c/lorem".to_string()],
                ..Default::default()
            },
            PackageAutoload {
                name: "e/e".to_string(),
                install_path: "e/e".to_string(),
                autoload: Autoload::new().add_file("testE.php"),
                requires: vec!["c/lorem".to_string()],
                ..Default::default()
            },
        ];
        let root_autoload = Autoload::new().add_file("root2.php");

        generator_for(&temp_dir)
            .generate(&packages, Some(&root_autoload), None)
            .unwrap();

        let files = generated(&temp_dir, "autoload_files.php");
        let paths = [
            "/c/lorem/testC.php",
            "/d/d/testD.php",
            "/b/bar/testB.php",
            "/e/e/testE.php",
            "/z/foo/testA.php",
            "/root2.php",
        ];
        let positions = paths.map(|path| files.find(path).unwrap());
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    // Ported from Composer\Test\Autoload\AutoloadGeneratorTest::testVendorsAutoloading.
    #[test]
    fn vendor_packages_generate_psr0_maps_and_an_empty_classmap() {
        let temp_dir = TempDir::new().unwrap();
        let packages = vec![
            package(
                "a/a",
                Autoload::new()
                    .add_psr0("A", "src/")
                    .add_psr0("A\\B", "lib/"),
            ),
            package("b/b", Autoload::new().add_psr0("B\\Sub\\Name", "src/")),
        ];

        generator_for(&temp_dir)
            .generate(&packages, None, None)
            .unwrap();

        let namespaces = generated(&temp_dir, "autoload_namespaces.php");
        for expected in [
            "'A' => array($vendorDir . '/a/a/src')",
            "'A\\\\B' => array($vendorDir . '/a/a/lib')",
            "'B\\\\Sub\\\\Name' => array($vendorDir . '/b/b/src')",
        ] {
            assert!(
                namespaces.contains(expected),
                "missing {expected} in {namespaces}"
            );
        }
        let classmap = generated(&temp_dir, "autoload_classmap.php");
        assert!(classmap.contains("'Composer\\\\InstalledVersions'"));
    }

    // Ported from Composer\Test\Autoload\AutoloadGeneratorTest::testVendorsAutoloadingWithMetapackages.
    #[test]
    fn metapackage_autoload_rules_are_ignored() {
        let temp_dir = TempDir::new().unwrap();
        let packages = vec![
            PackageAutoload {
                package_type: crate::package::package_type::METAPACKAGE.to_string(),
                ..package("a/a", Autoload::new().add_psr0("A", "src/"))
            },
            package("b/b", Autoload::new().add_psr0("B\\Sub\\Name", "src/")),
        ];

        generator_for(&temp_dir)
            .generate(&packages, None, None)
            .unwrap();

        let namespaces = generated(&temp_dir, "autoload_namespaces.php");
        assert!(!namespaces.contains("'A' =>"));
        assert!(namespaces.contains("'B\\\\Sub\\\\Name' => array($vendorDir . '/b/b/src')"));
    }

    // Ported from Composer\Test\Autoload\AutoloadGeneratorTest::testPSRToClassMapIgnoresNonExistingDir.
    #[test]
    fn optimized_classmap_ignores_missing_psr_directories() {
        let temp_dir = TempDir::new().unwrap();
        let generator = AutoloadGenerator::new(AutoloadConfig {
            vendor_dir: temp_dir.path().join("vendor"),
            base_dir: temp_dir.path().to_path_buf(),
            optimize: true,
            suffix: Some("ComposerPort".to_string()),
            ..Default::default()
        });
        let root = Autoload::new()
            .add_psr0("Prefix", "foo/bar/non/existing/")
            .add_psr4("Prefix\\", "foo/bar/non/existing2/");

        generator.generate(&[], Some(&root), None).unwrap();

        let classmap = generated(&temp_dir, "autoload_classmap.php");
        assert_eq!(classmap.matches("    '").count(), 1);
        assert!(classmap.contains("'Composer\\\\InstalledVersions'"));
    }

    // Ported from Composer\Test\Autoload\AutoloadGeneratorTest::testVendorsClassMapAutoloading.
    #[test]
    fn vendor_classmap_paths_are_scanned_across_packages() {
        let temp_dir = TempDir::new().unwrap();
        write_php(
            temp_dir.path().join("vendor/a/a/src/a.php"),
            "<?php class ClassMapFoo {}",
        );
        write_php(
            temp_dir.path().join("vendor/b/b/src/b.php"),
            "<?php class ClassMapBar {}",
        );
        write_php(
            temp_dir.path().join("vendor/b/b/lib/c.php"),
            "<?php class ClassMapBaz {}",
        );
        let packages = vec![
            package("a/a", Autoload::new().add_classmap("src/")),
            package(
                "b/b",
                Autoload::new().add_classmap("src/").add_classmap("lib/"),
            ),
        ];

        generator_for(&temp_dir)
            .generate(&packages, None, None)
            .unwrap();

        let classmap = generated(&temp_dir, "autoload_classmap.php");
        for expected in [
            "'ClassMapFoo' => $vendorDir . '/a/a/src/a.php'",
            "'ClassMapBar' => $vendorDir . '/b/b/src/b.php'",
            "'ClassMapBaz' => $vendorDir . '/b/b/lib/c.php'",
        ] {
            assert!(
                classmap.contains(expected),
                "missing {expected} in {classmap}"
            );
        }
    }

    // Ported from Composer\Test\Autoload\AutoloadGeneratorTest::testClassMapAutoloadingEmptyDirAndExactFile.
    #[test]
    fn classmap_accepts_package_root_current_directory_and_exact_file_paths() {
        let temp_dir = TempDir::new().unwrap();
        write_php(
            temp_dir.path().join("vendor/a/a/src/a.php"),
            "<?php class ClassMapFoo {}",
        );
        write_php(
            temp_dir.path().join("vendor/b/b/test.php"),
            "<?php class ClassMapBar {}",
        );
        write_php(
            temp_dir.path().join("vendor/c/c/foo/test.php"),
            "<?php class ClassMapBaz {}",
        );
        let packages = vec![
            package("a/a", Autoload::new().add_classmap("")),
            package("b/b", Autoload::new().add_classmap("test.php")),
            package("c/c", Autoload::new().add_classmap("./")),
        ];

        generator_for(&temp_dir)
            .generate(&packages, None, None)
            .unwrap();

        let classmap = generated(&temp_dir, "autoload_classmap.php");
        for class in ["ClassMapFoo", "ClassMapBar", "ClassMapBaz"] {
            assert!(classmap.contains(&format!("'{class}' =>")));
        }
        assert!(
            !generated(&temp_dir, "autoload_real.php").contains("setClassMapAuthoritative(true)")
        );
    }

    // Ported from Composer\Test\Autoload\AutoloadGeneratorTest::testClassMapAutoloadingAuthoritativeAndApcu.
    #[test]
    fn authoritative_apcu_mode_scans_psr4_paths_and_configures_the_loader() {
        let temp_dir = TempDir::new().unwrap();
        write_php(
            temp_dir.path().join("vendor/a/a/src/ClassMapFoo.php"),
            "<?php class ClassMapFoo {}",
        );
        let packages = vec![package("a/a", Autoload::new().add_psr4("", "src/"))];
        let generator = AutoloadGenerator::new(AutoloadConfig {
            vendor_dir: temp_dir.path().join("vendor"),
            base_dir: temp_dir.path().to_path_buf(),
            authoritative: true,
            apcu: true,
            suffix: Some("ComposerPort".to_string()),
            ..Default::default()
        });

        generator.generate(&packages, None, None).unwrap();

        assert!(generated(&temp_dir, "autoload_classmap.php").contains("'ClassMapFoo'"));
        let real = generated(&temp_dir, "autoload_real.php");
        assert!(real.contains("setClassMapAuthoritative(true)"));
        assert!(real.contains("setApcuPrefix('ComposerAutoloaderComposerPort')"));
    }

    // Ported from Composer\Test\Autoload\AutoloadGeneratorTest::testClassMapAutoloadingAuthoritativeAndApcuPrefix.
    #[test]
    fn custom_apcu_prefix_is_php_escaped() {
        let temp_dir = TempDir::new().unwrap();
        let generator = AutoloadGenerator::new(AutoloadConfig {
            vendor_dir: temp_dir.path().join("vendor"),
            base_dir: temp_dir.path().to_path_buf(),
            authoritative: true,
            apcu: true,
            apcu_prefix: Some("custom'Prefix".to_string()),
            suffix: Some("ComposerPort".to_string()),
            ..Default::default()
        });

        generator.generate(&[], None, None).unwrap();

        let real = generated(&temp_dir, "autoload_real.php");
        assert!(real.contains("setClassMapAuthoritative(true)"));
        assert!(real.contains("setApcuPrefix('custom\\'Prefix')"));
    }

    // Ported from Composer\Test\Autoload\AutoloadGeneratorTest::testFilesAutoloadGeneration.
    #[test]
    fn package_and_root_files_are_emitted_with_stable_identifiers() {
        let temp_dir = TempDir::new().unwrap();
        let packages = vec![
            package("a/a", Autoload::new().add_file("test.php")),
            package("b/b", Autoload::new().add_file("test2.php")),
            package(
                "c/c",
                Autoload::new()
                    .add_file("test3.php")
                    .add_file("foo/bar/test4.php"),
            ),
        ];
        let root = Autoload::new().add_file("root.php");

        generator_for(&temp_dir)
            .generate(&packages, Some(&root), None)
            .unwrap();

        let files = generated(&temp_dir, "autoload_files.php");
        for (package_name, path, expected_path) in [
            ("a/a", "test.php", "$vendorDir . '/a/a/test.php'"),
            ("b/b", "test2.php", "$vendorDir . '/b/b/test2.php'"),
            ("c/c", "test3.php", "$vendorDir . '/c/c/test3.php'"),
            (
                "c/c",
                "foo/bar/test4.php",
                "$vendorDir . '/c/c/foo/bar/test4.php'",
            ),
            ("__root__", "root.php", "$baseDir . '/root.php'"),
        ] {
            let identifier = AutoloadGenerator::compute_file_identifier(package_name, path);
            assert!(files.contains(&format!("'{identifier}' => {expected_path}")));
        }
        assert!(generated(&temp_dir, "autoload_real.php").contains("$filesToLoad"));
    }

    // Ported from Composer\Test\Autoload\AutoloadGeneratorTest::testEmptyPaths.
    #[test]
    fn empty_root_paths_refer_to_project_root_and_are_scanned() {
        let temp_dir = TempDir::new().unwrap();
        write_php(
            temp_dir.path().join("Foo/Bar.php"),
            "<?php namespace Foo; class Bar {}",
        );
        write_php(
            temp_dir.path().join("class.php"),
            "<?php namespace Classmap; class Foo {}",
        );
        let root = Autoload::new()
            .add_psr0("Foo", "")
            .add_psr4("Acme\\Foo\\", "")
            .add_classmap("");
        let generator = AutoloadGenerator::new(AutoloadConfig {
            vendor_dir: temp_dir.path().join("vendor"),
            base_dir: temp_dir.path().to_path_buf(),
            optimize: true,
            suffix: Some("ComposerPort".to_string()),
            ..Default::default()
        });

        generator.generate(&[], Some(&root), None).unwrap();

        assert!(generated(&temp_dir, "autoload_namespaces.php")
            .contains("'Foo' => array($baseDir . '/')"));
        assert!(generated(&temp_dir, "autoload_psr4.php")
            .contains("'Acme\\\\Foo\\\\' => array($baseDir . '/')"));
        let classmap = generated(&temp_dir, "autoload_classmap.php");
        assert!(classmap.contains("'Foo\\\\Bar' => $baseDir . '/Foo/Bar.php'"));
        assert!(classmap.contains("'Classmap\\\\Foo' => $baseDir . '/class.php'"));
    }

    // Ported from Composer\Test\Autoload\AutoloadGeneratorTest::testVendorSubstringPath.
    #[test]
    fn project_paths_containing_vendor_substrings_stay_basedir_relative() {
        let temp_dir = TempDir::new().unwrap();
        let root = Autoload::new()
            .add_psr0("Foo", "composer-test-autoload-src/src")
            .add_psr4("Acme\\Foo\\", "composer-test-autoload-src/src-psr4");

        generator_for(&temp_dir)
            .generate(&[], Some(&root), None)
            .unwrap();

        assert!(generated(&temp_dir, "autoload_namespaces.php")
            .contains("'Foo' => array($baseDir . '/composer-test-autoload-src/src')"));
        assert!(generated(&temp_dir, "autoload_psr4.php").contains(
            "'Acme\\\\Foo\\\\' => array($baseDir . '/composer-test-autoload-src/src-psr4')"
        ));
    }

    // Ported from Composer\Test\Autoload\AutoloadGeneratorTest::testExcludeFromClassmap.
    #[test]
    fn classmap_excludes_honor_directory_boundaries_and_wildcards() {
        let temp_dir = TempDir::new().unwrap();
        write_php(
            temp_dir.path().join("composersrc/Included.php"),
            "<?php class IncludedClass {}",
        );
        write_php(
            temp_dir
                .path()
                .join("composersrc/excludedTests/Excluded.php"),
            "<?php class ExcludedClass {}",
        );
        write_php(
            temp_dir
                .path()
                .join("composersrc/long/excluded/excsubpath/Wildcard.php"),
            "<?php class WildcardExcludedClass {}",
        );
        let root = Autoload::new()
            .add_classmap("composersrc/")
            .add_exclude("/composersrc/excludedTests/")
            .add_exclude("/composersrc/*/excluded/excsubpath")
            .add_exclude("composers");

        generator_for(&temp_dir)
            .generate(&[], Some(&root), None)
            .unwrap();

        let classmap = generated(&temp_dir, "autoload_classmap.php");
        assert!(classmap.contains("'IncludedClass'"));
        assert!(!classmap.contains("'ExcludedClass'"));
        assert!(!classmap.contains("'WildcardExcludedClass'"));
    }

    #[test]
    fn custom_apcu_prefix_is_written_to_the_loader() {
        let temp_dir = TempDir::new().unwrap();
        let config = AutoloadConfig {
            vendor_dir: temp_dir.path().join("vendor"),
            apcu: true,
            apcu_prefix: Some("fixture-prefix".to_string()),
            ..Default::default()
        };

        AutoloadGenerator::new(config)
            .generate(&[], None, None)
            .unwrap();
        let real =
            std::fs::read_to_string(temp_dir.path().join("vendor/composer/autoload_real.php"))
                .unwrap();
        assert!(real.contains("setApcuPrefix('fixture-prefix')"));
    }

    #[test]
    fn test_generate_installed_metadata_without_autoloader() {
        let temp_dir = TempDir::new().unwrap();
        let config = AutoloadConfig {
            vendor_dir: temp_dir.path().join("vendor"),
            ..Default::default()
        };

        let generator = AutoloadGenerator::new(config);
        generator.generate_installed_metadata(&[], None).unwrap();

        assert!(temp_dir
            .path()
            .join("vendor/composer/InstalledVersions.php")
            .exists());
        assert!(temp_dir
            .path()
            .join("vendor/composer/installed.json")
            .exists());
        assert!(temp_dir
            .path()
            .join("vendor/composer/installed.php")
            .exists());
        assert!(!temp_dir.path().join("vendor/autoload.php").exists());
    }

    #[test]
    fn test_extract_path_from_code_stays_under_configured_roots() {
        let temp_dir = TempDir::new().unwrap();
        let generator = AutoloadGenerator::new(AutoloadConfig {
            vendor_dir: temp_dir.path().join("vendor"),
            base_dir: temp_dir.path().to_path_buf(),
            ..Default::default()
        });

        let base_path = generator
            .extract_path_from_code("$baseDir . '/src'")
            .unwrap();
        assert_eq!(PathBuf::from(base_path), temp_dir.path().join("src"));
        let vendor_path = generator
            .extract_path_from_code("$vendorDir . '/vendor/package/src'")
            .unwrap();
        assert_eq!(
            PathBuf::from(vendor_path),
            temp_dir.path().join("vendor/vendor/package/src")
        );
    }

    #[test]
    fn test_generate_installed_php_with_packages() {
        let temp_dir = TempDir::new().unwrap();
        let config = AutoloadConfig {
            vendor_dir: temp_dir.path().join("vendor"),
            base_dir: temp_dir.path().to_path_buf(),
            ..Default::default()
        };

        let packages = vec![
            PackageAutoload {
                name: "vendor/package1".to_string(),
                install_path: "vendor/package1".to_string(),
                pretty_version: Some("1.0.0".to_string()),
                version: Some("1.0.0.0".to_string()),
                reference: Some("abc123".to_string()),
                package_type: "library".to_string(),
                dev_requirement: false,
                installation_source: Some("dist".to_string()),
                replaces: IndexMap::new(),
                provides: IndexMap::new(),
                ..Default::default()
            },
            PackageAutoload {
                name: "vendor/package2".to_string(),
                install_path: "vendor/package2".to_string(),
                pretty_version: Some("2.0.0".to_string()),
                version: Some("2.0.0.0".to_string()),
                reference: Some("def456".to_string()),
                package_type: "library".to_string(),
                dev_requirement: true,
                replaces: IndexMap::new(),
                provides: IndexMap::new(),
                ..Default::default()
            },
        ];

        let root = RootPackageInfo {
            name: "my/project".to_string(),
            pretty_version: "dev-main".to_string(),
            version: "dev-main".to_string(),
            reference: None,
            package_type: "project".to_string(),
            aliases: Vec::new(),
            dev_mode: true,
            ..Default::default()
        };

        let generator = AutoloadGenerator::new(config);
        let result = generator.generate(&packages, None, Some(&root));

        assert!(result.is_ok());

        // Check installed.php was created
        let installed_path = temp_dir.path().join("vendor/composer/installed.php");
        assert!(installed_path.exists());

        // Read and verify content
        let content = std::fs::read_to_string(&installed_path).unwrap();
        assert!(content.contains("'my/project'"));
        assert!(content.contains("'vendor/package1'"));
        assert!(content.contains("'vendor/package2'"));
        assert!(content.contains("'1.0.0'"));
        assert!(content.contains("'abc123'"));
        assert!(content.contains("'dev_requirement' => false"));
        assert!(content.contains("'dev_requirement' => true"));

        let installed_json =
            std::fs::read_to_string(temp_dir.path().join("vendor/composer/installed.json"))
                .unwrap();
        let installed: serde_json::Value = serde_json::from_str(&installed_json).unwrap();
        assert_eq!(installed["packages"].as_array().unwrap().len(), 2);
        assert_eq!(installed["dev"], true);
        assert_eq!(installed["dev-package-names"][0], "vendor/package2");
        assert_eq!(installed["packages"][0]["installation-source"], "dist");
    }

    #[test]
    fn test_generate_installed_php_with_provides_and_replaces() {
        let temp_dir = TempDir::new().unwrap();
        let config = AutoloadConfig {
            vendor_dir: temp_dir.path().join("vendor"),
            base_dir: temp_dir.path().to_path_buf(),
            ..Default::default()
        };

        let mut replaces = IndexMap::new();
        replaces.insert("old/package".to_string(), "1.0.0".to_string());

        let mut provides = IndexMap::new();
        provides.insert("psr/log-implementation".to_string(), "1.0.0".to_string());

        let packages = vec![PackageAutoload {
            name: "monolog/monolog".to_string(),
            install_path: "monolog/monolog".to_string(),
            pretty_version: Some("2.0.0".to_string()),
            version: Some("2.0.0.0".to_string()),
            reference: None,
            package_type: "library".to_string(),
            dev_requirement: false,
            replaces,
            provides,
            ..Default::default()
        }];

        let generator = AutoloadGenerator::new(config);
        let result = generator.generate(&packages, None, None);

        assert!(result.is_ok());

        let installed_path = temp_dir.path().join("vendor/composer/installed.php");
        let content = std::fs::read_to_string(&installed_path).unwrap();

        // Check that provides and replaces entries are present
        assert!(content.contains("'psr/log-implementation'"));
        assert!(content.contains("'provided'"));
        assert!(content.contains("'old/package'"));
        assert!(content.contains("'replaced'"));
    }

    #[test]
    fn composer_filesystem_repository_writes_installed_php_metadata() {
        let temp = TempDir::new().unwrap();
        let mut provider = PackageAutoload {
            name: "a/provider".to_string(),
            install_path: "vendor/{${passthru('bash -i')}}".to_string(),
            pretty_version: Some("1.1".to_string()),
            version: Some("1.1.0.0".to_string()),
            reference: Some("distref-as-no-source".to_string()),
            package_type: "library".to_string(),
            ..Default::default()
        };
        provider
            .provides
            .insert("foo/impl".to_string(), "^1.1".to_string());

        let mut provider2 = PackageAutoload {
            name: "a/provider2".to_string(),
            install_path: "a/provider2".to_string(),
            pretty_version: Some("1.2".to_string()),
            version: Some("1.2.0.0".to_string()),
            reference: Some("distref-as-installed-from-dist".to_string()),
            package_type: "library".to_string(),
            aliases: vec!["1.4".to_string()],
            ..Default::default()
        };
        provider2
            .provides
            .insert("foo/impl".to_string(), "self.version".to_string());

        let mut replacer = PackageAutoload {
            name: "b/replacer".to_string(),
            install_path: "b/replacer".to_string(),
            pretty_version: Some("2.2".to_string()),
            version: Some("2.2.0.0".to_string()),
            package_type: "library".to_string(),
            ..Default::default()
        };
        replacer
            .replaces
            .insert("foo/replaced".to_string(), "^3.0".to_string());

        let dev_package = PackageAutoload {
            name: "c/c".to_string(),
            install_path: "/foo/bar/ven/do{}r/c/c${}".to_string(),
            pretty_version: Some("3.0".to_string()),
            version: Some("3.0.0.0".to_string()),
            reference: Some("{${passthru('bash -i')}} Foo\\Bar\n\ttab\0".to_string()),
            package_type: "library".to_string(),
            dev_requirement: true,
            ..Default::default()
        };
        let metapackage = PackageAutoload {
            name: "meta/package".to_string(),
            package_type: "metapackage".to_string(),
            pretty_version: Some("3.0".to_string()),
            version: Some("3.0.0.0".to_string()),
            ..Default::default()
        };
        let root = RootPackageInfo {
            name: "__root__".to_string(),
            pretty_version: "dev-master".to_string(),
            version: "dev-master".to_string(),
            reference: Some("sourceref-by-default".to_string()),
            package_type: "library".to_string(),
            aliases: vec!["1.10.x-dev".to_string()],
            replaces: IndexMap::from([("root/replaced".to_string(), "*".to_string())]),
            dev_mode: true,
            ..Default::default()
        };
        let composer_dir = temp.path().join("vendor/composer");
        AutoloadGenerator::new(AutoloadConfig {
            vendor_dir: temp.path().join("vendor"),
            base_dir: temp.path().to_path_buf(),
            ..Default::default()
        })
        .generate_installed_metadata(
            &[provider, provider2, replacer, dev_package, metapackage],
            Some(&root),
        )
        .unwrap();

        let installed_path = composer_dir.join("installed.php");
        let installed = crate::repository::safely_load_installed_versions(&installed_path)
            .unwrap_or_else(|| {
                panic!(
                    "generated installed.php must use the safe value grammar:\n{}",
                    std::fs::read_to_string(installed_path).unwrap()
                )
            });

        assert_eq!(installed["root"]["name"], "__root__");
        assert_eq!(
            installed["root"]["aliases"],
            serde_json::json!(["1.10.x-dev"])
        );
        assert_eq!(
            installed["versions"]["a/provider"]["install_path"],
            format!(
                "{}/../vendor/{{${{passthru('bash -i')}}}}",
                composer_dir.display()
            )
        );
        assert_eq!(
            installed["versions"]["a/provider2"]["aliases"],
            serde_json::json!(["1.4"])
        );
        assert_eq!(
            installed["versions"]["foo/impl"]["provided"],
            serde_json::json!(["1.2", "^1.1"])
        );
        assert_eq!(
            installed["versions"]["foo/replaced"]["replaced"],
            serde_json::json!(["^3.0"])
        );
        assert_eq!(
            installed["versions"]["root/replaced"]["replaced"],
            serde_json::json!(["*"])
        );
        assert_eq!(
            installed["versions"]["c/c"]["install_path"],
            "/foo/bar/ven/do{}r/c/c${}"
        );
        assert_eq!(installed["versions"]["c/c"]["dev_requirement"], true);
        assert!(installed["versions"]["meta/package"]["install_path"].is_null());

        let installed_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(composer_dir.join("installed.json")).unwrap())
                .unwrap();
        let packages = installed_json["packages"].as_array().unwrap();
        let dev_package = packages
            .iter()
            .find(|package| package["name"] == "c/c")
            .unwrap();
        let absolute_install_path = PathBuf::from("/foo/bar/ven/do{}r/c/c${}");
        let expected_absolute_path = pathdiff::diff_paths(&absolute_install_path, &composer_dir)
            .unwrap_or(absolute_install_path)
            .to_string_lossy()
            .replace('\\', "/");
        assert_eq!(dev_package["install-path"], expected_absolute_path);
        assert_eq!(
            installed_json["dev-package-names"],
            serde_json::json!(["c/c"])
        );
    }

    #[test]
    fn composer_include_paths_are_ordered_loaded_and_removed_when_stale() {
        let temp = TempDir::new().unwrap();
        let mut a = package("a/a", Autoload::new().add_file("bootstrap.php"));
        a.include_paths = vec!["lib/".into()];
        let mut b = package("b/b", Autoload::default());
        b.include_paths = vec!["library".into()];
        let root = Autoload::new().add_file("root.php");
        generator_for(&temp)
            .with_root_include_paths(["/lib".into(), "/src".into()])
            .with_global_include_path(true)
            .generate(&[a, b], Some(&root), None)
            .unwrap();

        let include_paths = generated(&temp, "include_paths.php");
        let expected = [
            "$baseDir . '/lib'",
            "$baseDir . '/src'",
            "$vendorDir . '/a/a/lib'",
            "$vendorDir . '/b/b/library'",
        ];
        let positions = expected.map(|path| include_paths.find(path).unwrap());
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        let real = generated(&temp, "autoload_real.php");
        assert!(real.contains("require __DIR__ . '/include_paths.php'"));
        assert!(real.contains("$loader->setUseIncludePath(true)"));

        generator_for(&temp).generate(&[], None, None).unwrap();
        assert!(!temp
            .path()
            .join("vendor/composer/include_paths.php")
            .exists());
        assert!(!temp
            .path()
            .join("vendor/composer/autoload_files.php")
            .exists());
        let real = generated(&temp, "autoload_real.php");
        assert!(!real.contains("include_paths.php"));
        assert!(!real.contains("::$files"));
    }

    #[test]
    fn composer_duplicate_file_rules_emit_each_path_once() {
        let temp = TempDir::new().unwrap();
        let root = Autoload::new()
            .add_file("foo.php")
            .add_file("bar.php")
            .add_file("./foo.php")
            .add_file("././foo.php");
        let generator = generator_for(&temp);
        assert_eq!(
            generator.duplicate_file_autoload_paths(&[], Some(&root)),
            vec!["$baseDir . '/foo.php'"]
        );
        generator.generate(&[], Some(&root), None).unwrap();
        let files = generated(&temp, "autoload_files.php");
        assert_eq!(files.matches("$baseDir . '/foo.php'").count(), 1);
        assert_eq!(files.matches("$baseDir . '/bar.php'").count(), 1);
    }

    #[test]
    fn composer_pre_and_post_autoload_events_wrap_generation() {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let temp = TempDir::new().unwrap();
        let captured = events.clone();
        generator_for(&temp)
            .with_event_handler(move |event| {
                captured.lock().unwrap().push(event);
                Ok(())
            })
            .generate(&[], None, None)
            .unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                AutoloadGenerationEvent::PreGenerate,
                AutoloadGenerationEvent::PostGenerate
            ]
        );
    }

    #[test]
    fn composer_platform_check_uses_lower_bounds_extensions_filters_and_providers() {
        let temp = TempDir::new().unwrap();
        let requirements = PlatformCheckRequirements {
            requires: IndexMap::from([
                ("php".into(), "^7.2.8".into()),
                ("php-64bit".into(), "*".into()),
                ("ext-xml".into(), "*".into()),
                ("ext-pdo".into(), "^7.2".into()),
                ("ext-fileinfo".into(), "*".into()),
            ]),
            provides: IndexMap::from([("ext-XML".into(), "*".into())]),
            ignored: vec!["ext-fil*".into()],
            ..Default::default()
        };
        generator_for(&temp)
            .with_platform_check(requirements)
            .generate(&[], None, None)
            .unwrap();
        let platform = generated(&temp, "platform_check.php");
        assert!(platform.contains("PHP_VERSION_ID >= 70208"));
        assert!(platform.contains("PHP_INT_SIZE !== 8"));
        assert!(platform.contains("extension_loaded('pdo')"));
        assert!(!platform.contains("extension_loaded('xml')"));
        assert!(!platform.contains("extension_loaded('fileinfo')"));
        assert!(generated(&temp, "autoload_real.php")
            .contains("require __DIR__ . '/platform_check.php'"));

        generator_for(&temp)
            .with_platform_check(PlatformCheckRequirements {
                requires: IndexMap::from([("php".into(), "<8".into())]),
                ..Default::default()
            })
            .generate(&[], None, None)
            .unwrap();
        assert!(!temp
            .path()
            .join("vendor/composer/platform_check.php")
            .exists());
        assert!(!generated(&temp, "autoload_real.php").contains("platform_check.php"));
    }

    #[test]
    fn composer_installed_paths_are_relative_to_the_composer_directory() {
        let composer_dir = Path::new("/project/vendor/composer");
        assert_eq!(
            AutoloadGenerator::installed_json_path(composer_dir, "composer/pcre"),
            "./pcre"
        );
        assert_eq!(
            AutoloadGenerator::installed_json_path(composer_dir, "symfony/console"),
            "../symfony/console"
        );
        assert_eq!(
            AutoloadGenerator::installed_php_path("composer/pcre"),
            "__DIR__ . '/./pcre'"
        );
        assert_eq!(
            AutoloadGenerator::installed_php_path("symfony/console"),
            "__DIR__ . '/../symfony/console'"
        );
    }

    #[test]
    fn plugin_fileless_packages_have_null_install_paths() {
        let temp = TempDir::new().unwrap();
        let package = PackageAutoload {
            name: "vendor/fileless".into(),
            install_path: "vendor/fileless".into(),
            package_type: "custom-fileless".into(),
            fileless: true,
            pretty_version: Some("1.0.0".into()),
            version: Some("1.0.0.0".into()),
            ..Default::default()
        };
        generator_for(&temp)
            .generate_installed_metadata(&[package], None)
            .unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&generated(&temp, "installed.json")).unwrap();
        assert!(json["packages"][0]["install-path"].is_null());
        assert!(generated(&temp, "installed.php").contains("'install_path' => null"));
    }

    #[test]
    fn composer_non_dev_reachability_follows_replacements_and_cycles() {
        let temp = TempDir::new().unwrap();
        for (name, class) in [
            ("a/a", "A"),
            ("b/b", "B"),
            ("c/c", "C"),
            ("d/d", "D"),
            ("e/e", "E"),
        ] {
            write_php(
                temp.path().join(format!("vendor/{name}/src/{class}.php")),
                &format!("<?php class {class} {{}}"),
            );
        }
        let mut a = package("a/a", Autoload::new().add_classmap("src/A.php"));
        a.requires = vec!["b/b".into()];
        let mut b = package("b/b", Autoload::new().add_classmap("src/B.php"));
        b.requires = vec!["e/e".into()];
        let mut c = package("c/c", Autoload::new().add_classmap("src/C.php"));
        c.replaces.insert("b/b".into(), "*".into());
        c.requires = vec!["d/d".into()];
        let mut d = package("d/d", Autoload::new().add_classmap("src/D.php"));
        d.requires = vec!["a/a".into()];
        let e = package("e/e", Autoload::new().add_classmap("src/E.php"));

        generator_for(&temp)
            .with_root_requires(["a/a".into()])
            .generate(&[a, b, c, d, e], None, None)
            .unwrap();
        let classmap = generated(&temp, "autoload_classmap.php");
        for class in ["A", "C", "D"] {
            assert!(classmap.contains(&format!("'{class}' =>")));
        }
        for class in ["B", "E"] {
            assert!(!classmap.contains(&format!("'{class}' =>")));
        }
    }

    #[test]
    fn composer_target_dirs_adjust_root_and_vendor_autoload_paths() {
        let temp = TempDir::new().unwrap();
        write_php(
            temp.path().join("src/root.php"),
            "<?php class RootTargetClass {}",
        );
        write_php(
            temp.path().join("vendor/a/a/target/lib/vendor.php"),
            "<?php class VendorTargetClass {}",
        );
        let mut vendor = package(
            "a/a",
            Autoload::new()
                .add_classmap("lib/")
                .add_psr0("Vendor", "target/src/"),
        );
        vendor.target_dir = Some("target".into());
        let root = Autoload::new()
            .add_classmap("Main/Foo/src/")
            .add_file("Main/Foo/bar.php")
            .add_psr0("Main\\Foo", "");
        generator_for(&temp)
            .with_root_target_dir("Main/Foo")
            .generate(&[vendor], Some(&root), None)
            .unwrap();

        let classmap = generated(&temp, "autoload_classmap.php");
        assert!(classmap.contains("'RootTargetClass' => $baseDir . '/src/root.php'"));
        assert!(
            classmap.contains("'VendorTargetClass' => $vendorDir . '/a/a/target/lib/vendor.php'")
        );
        let namespaces = generated(&temp, "autoload_namespaces.php");
        assert!(namespaces.contains("'Main\\\\Foo' => array($baseDir . '/')"));
        assert!(namespaces.contains("$vendorDir . '/a/a/target/src'"));
        assert!(generated(&temp, "autoload_files.php").contains("$baseDir . '/bar.php'"));
    }

    #[test]
    fn composer_base_dir_generation_handles_same_nested_sibling_and_up_level_paths() {
        let temp = TempDir::new().unwrap();
        let same = AutoloadGenerator::new(AutoloadConfig {
            vendor_dir: temp.path().to_path_buf(),
            base_dir: temp.path().to_path_buf(),
            suffix: Some("Same".into()),
            ..Default::default()
        });
        same.generate(&[], Some(&Autoload::new().add_psr0("Same", "src")), None)
            .unwrap();
        assert!(
            std::fs::read_to_string(temp.path().join("composer/autoload_namespaces.php"))
                .unwrap()
                .contains("$baseDir = $vendorDir;")
        );

        let nested_vendor = temp.path().join("nested/vendor/subdir");
        AutoloadGenerator::new(AutoloadConfig {
            vendor_dir: nested_vendor.clone(),
            base_dir: temp.path().join("nested"),
            suffix: Some("Nested".into()),
            ..Default::default()
        })
        .generate(
            &[],
            Some(&Autoload::new().add_psr4("Nested\\", "src")),
            None,
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(nested_vendor.join("composer/autoload_psr4.php"))
                .unwrap()
                .contains("$baseDir = dirname($vendorDir, 2);")
        );

        let base = temp.path().join("working-dir");
        let vendor = temp.path().join("vendor");
        std::fs::create_dir_all(&base).unwrap();
        let root = Autoload::new()
            .add_psr0("Foo", "../path/../src")
            .add_psr4("Acme\\Foo\\", "../path/../src-psr4")
            .add_file("../test.php");
        AutoloadGenerator::new(AutoloadConfig {
            vendor_dir: vendor.clone(),
            base_dir: base,
            suffix: Some("Sibling".into()),
            ..Default::default()
        })
        .generate(&[], Some(&root), None)
        .unwrap();
        let namespaces =
            std::fs::read_to_string(vendor.join("composer/autoload_namespaces.php")).unwrap();
        assert!(namespaces.contains("$baseDir = dirname($vendorDir) . '/working-dir';"));
        assert!(namespaces.contains("$baseDir . '/../src'"));
        assert!(
            std::fs::read_to_string(vendor.join("composer/autoload_files.php"))
                .unwrap()
                .contains("$baseDir . '/../test.php'")
        );
    }

    #[test]
    fn composer_optimized_maps_enforce_psr_paths_and_symlink_exclusions() {
        let temp = TempDir::new().unwrap();
        write_php(
            temp.path().join("psr4/match.php"),
            "<?php namespace psr4; class match {}",
        );
        write_php(
            temp.path().join("psr4/badfile.php"),
            "<?php namespace psr4; class badclass {}",
        );
        write_php(
            temp.path().join("psr0/psr0/match.php"),
            "<?php class psr0_match {}",
        );
        write_php(
            temp.path().join("psr0/psr0/badfile.php"),
            "<?php class psr0_badclass {}",
        );
        write_php(
            temp.path().join("tools-real/MyClass.php"),
            "<?php class MyClass {}",
        );
        write_php(
            temp.path().join("tools-real/vendor/pkg/Hidden.php"),
            "<?php class HiddenVendorClass {}",
        );
        #[cfg(unix)]
        std::os::unix::fs::symlink(temp.path().join("tools-real"), temp.path().join("tools"))
            .unwrap();

        let root = Autoload::new()
            .add_psr4("psr4\\", "psr4/")
            .add_psr0("psr0_", "psr0/")
            .add_classmap("tools/")
            .add_exclude("**/vendor/");
        let generator = AutoloadGenerator::new(AutoloadConfig {
            vendor_dir: temp.path().join("vendor"),
            base_dir: temp.path().to_path_buf(),
            optimize: true,
            suffix: Some("Psr".into()),
            ..Default::default()
        });
        generator.generate(&[], Some(&root), None).unwrap();
        let classmap = generated(&temp, "autoload_classmap.php");
        for class in ["psr4\\match", "psr0_match"] {
            assert!(classmap.contains(&format!("'{}' =>", class.replace('\\', "\\\\"))));
        }
        #[cfg(unix)]
        assert!(classmap.contains("'MyClass' =>"));
        for class in ["psr4\\badclass", "psr0_badclass", "HiddenVendorClass"] {
            assert!(!classmap.contains(class));
        }
    }

    #[test]
    fn composer_missing_and_phar_paths_remain_in_generated_psr_maps() {
        let temp = TempDir::new().unwrap();
        let packages = [package(
            "dep/a",
            Autoload::new()
                .add_psr0("Foo", "./src")
                .add_psr4("Lorem\\", "lorem.phar"),
        )];
        let root = Autoload::new()
            .add_psr0("Bar", "dir/bar.phar/src")
            .add_psr4("Baz\\", "baz.phar");
        generator_for(&temp)
            .generate(&packages, Some(&root), None)
            .unwrap();
        let psr0 = generated(&temp, "autoload_namespaces.php");
        assert!(psr0.contains("$vendorDir . '/dep/a/src'"));
        assert!(psr0.contains("$baseDir . '/dir/bar.phar/src'"));
        let psr4 = generated(&temp, "autoload_psr4.php");
        assert!(psr4.contains("$vendorDir . '/dep/a/lorem.phar'"));
        assert!(psr4.contains("$baseDir . '/baz.phar'"));
    }

    #[test]
    fn composer_root_autoload_rules_override_vendor_paths_and_classmaps() {
        let temp = TempDir::new().unwrap();
        write_php(
            temp.path().join("src/classes.php"),
            "<?php namespace Foo; class Bar {}",
        );
        write_php(
            temp.path().join("vendor/a/a/classmap/classes.php"),
            "<?php namespace Foo; class Bar {}",
        );
        let package = package(
            "a/a",
            Autoload::new()
                .add_psr0("A\\B", "lib/")
                .add_classmap("classmap/"),
        );
        let root = Autoload::new()
            .add_psr0("A\\B", "lib/")
            .add_classmap("src/");
        generator_for(&temp)
            .generate(&[package], Some(&root), None)
            .unwrap();
        let namespaces = generated(&temp, "autoload_namespaces.php");
        assert!(
            namespaces.contains("'A\\\\B' => array($baseDir . '/lib', $vendorDir . '/a/a/lib')")
        );
        let classmap = generated(&temp, "autoload_classmap.php");
        assert!(classmap.contains("'Foo\\\\Bar' => $baseDir . '/src/classes.php'"));
        assert!(!classmap.contains("a/a/classmap/classes.php"));
    }
}
