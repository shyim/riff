//! Composer-compatible repository package filtering and canonical priority.

use std::sync::Arc;

use async_trait::async_trait;

use super::traits::{LoadResult, ProviderInfo, Repository, SearchMode, SearchResult};
use crate::filter_list::{FilterEntriesByList, PackageVersions};
use crate::json::SecurityAdvisory;
use crate::package::{package_name_matches, Package};

pub struct FilterRepository {
    repository: Arc<dyn Repository>,
    canonical: bool,
    only: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
}

impl FilterRepository {
    pub fn new(
        repository: Arc<dyn Repository>,
        canonical: bool,
        only: Vec<String>,
        exclude: Vec<String>,
    ) -> Self {
        Self::try_new(
            repository,
            canonical,
            (!only.is_empty()).then_some(only),
            (!exclude.is_empty()).then_some(exclude),
        )
        .expect("non-empty only and exclude filters cannot be combined")
    }

    /// Create a repository while retaining whether each filter was explicitly
    /// configured. Composer treats an absent `only` filter as allowing every
    /// package, while `only: []` allows none.
    pub fn try_new(
        repository: Arc<dyn Repository>,
        canonical: bool,
        only: Option<Vec<String>>,
        exclude: Option<Vec<String>>,
    ) -> Result<Self, String> {
        if only.is_some() && exclude.is_some() {
            return Err("only and exclude cannot both be specified for a repository".to_string());
        }

        Ok(Self {
            repository,
            canonical,
            only,
            exclude,
        })
    }

    fn allowed(&self, package: &str) -> bool {
        if let Some(only) = &self.only {
            return only
                .iter()
                .any(|pattern| package_name_matches(pattern, package));
        }
        self.exclude.as_ref().is_none_or(|exclude| {
            !exclude
                .iter()
                .any(|pattern| package_name_matches(pattern, package))
        })
    }
}

#[async_trait]
impl Repository for FilterRepository {
    fn name(&self) -> &str {
        self.repository.name()
    }

    fn canonical(&self) -> bool {
        self.canonical
    }

    async fn has_package(&self, name: &str) -> bool {
        self.allowed(name) && self.repository.has_package(name).await
    }

    async fn find_packages(&self, name: &str) -> Vec<Arc<Package>> {
        if self.allowed(name) {
            self.repository.find_packages(name).await
        } else {
            Vec::new()
        }
    }

    async fn find_package(&self, name: &str, version: &str) -> Option<Arc<Package>> {
        if self.allowed(name) {
            self.repository.find_package(name, version).await
        } else {
            None
        }
    }

    async fn find_packages_with_constraint(
        &self,
        name: &str,
        constraint: &str,
    ) -> Vec<Arc<Package>> {
        if self.allowed(name) {
            self.repository
                .find_packages_with_constraint(name, constraint)
                .await
        } else {
            Vec::new()
        }
    }

    async fn find_solver_packages_with_constraint(
        &self,
        name: &str,
        constraint: &str,
    ) -> Vec<Arc<Package>> {
        if self.allowed(name) {
            self.repository
                .find_solver_packages_with_constraint(name, constraint)
                .await
        } else {
            Vec::new()
        }
    }

    fn hydrate_package(&self, package: &Arc<Package>) -> Option<Package> {
        self.allowed(&package.name)
            .then(|| self.repository.hydrate_package(package))
            .flatten()
    }

    fn hydrate_package_for_transaction(&self, package: &Arc<Package>) -> Option<Package> {
        self.allowed(&package.name)
            .then(|| self.repository.hydrate_package_for_transaction(package))
            .flatten()
    }

    async fn get_packages(&self) -> Vec<Arc<Package>> {
        self.repository
            .get_packages()
            .await
            .into_iter()
            .filter(|package| self.allowed(&package.name))
            .collect()
    }

    async fn search(&self, query: &str, mode: SearchMode) -> Vec<SearchResult> {
        self.repository
            .search(query, mode)
            .await
            .into_iter()
            .filter(|result| self.allowed(&result.name))
            .collect()
    }

    async fn get_providers(&self, package_name: &str) -> Vec<ProviderInfo> {
        self.repository
            .get_providers(package_name)
            .await
            .into_iter()
            .filter(|provider| self.allowed(&provider.name))
            .collect()
    }

    async fn load_packages_batch(&self, packages: &[(String, Option<String>)]) -> LoadResult {
        let packages: Vec<_> = packages
            .iter()
            .filter(|(name, _)| self.allowed(name))
            .cloned()
            .collect();
        let mut result = self.repository.load_packages_batch(&packages).await;
        if !self.canonical {
            result.names_found.clear();
        }
        result
    }

    async fn get_security_advisories(
        &self,
        package_versions: &PackageVersions,
        allow_partial: bool,
    ) -> Result<Vec<SecurityAdvisory>, String> {
        let package_versions = package_versions
            .iter()
            .filter(|(name, _)| self.allowed(name))
            .map(|(name, versions)| (name.clone(), versions.clone()))
            .collect();
        self.repository
            .get_security_advisories(&package_versions, allow_partial)
            .await
    }

    async fn get_filter_entries(
        &self,
        package_versions: &PackageVersions,
        configured_lists: &[String],
    ) -> Result<FilterEntriesByList, String> {
        let package_versions = package_versions
            .iter()
            .filter(|(name, _)| self.allowed(name))
            .map(|(name, versions)| (name.clone(), versions.clone()))
            .collect();
        self.repository
            .get_filter_entries(&package_versions, configured_lists)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::FilterRepository;
    use crate::package::package_name_matches;
    use crate::repository::{PackageRepository, Repository};

    fn package_repository() -> Arc<dyn Repository> {
        Arc::new(
            PackageRepository::new(&serde_json::json!([
                {
                    "name": "foo/aaa",
                    "version": "1.0.0",
                    "dist": {"type": "zip", "url": "https://example.test/foo-aaa.zip"}
                },
                {
                    "name": "foo/bbb",
                    "version": "1.0.0",
                    "dist": {"type": "zip", "url": "https://example.test/foo-bbb.zip"}
                },
                {
                    "name": "bar/xxx",
                    "version": "1.0.0",
                    "dist": {"type": "zip", "url": "https://example.test/bar-xxx.zip"}
                },
                {
                    "name": "baz/yyy",
                    "version": "1.0.0",
                    "dist": {"type": "zip", "url": "https://example.test/baz-yyy.zip"}
                }
            ]))
            .unwrap(),
        )
    }

    async fn filtered_names(only: Option<Vec<&str>>, exclude: Option<Vec<&str>>) -> Vec<String> {
        let repo = FilterRepository::try_new(
            package_repository(),
            true,
            only.map(|patterns| patterns.into_iter().map(str::to_string).collect()),
            exclude.map(|patterns| patterns.into_iter().map(str::to_string).collect()),
        )
        .unwrap();
        repo.get_packages()
            .await
            .into_iter()
            .map(|package| package.name.clone())
            .collect()
    }

    #[test]
    fn composer_package_patterns_match_case_insensitively() {
        assert!(package_name_matches("symfony/*", "Symfony/Console"));
        assert!(package_name_matches("*/console", "symfony/console"));
        assert!(!package_name_matches("symfony/*", "psr/log"));
    }

    #[tokio::test]
    async fn composer_filter_repository_matching_data_provider() {
        assert_eq!(
            filtered_names(Some(vec!["foo/*"]), None).await,
            ["foo/aaa", "foo/bbb"]
        );
        assert_eq!(
            filtered_names(Some(vec!["foo/aaa", "baz/yyy"]), None).await,
            ["foo/aaa", "baz/yyy"]
        );
        assert_eq!(
            filtered_names(None, Some(vec!["foo/*", "baz/yyy"])).await,
            ["bar/xxx"]
        );
        assert_eq!(
            filtered_names(None, Some(vec!["foo/aa", "az/yyy"])).await,
            ["foo/aaa", "foo/bbb", "bar/xxx", "baz/yyy"]
        );
        assert!(filtered_names(Some(vec!["foo/aa", "az/yyy"]), None)
            .await
            .is_empty());
        assert!(filtered_names(Some(Vec::new()), None).await.is_empty());
        assert_eq!(
            filtered_names(None, None).await,
            ["foo/aaa", "foo/bbb", "bar/xxx", "baz/yyy"]
        );
        assert_eq!(
            filtered_names(None, Some(Vec::new())).await,
            ["foo/aaa", "foo/bbb", "bar/xxx", "baz/yyy"]
        );
    }

    #[test]
    fn composer_filter_repository_rejects_both_filters() {
        assert!(FilterRepository::try_new(
            package_repository(),
            true,
            Some(Vec::new()),
            Some(Vec::new()),
        )
        .is_err());
    }

    #[tokio::test]
    async fn composer_filter_repository_is_canonical_by_default() {
        let repo = FilterRepository::try_new(package_repository(), true, None, None).unwrap();
        let result = repo
            .load_packages_batch(&[("foo/aaa".to_string(), None)])
            .await;

        assert_eq!(result.packages.len(), 1);
        assert_eq!(result.names_found, ["foo/aaa"]);
    }

    #[tokio::test]
    async fn composer_filter_repository_can_be_noncanonical() {
        let repo = FilterRepository::try_new(package_repository(), false, None, None).unwrap();
        let result = repo
            .load_packages_batch(&[("foo/aaa".to_string(), None)])
            .await;

        assert_eq!(result.packages.len(), 1);
        assert!(result.names_found.is_empty());
    }
}
