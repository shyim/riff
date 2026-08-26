use async_trait::async_trait;
use std::sync::Arc;

use crate::filter_list::{FilterEntriesByList, PackageVersions};
use crate::json::SecurityAdvisory;
use crate::package::Package;

/// Search mode for repository searches
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchMode {
    /// Full text search in name and description
    #[default]
    Fulltext,
    /// Search by package name only
    Name,
    /// Search by vendor only
    Vendor,
}

/// Search result from a repository
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// Package name (vendor/package)
    pub name: String,
    /// Package description
    pub description: Option<String>,
    /// URL to package page
    pub url: Option<String>,
    /// Abandonment notice
    pub abandoned: Option<String>,
    /// Number of downloads
    pub downloads: Option<u64>,
    /// Number of favorites/stars
    pub favers: Option<u64>,
}

/// Provider information for virtual packages
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    /// Provider package name
    pub name: String,
    /// Provider description
    pub description: Option<String>,
    /// Provider type
    pub package_type: String,
}

/// Repository interface - read-only package source
#[async_trait]
pub trait Repository: Send + Sync {
    /// Get a unique name for this repository
    fn name(&self) -> &str;

    /// Whether finding a package here blocks lower-priority repositories.
    fn canonical(&self) -> bool {
        true
    }

    /// Check if the repository contains a package with the given name
    async fn has_package(&self, name: &str) -> bool;

    /// Find all versions of a package by name
    async fn find_packages(&self, name: &str) -> Vec<Arc<Package>>;

    /// Find a specific package version
    async fn find_package(&self, name: &str, version: &str) -> Option<Arc<Package>>;

    /// Find packages matching a version constraint
    async fn find_packages_with_constraint(
        &self,
        name: &str,
        constraint: &str,
    ) -> Vec<Arc<Package>>;

    /// Find packages for dependency resolution.
    ///
    /// Repositories may return packages with install-only metadata deferred. The
    /// selected packages must be passed to `hydrate_package` before transaction
    /// or installation work. The default preserves the regular repository API.
    async fn find_solver_packages_with_constraint(
        &self,
        name: &str,
        constraint: &str,
    ) -> Vec<Arc<Package>> {
        self.find_packages_with_constraint(name, constraint).await
    }

    /// Materialize metadata deferred by `find_solver_packages_with_constraint`.
    fn hydrate_package(&self, _package: &Arc<Package>) -> Option<Package> {
        None
    }

    /// Materialize only metadata required to plan a dry-run transaction.
    ///
    /// Repositories without a specialized projection retain the full
    /// hydration behavior.
    fn hydrate_package_for_transaction(&self, package: &Arc<Package>) -> Option<Package> {
        self.hydrate_package(package)
    }

    /// Get all packages in the repository
    async fn get_packages(&self) -> Vec<Arc<Package>>;

    /// Search for packages
    async fn search(&self, query: &str, mode: SearchMode) -> Vec<SearchResult>;

    /// Search for packages while optionally restricting the package type.
    ///
    /// Repositories with a native typed search endpoint can override this. The
    /// default keeps the behavior correct for local repositories.
    async fn search_with_type(
        &self,
        query: &str,
        mode: SearchMode,
        package_type: Option<&str>,
    ) -> Vec<SearchResult> {
        let mut results = self.search(query, mode).await;
        let Some(package_type) = package_type else {
            return results;
        };
        let names: std::collections::HashSet<_> = self
            .get_packages()
            .await
            .into_iter()
            .filter(|package| package.package_type() == package_type)
            .map(|package| package.name.clone())
            .collect();
        results.retain(|result| names.contains(&result.name));
        results
    }

    /// Get packages that provide a virtual package
    async fn get_providers(&self, package_name: &str) -> Vec<ProviderInfo>;

    /// Get the number of packages in the repository
    async fn count(&self) -> usize {
        self.get_packages().await.len()
    }

    /// Load multiple packages by name with constraints (batch loading).
    ///
    /// This is more efficient than calling `find_packages_with_constraint` multiple times
    /// as it allows repositories to optimize fetching (e.g., parallel HTTP requests).
    ///
    /// Returns packages found and names that were definitively found (to skip lower-priority repos).
    async fn load_packages_batch(
        &self,
        packages: &[(String, Option<String>)], // (name, optional constraint)
    ) -> LoadResult {
        let mut result = LoadResult {
            packages: Vec::new(),
            names_found: Vec::new(),
        };

        for (name, constraint) in packages {
            let found = if let Some(c) = constraint {
                self.find_packages_with_constraint(name, c).await
            } else {
                self.find_packages(name).await
            };

            if !found.is_empty() {
                result.names_found.push(name.clone());
                result.packages.extend(found);
            }
        }

        result
    }

    /// Fetch security advisories relevant to the candidate package versions.
    async fn get_security_advisories(
        &self,
        _package_versions: &PackageVersions,
        _allow_partial: bool,
    ) -> Result<Vec<SecurityAdvisory>, String> {
        Ok(Vec::new())
    }

    /// Fetch configured repository-provided dependency filter lists.
    async fn get_filter_entries(
        &self,
        _package_versions: &PackageVersions,
        _configured_lists: &[String],
    ) -> Result<FilterEntriesByList, String> {
        Ok(FilterEntriesByList::new())
    }
}

/// Writable repository interface - can add/remove packages
#[async_trait]
pub trait WritableRepository: Repository {
    /// Add a package to the repository
    async fn add_package(&mut self, package: Package);

    /// Remove a package from the repository
    async fn remove_package(&mut self, package: &Package);

    /// Check if this repository has been modified
    fn is_dirty(&self) -> bool;

    /// Persist changes to the repository
    async fn write(&self) -> std::io::Result<()>;
}

/// Configuration for a repository
#[derive(Debug, Clone)]
pub struct RepositoryConfig {
    /// Repository type
    pub repo_type: RepositoryType,
    /// Repository URL or path
    pub url: String,
    /// Additional options
    pub options: RepositoryOptions,
    /// Inline package data (for Package type repositories)
    pub package: Option<serde_json::Value>,
}

/// Repository type
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryType {
    /// Composer repository (Packagist-compatible)
    Composer,
    /// VCS repository (auto-detect)
    Vcs,
    /// Git repository
    Git,
    /// Mercurial repository
    Hg,
    /// Subversion repository
    Svn,
    /// Fossil repository
    Fossil,
    /// Perforce repository
    Perforce,
    /// Legacy PEAR package repository
    Pear,
    /// GitHub repository
    GitHub,
    /// GitLab repository
    GitLab,
    /// Bitbucket repository
    Bitbucket,
    /// Path repository (local directory)
    Path,
    /// Artifact repository (zip files in a directory)
    Artifact,
    /// Inline package definition
    Package,
}

/// Repository options
#[derive(Debug, Clone)]
pub struct RepositoryOptions {
    /// Use symlinks for path repositories
    pub symlink: Option<bool>,
    /// Use relative symlinks for path repositories
    pub relative: bool,
    /// Reference mode for path repositories: "none", "config", or "auto"
    pub reference: Option<String>,
    /// Version overrides for path repositories
    pub versions: std::collections::HashMap<String, String>,
    /// SSL verification options
    pub ssl_verify: Option<bool>,
    /// Canonical (affects priority)
    pub canonical: bool,
    /// Exclude packages matching these patterns
    pub exclude: Vec<String>,
    /// Only include packages matching these patterns
    pub only: Vec<String>,
}

impl Default for RepositoryOptions {
    fn default() -> Self {
        Self {
            symlink: None,
            relative: false,
            reference: None,
            versions: Default::default(),
            ssl_verify: None,
            canonical: true,
            exclude: Vec::new(),
            only: Vec::new(),
        }
    }
}

/// Result of loading packages from a repository
#[derive(Debug)]
pub struct LoadResult {
    /// Packages that were found
    pub packages: Vec<Arc<Package>>,
    /// Package names that were definitively found in this repository
    /// (should not be looked up in lower-priority repositories)
    pub names_found: Vec<String>,
}
