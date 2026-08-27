use anyhow::{Context, Result};
use colored::Colorize;
use foldhash::{HashMap, HashSet};
use riff_core::advisory::{
    AdvisoryPolicy, AuditAdvisory, AuditAdvisorySource, AuditBehavior, AuditFilterEntry,
    AuditReport,
};
use riff_core::config::Config;
use riff_core::installer::PackagePolicy;
use riff_core::json::{LockedPackage, RiffLockfile, RiffManifest, SecurityAdvisory};
use riff_core::policy_config::{
    PackagePolicyConfig, PolicyEnvironment, PolicyOperation, PolicyScope,
};
use riff_core::repository::InstalledRepository;
use riff_core::util::{canonical_package_name, is_platform_package};
use riff_core::{Package, Platform, Repository, RiffBuilder};
use riff_semver::Semver;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(usage_rs::Args, Debug)]
pub struct AuditArgs {
    /// Disables auditing of require-dev packages
    #[usage(long)]
    pub no_dev: bool,

    /// Output format (table, plain, json, or summary)
    #[usage(
        short,
        long,
        default = "table",
        choices("table", "plain", "json", "summary")
    )]
    pub format: String,

    /// Audit based on the lock file instead of the installed packages
    #[usage(long)]
    pub locked: bool,

    /// Behavior on abandoned packages (ignore, report, or fail)
    #[usage(long, choices("ignore", "report", "fail"))]
    pub abandoned: Option<String>,

    /// Ignore advisories with these severity levels; may be repeated
    #[usage(
        long,
        value_name = "SEVERITY",
        choices("low", "medium", "high", "critical")
    )]
    pub ignore_severity: Vec<String>,

    /// Ignore repositories and policy sources which cannot be reached
    #[usage(long)]
    pub ignore_unreachable: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

/// Audit data fetched while a lock-file installation is running.
///
/// The package policy is intentionally fetched against the complete locked set.
/// The result is filtered against the packages that were actually installed before
/// it is rendered.
pub(crate) struct PrefetchedInstallAudit {
    packages: Vec<LockedPackage>,
    dependency_policy: PackagePolicy,
    has_root_requirements: bool,
    vendor_dir: PathBuf,
}

pub(crate) async fn prefetch_for_install(
    working_dir: PathBuf,
    manifest: RiffManifest,
    config: Config,
    lock: RiffLockfile,
    no_dev: bool,
    output: riff_core::Output,
) -> Result<PrefetchedInstallAudit> {
    let manifest_value = serde_json::to_value(&manifest)?;
    let vendor_dir = working_dir.join(&config.vendor_dir);
    let packages = lock
        .packages
        .iter()
        .chain(if no_dev {
            [].iter()
        } else {
            lock.packages_dev.iter()
        })
        .cloned()
        .collect::<Vec<_>>();
    let runtime_packages = packages.iter().map(Package::from).collect::<Vec<_>>();
    let runtime_package_refs = runtime_packages.iter().collect::<Vec<_>>();
    let riff = RiffBuilder::new(working_dir)
        .with_config(config)
        .with_manifest(manifest)
        .with_lockfile(Some(lock))
        .with_platform(Platform::empty())
        .with_policy_environment(PolicyEnvironment::from_process())
        .with_output(output)
        .plugins_enabled(false)
        .build()?;
    let dependency_policy =
        PackagePolicy::load(&riff, &runtime_package_refs, PolicyScope::Audit, false).await?;

    Ok(PrefetchedInstallAudit {
        packages,
        dependency_policy,
        has_root_requirements: has_auditable_root_requirements(&manifest_value, no_dev),
        vendor_dir,
    })
}

pub(crate) async fn render_prefetched_install(
    prefetched: PrefetchedInstallAudit,
    args: AuditArgs,
    context: &crate::CommandContext,
) -> Result<i32> {
    let repository = InstalledRepository::new(prefetched.vendor_dir.clone());
    repository
        .load()
        .await
        .map_err(anyhow::Error::msg)
        .context("Failed to load installed packages")?;
    let installed_names = repository
        .get_packages()
        .await
        .into_iter()
        .map(|package| canonical_package_name(&package.name).into_owned())
        .collect::<HashSet<_>>();

    if installed_names.is_empty() {
        if !prefetched.has_root_requirements {
            riff_core::outln!(
                context.output(),
                "{}",
                "No packages - skipping audit.".yellow()
            );
            return Ok(0);
        }
        riff_core::outln!(context.output(),
            "No installed packages found. Please run \"riff install\" before running \"riff audit\""
        );
        return Ok(1);
    }

    let audited_packages = prefetched
        .packages
        .iter()
        .filter(|package| installed_names.contains(canonical_package_name(&package.name).as_ref()))
        .collect::<Vec<_>>();
    if audited_packages.is_empty() {
        riff_core::outln!(
            context.output(),
            "{}",
            "No packages - skipping audit.".yellow()
        );
        return Ok(0);
    }

    render_audit_result(
        &args,
        &prefetched.dependency_policy,
        audited_packages,
        context,
    )
}

pub async fn execute(args: AuditArgs, context: &crate::CommandContext) -> Result<i32> {
    execute_with_context(args, None, None, context).await
}

pub(crate) async fn execute_with_context(
    args: AuditArgs,
    existing_lock: Option<&RiffLockfile>,
    existing_installed_names: Option<&HashSet<String>>,
    context: &crate::CommandContext,
) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;

    let manifest = read_manifest(&working_dir, args.locked && existing_lock.is_none())?;
    let typed_manifest: RiffManifest =
        serde_json::from_value(manifest.clone()).context("Failed to parse composer.json")?;
    let config = Config::build(Some(&working_dir), true)?;

    let owned_installed_names;
    let installed_names: Option<&HashSet<String>> = if args.locked {
        None
    } else if let Some(names) = existing_installed_names {
        Some(names)
    } else {
        let repository = InstalledRepository::new(working_dir.join(&config.vendor_dir));
        repository
            .load()
            .await
            .map_err(anyhow::Error::msg)
            .context("Failed to load installed packages")?;
        owned_installed_names = repository
            .get_packages()
            .await
            .into_iter()
            .map(|package| canonical_package_name(&package.name).into_owned())
            .collect();
        Some(&owned_installed_names)
    };

    if let Some(installed_names) = installed_names {
        if installed_names.is_empty() {
            if !has_auditable_root_requirements(&manifest, args.no_dev) {
                riff_core::outln!(
                    context.output(),
                    "{}",
                    "No packages - skipping audit.".yellow()
                );
                return Ok(0);
            }
            riff_core::outln!(context.output(),
                "No installed packages found. Please run \"riff install\" before running \"riff audit\""
            );
            return Ok(1);
        }
    }

    let owned_lock;
    let lock = if let Some(lock) = existing_lock {
        lock
    } else {
        owned_lock = read_lock(&working_dir, args.locked)?;
        &owned_lock
    };

    let audited_packages: Vec<_> = lock
        .packages
        .iter()
        .chain(if args.no_dev {
            [].iter()
        } else {
            lock.packages_dev.iter()
        })
        .filter(|package| {
            installed_names
                .as_ref()
                .is_none_or(|names| names.contains(canonical_package_name(&package.name).as_ref()))
        })
        .collect();
    if audited_packages.is_empty() {
        riff_core::outln!(
            context.output(),
            "{}",
            "No packages - skipping audit.".yellow()
        );
        return Ok(0);
    }

    let mut riff = RiffBuilder::new(working_dir.clone())
        .with_config(config)
        .with_manifest(typed_manifest)
        .with_lockfile(Some(lock.clone()))
        .with_platform(Platform::empty())
        .with_policy_environment(PolicyEnvironment::from_process())
        .with_output(context.output().clone())
        .plugins_enabled(false)
        .build()?;
    if !args.ignore_severity.is_empty() {
        riff.package_policy.advisories = riff
            .package_policy
            .advisories
            .with_ignore_severity(args.ignore_severity.iter().map(String::as_str));
    }
    if args.ignore_unreachable {
        riff.package_policy = riff
            .package_policy
            .with_ignore_unreachable(&[PolicyScope::Audit])?;
    }
    let runtime_packages = audited_packages
        .iter()
        .map(|package| Package::from(*package))
        .collect::<Vec<_>>();
    let runtime_package_refs = runtime_packages.iter().collect::<Vec<_>>();
    let dependency_policy =
        PackagePolicy::load(&riff, &runtime_package_refs, PolicyScope::Audit, false).await?;

    render_audit_result(&args, &dependency_policy, audited_packages, context)
}

fn render_audit_result(
    args: &AuditArgs,
    dependency_policy: &PackagePolicy,
    audited_packages: Vec<&LockedPackage>,
    context: &crate::CommandContext,
) -> Result<i32> {
    let runtime_packages = audited_packages
        .iter()
        .map(|package| Package::from(*package))
        .collect::<Vec<_>>();
    let runtime_package_refs = runtime_packages.iter().collect::<Vec<_>>();
    let installed_versions = installed_versions(&runtime_packages);
    let policy = advisory_policy(&dependency_policy.config, &installed_versions);
    let advisories = dependency_policy
        .audit_advisories()
        .iter()
        .cloned()
        .map(audit_advisory);
    let filter_entries = dependency_policy
        .audit_filters(&runtime_package_refs)
        .into_iter()
        .map(audit_filter_entry);
    let report = evaluate_audit_report(
        &installed_versions,
        advisories,
        filter_entries,
        &policy,
        dependency_policy.unreachable_repositories().to_vec(),
    )?;

    let configured_abandoned = audit_behavior_name(dependency_policy.config.abandoned.audit);
    let abandoned_behavior = args.abandoned.as_deref().unwrap_or(configured_abandoned);
    let abandoned_packages: Vec<_> = if abandoned_behavior != "ignore" {
        audited_packages
            .iter()
            .copied()
            .filter(|package| {
                package.is_abandoned()
                    && !dependency_policy.config.abandoned.package_is_ignored(
                        &package.name,
                        &package.version,
                        PolicyOperation::Audit,
                    )
            })
            .collect()
    } else {
        Vec::new()
    };

    let has_vulnerabilities = report.has_failing_findings();
    let has_abandoned = !abandoned_packages.is_empty();

    match args.format.as_str() {
        "json" => {
            output_json(&report, &abandoned_packages, context)?;
        }
        "plain" => {
            output_plain(&report, &abandoned_packages, context)?;
        }
        "summary" => {
            output_summary(&report, context)?;
        }
        _ => {
            // table format (default)
            output_table(&report, &abandoned_packages, context)?;
        }
    }

    Ok(audit_exit_code(
        has_vulnerabilities,
        has_abandoned,
        abandoned_behavior,
    ))
}

fn read_manifest(working_dir: &Path, locked: bool) -> Result<serde_json::Value> {
    let path = working_dir.join("composer.json");
    if !path.exists() {
        if locked {
            return bail_locked_files_required();
        }
        return Ok(serde_json::json!({}));
    }
    let contents = std::fs::read_to_string(&path)?;
    match serde_json::from_str::<serde_json::Value>(&contents) {
        Ok(manifest) if manifest.is_object() => Ok(manifest),
        _ if locked => bail_locked_files_required(),
        _ => Err(anyhow::anyhow!("Failed to parse composer.json")),
    }
}

fn read_lock(working_dir: &Path, locked: bool) -> Result<RiffLockfile> {
    let path = working_dir.join("composer.lock");
    if !path.exists() {
        if locked {
            return bail_locked_files_required();
        }
        return Err(anyhow::anyhow!(
            "No composer.lock found. Run 'riff install' or 'riff update' first."
        ));
    }
    let contents = std::fs::read_to_string(&path)?;
    match serde_json::from_str::<RiffLockfile>(&contents) {
        Ok(lock) => Ok(lock),
        Err(_) if locked => bail_locked_files_required(),
        Err(error) => Err(error).context("Failed to parse composer.lock"),
    }
}

fn bail_locked_files_required<T>() -> Result<T> {
    Err(anyhow::anyhow!(
        "Valid composer.json and composer.lock files are required to run this command with --locked"
    ))
}

fn has_auditable_root_requirements(manifest: &serde_json::Value, no_dev: bool) -> bool {
    let has_packages = |section: &str| {
        manifest
            .get(section)
            .and_then(serde_json::Value::as_object)
            .is_some_and(|requirements| {
                requirements
                    .keys()
                    .any(|package| !is_platform_package(package))
            })
    };
    has_packages("require") || (!no_dev && has_packages("require-dev"))
}

#[cfg(test)]
fn package_policy_config(manifest: &serde_json::Value) -> Result<PackagePolicyConfig> {
    let config = manifest
        .get("config")
        .and_then(serde_json::Value::as_object);
    let policy = config
        .and_then(|config| config.get("policy"))
        .unwrap_or(&serde_json::Value::Null);
    let audit = config
        .and_then(|config| config.get("audit"))
        .unwrap_or(&serde_json::Value::Null);
    PackagePolicyConfig::from_raw(policy, audit, &PolicyEnvironment::from_process())
        .map_err(anyhow::Error::new)
}

fn advisory_policy(
    config: &PackagePolicyConfig,
    installed_versions: &BTreeMap<String, String>,
) -> AdvisoryPolicy {
    let mut policy = AdvisoryPolicy::default()
        .advisory_behavior(config.advisories.audit)
        .ignore_unreachable(config.ignore_unreachable.audit)
        .filter_behavior("malware", config.malware.audit);
    for (name, custom) in &config.custom_lists {
        policy = policy.filter_behavior(name, custom.audit);
    }
    for (identifier, rule) in &config.advisories.ignore_id {
        if rule.applies_to(PolicyOperation::Audit) {
            policy = policy.ignore_advisory(identifier.clone(), rule.reason.clone());
        }
    }
    for (package, rules) in &config.advisories.ignore {
        let applicable = installed_versions
            .iter()
            .filter(|(installed, _)| riff_core::package::package_name_matches(package, installed))
            .flat_map(|(_, version)| {
                rules.iter().filter(move |rule| {
                    rule.applies_to(PolicyOperation::Audit)
                        && rule
                            .constraint
                            .as_deref()
                            .is_none_or(|constraint| Semver::satisfies(version, constraint))
                })
            })
            .collect::<Vec<_>>();
        if !applicable.is_empty() {
            let reason = applicable.into_iter().find_map(|rule| rule.reason.clone());
            policy = policy.ignore_advisory(package.clone(), reason);
        }
    }
    for (severity, reason) in config
        .advisories
        .ignore_severity_for_operation(PolicyOperation::Audit)
    {
        policy = policy.ignore_severity(severity, reason);
    }
    policy
}

#[cfg(test)]
fn inline_filter_entries(manifest: &serde_json::Value) -> Vec<AuditFilterEntry> {
    let Some(repositories) = manifest.get("repositories") else {
        return Vec::new();
    };
    let repositories: Vec<_> = match repositories {
        serde_json::Value::Array(repositories) => repositories.iter().collect(),
        serde_json::Value::Object(repositories) => repositories.values().collect(),
        _ => Vec::new(),
    };
    repositories
        .into_iter()
        .filter_map(|repository| repository.get("filter")?.as_object())
        .flat_map(|lists| {
            lists.iter().flat_map(|(list_name, entries)| {
                entries
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(move |entry| {
                        Some(AuditFilterEntry {
                            package_name: entry.get("package")?.as_str()?.to_string(),
                            list_name: list_name.clone(),
                            constraint: entry
                                .get("constraint")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("*")
                                .to_string(),
                            url: entry
                                .get("url")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string),
                            reason: entry
                                .get("reason")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string),
                            id: entry
                                .get("id")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string),
                            source: entry
                                .get("source")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string),
                        })
                    })
            })
        })
        .collect()
}

fn evaluate_audit_report(
    installed_versions: &BTreeMap<String, String>,
    advisories: impl IntoIterator<Item = AuditAdvisory>,
    filter_entries: impl IntoIterator<Item = AuditFilterEntry>,
    policy: &AdvisoryPolicy,
    unreachable_repositories: Vec<String>,
) -> Result<AuditReport> {
    policy
        .evaluate(
            installed_versions,
            advisories,
            filter_entries,
            unreachable_repositories,
        )
        .map_err(anyhow::Error::new)
}

fn installed_versions(packages: &[Package]) -> BTreeMap<String, String> {
    let mut versions = BTreeMap::new();
    for package in packages {
        for name in std::iter::once(package.name.as_str())
            .chain(package.provide.keys().map(|name| name.as_str()))
            .chain(package.replace.keys().map(|name| name.as_str()))
        {
            versions.insert(
                canonical_package_name(name).into_owned(),
                package.version.to_string(),
            );
        }
    }
    versions
}

fn audit_advisory(advisory: SecurityAdvisory) -> AuditAdvisory {
    AuditAdvisory {
        title: advisory
            .title
            .unwrap_or_else(|| advisory.advisory_id.clone()),
        cve: advisory.cve,
        link: advisory.link,
        severity: advisory.severity,
        reported_at: advisory.reported_at.unwrap_or_default(),
        sources: advisory
            .sources
            .unwrap_or_default()
            .into_iter()
            .map(|source| AuditAdvisorySource {
                name: source.name,
                remote_id: source.remote_id,
            })
            .collect(),
        advisory_id: advisory.advisory_id,
        package_name: advisory.package_name,
        affected_versions: advisory.affected_versions,
    }
}

fn audit_filter_entry(entry: riff_core::filter_list::FilterListEntry) -> AuditFilterEntry {
    AuditFilterEntry {
        package_name: entry.package_name,
        list_name: entry.list_name,
        constraint: entry.constraint,
        url: entry.url,
        reason: entry.reason,
        id: entry.id,
        source: entry.source,
    }
}

const fn audit_behavior_name(behavior: AuditBehavior) -> &'static str {
    match behavior {
        AuditBehavior::Ignore => "ignore",
        AuditBehavior::Report => "report",
        AuditBehavior::Fail => "fail",
    }
}

fn audit_exit_code(
    has_vulnerabilities: bool,
    has_abandoned: bool,
    abandoned_behavior: &str,
) -> i32 {
    i32::from(has_vulnerabilities || (has_abandoned && abandoned_behavior == "fail"))
}

fn output_json(
    report: &AuditReport,
    abandoned_packages: &[&LockedPackage],
    context: &crate::CommandContext,
) -> Result<()> {
    #[derive(Serialize)]
    struct JsonOutput<'a> {
        advisories: serde_json::Value,
        #[serde(rename = "ignored-advisories", skip_serializing_if = "Option::is_none")]
        ignored_advisories:
            Option<&'a BTreeMap<String, Vec<riff_core::advisory::IgnoredAuditAdvisory>>>,
        #[serde(
            rename = "unreachable-repositories",
            skip_serializing_if = "Option::is_none"
        )]
        unreachable_repositories: Option<&'a [String]>,
        abandoned: serde_json::Value,
        filter: serde_json::Value,
    }

    let abandoned_map: HashMap<String, Option<String>> = abandoned_packages
        .iter()
        .map(|p| (p.name.clone(), p.abandoned_replacement().map(String::from)))
        .collect();

    let output = JsonOutput {
        advisories: btree_map_or_empty_array(&report.advisories)?,
        ignored_advisories: (!report.ignored_advisories.is_empty())
            .then_some(&report.ignored_advisories),
        unreachable_repositories: (!report.unreachable_repositories.is_empty())
            .then_some(report.unreachable_repositories.as_slice()),
        abandoned: map_or_empty_array(&abandoned_map)?,
        filter: btree_map_or_empty_array(&report.filter)?,
    };

    riff_core::outln!(
        context.output(),
        "{}",
        serde_json::to_string_pretty(&output)?
    );
    Ok(())
}

fn btree_map_or_empty_array<T: Serialize>(map: &BTreeMap<String, T>) -> Result<serde_json::Value> {
    if map.is_empty() {
        Ok(serde_json::json!([]))
    } else {
        Ok(serde_json::to_value(map)?)
    }
}

fn map_or_empty_array<T: Serialize>(map: &HashMap<String, T>) -> Result<serde_json::Value> {
    if map.is_empty() {
        Ok(serde_json::json!([]))
    } else {
        Ok(serde_json::to_value(map)?)
    }
}

fn output_table(
    response: &AuditReport,
    abandoned_packages: &[&LockedPackage],
    context: &crate::CommandContext,
) -> Result<()> {
    let total_advisories: usize = response.advisories.values().map(|v| v.len()).sum();
    let affected_packages = response.advisories.len();

    if total_advisories > 0 {
        riff_core::outln!(
            context.output(),
            "{}",
            security_summary(total_advisories, affected_packages, false)
                .red()
                .bold()
        );
        riff_core::outln!(context.output());

        for advisories in response.advisories.values() {
            for advisory in advisories {
                riff_core::outln!(context.output(), "{}", "─".repeat(80).bright_black());
                riff_core::outln!(
                    context.output(),
                    "{}: {}",
                    "Package".bold(),
                    advisory.package_name
                );
                riff_core::outln!(
                    context.output(),
                    "{}: {}",
                    "Severity".bold(),
                    colorize_severity(advisory.severity.as_deref())
                );
                riff_core::outln!(
                    context.output(),
                    "{}: {}",
                    "Advisory ID".bold(),
                    advisory.advisory_id
                );
                riff_core::outln!(
                    context.output(),
                    "{}: {}",
                    "CVE".bold(),
                    advisory.cve.as_deref().unwrap_or("NO CVE")
                );
                riff_core::outln!(context.output(), "{}: {}", "Title".bold(), advisory.title);
                if let Some(link) = &advisory.link {
                    riff_core::outln!(context.output(), "{}: {}", "URL".bold(), link);
                }
                riff_core::outln!(
                    context.output(),
                    "{}: {}",
                    "Affected versions".bold(),
                    advisory.affected_versions
                );
                riff_core::outln!(
                    context.output(),
                    "{}: {}",
                    "Reported at".bold(),
                    advisory.reported_at
                );
                riff_core::outln!(context.output());
            }
        }
    } else {
        riff_core::outln!(
            context.output(),
            "{}",
            security_summary(0, 0, false).green().bold()
        );
    }

    if let Some(summary) = response.filter_summary(false) {
        riff_core::outln!(context.output(), "{}", summary.yellow().bold());
        for diagnostic in response.filter_diagnostics() {
            riff_core::outln!(context.output(), "{diagnostic}");
        }
    }

    if !abandoned_packages.is_empty() {
        riff_core::outln!(
            context.output(),
            "{}",
            format!(
                "Found {} abandoned package{}:",
                abandoned_packages.len(),
                if abandoned_packages.len() > 1 {
                    "s"
                } else {
                    ""
                }
            )
            .yellow()
            .bold()
        );
        riff_core::outln!(context.output());

        for pkg in abandoned_packages {
            let replacement = pkg
                .abandoned_replacement()
                .map(|r| format!("Use {} instead", r))
                .unwrap_or_else(|| "No replacement was suggested".to_string());
            riff_core::outln!(
                context.output(),
                "  {} is abandoned. {}",
                pkg.name.yellow(),
                replacement
            );
        }
    }

    Ok(())
}

fn output_plain(
    response: &AuditReport,
    abandoned_packages: &[&LockedPackage],
    context: &crate::CommandContext,
) -> Result<()> {
    let total_advisories: usize = response.advisories.values().map(|v| v.len()).sum();
    let affected_packages = response.advisories.len();

    if total_advisories > 0 {
        riff_core::errln!(
            context.output(),
            "{}",
            security_summary(total_advisories, affected_packages, false)
        );

        let mut first = true;
        for advisories in response.advisories.values() {
            for advisory in advisories {
                if !first {
                    riff_core::errln!(context.output(), "--------");
                }
                riff_core::errln!(context.output(), "Package: {}", advisory.package_name);
                riff_core::errln!(
                    context.output(),
                    "Severity: {}",
                    advisory.severity.as_deref().unwrap_or("")
                );
                riff_core::errln!(context.output(), "Advisory ID: {}", advisory.advisory_id);
                riff_core::errln!(
                    context.output(),
                    "CVE: {}",
                    advisory.cve.as_deref().unwrap_or("NO CVE")
                );
                riff_core::errln!(context.output(), "Title: {}", advisory.title);
                riff_core::errln!(
                    context.output(),
                    "URL: {}",
                    advisory.link.as_deref().unwrap_or("")
                );
                riff_core::errln!(
                    context.output(),
                    "Affected versions: {}",
                    advisory.affected_versions
                );
                riff_core::errln!(context.output(), "Reported at: {}", advisory.reported_at);
                first = false;
            }
        }
    } else {
        riff_core::errln!(context.output(), "{}", security_summary(0, 0, false));
    }

    if let Some(summary) = response.filter_summary(false) {
        riff_core::errln!(context.output(), "{summary}");
        for diagnostic in response.filter_diagnostics() {
            riff_core::errln!(context.output(), "{diagnostic}");
        }
    }

    if !abandoned_packages.is_empty() {
        riff_core::errln!(
            context.output(),
            "Found {} abandoned package{}:",
            abandoned_packages.len(),
            if abandoned_packages.len() > 1 {
                "s"
            } else {
                ""
            }
        );

        for pkg in abandoned_packages {
            let replacement = pkg
                .abandoned_replacement()
                .map(|r| format!("Use {} instead", r))
                .unwrap_or_else(|| "No replacement was suggested".to_string());
            riff_core::errln!(
                context.output(),
                "{} is abandoned. {}",
                pkg.name,
                replacement
            );
        }
    }

    Ok(())
}

fn output_summary(response: &AuditReport, context: &crate::CommandContext) -> Result<()> {
    let total_advisories: usize = response.advisories.values().map(|v| v.len()).sum();
    let affected_packages = response.advisories.len();

    if total_advisories > 0 {
        riff_core::errln!(
            context.output(),
            "{}",
            security_summary(total_advisories, affected_packages, true)
        );
        riff_core::errln!(
            context.output(),
            "Run \"riff audit\" for a full list of advisories."
        );
    } else {
        riff_core::errln!(context.output(), "{}", security_summary(0, 0, true));
    }
    if let Some(summary) = response.filter_summary(true) {
        riff_core::errln!(context.output(), "{summary}");
    }

    Ok(())
}

fn security_summary(
    total_advisories: usize,
    affected_packages: usize,
    summary_only: bool,
) -> String {
    if total_advisories == 0 {
        return "No security vulnerability advisories found.".to_string();
    }
    format!(
        "Found {} security vulnerability advisor{} affecting {} package{}{}",
        total_advisories,
        if total_advisories == 1 { "y" } else { "ies" },
        affected_packages,
        if affected_packages == 1 { "" } else { "s" },
        if summary_only { "." } else { ":" }
    )
}

fn colorize_severity(severity: Option<&str>) -> colored::ColoredString {
    match severity {
        Some("critical") => "critical".red().bold(),
        Some("high") => "high".red(),
        Some("medium") => "medium".yellow(),
        Some("low") => "low".blue(),
        _ => "unknown".normal(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_empty_audit_maps_as_arrays() {
        let map: HashMap<String, Vec<AuditAdvisory>> = HashMap::default();
        assert_eq!(map_or_empty_array(&map).unwrap(), serde_json::json!([]));
    }

    #[test]
    fn composer_auditor_status_reflects_only_failing_findings() {
        for (vulnerable, abandoned, behavior, expected) in [
            (false, false, "fail", 0),
            (true, false, "fail", 1),
            (false, true, "ignore", 0),
            (false, true, "report", 0),
            (false, true, "fail", 1),
            (true, true, "fail", 1),
        ] {
            assert_eq!(
                audit_exit_code(vulnerable, abandoned, behavior),
                expected,
                "vulnerable={vulnerable}, abandoned={abandoned}, behavior={behavior}"
            );
        }
    }

    // Ported from Composer\Test\Command\AuditCommandTest::
    // testAuditPackageWithNoSecurityVulnerabilities.
    #[test]
    fn composer_audit_command_reports_a_clean_package_set() {
        let versions = BTreeMap::from([("safe/pkg".to_string(), "1.0.0".to_string())]);
        let report = evaluate_audit_report(
            &versions,
            std::iter::empty(),
            std::iter::empty(),
            &AdvisoryPolicy::default(),
            Vec::new(),
        )
        .unwrap();

        assert_eq!(
            security_summary(report.advisory_count(), report.advisories.len(), false),
            "No security vulnerability advisories found."
        );
        assert!(!report.has_failing_findings());
        assert_eq!(audit_exit_code(false, false, "fail"), 0);
    }

    // Ported from Composer\Test\Command\AuditCommandTest::
    // testAuditWithMalwareAndCustomListBothFail.
    #[test]
    fn composer_audit_command_fails_for_malware_and_custom_policy_matches() {
        let manifest = serde_json::json!({
            "repositories": {"packages": {
                "type": "package",
                "filter": {
                    "malware": [{
                        "package": "malicious/pkg",
                        "constraint": "*",
                        "reason": "malware sample"
                    }],
                    "company-banned": [{
                        "package": "banned/pkg",
                        "constraint": "*",
                        "reason": "company policy"
                    }]
                }
            }},
            "config": {"policy": {"company-banned": true}}
        });
        let config = package_policy_config(&manifest).unwrap();
        let versions = BTreeMap::from([
            ("safe/pkg".to_string(), "1.0.0".to_string()),
            ("malicious/pkg".to_string(), "1.0.0".to_string()),
            ("banned/pkg".to_string(), "1.0.0".to_string()),
        ]);
        let report = evaluate_audit_report(
            &versions,
            std::iter::empty(),
            inline_filter_entries(&manifest),
            &advisory_policy(&config, &versions),
            Vec::new(),
        )
        .unwrap();

        assert_eq!(
            report.filter_summary(false).as_deref(),
            Some("Found 2 packages matching filters:")
        );
        let diagnostics = report.filter_diagnostics().join("\n");
        assert!(diagnostics.contains("malicious/pkg"));
        assert!(diagnostics.contains("banned/pkg"));
        assert!(report.has_failing_findings());
        assert_eq!(audit_exit_code(true, false, "fail"), 1);
    }

    // Ported from Composer\Test\Command\AuditCommandTest::
    // testAuditWithCustomListAuditReportDoesNotFail.
    #[test]
    fn composer_audit_command_reports_custom_policy_matches_without_failing() {
        let manifest = serde_json::json!({
            "repositories": {"packages": {
                "type": "package",
                "filter": {"company-banned": [{
                    "package": "banned/pkg",
                    "constraint": "*",
                    "reason": "company policy"
                }]}
            }},
            "config": {"policy": {"company-banned": {"audit": "report"}}}
        });
        let config = package_policy_config(&manifest).unwrap();
        let versions = BTreeMap::from([
            ("safe/pkg".to_string(), "1.0.0".to_string()),
            ("banned/pkg".to_string(), "1.0.0".to_string()),
        ]);
        let report = evaluate_audit_report(
            &versions,
            std::iter::empty(),
            inline_filter_entries(&manifest),
            &advisory_policy(&config, &versions),
            Vec::new(),
        )
        .unwrap();

        assert_eq!(
            report.filter_summary(false).as_deref(),
            Some("Found 1 package matching filters:")
        );
        assert!(report
            .filter_diagnostics()
            .join("\n")
            .contains("banned/pkg"));
        assert!(!report.has_failing_findings());
        assert_eq!(audit_exit_code(false, false, "fail"), 0);
    }

    #[tokio::test]
    async fn update_context_avoids_reloading_installed_state() {
        let working_dir = tempfile::tempdir().unwrap();
        let args = AuditArgs {
            no_dev: false,
            format: "summary".into(),
            locked: false,
            abandoned: Some("report".into()),
            ignore_severity: Vec::new(),
            ignore_unreachable: false,
            working_dir: working_dir.path().into(),
        };
        let lock = RiffLockfile::default();
        let installed_names = HashSet::default();
        let context = crate::CommandContext::new(
            riff_core::RuntimeContext::new("php".into(), "riff".into()),
            riff_core::Platform::empty(),
        );

        let result =
            execute_with_context(args, Some(&lock), Some(&installed_names), &context).await;

        assert_eq!(result.unwrap(), 0);
    }
}
