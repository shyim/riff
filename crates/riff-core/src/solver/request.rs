use std::sync::Arc;

use indexmap::IndexMap;

use crate::package::Package;

/// A request specifies what needs to be resolved.
///
/// This includes root requirements, locked packages, and platform packages.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// Required packages from composer.json (name -> constraint)
    /// Uses IndexMap to preserve insertion order (critical for solver behavior)
    pub requires: IndexMap<String, String>,

    /// Development requirements (name -> constraint)
    /// Uses IndexMap to preserve insertion order
    pub dev_requires: IndexMap<String, String>,

    /// Fixed packages that cannot be changed (e.g., platform packages)
    pub fixed_packages: Vec<Arc<Package>>,

    /// Locked packages from composer.lock
    pub locked_packages: Vec<Arc<Package>>,

    /// Repository candidates excluded by package selection policy.
    pub excluded_packages: Vec<Arc<Package>>,

    /// Packages that must be updated (for partial updates)
    pub update_allowlist: Vec<String>,

    /// Whether the update allowlist is active.
    ///
    /// This is separate from `update_allowlist` because an explicitly empty
    /// partial-update selection means "keep every locked package", whereas no
    /// selection means a full update.
    pub update_restricted: bool,

    /// Whether this is a dev install
    pub install_dev: bool,

    /// Whether to prefer stable versions
    pub prefer_stable: bool,

    /// Whether to prefer lowest versions
    pub prefer_lowest: bool,
}

impl Request {
    /// Create a new empty request
    pub fn new() -> Self {
        Self {
            requires: IndexMap::new(),
            dev_requires: IndexMap::new(),
            fixed_packages: Vec::new(),
            locked_packages: Vec::new(),
            excluded_packages: Vec::new(),
            update_allowlist: Vec::new(),
            update_restricted: false,
            install_dev: true,
            prefer_stable: true,
            prefer_lowest: false,
        }
    }

    /// Add a requirement
    pub fn require(&mut self, name: impl Into<String>, constraint: impl Into<String>) -> &mut Self {
        self.requires
            .insert(name.into().to_lowercase(), constraint.into());
        self
    }

    /// Add a development requirement
    pub fn require_dev(
        &mut self,
        name: impl Into<String>,
        constraint: impl Into<String>,
    ) -> &mut Self {
        self.dev_requires
            .insert(name.into().to_lowercase(), constraint.into());
        self
    }

    /// Add a fixed package (cannot be changed)
    pub fn fix(&mut self, package: Package) -> &mut Self {
        self.fixed_packages.push(Arc::new(package));
        self
    }

    /// Add a locked package (from composer.lock)
    pub fn lock(&mut self, package: Package) -> &mut Self {
        self.locked_packages.push(Arc::new(package));
        self
    }

    /// Exclude an exact pool package from dependency resolution.
    pub fn exclude(&mut self, package: Arc<Package>) -> &mut Self {
        if !self
            .excluded_packages
            .iter()
            .any(|excluded| Arc::ptr_eq(excluded, &package))
        {
            self.excluded_packages.push(package);
        }
        self
    }

    pub fn has_exclusions(&self) -> bool {
        !self.excluded_packages.is_empty()
    }

    /// Set packages to update (partial update)
    pub fn update(&mut self, packages: Vec<String>) -> &mut Self {
        self.update_allowlist = packages.into_iter().map(|s| s.to_lowercase()).collect();
        self.update_restricted = true;
        self
    }

    /// Set whether to install dev dependencies
    pub fn with_dev(&mut self, install_dev: bool) -> &mut Self {
        self.install_dev = install_dev;
        self
    }

    /// Set preference for stable versions
    pub fn prefer_stable(&mut self, prefer: bool) -> &mut Self {
        self.prefer_stable = prefer;
        self
    }

    /// Set preference for lowest versions
    pub fn prefer_lowest(&mut self, prefer: bool) -> &mut Self {
        self.prefer_lowest = prefer;
        self
    }

    /// Get all requirements (including dev if enabled)
    pub fn all_requires(&self) -> impl Iterator<Item = (&String, &String)> {
        let main = self.requires.iter();
        let dev = if self.install_dev {
            Some(self.dev_requires.iter())
        } else {
            None
        };
        main.chain(dev.into_iter().flatten())
    }

    /// Check if a package is in the update allowlist
    pub fn is_update_allowed(&self, name: &str) -> bool {
        if !self.update_restricted {
            return true; // Full update
        }
        self.update_allowlist
            .iter()
            .any(|n| n == &name.to_lowercase())
    }

    /// Check if a package is fixed
    pub fn is_fixed(&self, name: &str) -> bool {
        let name_lower = name.to_lowercase();
        self.fixed_packages
            .iter()
            .any(|p| p.name.to_lowercase() == name_lower)
    }

    /// Get a fixed package by name
    pub fn get_fixed(&self, name: &str) -> Option<&Arc<Package>> {
        let name_lower = name.to_lowercase();
        self.fixed_packages
            .iter()
            .find(|p| p.name.to_lowercase() == name_lower)
    }

    /// Get a locked package by name
    pub fn get_locked(&self, name: &str) -> Option<&Arc<Package>> {
        let name_lower = name.to_lowercase();
        self.locked_packages
            .iter()
            .find(|p| p.name.to_lowercase() == name_lower)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_new() {
        let request = Request::new();
        assert!(request.requires.is_empty());
        assert!(request.install_dev);
    }

    #[test]
    fn test_request_require() {
        let mut request = Request::new();
        request.require("vendor/package", "^1.0");

        assert_eq!(
            request.requires.get("vendor/package"),
            Some(&"^1.0".to_string())
        );
    }

    #[test]
    fn composer_request_install_records_a_match_all_requirement() {
        let mut request = Request::new();
        request.require("foo", "*");

        assert_eq!(request.requires.len(), 1);
        assert_eq!(request.requires.get("foo").map(String::as_str), Some("*"));
    }

    #[test]
    fn composer_request_is_independent_of_repository_package_identity() {
        let first = Package::new("foo", "1.0.0");
        let second = Package::new("foo", "1.0.0");
        assert_eq!(first.name, second.name);

        let mut request = Request::new();
        request.require(&first.name, "=1.0.0");

        assert_eq!(request.requires.len(), 1);
        assert_eq!(
            request.requires.get(&second.name).map(String::as_str),
            Some("=1.0.0")
        );
    }

    #[test]
    fn test_request_all_requires() {
        let mut request = Request::new();
        request.require("vendor/prod", "^1.0");
        request.require_dev("vendor/dev", "^2.0");

        let all: Vec<_> = request.all_requires().collect();
        assert_eq!(all.len(), 2);

        request.with_dev(false);
        let all: Vec<_> = request.all_requires().collect();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn test_request_fixed() {
        let mut request = Request::new();
        request.fix(Package::new("php", "8.3.0"));

        assert!(request.is_fixed("php"));
        assert!(request.is_fixed("PHP")); // Case insensitive
        assert!(!request.is_fixed("ext-json"));
    }

    #[test]
    fn test_request_update_allowlist() {
        let mut request = Request::new();

        // Empty allowlist = full update
        assert!(request.is_update_allowed("vendor/package"));

        request.update(vec!["vendor/specific".to_string()]);
        assert!(request.is_update_allowed("vendor/specific"));
        assert!(!request.is_update_allowed("vendor/other"));

        request.update(Vec::new());
        assert!(!request.is_update_allowed("vendor/specific"));
    }
}
