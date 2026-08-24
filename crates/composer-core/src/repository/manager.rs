use std::hash::{Hash, Hasher};
use std::sync::Arc;

use foldhash::HashSet as FastHashSet;

use super::artifact::ArtifactRepository;
use super::package::PackageRepository;
use super::path::{PathRepository, PathRepositoryOptions};
use super::traits::{Repository, RepositoryConfig, RepositoryType, SearchMode, SearchResult};
use super::vcs::{VcsRepository, VcsType};
use super::ComposerRepository;
use super::PlatformRepository;
use crate::package::Package;

/// Manages multiple repositories with priority ordering
pub struct RepositoryManager {
    /// Repositories in priority order (first = highest priority)
    repositories: Vec<Arc<dyn Repository>>,
}

#[derive(Clone, Debug)]
struct PackageIdentity(Arc<Package>);

impl PartialEq for PackageIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.0.name == other.0.name && self.0.version == other.0.version
    }
}

impl Eq for PackageIdentity {}

impl Hash for PackageIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.name.hash(state);
        self.0.version.hash(state);
    }
}

fn push_unique_package(
    packages: &mut Vec<Arc<Package>>,
    seen: &mut FastHashSet<PackageIdentity>,
    package: Arc<Package>,
) {
    if seen.insert(PackageIdentity(Arc::clone(&package))) {
        packages.push(package);
    }
}

impl RepositoryManager {
    /// Create a new repository manager
    pub fn new() -> Self {
        Self {
            repositories: Vec::new(),
        }
    }

    /// Add a repository (will be added with lowest priority)
    pub fn add_repository(&mut self, repo: Arc<dyn Repository>) {
        self.repositories.push(repo);
    }

    /// Insert a repository at a specific position (0 = highest priority)
    pub fn insert_repository(&mut self, index: usize, repo: Arc<dyn Repository>) {
        self.repositories.insert(index, repo);
    }

    /// Get all repositories
    pub fn repositories(&self) -> &[Arc<dyn Repository>] {
        &self.repositories
    }

    /// Find packages by name across all repositories
    pub async fn find_packages(&self, name: &str) -> Vec<Arc<Package>> {
        let mut packages = Vec::new();
        let mut seen = FastHashSet::default();

        for repo in &self.repositories {
            let found = repo.find_packages(name).await;
            packages.reserve(found.len());
            seen.reserve(found.len());
            for pkg in found {
                push_unique_package(&mut packages, &mut seen, pkg);
            }
        }

        packages
    }

    /// Find a specific package version
    pub async fn find_package(&self, name: &str, version: &str) -> Option<Arc<Package>> {
        for repo in &self.repositories {
            if let Some(pkg) = repo.find_package(name, version).await {
                return Some(pkg);
            }
        }
        None
    }

    /// Find packages matching a version constraint across all repositories
    pub async fn find_packages_with_constraint(
        &self,
        name: &str,
        constraint: &str,
    ) -> Vec<Arc<Package>> {
        let mut packages = Vec::new();
        let mut seen = FastHashSet::default();

        for repo in &self.repositories {
            let found = repo.find_packages_with_constraint(name, constraint).await;
            packages.reserve(found.len());
            seen.reserve(found.len());
            for pkg in found {
                push_unique_package(&mut packages, &mut seen, pkg);
            }
        }

        packages
    }

    /// Find packages using repository-specific solver representations.
    pub async fn find_solver_packages_with_constraint(
        &self,
        name: &str,
        constraint: &str,
    ) -> Vec<Arc<Package>> {
        let mut packages = Vec::new();
        let mut seen = FastHashSet::default();

        for repo in &self.repositories {
            let found = repo
                .find_solver_packages_with_constraint(name, constraint)
                .await;
            packages.reserve(found.len());
            seen.reserve(found.len());
            for pkg in found {
                push_unique_package(&mut packages, &mut seen, pkg);
            }
        }

        packages
    }

    /// Materialize a package returned by the solver-specific repository path.
    pub fn hydrate_package(&self, package: &Arc<Package>) -> Package {
        self.repositories
            .iter()
            .find_map(|repository| repository.hydrate_package(package))
            .unwrap_or_else(|| package.as_ref().clone())
    }

    /// Materialize only fields needed by dry-run transaction planning.
    pub fn hydrate_package_for_transaction(&self, package: &Arc<Package>) -> Package {
        self.repositories
            .iter()
            .find_map(|repository| repository.hydrate_package_for_transaction(package))
            .unwrap_or_else(|| package.as_ref().clone())
    }

    /// Search across all repositories
    pub async fn search(&self, query: &str, mode: SearchMode) -> Vec<SearchResult> {
        let mut results = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for repo in &self.repositories {
            for result in repo.search(query, mode).await {
                if !seen.contains(&result.name) {
                    seen.insert(result.name.clone());
                    results.push(result);
                }
            }
        }

        results
    }

    /// Check if any repository has a package
    pub async fn has_package(&self, name: &str) -> bool {
        for repo in &self.repositories {
            if repo.has_package(name).await {
                return true;
            }
        }
        false
    }

    /// Create a repository manager from configuration
    pub async fn from_configs(configs: Vec<RepositoryConfig>) -> Result<Self, String> {
        let mut manager = Self::new();

        for config in configs {
            let repo: Arc<dyn Repository> = match config.repo_type {
                RepositoryType::Composer => {
                    // Composer/Packagist-compatible repository
                    let name = extract_repo_name(&config.url);
                    Arc::new(ComposerRepository::new(name, &config.url))
                }
                RepositoryType::Path => {
                    // Path repository for local packages
                    let options = extract_path_options(&config);
                    Arc::new(PathRepository::new(&config.url, options))
                }
                RepositoryType::Vcs => Arc::new(VcsRepository::new(&config.url, VcsType::Vcs)),
                RepositoryType::Git => Arc::new(VcsRepository::new(&config.url, VcsType::Git)),
                RepositoryType::GitHub => {
                    Arc::new(VcsRepository::new(&config.url, VcsType::GitHub))
                }
                RepositoryType::GitLab => {
                    Arc::new(VcsRepository::new(&config.url, VcsType::GitLab))
                }
                RepositoryType::Bitbucket => {
                    Arc::new(VcsRepository::new(&config.url, VcsType::Bitbucket))
                }
                RepositoryType::Artifact => {
                    // Artifact repository - scans directory for archive files
                    Arc::new(ArtifactRepository::new(&config.url))
                }
                RepositoryType::Package => {
                    // Inline package definitions
                    if let Some(package_data) = &config.package {
                        match PackageRepository::new(package_data) {
                            Ok(repo) => Arc::new(repo),
                            Err(e) => {
                                eprintln!("Warning: Failed to create package repository: {}", e);
                                continue;
                            }
                        }
                    } else {
                        eprintln!("Warning: Package repository missing 'package' field");
                        continue;
                    }
                }
            };

            manager.add_repository(repo);
        }

        Ok(manager)
    }

    /// Create a repository manager with default Packagist and platform repositories
    pub fn with_defaults() -> Self {
        let mut manager = Self::new();

        // Add platform repository (php, ext-*, etc.)
        manager.add_repository(Arc::new(PlatformRepository::default()));

        // Add packagist.org as default repository
        manager.add_repository(Arc::new(ComposerRepository::packagist()));

        manager
    }

    /// Add repositories from composer.json Repository definitions
    ///
    /// This method takes the Repository enum from the JSON schema and creates
    /// the appropriate repository implementations.
    pub fn add_from_json_repository(&mut self, repo: &crate::json::Repository) {
        let base_dir = std::env::current_dir().unwrap_or_default();
        self.add_from_json_repository_at(repo, base_dir);
    }

    /// Add a repository, resolving local paths relative to the project directory.
    pub fn add_from_json_repository_at(
        &mut self,
        repo: &crate::json::Repository,
        base_dir: impl AsRef<std::path::Path>,
    ) {
        use crate::json::Repository as JsonRepo;

        let result: Option<Arc<dyn Repository>> = match repo {
            JsonRepo::Composer { url, .. } => {
                let name = extract_repo_name(url);
                Some(Arc::new(ComposerRepository::new(name, url)))
            }
            JsonRepo::Path { url, options } => {
                let path_options = PathRepositoryOptions {
                    symlink: options.symlink,
                    relative: false,
                    reference: "auto".to_string(),
                    versions: std::collections::HashMap::new(),
                };
                Some(Arc::new(PathRepository::new_with_base(
                    url,
                    path_options,
                    base_dir,
                )))
            }
            JsonRepo::Package { package } => match PackageRepository::new(package) {
                Ok(repo) => Some(Arc::new(repo)),
                Err(e) => {
                    eprintln!("Warning: Failed to create package repository: {}", e);
                    None
                }
            },
            JsonRepo::Vcs { url } => Some(Arc::new(VcsRepository::new(url, VcsType::Vcs))),
            JsonRepo::Git { url } => Some(Arc::new(VcsRepository::new(url, VcsType::Git))),
            JsonRepo::GitHub { url } => Some(Arc::new(VcsRepository::new(url, VcsType::GitHub))),
            JsonRepo::GitLab { url } => Some(Arc::new(VcsRepository::new(url, VcsType::GitLab))),
            JsonRepo::Bitbucket { url } => {
                Some(Arc::new(VcsRepository::new(url, VcsType::Bitbucket)))
            }
            JsonRepo::Artifact { url } => Some(Arc::new(ArtifactRepository::new(url))),
            JsonRepo::Disabled(_) | JsonRepo::NamedDisabled { .. } => {
                // Disabled repositories are handled separately
                None
            }
        };

        if let Some(repo) = result {
            self.add_repository(repo);
        }
    }

    /// Add multiple repositories from composer.json
    pub fn add_from_json_repositories(&mut self, repos: &[crate::json::Repository]) {
        for repo in repos {
            self.add_from_json_repository(repo);
        }
    }
}

/// Extract a repository name from a URL
fn extract_repo_name(url: &str) -> String {
    // Try to extract host from URL
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            return host.to_string();
        }
    }
    // Fallback to the URL itself
    url.to_string()
}

/// Extract path repository options from config
fn extract_path_options(config: &RepositoryConfig) -> PathRepositoryOptions {
    PathRepositoryOptions {
        symlink: config.options.symlink,
        relative: config.options.relative,
        reference: config
            .options
            .reference
            .clone()
            .unwrap_or_else(|| "auto".to_string()),
        versions: config.options.versions.clone(),
    }
}

impl Default for RepositoryManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_identity_deduplicates_name_and_version_without_composite_keys() {
        let mut packages = Vec::new();
        let mut seen = FastHashSet::default();

        push_unique_package(
            &mut packages,
            &mut seen,
            Arc::new(Package::new("vendor/package", "1.0.0.0")),
        );
        push_unique_package(
            &mut packages,
            &mut seen,
            Arc::new(Package::new("vendor/package", "1.0.0.0")),
        );
        push_unique_package(
            &mut packages,
            &mut seen,
            Arc::new(Package::new("vendor/package", "2.0.0.0")),
        );
        push_unique_package(
            &mut packages,
            &mut seen,
            Arc::new(Package::new("other/package", "1.0.0.0")),
        );

        assert_eq!(packages.len(), 3);
        assert_eq!(seen.len(), 3);
    }
}
