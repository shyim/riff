use async_trait::async_trait;
use std::sync::Arc;

use super::traits::{ProviderInfo, Repository, SearchMode, SearchResult};
use crate::package::Package;
use riff_semver::{Constraint, Operator, VersionParser};

/// Repository containing explicitly supplied platform packages.
pub struct PlatformRepository {
    packages: Vec<Arc<Package>>,
}

impl PlatformRepository {
    /// Create a repository from caller-supplied virtual packages.
    pub fn from_packages(packages: Vec<Package>) -> Self {
        Self {
            packages: packages.into_iter().map(Arc::new).collect(),
        }
    }
}

#[async_trait]
impl Repository for PlatformRepository {
    fn name(&self) -> &str {
        "platform"
    }

    async fn has_package(&self, name: &str) -> bool {
        let name_lower = name.to_lowercase();
        self.packages
            .iter()
            .any(|p| p.name.to_lowercase() == name_lower)
    }

    async fn find_packages(&self, name: &str) -> Vec<Arc<Package>> {
        let name_lower = name.to_lowercase();
        self.packages
            .iter()
            .filter(|p| p.name.to_lowercase() == name_lower)
            .cloned()
            .collect()
    }

    async fn find_package(&self, name: &str, version: &str) -> Option<Arc<Package>> {
        let name_lower = name.to_lowercase();
        self.packages
            .iter()
            .find(|p| p.name.to_lowercase() == name_lower && p.version == version)
            .cloned()
    }

    async fn find_packages_with_constraint(
        &self,
        name: &str,
        constraint: &str,
    ) -> Vec<Arc<Package>> {
        let packages = self.find_packages(name).await;

        // Handle wildcard constraints
        if constraint == "*" || constraint.is_empty() {
            return packages;
        }

        // Parse the constraint
        let parser = VersionParser::new();
        let parsed_constraint = match parser.parse_constraints(constraint) {
            Ok(c) => c,
            Err(_) => return packages, // Be permissive on parse errors
        };

        // Filter packages by constraint
        packages
            .into_iter()
            .filter(|pkg| {
                // Normalize the package version
                let normalized = parser
                    .normalize(&pkg.version)
                    .unwrap_or_else(|_| pkg.version.to_string());

                // Create a version constraint (== normalized_version)
                let version_constraint = match Constraint::new(Operator::Equal, normalized) {
                    Ok(c) => c,
                    Err(_) => return true, // Be permissive
                };

                // Check if the version matches the constraint
                parsed_constraint.matches(&version_constraint)
            })
            .collect()
    }

    async fn get_packages(&self) -> Vec<Arc<Package>> {
        self.packages.clone()
    }

    async fn search(&self, query: &str, _mode: SearchMode) -> Vec<SearchResult> {
        let query_lower = query.to_lowercase();
        self.packages
            .iter()
            .filter(|p| p.name.to_lowercase().contains(&query_lower))
            .map(|p| SearchResult {
                name: p.name.clone(),
                description: p.description.clone(),
                url: None,
                abandoned: None,
                downloads: None,
                favers: None,
            })
            .collect()
    }

    async fn get_providers(&self, _package_name: &str) -> Vec<ProviderInfo> {
        Vec::new()
    }

    async fn count(&self) -> usize {
        self.packages.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn repository_contains_only_supplied_platform_packages() {
        let repository = PlatformRepository::from_packages(vec![
            Package::new("php", "8.4.2"),
            Package::new("ext-demo", "1.0"),
        ]);

        let packages = repository.get_packages().await;
        assert_eq!(packages.len(), 2);
        assert!(packages
            .iter()
            .any(|package| package.name == "php" && package.version == "8.4.2"));
        assert!(!packages.iter().any(|package| package.name == "ext-json"));
    }
}
