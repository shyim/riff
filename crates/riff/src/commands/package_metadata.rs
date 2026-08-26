use std::path::Path;

use anyhow::{Context, Result};
use riff_core::config::Config;
use riff_core::json::{Repository as RepositoryConfig, RiffManifest};
use riff_core::repository::{InstalledRepository, Repository as _};
use riff_core::Package;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageFunding {
    pub funding_type: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageMetadata {
    pub name: String,
    pub homepage: Option<String>,
    pub source_url: Option<String>,
    pub support_source: Option<String>,
    pub funding: Vec<PackageFunding>,
    pub default_branch: bool,
}

impl PackageMetadata {
    fn from_package(package: &Package) -> Self {
        Self {
            name: package.name.clone(),
            homepage: package.homepage.clone(),
            source_url: package.source.as_ref().map(|source| source.url.clone()),
            support_source: package
                .support
                .as_ref()
                .and_then(|support| support.source.clone()),
            funding: package
                .funding
                .iter()
                .map(|funding| PackageFunding {
                    funding_type: funding.funding_type.as_deref().map(str::to_owned),
                    url: funding.url.as_deref().map(str::to_owned),
                })
                .collect(),
            default_branch: package.default_branch.unwrap_or(false),
        }
    }

    fn from_inline(value: &serde_json::Value) -> Option<Self> {
        Some(Self {
            name: value.get("name")?.as_str()?.to_ascii_lowercase(),
            homepage: string_field(value, "homepage"),
            source_url: nested_string_field(value, "source", "url"),
            support_source: nested_string_field(value, "support", "source"),
            funding: value
                .get("funding")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|funding| {
                    let funding_type = string_field(funding, "type");
                    let url = string_field(funding, "url");
                    (funding_type.is_some() || url.is_some())
                        .then_some(PackageFunding { funding_type, url })
                })
                .collect(),
            default_branch: value
                .get("default-branch")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }
}

pub struct ProjectPackageMetadata {
    pub root: PackageMetadata,
    pub installed: Vec<PackageMetadata>,
    pub remote: Vec<PackageMetadata>,
}

impl ProjectPackageMetadata {
    pub async fn load(working_dir: &Path) -> Result<Self> {
        let manifest_path = working_dir.join("composer.json");
        let manifest: RiffManifest = serde_json::from_slice(
            &std::fs::read(&manifest_path)
                .with_context(|| format!("Failed to read {}", manifest_path.display()))?,
        )
        .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;

        let config = Config::build(Some(working_dir), true)?;
        let repository = InstalledRepository::new(config.get_vendor_dir());
        repository.load().await.map_err(anyhow::Error::msg)?;
        let mut installed = repository
            .get_packages()
            .await
            .into_iter()
            .map(|package| PackageMetadata::from_package(&package))
            .collect::<Vec<_>>();
        installed.sort_by(|left, right| left.name.cmp(&right.name));

        let mut remote = Vec::new();
        for repository in manifest.repositories.as_vec() {
            collect_inline_packages(&repository, &mut remote);
        }

        let root = PackageMetadata {
            name: manifest
                .name
                .clone()
                .unwrap_or_else(|| "__root__".to_string())
                .to_ascii_lowercase(),
            homepage: manifest.homepage,
            source_url: None,
            support_source: manifest.support.source,
            funding: manifest
                .funding
                .into_iter()
                .map(|funding| PackageFunding {
                    funding_type: Some(funding.funding_type),
                    url: Some(funding.url),
                })
                .collect(),
            default_branch: false,
        };

        Ok(Self {
            root,
            installed,
            remote,
        })
    }

    pub fn matching<'a>(
        &'a self,
        package_name: &'a str,
    ) -> impl Iterator<Item = &'a PackageMetadata> + 'a {
        std::iter::once(&self.root)
            .chain(&self.installed)
            .chain(&self.remote)
            .filter(|package| package.name.eq_ignore_ascii_case(package_name))
    }
}

fn collect_inline_packages(repository: &RepositoryConfig, packages: &mut Vec<PackageMetadata>) {
    match repository {
        RepositoryConfig::Filtered { repository, .. } => {
            collect_inline_packages(repository, packages);
        }
        RepositoryConfig::Package { package, .. } => {
            if let Some(values) = package.as_array() {
                packages.extend(values.iter().filter_map(PackageMetadata::from_inline));
            } else if let Some(package) = PackageMetadata::from_inline(package) {
                packages.push(package);
            }
        }
        _ => {}
    }
}

fn string_field(value: &serde_json::Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn nested_string_field(value: &serde_json::Value, object: &str, field: &str) -> Option<String> {
    value
        .get(object)
        .and_then(|value| string_field(value, field))
}
