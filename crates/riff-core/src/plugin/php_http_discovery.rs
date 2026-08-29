//! Native adapter for the `php-http/discovery` Composer plugin.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use riff_semver::VersionParser;

use crate::event::{EventListener, EventType, PreAutoloadDumpEvent, RiffEvent};
use crate::json::{LockedPackage, RiffLockfile, RiffManifest};
use crate::process::ProcessRunner;
use crate::riff::Riff;

use super::manager::{PluginDescriptor, PluginRegistrar};

pub const PACKAGE_NAME: &str = "php-http/discovery";

const GENERATED_STRATEGY: &str = "composer/GeneratedDiscoveryStrategy.php";
const MAX_REENTRY_DEPTH: u8 = 5;

const INTERFACE_MAP: &[(&str, &[&str])] = &[
    (
        "php-http/async-client-implementation",
        &["Http\\Client\\HttpAsyncClient"],
    ),
    (
        "php-http/client-implementation",
        &["Http\\Client\\HttpClient"],
    ),
    (
        "psr/http-client-implementation",
        &["Psr\\Http\\Client\\ClientInterface"],
    ),
    (
        "psr/http-factory-implementation",
        &[
            "Psr\\Http\\Message\\RequestFactoryInterface",
            "Psr\\Http\\Message\\ResponseFactoryInterface",
            "Psr\\Http\\Message\\ServerRequestFactoryInterface",
            "Psr\\Http\\Message\\StreamFactoryInterface",
            "Psr\\Http\\Message\\UploadedFileFactoryInterface",
            "Psr\\Http\\Message\\UriFactoryInterface",
        ],
    ),
];

#[derive(Clone, Copy)]
struct Candidate {
    package: &'static str,
    constraint: Option<&'static str>,
    dependencies: &'static [&'static str],
}

const ASYNC_CLIENTS: &[Candidate] = &[
    Candidate::new(
        "symfony/http-client",
        Some(">=6.3"),
        &[
            "guzzlehttp/promises",
            "psr/http-factory-implementation",
            "php-http/httplug",
        ],
    ),
    Candidate::new(
        "symfony/http-client",
        None,
        &[
            "guzzlehttp/promises",
            "php-http/message-factory",
            "psr/http-factory-implementation",
            "php-http/httplug",
        ],
    ),
    Candidate::new("php-http/guzzle7-adapter", None, &[]),
    Candidate::new("php-http/guzzle6-adapter", None, &[]),
    Candidate::new("php-http/curl-client", None, &[]),
    Candidate::new("php-http/react-adapter", None, &[]),
];

const HTTP_CLIENTS: &[Candidate] = &[
    Candidate::new(
        "symfony/http-client",
        Some(">=6.3"),
        &["psr/http-factory-implementation", "php-http/httplug"],
    ),
    Candidate::new(
        "symfony/http-client",
        None,
        &[
            "php-http/message-factory",
            "psr/http-factory-implementation",
            "php-http/httplug",
        ],
    ),
    Candidate::new("php-http/guzzle7-adapter", None, &[]),
    Candidate::new("php-http/guzzle6-adapter", None, &[]),
    Candidate::new("php-http/cakephp-adapter", None, &[]),
    Candidate::new("php-http/curl-client", None, &[]),
    Candidate::new("php-http/react-adapter", None, &[]),
    Candidate::new("php-http/buzz-adapter", None, &[]),
    Candidate::new("php-http/artax-adapter", None, &[]),
    Candidate::new("kriswallsmith/buzz", Some("^1"), &[]),
];

const PSR18_CLIENTS: &[Candidate] = &[
    Candidate::new(
        "symfony/http-client",
        None,
        &["psr/http-factory-implementation", "psr/http-client"],
    ),
    Candidate::new("guzzlehttp/guzzle", None, &[]),
    Candidate::new("kriswallsmith/buzz", Some("^1"), &[]),
];

const PSR7_MESSAGES: &[Candidate] = &[Candidate::new(
    PACKAGE_NAME,
    None,
    &["psr/http-factory-implementation"],
)];

const PSR17_FACTORIES: &[Candidate] = &[
    Candidate::new("nyholm/psr7", None, &[]),
    Candidate::new("guzzlehttp/psr7", Some(">=2"), &[]),
    Candidate::new("slim/psr7", None, &[]),
    Candidate::new("laminas/laminas-diactoros", None, &[]),
    Candidate::new("phalcon/cphalcon", Some("^4"), &[]),
    Candidate::new("http-interop/http-factory-guzzle", None, &[]),
    Candidate::new("http-interop/http-factory-diactoros", None, &[]),
    Candidate::new("http-interop/http-factory-slim", None, &[]),
    Candidate::new("httpsoft/http-message", None, &[]),
];

const PROVIDE_RULES: &[(&str, &[Candidate])] = &[
    ("php-http/async-client-implementation", ASYNC_CLIENTS),
    ("php-http/client-implementation", HTTP_CLIENTS),
    ("psr/http-client-implementation", PSR18_CLIENTS),
    ("psr/http-message-implementation", PSR7_MESSAGES),
    ("psr/http-factory-implementation", PSR17_FACTORIES),
];

const STICKINESS_RULES: &[(&str, &str, Option<&str>)] = &[
    ("symfony/http-client", "symfony/framework-bundle", None),
    ("php-http/guzzle7-adapter", "guzzlehttp/guzzle", Some("^7")),
    ("php-http/guzzle6-adapter", "guzzlehttp/guzzle", Some("^6")),
    ("php-http/guzzle5-adapter", "guzzlehttp/guzzle", Some("^5")),
    ("php-http/cakephp-adapter", "cakephp/cakephp", None),
    ("php-http/react-adapter", "react/event-loop", None),
    (
        "php-http/buzz-adapter",
        "kriswallsmith/buzz",
        Some("^0.15.1"),
    ),
    ("php-http/artax-adapter", "amphp/artax", Some("^3")),
    (
        "http-interop/http-factory-guzzle",
        "guzzlehttp/psr7",
        Some("^1"),
    ),
    ("http-interop/http-factory-slim", "slim/slim", Some("^3")),
];

impl Candidate {
    const fn new(
        package: &'static str,
        constraint: Option<&'static str>,
        dependencies: &'static [&'static str],
    ) -> Self {
        Self {
            package,
            constraint,
            dependencies,
        }
    }
}

pub struct PhpHttpDiscoveryPlugin;

pub(super) fn register(registrar: &mut PluginRegistrar) {
    let plugin = Arc::new(PhpHttpDiscoveryPlugin);
    registrar.descriptor(PluginDescriptor::new(PACKAGE_NAME));
    registrar.event(PACKAGE_NAME, EventType::PreAutoloadDump, plugin.clone());
    registrar.event(PACKAGE_NAME, EventType::PostUpdate, plugin);
}

#[async_trait(?Send)]
impl EventListener for PhpHttpDiscoveryPlugin {
    async fn handle(&self, event: &dyn RiffEvent, riff: &Riff) -> Result<i32> {
        match event.event_type() {
            EventType::PreAutoloadDump => {
                let Some(event) = event.as_any().downcast_ref::<PreAutoloadDumpEvent>() else {
                    return Ok(0);
                };
                self.pre_autoload_dump(event, riff)?;
            }
            EventType::PostUpdate => self.post_update(riff)?,
            _ => {}
        }

        Ok(0)
    }
}

impl PhpHttpDiscoveryPlugin {
    fn pre_autoload_dump(&self, event: &PreAutoloadDumpEvent, riff: &Riff) -> Result<()> {
        if !riff.vendor_dir().join(PACKAGE_NAME).is_dir() {
            return Ok(());
        }

        let generated_path = riff.vendor_dir().join(GENERATED_STRATEGY);
        let Some(code) = generated_strategy(&riff.manifest.extra)? else {
            remove_file_if_present(&generated_path)?;
            return Ok(());
        };

        if let Some(parent) = generated_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::read_to_string(&generated_path).ok().as_deref() != Some(code.as_str()) {
            std::fs::write(&generated_path, code)?;
        }
        event.add_classmap_path(generated_path);
        Ok(())
    }

    fn post_update(&self, riff: &Riff) -> Result<()> {
        let manifest_path = riff.working_dir.join("composer.json");
        let lock_path = riff.working_dir.join("composer.lock");
        let manifest: RiffManifest = serde_json::from_str(
            &std::fs::read_to_string(&manifest_path)
                .context("Failed to read composer.json for php-http/discovery")?,
        )
        .context("Failed to parse composer.json for php-http/discovery")?;
        let lock: RiffLockfile = serde_json::from_str(
            &std::fs::read_to_string(&lock_path)
                .context("Failed to read composer.lock for php-http/discovery")?,
        )
        .context("Failed to parse composer.lock for php-http/discovery")?;

        if !lock
            .packages
            .iter()
            .chain(lock.packages_dev.iter())
            .any(|package| package.name == PACKAGE_NAME)
        {
            return Ok(());
        }

        let missing = missing_packages(&manifest, &lock);
        if missing.production.is_empty() && missing.development.is_empty() {
            return Ok(());
        }

        let depth = std::env::var("RIFF_PHP_HTTP_DISCOVERY_DEPTH")
            .ok()
            .and_then(|value| value.parse::<u8>().ok())
            .unwrap_or(0);
        if depth >= MAX_REENTRY_DEPTH {
            bail!("php-http/discovery could not finish installing an HTTP implementation");
        }

        if !missing.production.is_empty() {
            run_require(riff, &missing.production, false, depth + 1)?;
        }

        // A nested update can satisfy the development requirements too. Re-read
        // project state before deciding whether a second invocation is needed.
        let manifest: RiffManifest = serde_json::from_str(
            &std::fs::read_to_string(&manifest_path)
                .context("Failed to refresh composer.json for php-http/discovery")?,
        )?;
        let lock: RiffLockfile = serde_json::from_str(
            &std::fs::read_to_string(&lock_path)
                .context("Failed to refresh composer.lock for php-http/discovery")?,
        )?;
        let missing = missing_packages(&manifest, &lock);
        if !missing.development.is_empty() {
            run_require(riff, &missing.development, true, depth + 1)?;
        }

        Ok(())
    }
}

fn generated_strategy(extra: &serde_json::Value) -> Result<Option<String>> {
    let Some(pinned) = extra.get("discovery") else {
        return Ok(None);
    };
    let pinned = pinned
        .as_object()
        .context("extra.discovery must be an object")?;
    if pinned.is_empty() {
        return Ok(None);
    }

    let all_interfaces: BTreeSet<_> = INTERFACE_MAP
        .iter()
        .flat_map(|(_, interfaces)| interfaces.iter().copied())
        .collect();
    let abstractions: BTreeSet<_> = INTERFACE_MAP.iter().map(|(name, _)| *name).collect();
    let mut cases = String::new();

    for (key, value) in pinned {
        let class = value
            .as_str()
            .with_context(|| format!("extra.discovery.{key} must be a class name"))?;
        let interfaces: Vec<&str> = if let Some((_, interfaces)) = INTERFACE_MAP
            .iter()
            .find(|(abstraction, _)| abstraction == key)
        {
            interfaces.to_vec()
        } else if all_interfaces.contains(key.as_str()) {
            vec![key]
        } else {
            bail!(
                "Invalid extra.discovery key {key}; expected one of: {}",
                abstractions.into_iter().collect::<Vec<_>>().join(", ")
            );
        };

        for interface in interfaces {
            cases.push_str("            case ");
            cases.push_str(&php_string(interface));
            cases.push_str(": return [['class' => ");
            cases.push_str(&php_string(class));
            cases.push_str("]];\n");
        }
    }

    Ok(Some(format!(
        "<?php\n\nnamespace Http\\Discovery\\Strategy;\n\nclass GeneratedDiscoveryStrategy implements DiscoveryStrategy\n{{\n    public static function getCandidates($type)\n    {{\n        switch ($type) {{\n{cases}            default: return [];\n        }}\n    }}\n}}\n"
    )))
}

fn php_string(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct MissingPackages {
    production: BTreeSet<String>,
    development: BTreeSet<String>,
}

#[derive(Clone, Copy)]
struct InstalledPackage<'a> {
    package: &'a LockedPackage,
    development: bool,
}

fn missing_packages(manifest: &RiffManifest, lock: &RiffLockfile) -> MissingPackages {
    // Root requirements participate only when the root directly opts in. A
    // dependency can independently opt in by requiring discovery itself.
    let mut requirements = if manifest.require.contains_key(PACKAGE_NAME) {
        [
            manifest.require.keys().cloned().collect::<BTreeSet<_>>(),
            manifest
                .require_dev
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
        ]
    } else {
        [BTreeSet::new(), BTreeSet::new()]
    };
    for package in &lock.packages {
        if package.require.contains_key(PACKAGE_NAME) {
            requirements[0].extend(package.require.keys().cloned());
        }
    }
    for package in &lock.packages_dev {
        if package.require.contains_key(PACKAGE_NAME) {
            requirements[1].extend(package.require.keys().cloned());
        }
    }

    let installed: HashMap<_, _> = lock
        .packages
        .iter()
        .map(|package| {
            (
                package.name.as_str(),
                InstalledPackage {
                    package,
                    development: false,
                },
            )
        })
        .chain(lock.packages_dev.iter().map(|package| {
            (
                package.name.as_str(),
                InstalledPackage {
                    package,
                    development: true,
                },
            )
        }))
        .collect();
    let pinned = pinned_abstractions(&manifest.extra);
    let mut missing = MissingPackages::default();

    collect_missing(
        &requirements[0],
        false,
        &installed,
        &pinned,
        &mut missing.production,
    );
    collect_missing(
        &requirements[1],
        true,
        &installed,
        &pinned,
        &mut missing.development,
    );
    for package in &missing.production {
        missing.development.remove(package);
    }
    missing
}

fn collect_missing(
    requirements: &BTreeSet<String>,
    development: bool,
    installed: &HashMap<&str, InstalledPackage<'_>>,
    pinned: &BTreeSet<&str>,
    missing: &mut BTreeSet<String>,
) {
    let mut queue: VecDeque<&str> = requirements
        .iter()
        .filter_map(|requirement| provide_rule(requirement).map(|_| requirement.as_str()))
        .collect();
    let mut visited = BTreeSet::new();

    while let Some(abstraction) = queue.pop_front() {
        if !visited.insert(abstraction) || pinned.contains(abstraction) {
            continue;
        }
        let candidates = provide_rule(abstraction).expect("queued rules exist");
        let candidate = choose_candidate(candidates, development, installed);

        if !is_available(
            candidate.package,
            candidate.constraint,
            development,
            installed,
        ) {
            missing.insert(candidate.package.to_string());
        }
        for dependency in candidate.dependencies {
            if provide_rule(dependency).is_some() {
                queue.push_back(dependency);
            } else if !is_available(dependency, None, development, installed) {
                missing.insert((*dependency).to_string());
            }
        }
    }
}

fn choose_candidate<'a>(
    candidates: &'a [Candidate],
    development: bool,
    installed: &HashMap<&str, InstalledPackage<'_>>,
) -> &'a Candidate {
    if let Some(candidate) = candidates.iter().find(|candidate| {
        is_available(
            candidate.package,
            candidate.constraint,
            development,
            installed,
        )
    }) {
        return candidate;
    }

    for (candidate_name, sticky_name, sticky_constraint) in STICKINESS_RULES {
        if candidates
            .iter()
            .any(|candidate| candidate.package == *candidate_name)
            && is_available(sticky_name, *sticky_constraint, development, installed)
        {
            return candidates
                .iter()
                .find(|candidate| candidate.package == *candidate_name)
                .expect("stickiness candidate exists");
        }
    }

    &candidates[0]
}

fn is_available(
    name: &str,
    constraint: Option<&str>,
    development: bool,
    installed: &HashMap<&str, InstalledPackage<'_>>,
) -> bool {
    let Some(installed) = installed.get(name) else {
        return false;
    };
    if !development && installed.development {
        return false;
    }
    constraint.is_none_or(|constraint| {
        VersionParser::new()
            .parse_constraints_cached(constraint)
            .is_ok_and(|constraint| constraint.satisfies(&installed.package.version))
    })
}

fn provide_rule(abstraction: &str) -> Option<&'static [Candidate]> {
    PROVIDE_RULES
        .iter()
        .find_map(|(name, candidates)| (*name == abstraction).then_some(*candidates))
}

fn pinned_abstractions(extra: &serde_json::Value) -> BTreeSet<&str> {
    let Some(pinned) = extra
        .get("discovery")
        .and_then(serde_json::Value::as_object)
    else {
        return BTreeSet::new();
    };
    INTERFACE_MAP
        .iter()
        .filter_map(|(abstraction, interfaces)| {
            (pinned.contains_key(*abstraction)
                || interfaces
                    .iter()
                    .all(|interface| pinned.contains_key(*interface)))
            .then_some(*abstraction)
        })
        .collect()
}

fn run_require(
    riff: &Riff,
    packages: &BTreeSet<String>,
    development: bool,
    depth: u8,
) -> Result<()> {
    let mut command = riff.runtime.riff_command();
    command
        .arg("require")
        .args(packages)
        .arg("-d")
        .arg(&riff.working_dir)
        .env("RIFF_PHP_HTTP_DISCOVERY_DEPTH", depth.to_string());
    if development {
        command.arg("--dev");
    }

    let process_output = ProcessRunner::new(riff.output())
        .with_timeout_seconds(riff.config.process_timeout)
        .execute(&mut command)
        .context("Failed to auto-install a php-http/discovery implementation")?;
    if !process_output.status.success() {
        bail!(
            "php-http/discovery implementation installation failed with {}",
            process_output.status
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use super::*;

    fn locked(name: &str, version: &str, requires: &[(&str, &str)]) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            require: requires
                .iter()
                .map(|(name, constraint)| (name.to_string(), constraint.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn composer_requiring(abstraction: &str) -> RiffManifest {
        RiffManifest {
            require: IndexMap::from([
                (PACKAGE_NAME.to_string(), "^1.20".to_string()),
                (abstraction.to_string(), "*".to_string()),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn generates_strategy_for_abstractions_and_interfaces() {
        let extra = serde_json::json!({
            "discovery": {
                "psr/http-client-implementation": "App\\HttpClient",
                "Psr\\Http\\Message\\UriFactoryInterface": "App\\UriFactory"
            }
        });
        let code = generated_strategy(&extra).unwrap().unwrap();
        assert!(code.contains("case 'Psr\\\\Http\\\\Client\\\\ClientInterface'"));
        assert!(code.contains("'class' => 'App\\\\HttpClient'"));
        assert!(code.contains("case 'Psr\\\\Http\\\\Message\\\\UriFactoryInterface'"));
    }

    #[test]
    fn rejects_invalid_pin() {
        let extra = serde_json::json!({"discovery": {"unknown": "App\\Client"}});
        assert!(generated_strategy(&extra).is_err());
    }

    #[test]
    fn selects_default_psr18_client_and_factory() {
        let manifest = composer_requiring("psr/http-client-implementation");
        let lock = RiffLockfile {
            packages: vec![locked(PACKAGE_NAME, "1.20.0", &[])],
            ..Default::default()
        };

        let missing = missing_packages(&manifest, &lock);
        assert_eq!(
            missing.production,
            BTreeSet::from([
                "nyholm/psr7".to_string(),
                "psr/http-client".to_string(),
                "symfony/http-client".to_string(),
            ])
        );
    }

    #[test]
    fn a_transitive_dependency_can_opt_in_to_discovery() {
        let manifest = RiffManifest {
            require: IndexMap::from([("vendor/sdk".to_string(), "^1".to_string())]),
            ..Default::default()
        };
        let lock = RiffLockfile {
            packages: vec![
                locked(PACKAGE_NAME, "1.20.0", &[]),
                locked(
                    "vendor/sdk",
                    "1.0.0",
                    &[
                        (PACKAGE_NAME, "^1.20"),
                        ("psr/http-client-implementation", "*"),
                    ],
                ),
            ],
            ..Default::default()
        };

        assert!(missing_packages(&manifest, &lock)
            .production
            .contains("symfony/http-client"));
    }

    #[test]
    fn keeps_an_installed_guzzle_client_sticky() {
        let manifest = composer_requiring("php-http/client-implementation");
        let lock = RiffLockfile {
            packages: vec![
                locked(PACKAGE_NAME, "1.20.0", &[]),
                locked("guzzlehttp/guzzle", "7.9.0", &[]),
            ],
            ..Default::default()
        };

        let missing = missing_packages(&manifest, &lock);
        assert_eq!(
            missing.production,
            BTreeSet::from(["php-http/guzzle7-adapter".to_string()])
        );
    }

    #[test]
    fn a_pin_suppresses_automatic_installation() {
        let manifest = RiffManifest {
            extra: serde_json::json!({
                "discovery": {"psr/http-client-implementation": "App\\Client"}
            }),
            ..composer_requiring("psr/http-client-implementation")
        };
        let lock = RiffLockfile {
            packages: vec![locked(PACKAGE_NAME, "1.20.0", &[])],
            ..Default::default()
        };

        assert!(missing_packages(&manifest, &lock).production.is_empty());
    }
}
