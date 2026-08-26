use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use riff_core::config::Config;
use riff_core::json::{RiffLockfile, RiffManifest};
use riff_core::repository::InstalledPackage;
use riff_core::Package;
use serde::Serialize;

#[derive(Debug, usage_rs::Args)]
pub struct LicensesArgs {
    /// Output format: text, json, or summary
    #[usage(short = 'f', long, default = "text")]
    pub format: String,

    /// Exclude development dependencies
    #[usage(long)]
    pub no_dev: bool,

    /// Read dependency licenses from composer.lock instead of installed packages
    #[usage(long)]
    pub locked: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug, Serialize)]
struct JsonLicenseReport<'a> {
    name: &'a str,
    version: &'a str,
    license: Vec<String>,
    dependencies: BTreeMap<String, JsonDependency>,
}

#[derive(Debug, Serialize)]
struct JsonDependency {
    version: String,
    license: Vec<String>,
}

pub fn execute(args: LicensesArgs) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let manifest_path = working_dir.join("composer.json");
    let manifest: RiffManifest = serde_json::from_slice(
        &fs::read(&manifest_path)
            .with_context(|| format!("Failed to read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;

    let mut packages = if args.locked {
        load_locked_packages(&working_dir, args.no_dev)?
    } else {
        let config = Config::build(Some(&working_dir), true)?;
        let installed =
            load_installed_packages(&config.get_vendor_dir().join("composer/installed.json"))?;
        if args.no_dev {
            filter_required_packages(installed, manifest.require.keys().map(String::as_str))
        } else {
            installed
        }
    };
    packages.sort_by(|left, right| left.name.cmp(&right.name));

    let root_name = manifest.name.as_deref().unwrap_or("__root__");
    let root_version = manifest.version.as_deref().unwrap_or("dev-main");
    let root_licenses = manifest.licenses();
    match args.format.as_str() {
        "text" => print_text(root_name, root_version, &root_licenses, &packages),
        "json" => print_json(root_name, root_version, root_licenses, &packages)?,
        "summary" => print_summary(&packages),
        format => bail!(
            "Unsupported format \"{format}\". See help for supported formats: text, json, summary"
        ),
    }

    Ok(0)
}

fn load_locked_packages(working_dir: &Path, no_dev: bool) -> Result<Vec<Package>> {
    let lock_path = working_dir.join("composer.lock");
    if !lock_path.is_file() {
        bail!(
            "Valid composer.json and composer.lock files are required to run this command with --locked"
        );
    }
    let lock: RiffLockfile = serde_json::from_slice(&fs::read(&lock_path)?)
        .with_context(|| format!("Failed to parse {}", lock_path.display()))?;
    let mut packages: Vec<_> = lock.packages.iter().map(Package::from).collect();
    if !no_dev {
        packages.extend(lock.packages_dev.iter().map(Package::from));
    }
    Ok(packages)
}

fn load_installed_packages(path: &Path) -> Result<Vec<Package>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let package_value = if value.is_array() {
        value
    } else {
        value
            .get("packages")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()))
    };
    let installed: Vec<InstalledPackage> = serde_json::from_value(package_value)
        .with_context(|| format!("Failed to parse packages in {}", path.display()))?;
    Ok(installed.iter().map(Package::from_installed_json).collect())
}

fn filter_required_packages<'a>(
    packages: Vec<Package>,
    required: impl Iterator<Item = &'a str>,
) -> Vec<Package> {
    let mut by_capability: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, package) in packages.iter().enumerate() {
        by_capability
            .entry(package.name.to_lowercase())
            .or_default()
            .push(index);
        for capability in package.provide.keys().chain(package.replace.keys()) {
            by_capability
                .entry(capability.to_lowercase().to_string())
                .or_default()
                .push(index);
        }
    }

    let mut queue: VecDeque<_> = required.map(str::to_lowercase).collect();
    let mut selected = HashSet::new();
    while let Some(requirement) = queue.pop_front() {
        if is_platform_package(&requirement) {
            continue;
        }
        let Some(indices) = by_capability.get(&requirement) else {
            continue;
        };
        for &index in indices {
            if selected.insert(index) {
                queue.extend(
                    packages[index]
                        .require
                        .keys()
                        .map(|name| name.to_lowercase().to_string()),
                );
            }
        }
    }

    packages
        .into_iter()
        .enumerate()
        .filter_map(|(index, package)| selected.contains(&index).then_some(package))
        .collect()
}

fn is_platform_package(name: &str) -> bool {
    name == "php"
        || name == "hhvm"
        || name == "composer"
        || name.starts_with("ext-")
        || name.starts_with("lib-")
        || name.starts_with("composer-")
}

fn pretty_version(package: &Package) -> &str {
    package
        .pretty_version
        .as_deref()
        .unwrap_or(&package.version)
}

fn license_text(licenses: &[impl AsRef<str>]) -> String {
    if licenses.is_empty() {
        "none".to_owned()
    } else {
        licenses
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn print_text(root_name: &str, root_version: &str, root_licenses: &[String], packages: &[Package]) {
    riff_core::outln!("Name: {root_name}");
    riff_core::outln!("Version: {root_version}");
    riff_core::outln!("Licenses: {}", license_text(root_licenses));
    riff_core::outln!("Dependencies:");
    riff_core::outln!();
    riff_core::outln!("Name\tVersion\tLicenses");
    for package in packages {
        riff_core::outln!(
            "{}\t{}\t{}",
            package.pretty_name(),
            pretty_version(package),
            license_text(&package.license)
        );
    }
}

fn print_json(
    root_name: &str,
    root_version: &str,
    root_licenses: Vec<String>,
    packages: &[Package],
) -> Result<()> {
    let dependencies = packages
        .iter()
        .map(|package| {
            (
                package.pretty_name().to_owned(),
                JsonDependency {
                    version: pretty_version(package).to_owned(),
                    license: package.license.iter().map(ToString::to_string).collect(),
                },
            )
        })
        .collect();
    let report = JsonLicenseReport {
        name: root_name,
        version: root_version,
        license: root_licenses,
        dependencies,
    };
    riff_core::outln!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn print_summary(packages: &[Package]) {
    let mut counts = HashMap::<String, usize>::new();
    for package in packages {
        if package.license.is_empty() {
            *counts.entry("none".to_owned()).or_default() += 1;
        } else {
            for license in &package.license {
                *counts.entry(license.to_string()).or_default() += 1;
            }
        }
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by(|(left_name, left_count), (right_name, right_count)| {
        right_count.cmp(left_count).then(left_name.cmp(right_name))
    });
    riff_core::outln!("License\tNumber of dependencies");
    for (license, count) in counts {
        riff_core::outln!("{license}\t{count}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_dev_filter_follows_transitive_dependencies_and_virtual_providers() {
        let mut direct = Package::new("vendor/direct", "1.0.0");
        direct
            .require
            .insert("virtual/transitive".to_owned(), "*".into());
        let mut provider = Package::new("vendor/provider", "1.0.0");
        provider
            .provide
            .insert("virtual/transitive".to_owned(), "1.0".into());
        let dev = Package::new("vendor/dev", "1.0.0");

        let selected =
            filter_required_packages(vec![direct, provider, dev], ["vendor/direct"].into_iter());
        assert_eq!(
            selected
                .iter()
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            ["vendor/direct", "vendor/provider"]
        );
    }
}
