use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde::Serialize;
use sonata_core::{
    config::Config,
    is_platform_package,
    json::{ComposerJson, ComposerLock, LockedPackage},
    package::Package,
    repository::{InstalledRepository, Repository},
};
use sonata_semver::VersionParser;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::platform::AppContext;

#[derive(usage_rs::Args, Debug)]
pub struct CheckPlatformReqsArgs {
    /// Disables checking of require-dev packages requirements
    #[usage(long)]
    pub no_dev: bool,

    /// Checks requirements only from composer.lock
    #[usage(long)]
    pub lock: bool,

    /// Output format
    #[usage(short, long, default = "text", choices("text", "json"))]
    pub format: String,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Clone, Debug)]
struct PackageMetadata {
    name: String,
    version: String,
    require: IndexMap<String, String>,
    provide: IndexMap<String, String>,
    replace: IndexMap<String, String>,
}

impl From<&LockedPackage> for PackageMetadata {
    fn from(package: &LockedPackage) -> Self {
        Self {
            name: package.name.clone(),
            version: package.version.clone(),
            require: package.require.clone(),
            provide: package.provide.clone(),
            replace: package.replace.clone(),
        }
    }
}

impl From<&Package> for PackageMetadata {
    fn from(package: &Package) -> Self {
        Self {
            name: package.name.clone(),
            version: package.pretty_version().to_string(),
            require: package
                .require
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            provide: package
                .provide
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            replace: package
                .replace
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        }
    }
}

#[derive(Clone, Debug)]
struct Requirement {
    source: String,
    kind: &'static str,
    target: String,
    constraint: String,
}

#[derive(Clone, Debug)]
enum CandidateConstraint {
    Version(String),
    Provided(String),
}

#[derive(Clone, Debug)]
struct Candidate {
    package_name: String,
    constraint: CandidateConstraint,
}

impl Candidate {
    fn displayed_version(&self) -> &str {
        match &self.constraint {
            CandidateConstraint::Version(version) | CandidateConstraint::Provided(version) => {
                version
            }
        }
    }

    fn provider(&self, requirement: &str) -> Option<String> {
        (self.package_name != requirement).then(|| format!("provided by {}", self.package_name))
    }

    fn satisfies(&self, requirement: &str, parser: &VersionParser) -> Result<bool> {
        let required = parser
            .parse_constraints_cached(requirement)
            .with_context(|| format!("Invalid platform constraint {requirement:?}"))?;

        match &self.constraint {
            CandidateConstraint::Version(version) => Ok(required.satisfies(version)),
            CandidateConstraint::Provided(constraint) => required
                .intersects(constraint)
                .with_context(|| format!("Invalid provider constraint {constraint:?}")),
        }
    }
}

#[derive(Debug, Serialize)]
struct FailedRequirement {
    source: String,
    #[serde(rename = "type")]
    kind: String,
    target: String,
    constraint: String,
}

impl From<&Requirement> for FailedRequirement {
    fn from(requirement: &Requirement) -> Self {
        Self {
            source: requirement.source.clone(),
            kind: requirement.kind.to_string(),
            target: requirement.target.clone(),
            constraint: requirement.constraint.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct PlatformResult {
    name: String,
    version: String,
    status: &'static str,
    failed_requirement: Option<FailedRequirement>,
    provider: Option<String>,
}

pub async fn execute(args: CheckPlatformReqsArgs, context: &AppContext) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let composer_path = working_dir.join("composer.json");
    let composer_content = std::fs::read_to_string(&composer_path)
        .with_context(|| format!("No composer.json found in {}", working_dir.display()))?;
    let composer: ComposerJson = serde_json::from_str(&composer_content)
        .with_context(|| format!("Failed to parse {}", composer_path.display()))?;

    let (packages, source_description) =
        load_packages(&working_dir, args.lock, args.no_dev).await?;
    eprintln!(
        "Checking {}platform requirements {}",
        if args.no_dev { "non-dev " } else { "" },
        source_description
    );

    let requirements = collect_requirements(&composer, &packages, args.no_dev);
    let config = Config::build(Some(&working_dir), true)?;
    let platform_packages = context.packages(&config)?;
    let candidates = collect_candidates(&composer, &packages, platform_packages);
    let (results, exit_code) = evaluate(requirements, candidates)?;

    if args.format == "json" {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        print_text(&results);
    }

    Ok(exit_code)
}

async fn load_packages(
    working_dir: &Path,
    lock_only: bool,
    no_dev: bool,
) -> Result<(Vec<PackageMetadata>, &'static str)> {
    let lock_path = working_dir.join("composer.lock");

    if !lock_only {
        let config = Config::build(Some(working_dir), true)?;
        let vendor_dir = working_dir.join(&config.vendor_dir);
        let repository = InstalledRepository::new(&vendor_dir);
        repository.load().await.map_err(anyhow::Error::msg)?;
        let installed = repository.get_packages().await;

        if !installed.is_empty() {
            let dev_names = if no_dev {
                installed_dev_package_names(&vendor_dir, &lock_path)
            } else {
                HashSet::new()
            };
            let packages = installed
                .iter()
                .filter(|package| !dev_names.contains(package.name()))
                .map(|package| PackageMetadata::from(package.as_ref()))
                .collect();
            return Ok((packages, "for packages in the vendor dir"));
        }

        eprintln!("No vendor dir present, falling back to composer.lock");
    }

    let lock = read_lock(&lock_path)?;
    let mut packages: Vec<_> = lock.packages.iter().map(PackageMetadata::from).collect();
    if !no_dev {
        packages.extend(lock.packages_dev.iter().map(PackageMetadata::from));
    }

    Ok((packages, "using the lock file"))
}

fn read_lock(path: &Path) -> Result<ComposerLock> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("No composer.lock found at {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))
}

fn installed_dev_package_names(vendor_dir: &Path, lock_path: &Path) -> HashSet<String> {
    let installed_path = vendor_dir.join("composer/installed.json");
    if let Ok(content) = std::fs::read_to_string(installed_path) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(names) = value
                .get("dev-package-names")
                .and_then(|value| value.as_array())
            {
                return names
                    .iter()
                    .filter_map(|name| name.as_str().map(str::to_lowercase))
                    .collect();
            }
        }
    }

    read_lock(lock_path)
        .map(|lock| {
            lock.packages_dev
                .into_iter()
                .map(|package| package.name.to_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

fn collect_requirements(
    composer: &ComposerJson,
    packages: &[PackageMetadata],
    no_dev: bool,
) -> BTreeMap<String, Vec<Requirement>> {
    let mut requirements = BTreeMap::new();
    add_requirements(&mut requirements, "__root__", "requires", &composer.require);
    if !no_dev {
        add_requirements(
            &mut requirements,
            "__root__",
            "requires (for development)",
            &composer.require_dev,
        );
    }

    for package in packages {
        add_requirements(
            &mut requirements,
            &package.name,
            "requires",
            &package.require,
        );
    }

    requirements.retain(|name, _| is_platform_package(name));
    requirements
}

fn add_requirements(
    requirements: &mut BTreeMap<String, Vec<Requirement>>,
    source: &str,
    kind: &'static str,
    package_requirements: &IndexMap<String, String>,
) {
    for (target, constraint) in package_requirements {
        let target = target.to_lowercase();
        requirements
            .entry(target.clone())
            .or_default()
            .push(Requirement {
                source: source.to_string(),
                kind,
                target,
                constraint: constraint.clone(),
            });
    }
}

fn collect_candidates(
    composer: &ComposerJson,
    packages: &[PackageMetadata],
    platform_packages: Vec<sonata_core::Package>,
) -> BTreeMap<String, Vec<Candidate>> {
    let mut candidates: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();

    for package in platform_packages {
        let version = package.pretty_version().to_string();
        candidates
            .entry(package.name.clone())
            .or_default()
            .push(Candidate {
                package_name: package.name,
                constraint: CandidateConstraint::Version(version),
            });
    }

    add_provider_candidates(
        &mut candidates,
        "__root__",
        composer.version.as_deref().unwrap_or("dev-main"),
        &composer.provide,
    );
    add_provider_candidates(
        &mut candidates,
        "__root__",
        composer.version.as_deref().unwrap_or("dev-main"),
        &composer.replace,
    );

    for package in packages {
        add_provider_candidates(
            &mut candidates,
            &package.name,
            &package.version,
            &package.provide,
        );
        add_provider_candidates(
            &mut candidates,
            &package.name,
            &package.version,
            &package.replace,
        );
    }

    candidates
}

fn add_provider_candidates(
    candidates: &mut BTreeMap<String, Vec<Candidate>>,
    package_name: &str,
    package_version: &str,
    provided: &IndexMap<String, String>,
) {
    for (target, constraint) in provided {
        let constraint = if constraint == "self.version" {
            format!("={package_version}")
        } else {
            constraint.clone()
        };
        candidates
            .entry(target.to_lowercase())
            .or_default()
            .push(Candidate {
                package_name: package_name.to_string(),
                constraint: CandidateConstraint::Provided(constraint),
            });
    }
}

fn evaluate(
    requirements: BTreeMap<String, Vec<Requirement>>,
    candidates: BTreeMap<String, Vec<Candidate>>,
) -> Result<(Vec<PlatformResult>, i32)> {
    let parser = VersionParser::new();
    let mut results = Vec::new();
    let mut exit_code = 0;

    for (name, links) in requirements {
        let Some(package_candidates) = candidates.get(&name) else {
            results.push(PlatformResult {
                name,
                version: "n/a".to_string(),
                status: "missing",
                failed_requirement: links.first().map(FailedRequirement::from),
                provider: None,
            });
            exit_code = exit_code.max(2);
            continue;
        };

        let mut failed_candidates = Vec::new();
        let mut matched = false;
        for candidate in package_candidates {
            let mut failed_link = None;
            for link in &links {
                if !candidate.satisfies(&link.constraint, &parser)? {
                    failed_link = Some(link);
                    break;
                }
            }

            if let Some(link) = failed_link {
                failed_candidates.push(PlatformResult {
                    name: name.clone(),
                    version: candidate.displayed_version().to_string(),
                    status: "failed",
                    failed_requirement: Some(FailedRequirement::from(link)),
                    provider: candidate.provider(&name),
                });
            } else {
                results.push(PlatformResult {
                    name: name.clone(),
                    version: candidate.displayed_version().to_string(),
                    status: "success",
                    failed_requirement: None,
                    provider: candidate.provider(&name),
                });
                matched = true;
                break;
            }
        }

        if !matched {
            results.extend(failed_candidates);
            exit_code = exit_code.max(1);
        }
    }

    Ok((results, exit_code))
}

fn print_text(results: &[PlatformResult]) {
    let name_width = results
        .iter()
        .map(|result| result.name.len())
        .max()
        .unwrap_or(0);
    let version_width = results
        .iter()
        .map(|result| result.version.len())
        .max()
        .unwrap_or(0);

    for result in results {
        let failure = result
            .failed_requirement
            .as_ref()
            .map(|requirement| {
                format!(
                    "{} {} {} ({})",
                    requirement.source,
                    requirement.kind,
                    requirement.target,
                    requirement.constraint
                )
            })
            .unwrap_or_default();
        let provider = result
            .provider
            .as_ref()
            .map(|provider| format!(" {provider}"))
            .unwrap_or_default();
        println!(
            "{:<name_width$} {:<version_width$} {:<48} {}{}",
            result.name, result.version, failure, result.status, provider
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requirement(constraint: &str) -> Requirement {
        Requirement {
            source: "__root__".to_string(),
            kind: "requires",
            target: "ext-example".to_string(),
            constraint: constraint.to_string(),
        }
    }

    #[test]
    fn reports_successful_version_candidate() {
        let requirements = BTreeMap::from([("ext-example".to_string(), vec![requirement("^8.5")])]);
        let candidates = BTreeMap::from([(
            "ext-example".to_string(),
            vec![Candidate {
                package_name: "ext-example".to_string(),
                constraint: CandidateConstraint::Version("8.5.9".to_string()),
            }],
        )]);

        let (results, exit_code) = evaluate(requirements, candidates).unwrap();
        assert_eq!(exit_code, 0);
        assert_eq!(results[0].status, "success");
    }

    #[test]
    fn distinguishes_failed_and_missing_requirements() {
        let requirements = BTreeMap::from([
            ("ext-example".to_string(), vec![requirement("^7.0")]),
            (
                "ext-missing".to_string(),
                vec![Requirement {
                    target: "ext-missing".to_string(),
                    ..requirement("*")
                }],
            ),
        ]);
        let candidates = BTreeMap::from([(
            "ext-example".to_string(),
            vec![Candidate {
                package_name: "ext-example".to_string(),
                constraint: CandidateConstraint::Version("8.5.9".to_string()),
            }],
        )]);

        let (results, exit_code) = evaluate(requirements, candidates).unwrap();
        assert_eq!(exit_code, 2);
        assert!(results.iter().any(|result| result.status == "failed"));
        assert!(results.iter().any(|result| result.status == "missing"));
    }

    #[test]
    fn accepts_intersecting_provider_constraint() {
        let requirements = BTreeMap::from([("ext-example".to_string(), vec![requirement("^2.0")])]);
        let candidates = BTreeMap::from([(
            "ext-example".to_string(),
            vec![Candidate {
                package_name: "vendor/polyfill".to_string(),
                constraint: CandidateConstraint::Provided("^2.1".to_string()),
            }],
        )]);

        let (results, exit_code) = evaluate(requirements, candidates).unwrap();
        assert_eq!(exit_code, 0);
        assert_eq!(
            results[0].provider.as_deref(),
            Some("provided by vendor/polyfill")
        );
    }
}
