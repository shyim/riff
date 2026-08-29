use std::hash::{Hash, Hasher};
use std::sync::Arc;

use foldhash::HashSet as FastHashSet;

use super::artifact::ArtifactRepository;
use super::filter::FilterRepository;
use super::package::PackageRepository;
use super::path::{PathRepository, PathRepositoryOptions};
use super::traits::{Repository, RepositoryConfig, RepositoryType, SearchMode, SearchResult};
use super::vcs::{VcsRepository, VcsType};
use super::ComposerRepository;
use super::PlatformRepository;
use crate::cache::runtime_cache_dir;
use crate::output::Output;
use crate::package::Package;
use crate::session::RiffSession;
use riff_semver::VersionParser;

/// Manages multiple repositories with priority ordering
pub struct RepositoryManager {
    /// Repositories in priority order (first = highest priority)
    repositories: Vec<Arc<dyn Repository>>,
    output: Output,
}

/// Packages returned for solving together with repository-priority context.
#[derive(Debug)]
pub struct SolverPackageLookup {
    pub packages: Vec<Arc<Package>>,
    pub blocked_by_higher_priority_repository: bool,
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
            output: Output::silent(),
        }
    }

    pub fn with_output(mut self, output: Output) -> Self {
        self.output = output;
        self
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
            let stops_lookup = !found.is_empty() && repo.canonical();
            packages.reserve(found.len());
            seen.reserve(found.len());
            for pkg in found {
                push_unique_package(&mut packages, &mut seen, pkg);
            }
            if stops_lookup {
                break;
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
            if repo.canonical() && repo.has_package(name).await {
                return None;
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
            let stops_lookup =
                repo.canonical() && (!found.is_empty() || repo.has_package(name).await);
            packages.reserve(found.len());
            seen.reserve(found.len());
            for pkg in found {
                push_unique_package(&mut packages, &mut seen, pkg);
            }
            if stops_lookup {
                break;
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
        self.find_solver_packages_with_diagnostics(name, constraint)
            .await
            .packages
    }

    /// Find solver packages and retain whether a canonical repository hid a
    /// lower-priority repository containing the same package name.
    pub async fn find_solver_packages_with_diagnostics(
        &self,
        name: &str,
        constraint: &str,
    ) -> SolverPackageLookup {
        let mut packages = Vec::new();
        let mut seen = FastHashSet::default();
        let mut blocked_by_higher_priority_repository = false;

        for (index, repo) in self.repositories.iter().enumerate() {
            let found = repo
                .find_solver_packages_with_constraint(name, constraint)
                .await;
            let stops_lookup =
                repo.canonical() && (!found.is_empty() || repo.has_package(name).await);
            packages.reserve(found.len());
            seen.reserve(found.len());
            for pkg in found {
                push_unique_package(&mut packages, &mut seen, pkg);
            }
            if stops_lookup {
                for lower_priority_repo in &self.repositories[index + 1..] {
                    if lower_priority_repo.has_package(name).await {
                        blocked_by_higher_priority_repository = true;
                        break;
                    }
                }
                break;
            }
        }

        SolverPackageLookup {
            packages,
            blocked_by_higher_priority_repository,
        }
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

    /// Get all packages across repositories in repository order.
    pub async fn get_packages(&self) -> Vec<Arc<Package>> {
        let mut packages = Vec::new();
        for repository in &self.repositories {
            packages.extend(repository.get_packages().await);
        }
        packages
    }

    /// Find direct packages plus installed packages that provide or replace a name.
    pub async fn find_packages_with_replacers_and_providers(
        &self,
        name: &str,
        constraint: Option<&str>,
    ) -> Vec<Arc<Package>> {
        let name = name.to_ascii_lowercase();
        let parsed_constraint = constraint.and_then(|constraint| {
            VersionParser::new()
                .parse_constraints_cached(constraint)
                .ok()
        });
        let mut matches = Vec::new();

        for repository in &self.repositories {
            for package in repository.get_packages().await {
                if package.name.eq_ignore_ascii_case(&name) {
                    if parsed_constraint
                        .as_ref()
                        .is_none_or(|constraint| constraint.satisfies(&package.version))
                    {
                        matches.push(package);
                    }
                    continue;
                }

                let provided = package
                    .provide
                    .iter()
                    .chain(package.replace.iter())
                    .filter(|(target, _)| target.eq_ignore_ascii_case(&name))
                    .any(|(_, provided_constraint)| {
                        let provided_constraint = if provided_constraint == "self.version" {
                            format!("={}", package.version)
                        } else {
                            provided_constraint.to_string()
                        };
                        parsed_constraint.as_ref().is_none_or(|constraint| {
                            constraint.intersects(&provided_constraint).unwrap_or(false)
                        })
                    });
                if provided {
                    matches.push(package);
                }
            }
        }

        matches
    }

    /// Count all packages across the managed repositories.
    pub async fn count(&self) -> usize {
        let mut count = 0;
        for repository in &self.repositories {
            count += repository.count().await;
        }
        count
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
                    Arc::new(ComposerRepository::with_cache(
                        name,
                        &config.url,
                        runtime_cache_dir(),
                    ))
                }
                RepositoryType::Path => {
                    // Path repository for local packages
                    let options = extract_path_options(&config);
                    Arc::new(PathRepository::new(&config.url, options))
                }
                RepositoryType::Vcs => Arc::new(manager.vcs_repository(&config.url, VcsType::Vcs)),
                RepositoryType::Git => Arc::new(manager.vcs_repository(&config.url, VcsType::Git)),
                RepositoryType::Hg => Arc::new(manager.vcs_repository(&config.url, VcsType::Hg)),
                RepositoryType::Svn => Arc::new(manager.vcs_repository(&config.url, VcsType::Svn)),
                RepositoryType::Fossil => {
                    Arc::new(manager.vcs_repository(&config.url, VcsType::Fossil))
                }
                RepositoryType::Perforce => {
                    Arc::new(manager.vcs_repository(&config.url, VcsType::Perforce))
                }
                RepositoryType::Pear => {
                    return Err(
                        "The PEAR repository has been removed from Composer 2.x".to_string()
                    );
                }
                RepositoryType::GitHub => {
                    Arc::new(manager.vcs_repository(&config.url, VcsType::GitHub))
                }
                RepositoryType::GitLab => {
                    Arc::new(manager.vcs_repository(&config.url, VcsType::GitLab))
                }
                RepositoryType::Bitbucket => {
                    Arc::new(manager.vcs_repository(&config.url, VcsType::Bitbucket))
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
                                crate::errln!(
                                    manager.output,
                                    "Warning: Failed to create package repository: {}",
                                    e
                                );
                                continue;
                            }
                        }
                    } else {
                        crate::warnln!(
                            manager.output,
                            "Warning: Package repository missing 'package' field"
                        );
                        continue;
                    }
                }
            };

            let repo: Arc<dyn Repository> = if !config.options.canonical
                || !config.options.only.is_empty()
                || !config.options.exclude.is_empty()
            {
                Arc::new(FilterRepository::new(
                    repo,
                    config.options.canonical,
                    config.options.only,
                    config.options.exclude,
                ))
            } else {
                repo
            };
            manager.add_repository(repo);
        }

        Ok(manager)
    }

    /// Create a repository manager with Packagist and supplied platform packages.
    pub fn with_defaults(platform_packages: Vec<Package>) -> Self {
        let mut manager = Self::new();

        manager.add_repository(Arc::new(PlatformRepository::from_packages(
            platform_packages,
        )));

        // Add packagist.org as default repository
        manager.add_repository(Arc::new(ComposerRepository::packagist_with_cache(
            runtime_cache_dir(),
        )));

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
        self.add_from_json_repository_at_inner(repo, base_dir.as_ref(), None);
    }

    pub(crate) fn add_from_json_repository_at_in_session(
        &mut self,
        repo: &crate::json::Repository,
        base_dir: impl AsRef<std::path::Path>,
        session: &RiffSession,
    ) {
        self.add_from_json_repository_at_inner(repo, base_dir.as_ref(), Some(session));
    }

    fn add_from_json_repository_at_inner(
        &mut self,
        repo: &crate::json::Repository,
        base_dir: &std::path::Path,
        session: Option<&RiffSession>,
    ) {
        use crate::json::Repository as JsonRepo;

        let result: Option<Arc<dyn Repository>> = match repo {
            JsonRepo::Filtered {
                repository,
                canonical,
                only,
                exclude,
            } => {
                let mut nested = RepositoryManager::new();
                nested.add_from_json_repository_at_inner(repository, base_dir, session);
                nested.repositories.pop().map(|repository| {
                    Arc::new(FilterRepository::new(
                        repository,
                        *canonical,
                        only.clone(),
                        exclude.clone(),
                    )) as Arc<dyn Repository>
                })
            }
            JsonRepo::Composer { url, filter, .. } => {
                let name = extract_repo_name(url);
                if let Some(session) = session {
                    Some(session.composer_repository(name, url, filter) as Arc<dyn Repository>)
                } else {
                    let mut repository =
                        ComposerRepository::with_cache(name, url, runtime_cache_dir());
                    repository.set_user_filter_config(filter.clone());
                    Some(Arc::new(repository))
                }
            }
            JsonRepo::Path { url, options } => {
                let path_options = PathRepositoryOptions {
                    symlink: options.symlink,
                    relative: options
                        .relative
                        .unwrap_or_else(|| !std::path::Path::new(url).is_absolute()),
                    reference: options
                        .reference
                        .clone()
                        .unwrap_or_else(|| "auto".to_string()),
                    versions: options.versions.clone().into_iter().collect(),
                };
                Some(Arc::new(PathRepository::new_with_base(
                    url,
                    path_options,
                    base_dir,
                )))
            }
            JsonRepo::Package { package, .. } => match PackageRepository::new(package) {
                Ok(repo) => Some(Arc::new(repo)),
                Err(e) => {
                    crate::warnln!(
                        self.output,
                        "Warning: Failed to create package repository: {}",
                        e
                    );
                    None
                }
            },
            JsonRepo::Vcs { url } => Some(Arc::new(self.vcs_repository(url, VcsType::Vcs))),
            JsonRepo::Git { url } => Some(Arc::new(self.vcs_repository(url, VcsType::Git))),
            JsonRepo::Hg { url } => Some(Arc::new(self.vcs_repository(url, VcsType::Hg))),
            JsonRepo::Svn { url } => Some(Arc::new(self.vcs_repository(url, VcsType::Svn))),
            JsonRepo::Fossil { url } => Some(Arc::new(self.vcs_repository(url, VcsType::Fossil))),
            JsonRepo::Perforce { url } => {
                Some(Arc::new(self.vcs_repository(url, VcsType::Perforce)))
            }
            JsonRepo::Pear { .. } => {
                crate::warnln!(
                    self.output,
                    "Warning: The PEAR repository has been removed from Composer 2.x"
                );
                None
            }
            JsonRepo::GitHub { url } => Some(Arc::new(self.vcs_repository(url, VcsType::GitHub))),
            JsonRepo::GitLab { url } => Some(Arc::new(self.vcs_repository(url, VcsType::GitLab))),
            JsonRepo::Bitbucket { url } => {
                Some(Arc::new(self.vcs_repository(url, VcsType::Bitbucket)))
            }
            JsonRepo::Artifact { url } => {
                Some(Arc::new(ArtifactRepository::new_with_base(url, base_dir)))
            }
            JsonRepo::Disabled(_) | JsonRepo::NamedDisabled { .. } => {
                // Disabled repositories are handled separately
                None
            }
        };

        if let Some(repo) = result {
            self.add_repository(repo);
        }
    }

    fn vcs_repository(&self, url: impl Into<String>, vcs_type: VcsType) -> VcsRepository {
        VcsRepository::new(url, vcs_type).with_output(self.output.clone())
    }

    /// Add multiple repositories from composer.json
    pub fn add_from_json_repositories(&mut self, repos: &[crate::json::Repository]) {
        for repo in repos {
            self.add_from_json_repository(repo);
        }
    }
}

/// Generate a stable repository name from its URL or positional index, adding
/// Composer-compatible numeric suffixes when that name is already occupied.
pub fn generate_repository_name<'a>(
    index: impl ToString,
    url: Option<&str>,
    existing_names: impl IntoIterator<Item = &'a str>,
) -> String {
    let base = url
        .and_then(|url| url::Url::parse(url).ok())
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| index.to_string());
    let existing: std::collections::HashSet<_> = existing_names.into_iter().collect();
    if !existing.contains(base.as_str()) {
        return base;
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{base}{suffix}");
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
        suffix += 1;
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

    fn repository_config(repo_type: RepositoryType, url: impl Into<String>) -> RepositoryConfig {
        RepositoryConfig {
            repo_type,
            url: url.into(),
            options: Default::default(),
            package: None,
        }
    }

    fn package_repository(version: &str) -> Arc<dyn Repository> {
        Arc::new(
            PackageRepository::new(&serde_json::json!({
                "name": "vendor/package",
                "version": version,
                "dist": {"type": "zip", "url": "https://example.test/package.zip"}
            }))
            .unwrap(),
        )
    }

    fn array_repository(packages: &[(&str, &str)]) -> Arc<dyn Repository> {
        let packages: Vec<_> = packages
            .iter()
            .map(|(name, version)| {
                serde_json::json!({
                    "name": name,
                    "version": version,
                    "dist": {
                        "type": "zip",
                        "url": format!("https://example.test/{name}-{version}.zip")
                    }
                })
            })
            .collect();
        Arc::new(PackageRepository::new(&serde_json::Value::Array(packages)).unwrap())
    }

    fn noncanonical(repository: Arc<dyn Repository>) -> Arc<dyn Repository> {
        Arc::new(FilterRepository::new(
            repository,
            false,
            Vec::new(),
            Vec::new(),
        ))
    }

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

    #[test]
    fn composer_repository_manager_prepends_repositories() {
        let first = package_repository("1.0.0");
        let second = package_repository("2.0.0");
        let mut manager = RepositoryManager::new();

        manager.add_repository(Arc::clone(&first));
        manager.insert_repository(0, Arc::clone(&second));

        assert_eq!(manager.repositories().len(), 2);
        assert!(Arc::ptr_eq(&manager.repositories()[0], &second));
        assert!(Arc::ptr_eq(&manager.repositories()[1], &first));
    }

    #[tokio::test]
    async fn composer_repository_manager_creates_supported_repository_types() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut package = repository_config(RepositoryType::Package, "");
        package.package = Some(serde_json::json!([]));
        let cases = vec![
            repository_config(RepositoryType::Composer, "http://example.org"),
            repository_config(RepositoryType::Vcs, "http://github.com/foo/bar"),
            repository_config(RepositoryType::Git, "http://github.com/foo/bar"),
            repository_config(RepositoryType::Git, "git@example.org:foo/bar.git"),
            repository_config(RepositoryType::Hg, "https://example.org/foo/bar"),
            repository_config(RepositoryType::Svn, "svn://example.org/foo/bar"),
            repository_config(RepositoryType::Fossil, "https://example.org/foo/bar"),
            repository_config(RepositoryType::Perforce, "perforce.example.org:1666"),
            repository_config(RepositoryType::GitHub, "https://github.com/foo/bar"),
            repository_config(RepositoryType::GitLab, "https://gitlab.com/foo/bar"),
            repository_config(RepositoryType::Bitbucket, "https://bitbucket.org/foo/bar"),
            repository_config(RepositoryType::Path, temp.path().to_string_lossy()),
            package,
            repository_config(
                RepositoryType::Artifact,
                temp.path().join("zips").to_string_lossy(),
            ),
        ];

        for config in cases {
            let manager = RepositoryManager::from_configs(vec![
                repository_config(RepositoryType::Composer, "http://example.org"),
                config,
            ])
            .await
            .unwrap();

            assert_eq!(manager.repositories().len(), 2);
        }
    }

    // Ported from Composer\Test\Repository\RepositoryFactoryTest::
    // testGenerateRepositoryName.
    #[test]
    fn composer_repository_factory_generates_unique_repository_names() {
        assert_eq!(generate_repository_name(0, None, []), "0");
        assert_eq!(generate_repository_name(0, None, ["0"]), "02");
        assert_eq!(
            generate_repository_name(0, Some("https://example.org"), []),
            "example.org"
        );
        assert_eq!(
            generate_repository_name(0, Some("https://example.org/repository"), ["example.org"]),
            "example.org2"
        );
        assert_eq!(
            generate_repository_name("example.org", Some("https://example.org/repository"), []),
            "example.org"
        );
    }

    #[tokio::test]
    async fn composer_repository_manager_rejects_invalid_repository_types() {
        let pear = RepositoryManager::from_configs(vec![repository_config(
            RepositoryType::Pear,
            "http://pear.example.org/foo",
        )])
        .await;
        assert!(matches!(pear, Err(error) if error.contains("PEAR repository has been removed")));

        let invalid = serde_json::from_value::<crate::json::Repository>(serde_json::json!({
            "type": "invalid"
        }));
        assert!(matches!(
            invalid,
            Err(error) if error.to_string().contains("unsupported repository type invalid")
        ));
    }

    #[tokio::test]
    async fn composer_repository_manager_wraps_filtered_repositories() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("composer.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "bar/baz",
                "version": "1.0.0"
            }))
            .unwrap(),
        )
        .unwrap();

        let mut filtered = repository_config(
            RepositoryType::Path,
            temp.path().to_string_lossy().to_string(),
        );
        filtered.options.only = vec!["foo/bar".to_string()];
        let filtered_manager = RepositoryManager::from_configs(vec![filtered])
            .await
            .unwrap();

        assert_eq!(filtered_manager.repositories().len(), 1);
        assert!(filtered_manager.repositories()[0]
            .name()
            .starts_with("path repo ("));
        assert!(filtered_manager.get_packages().await.is_empty());

        let unfiltered_manager = RepositoryManager::from_configs(vec![repository_config(
            RepositoryType::Path,
            temp.path().to_string_lossy().to_string(),
        )])
        .await
        .unwrap();
        let packages = unfiltered_manager.get_packages().await;
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "bar/baz");
    }

    #[tokio::test]
    async fn canonical_repositories_stop_lower_priority_lookups() {
        let mut manager = RepositoryManager::new();
        manager.add_repository(package_repository("1.0.0"));
        manager.add_repository(package_repository("2.0.0"));

        let packages = manager.find_packages("vendor/package").await;
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].pretty_version(), "1.0.0");
    }

    #[tokio::test]
    async fn canonical_repository_blocks_lower_only_matching_version() {
        let mut manager = RepositoryManager::new();
        manager.add_repository(package_repository("1.0.0"));
        manager.add_repository(package_repository("2.0.0"));

        assert!(manager
            .find_package("vendor/package", "2.0.0")
            .await
            .is_none());
        let packages = manager
            .find_packages_with_constraint("vendor/package", "^2.0")
            .await;
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].pretty_version(), "1.0.0");
        let solver_packages = manager
            .find_solver_packages_with_constraint("vendor/package", "^2.0")
            .await;
        assert_eq!(solver_packages.len(), 1);
        assert_eq!(solver_packages[0].pretty_version(), "1.0.0");

        let lookup = manager
            .find_solver_packages_with_diagnostics("vendor/package", "^2.0")
            .await;
        assert!(lookup.blocked_by_higher_priority_repository);
        assert_eq!(lookup.packages.len(), 1);
    }

    #[tokio::test]
    async fn noncanonical_and_only_filters_allow_lower_priority_packages() {
        let mut manager = RepositoryManager::new();
        manager.add_repository(Arc::new(FilterRepository::new(
            package_repository("1.0.0"),
            false,
            vec!["other/*".to_string()],
            Vec::new(),
        )));
        manager.add_repository(package_repository("2.0.0"));

        let packages = manager.find_packages("vendor/package").await;
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].pretty_version(), "2.0.0");
    }

    #[tokio::test]
    async fn composer_composite_repository_has_packages_across_children() {
        let mut manager = RepositoryManager::new();
        manager.add_repository(array_repository(&[("foo/package", "1")]));
        manager.add_repository(array_repository(&[("bar/package", "1")]));

        assert!(manager.has_package("foo/package").await);
        assert!(manager.has_package("bar/package").await);
        assert!(!manager.has_package("missing/package").await);
    }

    #[tokio::test]
    async fn composer_composite_repository_finds_specific_package() {
        let mut manager = RepositoryManager::new();
        manager.add_repository(array_repository(&[("foo/package", "1")]));
        manager.add_repository(array_repository(&[("bar/package", "1")]));

        let foo = manager.find_package("foo/package", "1").await.unwrap();
        assert_eq!(foo.name, "foo/package");
        assert_eq!(foo.pretty_version.as_deref(), Some("1"));
        let bar = manager.find_package("bar/package", "1").await.unwrap();
        assert_eq!(bar.name, "bar/package");
        assert!(manager.find_package("foo/package", "2").await.is_none());
    }

    #[tokio::test]
    async fn composer_composite_repository_finds_versions_across_children() {
        let mut manager = RepositoryManager::new();
        manager.add_repository(noncanonical(array_repository(&[
            ("foo/package", "1"),
            ("foo/package", "2"),
            ("bat/package", "1"),
        ])));
        manager.add_repository(noncanonical(array_repository(&[
            ("bar/package", "1"),
            ("bar/package", "2"),
            ("foo/package", "3"),
        ])));

        assert_eq!(manager.find_packages("bat/package").await.len(), 1);
        assert_eq!(manager.find_packages("bar/package").await.len(), 2);
        let foo = manager.find_packages("foo/package").await;
        assert_eq!(foo.len(), 3);
        assert_eq!(foo[0].pretty_version(), "1");
        assert_eq!(foo[1].pretty_version(), "2");
        assert_eq!(foo[2].pretty_version(), "3");
    }

    #[tokio::test]
    async fn composer_composite_repository_gets_packages_in_child_order() {
        let mut manager = RepositoryManager::new();
        manager.add_repository(array_repository(&[("foo/package", "1")]));
        manager.add_repository(array_repository(&[("bar/package", "1")]));

        let packages = manager.get_packages().await;
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "foo/package");
        assert_eq!(packages[1].name, "bar/package");
    }

    #[tokio::test]
    async fn composer_composite_repository_adds_repository() {
        let mut manager = RepositoryManager::new();
        manager.add_repository(array_repository(&[("foo/package", "1")]));
        assert_eq!(manager.count().await, 1);

        manager.add_repository(array_repository(&[
            ("bar/package", "1"),
            ("bar/package", "2"),
            ("bar/package", "3"),
        ]));
        assert_eq!(manager.count().await, 4);
    }

    #[tokio::test]
    async fn composer_composite_repository_counts_all_packages() {
        let mut manager = RepositoryManager::new();
        manager.add_repository(array_repository(&[("foo/package", "1")]));
        manager.add_repository(array_repository(&[("bar/package", "1")]));

        assert_eq!(manager.count().await, 2);
    }

    #[tokio::test]
    async fn composer_composite_repository_empty_methods_return_empty() {
        let manager = RepositoryManager::new();

        assert!(manager.find_packages("foo/package").await.is_empty());
        assert!(manager.search("foo", SearchMode::Name).await.is_empty());
        assert!(manager.get_packages().await.is_empty());
        assert_eq!(manager.count().await, 0);
    }

    #[tokio::test]
    async fn composer_installed_repository_finds_replacers_and_providers() {
        let first: Arc<dyn Repository> = Arc::new(
            PackageRepository::new(&serde_json::json!([
                {
                    "name": "foo/package",
                    "version": "1",
                    "replace": {"provided/package": "*"},
                    "dist": {"type": "zip", "url": "https://example.test/foo-1.zip"}
                },
                {
                    "name": "foo/package",
                    "version": "2",
                    "dist": {"type": "zip", "url": "https://example.test/foo-2.zip"}
                }
            ]))
            .unwrap(),
        );
        let second: Arc<dyn Repository> = Arc::new(
            PackageRepository::new(&serde_json::json!([
                {
                    "name": "bar/package",
                    "version": "1",
                    "dist": {"type": "zip", "url": "https://example.test/bar-1.zip"}
                },
                {
                    "name": "bar/package",
                    "version": "2",
                    "provide": {"provided/package": "*"},
                    "dist": {"type": "zip", "url": "https://example.test/bar-2.zip"}
                }
            ]))
            .unwrap(),
        );
        let mut manager = RepositoryManager::new();
        manager.add_repository(first);
        manager.add_repository(second);

        let foo = manager
            .find_packages_with_replacers_and_providers("foo/package", Some("2"))
            .await;
        assert_eq!(foo.len(), 1);
        assert_eq!(foo[0].name, "foo/package");
        assert_eq!(foo[0].pretty_version(), "2");

        let bar = manager
            .find_packages_with_replacers_and_providers("bar/package", Some("1"))
            .await;
        assert_eq!(bar.len(), 1);
        assert_eq!(bar[0].name, "bar/package");
        assert_eq!(bar[0].pretty_version(), "1");

        let provided = manager
            .find_packages_with_replacers_and_providers("provided/package", None)
            .await;
        let provided: Vec<_> = provided
            .iter()
            .map(|package| (package.name.as_str(), package.pretty_version()))
            .collect();
        assert_eq!(provided, [("foo/package", "1"), ("bar/package", "2")]);
    }

    #[test]
    fn json_repository_filter_options_round_trip() {
        let configured: crate::json::Repository = serde_json::from_value(serde_json::json!({
            "type": "composer",
            "url": "https://example.test",
            "canonical": false,
            "only": ["vendor/*"]
        }))
        .unwrap();
        let crate::json::Repository::Filtered {
            repository: inner,
            canonical,
            only,
            exclude,
        } = &configured
        else {
            panic!("repository filters were not retained");
        };
        assert!(matches!(
            inner.as_ref(),
            crate::json::Repository::Composer { .. }
        ));
        assert!(!canonical);
        assert_eq!(only, &["vendor/*"]);
        assert!(exclude.is_empty());
        assert_eq!(
            serde_json::to_value(inner.as_ref()).unwrap()["type"],
            "composer"
        );
        assert_eq!(
            serde_json::to_value(&configured).unwrap()["canonical"],
            false
        );
    }
}
