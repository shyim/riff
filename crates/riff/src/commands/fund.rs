use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};

use super::package_metadata::{PackageFunding, PackageMetadata, ProjectPackageMetadata};

type FundingTree = BTreeMap<String, BTreeMap<String, Vec<String>>>;

#[derive(Debug, usage_rs::Args)]
pub struct FundArgs {
    /// Format of the output: text or json
    #[usage(short = 'f', long, default = "text")]
    pub format: String,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub async fn execute(args: FundArgs) -> Result<i32> {
    if args.format != "text" && args.format != "json" {
        riff_core::errln!(
            "Unsupported format \"{}\". See help for supported formats: text, json",
            args.format
        );
        return Ok(1);
    }

    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let metadata = ProjectPackageMetadata::load(&working_dir).await?;
    let fundings = collect_fundings(&metadata);

    if args.format == "json" {
        riff_core::outln!("{}", serde_json::to_string_pretty(&fundings)?);
    } else if fundings.is_empty() {
        riff_core::outln!(
            "No funding links were found in your package dependencies. This doesn't mean they don't need your support!"
        );
    } else {
        render_text(&fundings);
    }

    Ok(0)
}

fn collect_fundings(metadata: &ProjectPackageMetadata) -> FundingTree {
    let remote_default_branches = metadata
        .remote
        .iter()
        .filter(|package| package.default_branch && !package.funding.is_empty())
        .fold(BTreeMap::new(), |mut packages, package| {
            packages
                .entry(package.name.to_ascii_lowercase())
                .or_insert(package);
            packages
        });

    let mut tree = FundingTree::new();
    for installed in &metadata.installed {
        let package = remote_default_branches
            .get(&installed.name.to_ascii_lowercase())
            .copied()
            .unwrap_or(installed);
        insert_package_funding(&mut tree, package);
    }
    for links in tree.values_mut() {
        for packages in links.values_mut() {
            packages.sort();
            packages.dedup();
        }
    }
    tree
}

fn insert_package_funding(tree: &mut FundingTree, package: &PackageMetadata) {
    let Some((vendor, package_name)) = package.name.split_once('/') else {
        return;
    };
    for funding in &package.funding {
        let Some(url) = funding.url.as_deref().filter(|url| !url.is_empty()) else {
            continue;
        };
        let url = normalize_funding_url(funding, url);
        tree.entry(vendor.to_string())
            .or_default()
            .entry(url)
            .or_default()
            .push(package_name.to_string());
    }
}

fn normalize_funding_url(funding: &PackageFunding, url: &str) -> String {
    if funding.funding_type.as_deref() == Some("github") {
        if let Some(account) = url.strip_prefix("https://github.com/") {
            if !account.is_empty() && !account.contains('/') {
                return format!("https://github.com/sponsors/{account}");
            }
        }
    }
    url.to_string()
}

fn render_text(fundings: &FundingTree) {
    riff_core::outln!(
        "The following packages were found in your dependencies which publish funding information:"
    );
    let mut previous_packages = None;
    for (vendor, links) in fundings {
        riff_core::outln!();
        riff_core::outln!("{vendor}");
        for (url, packages) in links {
            let packages = format!("  {}", packages.join(", "));
            if previous_packages.as_deref() != Some(packages.as_str()) {
                riff_core::outln!("{packages}");
                previous_packages = Some(packages);
            }
            riff_core::outln!("    {url}");
        }
    }
    riff_core::outln!();
    riff_core::outln!(
        "Please consider following these links and sponsoring the work of package authors!"
    );
    riff_core::outln!("Thank you!");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_profile_links_are_normalized_but_nested_links_are_preserved() {
        let github = PackageFunding {
            funding_type: Some("github".to_string()),
            url: None,
        };
        assert_eq!(
            normalize_funding_url(&github, "https://github.com/composer"),
            "https://github.com/sponsors/composer"
        );
        assert_eq!(
            normalize_funding_url(&github, "https://github.com/org/project"),
            "https://github.com/org/project"
        );
    }
}
