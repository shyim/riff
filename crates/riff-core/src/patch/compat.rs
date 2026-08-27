//! Composer Patches compatibility for Riff's core patch subsystem.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context};
use async_trait::async_trait;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use url::Url;

use crate::installer::PackageInstallHook;
use crate::package::Package;
use crate::riff::Riff;
use crate::util::canonical_package_name;
use crate::{Result, RiffError};

use super::engine;
use super::native::resolve_native_patches;

const PLUGIN_PACKAGE: &str = "cweagans/composer-patches";
const CONFIGURABLE_PLUGIN_PACKAGE: &str = "cweagans/composer-configurable-plugin";
const DEFAULT_PATCHES_FILE: &str = "patches.json";
const PATCHES_LOCK_FILE: &str = "patches.lock.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PatchDefinition {
    package: String,
    description: String,
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    depth: Option<u32>,
    #[serde(default = "empty_object")]
    extra: Value,
}

type PatchCollection = IndexMap<String, Vec<PatchDefinition>>;

#[derive(Clone, Debug)]
struct PreparedPatch {
    description: String,
    url: String,
    sha256: String,
    depths: Vec<u32>,
    path: PathBuf,
    strict: bool,
    legacy_report: bool,
}

struct PreparedPatchSet {
    patches: HashMap<String, Vec<PreparedPatch>>,
    write_legacy_report: bool,
    output: crate::output::Output,
    // Remote patch files must remain alive until every package operation ends.
    _temporary_directory: TempDir,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompatibilityRelockResult {
    pub patch_count: usize,
    pub legacy: bool,
}

#[derive(Debug)]
struct PatchSettings {
    patches_file: String,
    package_depths: HashMap<String, u32>,
    default_patch_depth: u32,
    ignore_dependency_patches: HashSet<String>,
    disabled_resolvers: HashSet<String>,
    remote_downloads_enabled: bool,
    standard_patcher_enabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PluginGeneration {
    LegacyV1,
    V2,
}

#[derive(Debug, Default)]
struct LegacySettings {
    package_depths: HashMap<String, u32>,
    ignored_dependency_patches: HashMap<String, HashMap<String, HashSet<String>>>,
}

enum PreparationMode {
    Legacy(LegacySettings),
    V2(PatchSettings),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PatchLocation {
    Local,
    Http,
    Https,
}

impl Default for PatchSettings {
    fn default() -> Self {
        Self {
            patches_file: DEFAULT_PATCHES_FILE.to_string(),
            package_depths: HashMap::new(),
            default_patch_depth: 1,
            ignore_dependency_patches: HashSet::new(),
            disabled_resolvers: HashSet::new(),
            remote_downloads_enabled: true,
            standard_patcher_enabled: true,
        }
    }
}

pub(crate) async fn prepare(
    riff: &Riff,
    packages: &[Package],
    dry_run: bool,
) -> anyhow::Result<Option<Arc<dyn PackageInstallHook>>> {
    prepare_with_options(riff, packages, dry_run, true).await
}

async fn prepare_with_options(
    riff: &Riff,
    packages: &[Package],
    dry_run: bool,
    write_generated_lock: bool,
) -> anyhow::Result<Option<Arc<dyn PackageInstallHook>>> {
    let plugin = packages
        .iter()
        .find(|package| canonical_package_name(&package.name) == PLUGIN_PACKAGE);
    let has_native = riff
        .manifest
        .extra
        .get("riff")
        .and_then(|value| value.get("patched-dependencies"))
        .is_some_and(is_nonempty);
    let has_compat = has_compat_configuration(riff, packages);
    if !has_native && !has_compat {
        return Ok(None);
    }

    let generation = plugin
        .and_then(|plugin| plugin_generation(plugin.version()))
        .unwrap_or_else(|| detect_generation(&riff.manifest.extra));

    // A dry run may validate plugin compatibility, but it must not read or
    // download patches, create a lock, or invoke git.
    if dry_run {
        return Ok(None);
    }

    let lock_path = riff.working_dir.join(PATCHES_LOCK_FILE);
    let (mut patches, generate_lock, preparation_mode) = match generation {
        PluginGeneration::LegacyV1 => {
            let settings = parse_legacy_settings(&riff.manifest.extra)?;
            let patches =
                resolve_legacy_definitions(&riff.working_dir, &riff.manifest.extra, packages)?;
            (patches, false, PreparationMode::Legacy(settings))
        }
        PluginGeneration::V2 => {
            let settings = parse_settings(&riff.manifest.extra)?;
            let locked_patches = lock_path
                .exists()
                .then(|| read_lock_file(&lock_path))
                .transpose()?
                .flatten();
            let (patches, generate_lock) = if let Some(patches) = locked_patches {
                (patches, false)
            } else {
                (
                    resolve_definitions(
                        &riff.working_dir,
                        &riff.manifest.extra,
                        &settings,
                        packages,
                    )?,
                    true,
                )
            };
            (patches, generate_lock, PreparationMode::V2(settings))
        }
    };

    let native_patches = resolve_native_patches(&riff.working_dir, &riff.manifest.extra, packages)?;
    if patches.values().all(Vec::is_empty) && native_patches.is_empty() {
        if generate_lock && write_generated_lock {
            write_lock_file(&lock_path, &patches)?;
        }
        return Ok(None);
    }

    let patch_cache_dir = crate::cache::runtime_cache_dir().join("patches");
    tokio::fs::create_dir_all(&patch_cache_dir)
        .await
        .with_context(|| {
            format!(
                "Failed to create the Composer patches cache directory at {}",
                patch_cache_dir.display()
            )
        })?;
    let temporary_directory = tempfile::tempdir_in(&patch_cache_dir)
        .context("Failed to create a temporary directory for Composer patches")?;
    let mut prepared = prepare_patches(
        riff,
        &preparation_mode,
        &mut patches,
        temporary_directory.path(),
    )
    .await?;
    if generation == PluginGeneration::LegacyV1
        && legacy_flag(
            &riff.manifest.extra,
            "composer-exit-on-patch-failure",
            "COMPOSER_EXIT_ON_PATCH_FAILURE",
        )
    {
        for patches in prepared.values_mut() {
            for patch in patches {
                patch.strict = true;
            }
        }
    }
    for patch in native_patches {
        prepared
            .entry(patch.package.clone())
            .or_default()
            .push(PreparedPatch {
                description: format!("Riff native patch {}", patch.selector),
                url: patch.path.display().to_string(),
                sha256: patch.sha256,
                depths: vec![1],
                path: patch.path,
                strict: true,
                legacy_report: false,
            });
    }

    let selected_packages: HashSet<_> = packages
        .iter()
        .map(|package| canonical_package_name(&package.name).into_owned())
        .collect();
    prepared.retain(|package, _| selected_packages.contains(package));

    if generate_lock && write_generated_lock {
        write_lock_file(&lock_path, &patches)?;
    }

    if prepared.values().all(Vec::is_empty) {
        return Ok(None);
    }

    Ok(Some(Arc::new(PreparedPatchSet {
        patches: prepared,
        output: riff.output().clone(),
        write_legacy_report: generation == PluginGeneration::LegacyV1
            && !legacy_flag(
                &riff.manifest.extra,
                "composer-patches-skip-reporting",
                "COMPOSER_PATCHES_SKIP_REPORTING",
            ),
        _temporary_directory: temporary_directory,
    })))
}

pub(crate) async fn desired_fingerprints(
    riff: &Riff,
    packages: &[Package],
) -> anyhow::Result<BTreeMap<String, String>> {
    Ok(prepare_with_options(riff, packages, false, false)
        .await?
        .map(|hook| hook.fingerprints())
        .unwrap_or_default())
}

pub async fn relock_compatibility(
    riff: &Riff,
    packages: &[Package],
) -> anyhow::Result<Option<CompatibilityRelockResult>> {
    if !has_compat_configuration(riff, packages) {
        return Ok(None);
    }
    let plugin = packages
        .iter()
        .find(|package| canonical_package_name(&package.name) == PLUGIN_PACKAGE);
    let generation = plugin
        .and_then(|plugin| plugin_generation(plugin.version()))
        .unwrap_or_else(|| detect_generation(&riff.manifest.extra));
    if generation == PluginGeneration::LegacyV1 {
        let settings = parse_legacy_settings(&riff.manifest.extra)?;
        let mut patches =
            resolve_legacy_definitions(&riff.working_dir, &riff.manifest.extra, packages)?;
        let patch_count = patches.values().map(Vec::len).sum();
        if patch_count > 0 {
            let patch_cache_dir = crate::cache::runtime_cache_dir().join("patches");
            tokio::fs::create_dir_all(&patch_cache_dir).await?;
            let temporary_directory = tempfile::tempdir_in(&patch_cache_dir)?;
            let _ = prepare_patches(
                riff,
                &PreparationMode::Legacy(settings),
                &mut patches,
                temporary_directory.path(),
            )
            .await?;
        }
        return Ok(Some(CompatibilityRelockResult {
            patch_count,
            legacy: true,
        }));
    }

    let mut settings = parse_settings(&riff.manifest.extra)?;
    let mut patches =
        resolve_definitions(&riff.working_dir, &riff.manifest.extra, &settings, packages)?;
    let patch_count = patches.values().map(Vec::len).sum();
    let patch_cache_dir = crate::cache::runtime_cache_dir().join("patches");
    tokio::fs::create_dir_all(&patch_cache_dir).await?;
    let temporary_directory = tempfile::tempdir_in(&patch_cache_dir)?;
    // Relocking resolves, downloads and hashes patches; disabling runtime
    // patchers does not disable the lock operation itself.
    settings.standard_patcher_enabled = true;
    let mode = PreparationMode::V2(settings);
    let _ = prepare_patches(riff, &mode, &mut patches, temporary_directory.path()).await?;
    write_lock_file(&riff.working_dir.join(PATCHES_LOCK_FILE), &patches)?;
    Ok(Some(CompatibilityRelockResult {
        patch_count,
        legacy: false,
    }))
}

fn has_compat_configuration(riff: &Riff, packages: &[Package]) -> bool {
    let extra = &riff.manifest.extra;
    riff.working_dir.join(PATCHES_LOCK_FILE).is_file()
        || riff.working_dir.join(DEFAULT_PATCHES_FILE).is_file()
        || [
            "patches",
            "patches-file",
            "patchLevel",
            "patches-ignore",
            "enable-patching",
            "composer-exit-on-patch-failure",
            "composer-patches-skip-reporting",
            "composer-patches",
        ]
        .iter()
        .any(|key| extra.get(*key).is_some_and(is_nonempty))
        || packages.iter().any(|package| {
            package
                .extra
                .as_ref()
                .and_then(|extra| extra.get("patches"))
                .is_some_and(is_nonempty)
        })
}

fn detect_generation(extra: &Value) -> PluginGeneration {
    if [
        "patchLevel",
        "patches-file",
        "patches-ignore",
        "enable-patching",
        "composer-exit-on-patch-failure",
        "composer-patches-skip-reporting",
    ]
    .iter()
    .any(|key| extra.get(*key).is_some())
    {
        PluginGeneration::LegacyV1
    } else {
        PluginGeneration::V2
    }
}

fn plugin_generation(version: &str) -> Option<PluginGeneration> {
    match version.trim_start_matches(['v', 'V']).split('.').next() {
        Some("1") => Some(PluginGeneration::LegacyV1),
        Some("2") => Some(PluginGeneration::V2),
        _ => None,
    }
}

fn parse_settings(extra: &Value) -> anyhow::Result<PatchSettings> {
    let Some(config) = extra.get("composer-patches") else {
        return Ok(PatchSettings::default());
    };
    let config = config
        .as_object()
        .context("extra.composer-patches must be an object")?;

    let mut settings = PatchSettings {
        disabled_resolvers: parse_class_list(config.get("disable-resolvers"), "disable-resolvers")?,
        ..PatchSettings::default()
    };
    validate_known_classes(
        &settings.disabled_resolvers,
        &[
            "\\cweagans\\Composer\\Resolver\\RootComposer",
            "\\cweagans\\Composer\\Resolver\\PatchesFile",
            "\\cweagans\\Composer\\Resolver\\Dependencies",
        ],
        "resolver",
    )?;
    let disabled_downloaders =
        parse_class_list(config.get("disable-downloaders"), "disable-downloaders")?;
    validate_known_classes(
        &disabled_downloaders,
        &["\\cweagans\\Composer\\Downloader\\ComposerDownloader"],
        "downloader",
    )?;
    settings.remote_downloads_enabled =
        !disabled_downloaders.contains("\\cweagans\\Composer\\Downloader\\ComposerDownloader");
    let disabled_patchers = parse_class_list(config.get("disable-patchers"), "disable-patchers")?;
    validate_known_classes(
        &disabled_patchers,
        &[
            "\\cweagans\\Composer\\Patcher\\FreeformPatcher",
            "\\cweagans\\Composer\\Patcher\\GitPatcher",
            "\\cweagans\\Composer\\Patcher\\GitInitPatcher",
        ],
        "patcher",
    )?;
    settings.standard_patcher_enabled = !(disabled_patchers
        .contains("\\cweagans\\Composer\\Patcher\\GitPatcher")
        && disabled_patchers.contains("\\cweagans\\Composer\\Patcher\\GitInitPatcher"));
    settings.ignore_dependency_patches = parse_string_list(
        config.get("ignore-dependency-patches"),
        "ignore-dependency-patches",
    )?
    .into_iter()
    .map(|package| canonical_package_name(&package).into_owned())
    .collect();
    if let Some(value) = config.get("patches-file") {
        settings.patches_file = value
            .as_str()
            .context("extra.composer-patches.patches-file must be a string")?
            .to_string();
    }
    if let Some(value) = config.get("default-patch-depth") {
        settings.default_patch_depth = parse_depth(value, "default-patch-depth")?;
    }
    if let Some(value) = config.get("package-depths") {
        let depths = value
            .as_object()
            .context("extra.composer-patches.package-depths must be an object")?;
        for (package, depth) in depths {
            settings.package_depths.insert(
                canonical_package_name(package).into_owned(),
                parse_depth(depth, &format!("package-depths.{package}"))?,
            );
        }
    }
    Ok(settings)
}

fn parse_class_list(value: Option<&Value>, field: &str) -> anyhow::Result<HashSet<String>> {
    Ok(parse_string_list(value, field)?.into_iter().collect())
}

fn parse_string_list(value: Option<&Value>, field: &str) -> anyhow::Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .with_context(|| format!("extra.composer-patches.{field} must be an array"))?;
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value.as_str().map(ToOwned::to_owned).with_context(|| {
                format!("extra.composer-patches.{field}[{index}] must be a string")
            })
        })
        .collect()
}

fn validate_known_classes(
    configured: &HashSet<String>,
    known: &[&str],
    kind: &str,
) -> anyhow::Result<()> {
    if let Some(unknown) = configured
        .iter()
        .find(|class| !known.contains(&class.as_str()))
    {
        bail!(
            "custom Composer Patches {kind} {unknown} cannot execute in Riff; remove it from extra.composer-patches.disable-{kind}s"
        );
    }
    Ok(())
}

fn parse_legacy_settings(extra: &Value) -> anyhow::Result<LegacySettings> {
    let mut settings = LegacySettings::default();
    if let Some(value) = extra.get("patchLevel") {
        let depths = value
            .as_object()
            .context("legacy extra.patchLevel must be an object")?;
        for (package, depth) in depths {
            let depth = depth.as_str().with_context(|| {
                format!("legacy extra.patchLevel.{package} must use a value such as -p1")
            })?;
            let number = depth.strip_prefix("-p").with_context(|| {
                format!("legacy extra.patchLevel.{package} must use a value such as -p1")
            })?;
            let number = number.parse::<u32>().with_context(|| {
                format!("legacy extra.patchLevel.{package} must use a value such as -p1")
            })?;
            settings
                .package_depths
                .insert(canonical_package_name(package).into_owned(), number);
        }
    }
    if let Some(value) = extra.get("patches-ignore") {
        let providers = value
            .as_object()
            .context("legacy extra.patches-ignore must be an object")?;
        for (provider, value) in providers {
            let targets = value.as_object().with_context(|| {
                format!("legacy extra.patches-ignore.{provider} must be an object")
            })?;
            let mut ignored_targets = HashMap::new();
            for (target, value) in targets {
                let definitions = value.as_object().with_context(|| {
                    format!(
                        "legacy extra.patches-ignore.{provider}.{target} must use compact patch format"
                    )
                })?;
                let mut urls = HashSet::new();
                for (description, url) in definitions {
                    let url = url.as_str().with_context(|| {
                        format!(
                            "legacy ignored patch {target}: {description} must have a string URL"
                        )
                    })?;
                    urls.insert(url.to_string());
                }
                ignored_targets.insert(canonical_package_name(target).into_owned(), urls);
            }
            settings.ignored_dependency_patches.insert(
                canonical_package_name(provider).into_owned(),
                ignored_targets,
            );
        }
    }
    Ok(settings)
}

fn legacy_patching_enabled(extra: &Value) -> bool {
    if extra.get("patches").is_some_and(is_nonempty)
        || extra.get("patches-ignore").is_some_and(is_nonempty)
        || extra
            .get("patches-file")
            .is_some_and(|value| !value.is_null())
    {
        true
    } else {
        extra.get("enable-patching").is_some_and(is_php_truthy)
    }
}

fn is_nonempty(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        Value::Number(_) => true,
    }
}

fn legacy_flag(extra: &Value, key: &str, environment: &str) -> bool {
    let configured = extra.get(key).is_some_and(is_php_truthy);
    let environment = std::env::var_os(environment).is_some_and(|value| {
        let value = value.to_string_lossy();
        !value.is_empty() && value != "0"
    });
    configured || environment
}

fn is_php_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::String(value) => !value.is_empty() && value != "0",
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
    }
}

fn parse_depth(value: &Value, field: &str) -> anyhow::Result<u32> {
    let depth = value.as_u64().with_context(|| {
        format!("extra.composer-patches.{field} must be a non-negative integer")
    })?;
    u32::try_from(depth).with_context(|| format!("patch depth for {field} is too large"))
}

fn resolve_legacy_definitions(
    root: &Path,
    extra: &Value,
    packages: &[Package],
) -> anyhow::Result<PatchCollection> {
    let settings = parse_legacy_settings(extra)?;
    if !legacy_patching_enabled(extra) {
        return Ok(PatchCollection::new());
    }
    let root_definitions = if let Some(patches) = extra.get("patches") {
        Some((patches.clone(), "root".to_string()))
    } else if let Some(patches_file) = extra.get("patches-file") {
        let patches_file = patches_file
            .as_str()
            .context("legacy extra.patches-file must be a string")?;
        let path = resolve_project_file(root, patches_file, "legacy patches file")?;
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let json: Value = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        Some((
            json.get("patches").unwrap_or(&json).clone(),
            format!("patches-file:{patches_file}"),
        ))
    } else {
        None
    };

    let mut patches = PatchCollection::new();
    if let Some((definitions, provenance)) = root_definitions {
        merge_legacy_patch_value(&mut patches, &definitions, &provenance, None, None)?;
    }
    for source in packages {
        let Some(definitions) = source.extra.as_ref().and_then(|extra| extra.get("patches")) else {
            continue;
        };
        if !is_nonempty(definitions) {
            continue;
        }
        merge_legacy_patch_value(
            &mut patches,
            definitions,
            &format!("dependency:{}", source.name),
            Some(source),
            settings
                .ignored_dependency_patches
                .get(canonical_package_name(&source.name).as_ref()),
        )?;
    }
    Ok(patches)
}

fn merge_legacy_patch_value(
    patches: &mut PatchCollection,
    definitions: &Value,
    provenance: &str,
    dependency: Option<&Package>,
    ignored: Option<&HashMap<String, HashSet<String>>>,
) -> anyhow::Result<()> {
    let targets = definitions
        .as_object()
        .context("legacy patch definitions must be an object keyed by package name")?;
    for (package, definitions) in targets {
        let compact = definitions.as_object().with_context(|| {
            format!(
                "legacy patches for {package} must use compact description-to-URL object format"
            )
        })?;
        let package = canonical_package_name(package).into_owned();
        let package_patches = patches.entry(package.clone()).or_default();
        for (description, url) in compact {
            let url = url.as_str().with_context(|| {
                format!("legacy patch {package}: {description} must have a string URL")
            })?;
            if ignored
                .and_then(|targets| targets.get(&package))
                .is_some_and(|urls| urls.contains(url))
            {
                continue;
            }
            if let (Some(dependency), PatchLocation::Local) = (dependency, patch_location(url)?) {
                bail!(
                    "dependency-provided legacy patch '{}' from {} must use an HTTP(S) URL, not local path {}",
                    description,
                    dependency.name,
                    url
                );
            }
            let duplicate = package_patches.iter().any(|known| known.url == url);
            if !duplicate {
                package_patches.push(PatchDefinition {
                    package: package.clone(),
                    description: description.clone(),
                    url: url.to_string(),
                    sha256: None,
                    depth: None,
                    extra: provenance_extra(provenance),
                });
            }
        }
    }
    Ok(())
}

fn resolve_definitions(
    root: &Path,
    extra: &Value,
    settings: &PatchSettings,
    packages: &[Package],
) -> anyhow::Result<PatchCollection> {
    let mut patches = PatchCollection::new();
    if !resolver_disabled(settings, "RootComposer") {
        if let Some(root_patches) = extra.get("patches") {
            merge_patch_value(&mut patches, root_patches, "root")?;
        }
    }

    let patches_file = resolve_optional_project_file(root, &settings.patches_file, "patches file")?;
    if !resolver_disabled(settings, "PatchesFile") && patches_file.exists() {
        let content = std::fs::read_to_string(&patches_file)
            .with_context(|| format!("Failed to read {}", patches_file.display()))?;
        let json: Value = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", patches_file.display()))?;
        let definitions = json.get("patches").with_context(|| {
            format!(
                "{} must contain a top-level patches object",
                patches_file.display()
            )
        })?;
        merge_patch_value(
            &mut patches,
            definitions,
            &format!("patches-file:{}", settings.patches_file),
        )?;
    }
    if !resolver_disabled(settings, "Dependencies") {
        for source in packages {
            let source_name = canonical_package_name(&source.name).into_owned();
            if settings.ignore_dependency_patches.contains(&source_name) {
                continue;
            }
            let Some(definitions) = source.extra.as_ref().and_then(|extra| extra.get("patches"))
            else {
                continue;
            };
            if !is_nonempty(definitions) {
                continue;
            }
            let before: HashMap<_, _> = patches
                .iter()
                .map(|(target, values)| (target.clone(), values.len()))
                .collect();
            merge_patch_value(
                &mut patches,
                definitions,
                &format!("dependency:{source_name}"),
            )?;
            for (target, values) in &patches {
                for patch in values.iter().skip(*before.get(target).unwrap_or(&0)) {
                    if patch_location(&patch.url)? == PatchLocation::Local {
                        bail!(
                            "dependency-provided patch '{}' from {} must use an HTTP(S) URL, not local path {}",
                            patch.description,
                            source.name,
                            patch.url
                        );
                    }
                }
            }
        }
    }
    Ok(patches)
}

fn resolver_disabled(settings: &PatchSettings, short_name: &str) -> bool {
    settings
        .disabled_resolvers
        .iter()
        .any(|class| class.rsplit('\\').next() == Some(short_name))
}

fn merge_patch_value(
    collection: &mut PatchCollection,
    value: &Value,
    provenance: &str,
) -> anyhow::Result<()> {
    let targets = value
        .as_object()
        .context("patch definitions must be an object keyed by package name")?;
    for (package, definitions) in targets {
        let package = canonical_package_name(package).into_owned();
        let parsed = parse_package_definitions(&package, definitions, provenance)?;
        let existing = collection.entry(package).or_default();
        for patch in parsed {
            let duplicate = existing.iter().any(|known| {
                known.url == patch.url
                    || matches!((&known.sha256, &patch.sha256), (Some(a), Some(b)) if a == b)
            });
            if !duplicate {
                existing.push(patch);
            }
        }
    }
    Ok(())
}

fn parse_package_definitions(
    package: &str,
    value: &Value,
    provenance: &str,
) -> anyhow::Result<Vec<PatchDefinition>> {
    match value {
        Value::Object(compact) => compact
            .iter()
            .map(|(description, url)| {
                Ok(PatchDefinition {
                    package: package.to_string(),
                    description: description.clone(),
                    url: url
                        .as_str()
                        .with_context(|| {
                            format!("compact patch {package}: {description} must have a string URL")
                        })?
                        .to_string(),
                    sha256: None,
                    depth: None,
                    extra: provenance_extra(provenance),
                })
            })
            .collect(),
        Value::Array(expanded) => expanded
            .iter()
            .enumerate()
            .map(|(index, definition)| {
                let object = definition.as_object().with_context(|| {
                    format!("expanded patch {package}[{index}] must be an object")
                })?;
                let description = required_string(object, "description", package, index)?;
                let url = required_string(object, "url", package, index)?;
                let sha256 = optional_string(object, "sha256", package, index)?;
                validate_sha256(sha256.as_deref(), package, &description)?;
                let depth = object
                    .get("depth")
                    .filter(|value| !value.is_null())
                    .map(|value| parse_patch_depth(value, package, index))
                    .transpose()?;
                let mut extra = object
                    .get("extra")
                    .cloned()
                    .unwrap_or_else(empty_object);
                let extra_object = extra.as_object_mut().with_context(|| {
                    format!("expanded patch {package}[{index}].extra must be an object")
                })?;
                if extra_object.contains_key("freeform") {
                    bail!(
                        "expanded patch {package}[{index}] uses extra.freeform, which riff does not support"
                    );
                }
                extra_object.insert(
                    "provenance".to_string(),
                    Value::String(provenance.to_string()),
                );
                Ok(PatchDefinition {
                    package: package.to_string(),
                    description,
                    url,
                    sha256,
                    depth,
                    extra,
                })
            })
            .collect(),
        _ => bail!("patches for {package} must use compact object or expanded array format"),
    }
}

fn required_string(
    object: &Map<String, Value>,
    field: &str,
    package: &str,
    index: usize,
) -> anyhow::Result<String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .with_context(|| format!("expanded patch {package}[{index}].{field} must be a string"))
}

fn optional_string(
    object: &Map<String, Value>,
    field: &str,
    package: &str,
    index: usize,
) -> anyhow::Result<Option<String>> {
    object
        .get(field)
        .map(|value| {
            if value.is_null() {
                return Ok(None);
            }
            value
                .as_str()
                .map(ToOwned::to_owned)
                .with_context(|| {
                    format!("expanded patch {package}[{index}].{field} must be a string")
                })
                .map(Some)
        })
        .transpose()
        .map(Option::flatten)
}

fn parse_patch_depth(value: &Value, package: &str, index: usize) -> anyhow::Result<u32> {
    let depth = value
        .as_u64()
        .with_context(|| format!("expanded patch {package}[{index}].depth must be an integer"))?;
    u32::try_from(depth).context("patch depth is too large")
}

fn provenance_extra(provenance: &str) -> Value {
    Value::Object(Map::from_iter([(
        "provenance".to_string(),
        Value::String(provenance.to_string()),
    )]))
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

fn read_lock_file(path: &Path) -> anyhow::Result<Option<PatchCollection>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let json: Value =
        serde_json::from_str(&content).with_context(|| format!("Malformed {}", path.display()))?;
    let object = json
        .as_object()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    let Some(patches) = object.get("patches") else {
        return Ok(None);
    };
    let targets = patches
        .as_object()
        .with_context(|| format!("{}.patches must be an object", path.display()))?;

    let mut collection = PatchCollection::new();
    for (target, definitions) in targets {
        let definitions = definitions
            .as_array()
            .with_context(|| format!("locked patches for {target} must be an array"))?;
        for (index, definition) in definitions.iter().enumerate() {
            let patch: PatchDefinition = serde_json::from_value(definition.clone())
                .with_context(|| format!("Invalid locked patch {target}[{index}]"))?;
            if canonical_package_name(&patch.package) != canonical_package_name(target) {
                bail!(
                    "locked patch {target}[{index}] names package {}, which does not match its collection key",
                    patch.package
                );
            }
            validate_sha256(patch.sha256.as_deref(), target, &patch.description)?;
            let extra = patch.extra.as_object().with_context(|| {
                format!("locked patch {target}[{index}].extra must be an object")
            })?;
            if extra.contains_key("freeform") {
                bail!("locked patch {target}[{index}] uses unsupported extra.freeform");
            }
            collection
                .entry(canonical_package_name(target).into_owned())
                .or_default()
                .push(patch);
        }
    }
    Ok(Some(collection))
}

async fn prepare_patches(
    riff: &Riff,
    mode: &PreparationMode,
    patches: &mut PatchCollection,
    temporary_directory: &Path,
) -> anyhow::Result<HashMap<String, Vec<PreparedPatch>>> {
    if matches!(mode, PreparationMode::V2(settings) if !settings.standard_patcher_enabled)
        && patches.values().any(|patches| !patches.is_empty())
    {
        bail!(
            "all standard Composer Patches patchers are disabled; Riff cannot apply the configured patches"
        );
    }
    let content_cache_dir = crate::cache::runtime_cache_dir().join("patches/content");
    tokio::fs::create_dir_all(&content_cache_dir)
        .await
        .with_context(|| format!("Failed to create {}", content_cache_dir.display()))?;
    let mut prepared = HashMap::new();
    let mut counter = 0usize;
    for (package, definitions) in patches {
        let mut package_patches = Vec::with_capacity(definitions.len());
        for definition in definitions {
            let location = patch_location(&definition.url)?;
            let path = if location != PatchLocation::Local {
                if matches!(mode, PreparationMode::V2(settings) if !settings.remote_downloads_enabled)
                {
                    bail!(
                        "ComposerDownloader is disabled, so remote patch {} cannot be downloaded",
                        definition.url
                    );
                }
                if location == PatchLocation::Http && riff.config.secure_http {
                    bail!(
                        "Refusing insecure HTTP patch URL {} while config.secure-http is enabled",
                        definition.url
                    );
                }
                let cached = if let Some(expected) = definition.sha256.as_deref() {
                    let candidate =
                        content_cache_dir.join(format!("{}.patch", expected.to_ascii_lowercase()));
                    if candidate.is_file()
                        && sha256_file(&candidate)
                            .await
                            .with_context(|| {
                                format!("Failed to verify cached patch {}", candidate.display())
                            })?
                            .eq_ignore_ascii_case(expected)
                    {
                        Some(candidate)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(cached) = cached {
                    cached
                } else {
                    let destination = temporary_directory.join(format!("{counter}.patch"));
                    counter += 1;
                    riff.http_client
                        .download(&definition.url, &destination, None::<fn(u64, u64)>)
                        .await
                        .with_context(|| {
                            format!(
                                "Failed to download patch '{}' for {package} from {}",
                                definition.description, definition.url
                            )
                        })?;
                    destination
                }
            } else {
                resolve_project_file(&riff.working_dir, &definition.url, "local patch")?
            };

            let actual_sha256 = sha256_file(&path).await.with_context(|| {
                format!(
                    "Failed to hash patch '{}' for {package} at {}",
                    definition.description,
                    path.display()
                )
            })?;
            if let Some(expected) = &definition.sha256 {
                if !expected.eq_ignore_ascii_case(&actual_sha256) {
                    bail!(
                        "SHA-256 mismatch for patch '{}' on {package}: expected {expected}, got {actual_sha256}",
                        definition.description
                    );
                }
            }
            definition.sha256 = Some(actual_sha256.clone());
            let path = if location != PatchLocation::Local {
                let cached = content_cache_dir.join(format!("{actual_sha256}.patch"));
                if path != cached {
                    cache_patch_atomically(&path, &cached).with_context(|| {
                        format!("Failed to cache remote patch at {}", cached.display())
                    })?;
                }
                cached
            } else {
                path
            };
            let path = if matches!(mode, PreparationMode::Legacy(_)) {
                normalize_legacy_patch(&path, temporary_directory, &mut counter)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to normalize legacy patch '{}' for {package}",
                            definition.description
                        )
                    })?
            } else {
                path
            };
            let depths = match mode {
                PreparationMode::Legacy(settings) => settings
                    .package_depths
                    .get(package)
                    .copied()
                    .map(|depth| vec![depth])
                    .unwrap_or_else(|| vec![1, 0, 2, 4]),
                PreparationMode::V2(settings) => {
                    let depth = resolve_depth(package, definition.depth, settings);
                    definition.depth = Some(depth);
                    vec![depth]
                }
            };
            package_patches.push(PreparedPatch {
                description: definition.description.clone(),
                url: definition.url.clone(),
                sha256: actual_sha256,
                depths,
                path,
                strict: matches!(mode, PreparationMode::V2(_)),
                legacy_report: matches!(mode, PreparationMode::Legacy(_)),
            });
        }
        prepared.insert(package.clone(), package_patches);
    }
    Ok(prepared)
}

async fn normalize_legacy_patch(
    source: &Path,
    temporary_directory: &Path,
    counter: &mut usize,
) -> anyhow::Result<PathBuf> {
    let contents = tokio::fs::read(source).await?;
    let Some(normalized) = normalize_legacy_patch_contents(&contents)? else {
        return Ok(source.to_path_buf());
    };
    let destination = temporary_directory.join(format!("normalized-{counter}.patch"));
    *counter += 1;
    tokio::fs::write(&destination, normalized).await?;
    Ok(destination)
}

fn normalize_legacy_patch_contents(contents: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let text = std::str::from_utf8(contents)
        .context("legacy compatibility normalization requires a UTF-8 text patch")?;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut replacements = HashMap::new();

    for (index, line) in lines.iter().enumerate() {
        if line.trim_end_matches(['\r', '\n']) != "--- /dev/null" {
            continue;
        }
        let Some(new_line) = lines.get(index + 1) else {
            continue;
        };
        let Some(new_path) = new_line.trim_end_matches(['\r', '\n']).strip_prefix("+++ ") else {
            continue;
        };
        let old_count = lines[index + 2..]
            .iter()
            .take_while(|line| !line.starts_with("--- "))
            .find_map(|line| legacy_hunk_old_count(line));
        if old_count.is_some_and(|count| count > 0) {
            let newline = if line.ends_with("\r\n") { "\r\n" } else { "\n" };
            replacements.insert(index, format!("--- {new_path}{newline}"));
        }
    }

    if replacements.is_empty() {
        return Ok(None);
    }
    let mut normalized = Vec::with_capacity(contents.len());
    for (index, line) in lines.iter().enumerate() {
        if let Some(replacement) = replacements.get(&index) {
            normalized.extend_from_slice(replacement.as_bytes());
        } else {
            normalized.extend_from_slice(line.as_bytes());
        }
    }
    Ok(Some(normalized))
}

fn legacy_hunk_old_count(line: &str) -> Option<u64> {
    let range = line.strip_prefix("@@ -")?.split_whitespace().next()?;
    match range.split_once(',') {
        Some((_, count)) => count.parse().ok(),
        None => Some(1),
    }
}

fn resolve_depth(package: &str, explicit: Option<u32>, settings: &PatchSettings) -> u32 {
    explicit
        .or_else(|| settings.package_depths.get(package).copied())
        .or_else(|| (package == "drupal/core").then_some(2))
        .unwrap_or(settings.default_patch_depth)
}

fn patch_location(value: &str) -> anyhow::Result<PatchLocation> {
    let Ok(url) = Url::parse(value) else {
        return Ok(PatchLocation::Local);
    };
    match url.scheme() {
        "https" => Ok(PatchLocation::Https),
        "http" => Ok(PatchLocation::Http),
        scheme => bail!(
            "Unsupported patch URL scheme '{scheme}' in {value}; use a local path or HTTP(S) URL"
        ),
    }
}

fn resolve_project_file(root: &Path, value: &str, description: &str) -> anyhow::Result<PathBuf> {
    let candidate = resolve_optional_project_file(root, value, description)?;
    candidate
        .canonicalize()
        .with_context(|| format!("Failed to resolve {description} {}", candidate.display()))
}

fn resolve_optional_project_file(
    root: &Path,
    value: &str,
    description: &str,
) -> anyhow::Result<PathBuf> {
    use std::path::Component;

    let relative = Path::new(value);
    if relative.is_absolute() {
        bail!("{description} path must be relative to the project root: {value}");
    }
    if relative
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("{description} path escapes the project root: {value}");
    }
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("Failed to resolve project root {}", root.display()))?;
    let candidate = canonical_root.join(relative);
    if candidate.exists() {
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("Failed to resolve {description} {}", candidate.display()))?;
        if !canonical.starts_with(&canonical_root) {
            bail!("{description} path escapes the project root: {value}");
        }
        return Ok(canonical);
    }
    Ok(candidate)
}

async fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut buffer = vec![0; 64 * 1024];
    let mut hasher = Sha256::new();
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn cache_patch_atomically(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let contents = std::fs::read(source)
        .with_context(|| format!("Failed to read downloaded patch {}", source.display()))?;
    let parent = destination
        .parent()
        .context("patch cache destination has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o644))?;
    }
    temporary.write_all(&contents)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(destination)
        .map_err(|error| error.error)?;
    Ok(())
}

fn validate_sha256(sha256: Option<&str>, package: &str, description: &str) -> anyhow::Result<()> {
    if let Some(sha256) = sha256 {
        if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("Invalid SHA-256 for patch '{description}' on {package}");
        }
    }
    Ok(())
}

fn write_lock_file(path: &Path, patches: &PatchCollection) -> anyhow::Result<()> {
    let patches_value = serde_json::to_value(patches).context("Failed to serialize patches")?;
    let hash = composer_collection_hash(&patches_value)?;

    let document = Value::Object(Map::from_iter([
        ("_hash".to_string(), Value::String(hash)),
        ("patches".to_string(), patches_value),
    ]));
    let mut bytes = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, formatter);
    document
        .serialize(&mut serializer)
        .context("Failed to serialize patches.lock.json")?;
    bytes.push(b'\n');

    let parent = path
        .parent()
        .context("patch lock has no parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .context("Failed to create temporary patches lock")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map(|metadata| metadata.permissions().mode())
            .unwrap_or(0o644);
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Failed to atomically write {}", path.display()))?;
    Ok(())
}

/// Composer's `JsonFile::encode($value, 0)` uses PHP's default JSON escaping
/// (notably `\/` and UTF-16 `\u` escapes). Reproduce it for byte-compatible
/// Composer Patches lock hashes while keeping the on-disk JSON human-readable.
fn composer_collection_hash(patches: &Value) -> anyhow::Result<String> {
    let collection = Value::Object(Map::from_iter([("patches".to_string(), patches.clone())]));
    let mut encoded = String::new();
    encode_php_json(&collection, &mut encoded)?;
    Ok(format!("{:x}", Sha256::digest(encoded.as_bytes())))
}

fn encode_php_json(value: &Value, output: &mut String) -> anyhow::Result<()> {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&php_number(value)?),
        Value::String(value) => encode_php_json_string(value, output),
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                encode_php_json(value, output)?;
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            for (index, (key, value)) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                encode_php_json_string(key, output);
                output.push(':');
                encode_php_json(value, output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

fn php_number(value: &serde_json::Number) -> anyhow::Result<String> {
    if let Some(value) = value.as_i64() {
        return Ok(value.to_string());
    }
    if let Some(value) = value.as_u64() {
        return Ok(value.to_string());
    }
    let mut value = value.to_string();
    if value == "-0.0" {
        return Ok("-0".to_string());
    }
    if let Some(exponent) = value.find(['e', 'E']) {
        if !value[..exponent].contains('.') {
            value.insert_str(exponent, ".0");
        }
        return Ok(value.to_lowercase());
    }
    if value.ends_with(".0") {
        value.truncate(value.len() - 2);
    }
    Ok(value)
}

fn encode_php_json_string(value: &str, output: &mut String) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '/' => output.push_str("\\/"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character <= '\u{1f}' => {
                output.push_str(&format!("\\u{:04x}", character as u32));
            }
            character if character.is_ascii() => output.push(character),
            character => {
                let codepoint = character as u32;
                if codepoint <= 0xffff {
                    output.push_str(&format!("\\u{codepoint:04x}"));
                } else {
                    let supplementary = codepoint - 0x1_0000;
                    let high = 0xd800 + (supplementary >> 10);
                    let low = 0xdc00 + (supplementary & 0x3ff);
                    output.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
                }
            }
        }
    }
    output.push('"');
}

#[async_trait]
impl PackageInstallHook for PreparedPatchSet {
    async fn after_install(&self, package: &Package, install_path: &Path) -> Result<()> {
        let package_name = canonical_package_name(&package.name);
        if matches!(
            package_name.as_ref(),
            PLUGIN_PACKAGE | CONFIGURABLE_PLUGIN_PACKAGE
        ) {
            return Ok(());
        }
        let Some(patches) = self.patches.get(package_name.as_ref()) else {
            return Ok(());
        };

        crate::outln!(self.output, "  - Patching {}", package.name);
        for patch in patches {
            log::debug!(
                "Applying patch '{}' ({}, sha256 {}, candidate depths {:?}) to {}",
                patch.description,
                patch.url,
                patch.sha256,
                patch.depths,
                package.name
            );
            if let Err(error) = apply_prepared_patch(package, install_path, patch).await {
                if patch.strict {
                    return Err(error);
                }
                crate::errln!(
                    self.output,
                    "Warning: Could not apply legacy patch '{}' for {} from {}; skipping it: {}",
                    patch.description,
                    package.name,
                    patch.url,
                    error
                );
            }
        }
        if self.write_legacy_report {
            let legacy: Vec<_> = patches.iter().filter(|patch| patch.legacy_report).collect();
            write_legacy_report(install_path, &legacy).await?;
        }
        Ok(())
    }

    fn fingerprints(&self) -> BTreeMap<String, String> {
        self.patches
            .iter()
            .map(|(package, patches)| {
                let mut hasher = Sha256::new();
                for patch in patches {
                    hasher.update(patch.sha256.as_bytes());
                    hasher.update([0]);
                    for depth in &patch.depths {
                        hasher.update(depth.to_le_bytes());
                    }
                    hasher.update([
                        u8::from(patch.strict),
                        u8::from(patch.legacy_report),
                        u8::from(self.write_legacy_report),
                    ]);
                    if patch.legacy_report && self.write_legacy_report {
                        hasher.update(patch.description.as_bytes());
                        hasher.update([0]);
                        hasher.update(patch.url.as_bytes());
                    }
                    hasher.update([0xff]);
                }
                (package.clone(), format!("{:x}", hasher.finalize()))
            })
            .collect()
    }
}

async fn apply_prepared_patch(
    package: &Package,
    install_path: &Path,
    patch: &PreparedPatch,
) -> Result<()> {
    let contents = tokio::fs::read_to_string(&patch.path)
        .await
        .map_err(|error| {
            RiffError::InstallationFailed(format!(
                "failed to read patch '{}' for {} from {}: {error}",
                patch.description, package.name, patch.url
            ))
        })?;
    let mut failures = Vec::new();
    for depth in &patch.depths {
        match engine::apply_patch(install_path, &contents, *depth) {
            Ok(()) => return Ok(()),
            Err(error) => failures.push(format!("-p{depth}: {error}")),
        }
    }
    Err(patch_error(
        package,
        patch,
        patch.depths[0],
        "applying",
        &failures.join("\n"),
    ))
}

fn patch_error(
    package: &Package,
    patch: &PreparedPatch,
    depth: u32,
    action: &str,
    stderr: &str,
) -> RiffError {
    RiffError::InstallationFailed(format!(
        "{action} patch '{}' for {} from {} at depth {} failed:\n{}",
        patch.description, package.name, patch.url, depth, stderr
    ))
}

async fn write_legacy_report(install_path: &Path, patches: &[&PreparedPatch]) -> Result<()> {
    let mut report = String::from(
        "This file was automatically generated by Composer Patches (https://github.com/cweagans/composer-patches)\nPatches applied to this directory:\n\n",
    );
    for patch in patches {
        report.push_str(&patch.description);
        report.push('\n');
        report.push_str("Source: ");
        report.push_str(&patch.url);
        report.push_str("\n\n\n");
    }
    tokio::fs::write(install_path.join("PATCHES.txt"), report).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AllowPlugins, Config};
    use crate::json::RiffManifest;
    use serde_json::json;

    fn test_riff(directory: &Path, extra: Value, allow_plugins: bool) -> Riff {
        let mut config = Config::with_base_dir(directory);
        config.allow_plugins = AllowPlugins::Bool(allow_plugins);
        Riff::builder(directory.to_path_buf())
            .with_platform(crate::Platform::empty())
            .with_config(config)
            .with_manifest(RiffManifest {
                extra,
                ..RiffManifest::default()
            })
            .build()
            .unwrap()
    }

    fn plugin(version: &str) -> Package {
        let mut package = Package::new(PLUGIN_PACKAGE, version);
        package.package_type = "composer-plugin".into();
        package
    }

    #[test]
    fn parses_compact_and_expanded_and_deduplicates() {
        let mut patches = PatchCollection::new();
        merge_patch_value(
            &mut patches,
            &json!({"Vendor/Package": {"First": "one.patch"}}),
            "root",
        )
        .unwrap();
        merge_patch_value(
            &mut patches,
            &json!({"vendor/package": [
                {"description": "Duplicate", "url": "one.patch"},
                {"description": "Second", "url": "two.patch", "depth": 2,
                 "extra": {"issue": "123"}}
            ]}),
            "patches-file:patches.json",
        )
        .unwrap();

        let package = &patches["vendor/package"];
        assert_eq!(package.len(), 2);
        assert_eq!(package[0].extra["provenance"], "root");
        assert_eq!(package[1].extra["issue"], "123");
        assert_eq!(package[1].extra["provenance"], "patches-file:patches.json");
    }

    #[test]
    fn reads_custom_patches_file() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("project-patches.json"),
            r#"{"patches":{"vendor/package":{"From file":"fix.patch"}}}"#,
        )
        .unwrap();
        let extra = json!({"composer-patches": {"patches-file": "project-patches.json"}});
        let settings = parse_settings(&extra).unwrap();
        let patches = resolve_definitions(directory.path(), &extra, &settings, &[]).unwrap();
        assert_eq!(patches["vendor/package"][0].description, "From file");
        assert_eq!(
            patches["vendor/package"][0].extra["provenance"],
            "patches-file:project-patches.json"
        );
    }

    #[test]
    fn v2_patches_file_requires_top_level_patches_key() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(DEFAULT_PATCHES_FILE),
            r#"{"vendor/package":{"From file":"fix.patch"}}"#,
        )
        .unwrap();
        let error = resolve_definitions(
            directory.path(),
            &Value::Object(Map::new()),
            &PatchSettings::default(),
            &[],
        )
        .unwrap_err();
        assert!(error.to_string().contains("top-level patches object"));
    }

    #[test]
    fn legacy_root_patches_take_precedence_over_patches_file() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("legacy.json"),
            r#"{"patches":{"vendor/package":{"From file":"file.patch"}}}"#,
        )
        .unwrap();
        let extra = json!({
            "patches": {"vendor/package": {"From root": "root.patch"}},
            "patches-file": "legacy.json"
        });
        let patches = resolve_legacy_definitions(directory.path(), &extra, &[]).unwrap();
        assert_eq!(patches["vendor/package"].len(), 1);
        assert_eq!(patches["vendor/package"][0].description, "From root");
    }

    #[test]
    fn legacy_patches_file_and_patch_level_are_parsed() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("legacy.json"),
            r#"{"patches":{"Vendor/Package":{"From file":"file.patch"}}}"#,
        )
        .unwrap();
        let extra = json!({
            "patches-file": "legacy.json",
            "patchLevel": {"Vendor/Package": "-p2"}
        });
        let settings = parse_legacy_settings(&extra).unwrap();
        let patches = resolve_legacy_definitions(directory.path(), &extra, &[]).unwrap();
        assert_eq!(settings.package_depths["vendor/package"], 2);
        assert_eq!(patches["vendor/package"][0].description, "From file");
    }

    #[test]
    fn legacy_dependency_patches_require_opt_in_without_root_patches() {
        let directory = tempfile::tempdir().unwrap();
        let mut dependency = Package::new("provider/package", "1.0.0");
        dependency.extra = Some(json!({"patches": {
            "vendor/package": {"Dependency fix": "https://example.com/fix.patch"}
        }}));

        assert!(
            resolve_legacy_definitions(directory.path(), &json!({}), &[dependency.clone()])
                .unwrap()
                .is_empty()
        );
        let patches = resolve_legacy_definitions(
            directory.path(),
            &json!({"enable-patching": true}),
            &[dependency],
        )
        .unwrap();
        assert_eq!(patches["vendor/package"].len(), 1);
    }

    #[test]
    fn legacy_dependency_patch_urls_can_be_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let mut dependency = Package::new("provider/package", "1.0.0");
        dependency.extra = Some(json!({"patches": {"vendor/package": {
            "Ignored": "https://example.com/ignored.patch",
            "Kept": "https://example.com/kept.patch"
        }}}));
        let extra = json!({"patches-ignore": {"provider/package": {
            "vendor/package": {"Ignored": "https://example.com/ignored.patch"}
        }}});

        let patches = resolve_legacy_definitions(directory.path(), &extra, &[dependency]).unwrap();
        assert_eq!(patches["vendor/package"].len(), 1);
        assert_eq!(patches["vendor/package"][0].description, "Kept");
    }

    #[test]
    fn legacy_expanded_definitions_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let error = resolve_legacy_definitions(
            directory.path(),
            &json!({"patches": {"vendor/package": [{
                "description": "Expanded", "url": "fix.patch"
            }]}}),
            &[],
        )
        .unwrap_err();
        assert!(error.to_string().contains("compact"));
    }

    #[test]
    fn legacy_normalization_only_rewrites_false_dev_null_headers() {
        let malformed = b"--- /dev/null\n+++ ../src/Existing.php\n@@ -4,2 +4,3 @@\n old\n+new\n";
        let normalized = normalize_legacy_patch_contents(malformed).unwrap().unwrap();
        assert!(normalized.starts_with(b"--- ../src/Existing.php\n+++ ../src/Existing.php\n"));

        let new_file = b"--- /dev/null\n+++ b/src/New.php\n@@ -0,0 +1 @@\n+new\n";
        assert!(normalize_legacy_patch_contents(new_file).unwrap().is_none());
    }

    #[test]
    fn legacy_flags_use_php_truthiness() {
        assert!(is_php_truthy(&Value::Bool(true)));
        assert!(is_php_truthy(&Value::String("yes".to_string())));
        assert!(!is_php_truthy(&Value::String("0".to_string())));
        assert!(!is_php_truthy(&Value::Bool(false)));
    }

    #[tokio::test]
    async fn patch_paths_cannot_escape_the_package_directory() {
        let directory = tempfile::tempdir().unwrap();
        let install_path = directory.path().join("package");
        std::fs::create_dir(&install_path).unwrap();
        let patch_path = directory.path().join("escape.patch");
        std::fs::write(
            &patch_path,
            "--- ../outside.txt\n+++ ../outside.txt\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .unwrap();
        let patch = std::fs::read_to_string(&patch_path).unwrap();
        let error = engine::apply_patch(&install_path, &patch, 0).unwrap_err();
        assert!(error.to_string().contains("unsafe patch path"));
    }

    #[test]
    fn rejects_freeform_patch_extra() {
        let mut patches = PatchCollection::new();
        let error = merge_patch_value(
            &mut patches,
            &json!({"vendor/package": [{
                "description": "No", "url": "x.patch", "extra": {"freeform": {"x": 1}}
            }]}),
            "root",
        )
        .unwrap_err();
        assert!(error.to_string().contains("freeform"));
    }

    #[test]
    fn deduplicates_matching_non_null_checksums() {
        let checksum = "b".repeat(64);
        let mut patches = PatchCollection::new();
        merge_patch_value(
            &mut patches,
            &json!({"vendor/package": [
                {"description": "First", "url": "first.patch", "sha256": checksum},
                {"description": "Duplicate", "url": "mirror.patch", "sha256": "b".repeat(64)}
            ]}),
            "root",
        )
        .unwrap();
        assert_eq!(patches["vendor/package"].len(), 1);
        assert_eq!(patches["vendor/package"][0].description, "First");
    }

    #[test]
    fn rejects_unsupported_advanced_configuration() {
        let error = parse_settings(&json!({
            "composer-patches": {"disable-resolvers": ["custom/resolver"]}
        }))
        .unwrap_err();
        assert!(error.to_string().contains("disable-resolvers"));
    }

    #[test]
    fn writes_a_deterministic_composer_patches_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(PATCHES_LOCK_FILE);
        let mut patches = PatchCollection::new();
        patches.insert(
            "vendor/package".to_string(),
            vec![PatchDefinition {
                package: "vendor/package".to_string(),
                description: "Fix".to_string(),
                url: "fix.patch".to_string(),
                sha256: Some("a".repeat(64)),
                depth: Some(1),
                extra: provenance_extra("root"),
            }],
        );
        write_lock_file(&path, &patches).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        write_lock_file(&path, &patches).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);
        let json: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(
            json["_hash"],
            composer_collection_hash(&json["patches"]).unwrap()
        );
        assert!(first.ends_with('\n'));
    }

    #[test]
    fn lock_hash_matches_composer_patches_php_encoding() {
        let patches = json!({"drupal/page_manager": [{
            "package": "drupal/page_manager",
            "description": "make __sleep() function compatible with new type hinted return in upstream ctools: https://www.drupal.org/project/page_manager/issues/3455521",
            "url": "patches/page_manager.3455521-0.patch",
            "sha256": "51f641d189d3da8ea19db5cdd82c960be4b0d6804f87248bb3ae0c896cbebbf8",
            "depth": 1,
            "extra": {"provenance": "patches-file:patches.json"}
        }]});
        assert_eq!(
            composer_collection_hash(&patches).unwrap(),
            "7a8e0acb4f81ccf52f494fcf7da0b176e2aa60d0356efc1f6062a64a7a30ed6d"
        );
    }

    #[test]
    fn cached_patch_writes_replace_corrupt_content_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.patch");
        let destination = directory.path().join("cached.patch");
        std::fs::write(&source, "expected").unwrap();
        std::fs::write(&destination, "corrupt").unwrap();

        cache_patch_atomically(&source, &destination).unwrap();

        assert_eq!(std::fs::read_to_string(destination).unwrap(), "expected");
    }

    #[test]
    fn existing_lock_is_authoritative() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(PATCHES_LOCK_FILE);
        let json = json!({"_hash": "ignored", "patches": {"vendor/package": [{
            "package": "vendor/package", "description": "Locked", "url": "locked.patch",
            "sha256": null, "depth": null, "extra": {}
        }]}});
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let patches = read_lock_file(&path).unwrap().unwrap();
        assert_eq!(patches["vendor/package"][0].description, "Locked");
    }

    #[test]
    fn lock_without_patches_requests_regeneration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(PATCHES_LOCK_FILE);
        std::fs::write(&path, r#"{"_hash":"old"}"#).unwrap();
        assert!(read_lock_file(&path).unwrap().is_none());
    }

    #[test]
    fn malformed_lock_has_an_actionable_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(PATCHES_LOCK_FILE);
        std::fs::write(&path, r#"{"patches":null}"#).unwrap();
        let error = read_lock_file(&path).unwrap_err();
        assert!(error.to_string().contains("patches must be an object"));
    }

    #[test]
    fn resolves_depth_precedence() {
        let settings = PatchSettings {
            default_patch_depth: 4,
            package_depths: HashMap::from([("vendor/package".to_string(), 3)]),
            ..PatchSettings::default()
        };
        assert_eq!(resolve_depth("vendor/package", Some(5), &settings), 5);
        assert_eq!(resolve_depth("vendor/package", None, &settings), 3);
        assert_eq!(resolve_depth("drupal/core", None, &settings), 2);
        assert_eq!(resolve_depth("other/package", None, &settings), 4);
    }

    #[test]
    fn rejects_project_path_escape() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let outside = directory.path().join("outside.patch");
        std::fs::write(&outside, "patch").unwrap();
        let error = resolve_project_file(&project, "../outside.patch", "local patch").unwrap_err();
        assert!(error.to_string().contains("escapes"));
    }

    #[tokio::test]
    async fn dry_run_has_no_side_effects_and_plugin_policy_does_not_gate_patching() {
        let directory = tempfile::tempdir().unwrap();
        let extra = json!({"patches": "deliberately invalid"});
        let packages = vec![plugin("2.0.0.0"), Package::new("vendor/package", "1.0.0.0")];

        let allowed = test_riff(directory.path(), extra.clone(), true);
        assert!(prepare(&allowed, &packages, true).await.unwrap().is_none());
        assert!(!directory.path().join(PATCHES_LOCK_FILE).exists());

        std::fs::write(directory.path().join("fix.patch"), "").unwrap();
        let denied = test_riff(
            directory.path(),
            json!({"patches": {"vendor/package": {"Fix": "fix.patch"}}}),
            false,
        );
        assert!(prepare(&denied, &packages, false).await.unwrap().is_some());
        assert!(directory.path().join(PATCHES_LOCK_FILE).exists());
    }

    #[tokio::test]
    async fn fingerprint_inspection_does_not_create_a_composer_patch_lock() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("fix.patch"), "patch body").unwrap();
        let riff = test_riff(
            directory.path(),
            json!({"patches": {"vendor/package": {"Fix": "fix.patch"}}}),
            true,
        );
        let packages = vec![plugin("2.0.0.0"), Package::new("vendor/package", "1.0.0")];

        let fingerprints = desired_fingerprints(&riff, &packages).await.unwrap();

        assert!(fingerprints.contains_key("vendor/package"));
        assert!(!directory.path().join(PATCHES_LOCK_FILE).exists());
    }

    #[tokio::test]
    async fn dependency_defined_patches_require_remote_urls() {
        let directory = tempfile::tempdir().unwrap();
        let riff = test_riff(directory.path(), json!({}), true);
        let mut dependency = Package::new("vendor/package", "1.0.0");
        dependency.extra = Some(json!({"patches": {"other/package": {"Fix": "fix.patch"}}}));
        let error = prepare(&riff, &[plugin("2.0.0.0"), dependency], false)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("must use an HTTP(S) URL"));
    }

    #[tokio::test]
    async fn unsupported_plugin_version_is_inert_without_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let riff = test_riff(directory.path(), json!({}), true);
        assert!(prepare(&riff, &[plugin("3.0.0.0")], false)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn legacy_mode_uses_depth_fallback_writes_report_and_does_not_lock() {
        let directory = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(directory.path())
            .status()
            .unwrap();
        assert!(status.success());

        let patch = "--- file.txt\n+++ file.txt\n@@ -1 +1 @@\n-old\n+new\n";
        std::fs::write(directory.path().join("legacy.patch"), patch).unwrap();
        let shopware_style_patch =
            "--- /dev/null\n+++ ../src/Added.php\n@@ -0,0 +1 @@\n+<?php // added\n";
        std::fs::write(
            directory.path().join("shopware-style.patch"),
            shopware_style_patch,
        )
        .unwrap();
        let riff = test_riff(
            directory.path(),
            json!({"patches": {"vendor/package": {
                "Legacy fix": "legacy.patch",
                "Shopware path style": "shopware-style.patch"
            }}}),
            true,
        );
        let packages = vec![plugin("1.7.3.0"), Package::new("vendor/package", "1.0.0")];
        let hook = prepare(&riff, &packages, false).await.unwrap().unwrap();
        assert!(!directory.path().join(PATCHES_LOCK_FILE).exists());

        let install_path = directory.path().join("vendor/vendor/package");
        std::fs::create_dir_all(install_path.join("src")).unwrap();
        std::fs::write(install_path.join("file.txt"), "old\n").unwrap();
        hook.after_install(&packages[1], &install_path)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(install_path.join("file.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(install_path.join("src/Added.php")).unwrap(),
            "<?php // added\n"
        );
        let report = std::fs::read_to_string(install_path.join("PATCHES.txt")).unwrap();
        assert!(report.contains("Legacy fix\nSource: legacy.patch"));
        assert!(report.contains("Shopware path style\nSource: shopware-style.patch"));
    }

    #[tokio::test]
    async fn legacy_failures_skip_by_default_and_can_be_strict() {
        let directory = tempfile::tempdir().unwrap();
        let patch = "--- a/missing.txt\n+++ b/missing.txt\n@@ -1 +1 @@\n-old\n+new\n";
        let patch_path = directory.path().join("stale.patch");
        std::fs::write(&patch_path, patch).unwrap();
        let package = Package::new("vendor/package", "1.0.0");
        let install_path = directory.path().join("vendor/vendor/package");
        std::fs::create_dir_all(&install_path).unwrap();
        let prepared_patch = PreparedPatch {
            description: "Stale".to_string(),
            url: "stale.patch".to_string(),
            sha256: "irrelevant".to_string(),
            depths: vec![1, 0, 2, 4],
            path: patch_path,
            strict: false,
            legacy_report: true,
        };
        let hook = PreparedPatchSet {
            patches: HashMap::from([("vendor/package".to_string(), vec![prepared_patch.clone()])]),
            write_legacy_report: true,
            output: crate::output::Output::silent(),
            _temporary_directory: tempfile::tempdir_in(directory.path()).unwrap(),
        };
        hook.after_install(&package, &install_path).await.unwrap();
        assert!(install_path.join("PATCHES.txt").exists());

        let mut strict_patch = prepared_patch;
        strict_patch.strict = true;
        let strict_hook = PreparedPatchSet {
            patches: HashMap::from([("vendor/package".to_string(), vec![strict_patch])]),
            write_legacy_report: true,
            output: crate::output::Output::silent(),
            _temporary_directory: tempfile::tempdir_in(directory.path()).unwrap(),
        };
        assert!(strict_hook
            .after_install(&package, &install_path)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn rejects_insecure_http_before_downloading() {
        let directory = tempfile::tempdir().unwrap();
        let riff = test_riff(
            directory.path(),
            json!({"patches": {"vendor/package": {"Fix": "http://127.0.0.1:9/fix.patch"}}}),
            true,
        );
        let packages = vec![plugin("2.0.0.0"), Package::new("vendor/package", "1.0.0")];
        let error = prepare(&riff, &packages, false).await.err().unwrap();
        assert!(error.to_string().contains("Refusing insecure HTTP"));
        assert!(!directory.path().join(PATCHES_LOCK_FILE).exists());
    }

    #[tokio::test]
    async fn prepares_locks_and_applies_a_local_patch_without_plugin_package() {
        let directory = tempfile::tempdir().unwrap();
        let patch = "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n";
        std::fs::write(directory.path().join("fix.patch"), patch).unwrap();
        let riff = test_riff(
            directory.path(),
            json!({"patches": {"vendor/package": {"Change text": "fix.patch"}}}),
            true,
        );
        let packages = vec![Package::new("vendor/package", "1.0.0")];
        let hook = prepare(&riff, &packages, false).await.unwrap().unwrap();
        let lock_path = directory.path().join(PATCHES_LOCK_FILE);
        let lock: Value = serde_json::from_slice(&std::fs::read(lock_path).unwrap()).unwrap();
        assert_eq!(lock["patches"]["vendor/package"][0]["depth"], 1);
        assert_eq!(
            lock["patches"]["vendor/package"][0]["sha256"],
            format!("{:x}", Sha256::digest(patch.as_bytes()))
        );

        let install_path = directory.path().join("vendor/vendor/package");
        std::fs::create_dir_all(&install_path).unwrap();
        std::fs::write(install_path.join("file.txt"), "old\n").unwrap();
        hook.after_install(&packages[0], &install_path)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(install_path.join("file.txt")).unwrap(),
            "new\n"
        );
    }

    #[tokio::test]
    async fn preparation_does_not_rewrite_an_existing_lock() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("locked.patch"), "locked contents").unwrap();
        let lock_path = directory.path().join(PATCHES_LOCK_FILE);
        let lock = r#"{
    "_hash": "authoritative",
    "patches": {
        "vendor/package": [{
            "package": "vendor/package",
            "description": "Locked",
            "url": "locked.patch",
            "sha256": null,
            "depth": null,
            "extra": {}
        }]
    }
}
"#;
        std::fs::write(&lock_path, lock).unwrap();
        let riff = test_riff(
            directory.path(),
            json!({"patches": {"vendor/package": {"Ignored": "missing.patch"}}}),
            true,
        );
        let packages = vec![plugin("2.0.0.0"), Package::new("vendor/package", "1.0.0")];
        assert!(prepare(&riff, &packages, false).await.unwrap().is_some());
        assert_eq!(std::fs::read_to_string(lock_path).unwrap(), lock);
    }

    #[tokio::test]
    async fn checksum_mismatch_does_not_write_lock() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("fix.patch"), "patch").unwrap();
        let riff = test_riff(
            directory.path(),
            json!({"patches": {"vendor/package": [{
                "description": "Fix", "url": "fix.patch", "sha256": "a".repeat(64)
            }]}}),
            true,
        );
        let packages = vec![plugin("2.0.0.0"), Package::new("vendor/package", "1.0.0")];
        let error = prepare(&riff, &packages, false).await.err().unwrap();
        assert!(error.to_string().contains("SHA-256 mismatch"));
        assert!(!directory.path().join(PATCHES_LOCK_FILE).exists());
    }

    #[tokio::test]
    async fn downloads_http_patch_when_secure_http_is_disabled() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let patch = b"remote patch body";
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                patch.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(patch).await.unwrap();
        });

        let directory = tempfile::tempdir().unwrap();
        let url = format!("http://{address}/fix.patch");
        let mut riff = test_riff(
            directory.path(),
            json!({"patches": {"vendor/package": {"Remote": url}}}),
            true,
        );
        riff.config.secure_http = false;
        let packages = vec![plugin("2.0.0.0"), Package::new("vendor/package", "1.0.0")];
        let hook = prepare(&riff, &packages, false).await.unwrap();
        assert!(hook.is_some());
        server.await.unwrap();

        let lock: Value = serde_json::from_slice(
            &std::fs::read(directory.path().join(PATCHES_LOCK_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            lock["patches"]["vendor/package"][0]["sha256"],
            format!("{:x}", Sha256::digest(patch))
        );
    }
}
