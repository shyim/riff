use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::cache::runtime_cache_dir;
use crate::package::Package;
use crate::util::canonical_package_name;

use super::engine::create_patch;
use super::native::{native_declarations, relock_native, validate_native_patch_path};
use super::NATIVE_PATCH_LOCK_FILE;

const PATCH_EDIT_STATE_FILE: &str = ".riff-patch-state.json";
const PATCH_EDIT_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredPatchEdit {
    format_version: u32,
    selector: String,
    package: String,
    version: String,
    version_normalized: String,
    project: PathBuf,
    source_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchEdit {
    pub selector: String,
    pub package: String,
    pub version: String,
    pub version_normalized: String,
    pub project: PathBuf,
    pub source_dir: PathBuf,
    pub user_dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchCommitResult {
    pub selector: String,
    pub patch_path: PathBuf,
    pub appended: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PatchRemoveResult {
    pub selectors: Vec<String>,
    pub deleted_files: Vec<PathBuf>,
    pub preserved_files: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

struct RiffLockfileMutation {
    path: PathBuf,
    previous: Vec<u8>,
    document: Value,
}

/// Refuse to create an edit snapshot from a package tree whose patch state is stale.
pub fn ensure_applied_patch_state_current(
    vendor_dir: &Path,
    desired: &BTreeMap<String, String>,
) -> Result<()> {
    let applied = super::read_applied_patch_state(vendor_dir);
    let changed = super::changed_patch_packages(&applied, desired);
    if !changed.is_empty() {
        bail!(
            "installed package patches are out of date for {}; run `riff install` before `riff patch`",
            changed.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(())
}

/// Snapshot one installed package into immutable `source/` and editable `user/` trees.
pub fn begin_patch_edit(
    project: &Path,
    vendor_dir: &Path,
    installed_packages: &[Package],
    extra: &Value,
    package_spec: &str,
    edit_parent: Option<&Path>,
) -> Result<PatchEdit> {
    let project = project
        .canonicalize()
        .with_context(|| format!("Failed to resolve project root {}", project.display()))?;
    let (requested_name, requested_version) = split_optional_package_spec(package_spec)?;
    let package = select_installed_package(
        installed_packages,
        &requested_name,
        requested_version.as_deref(),
    )?;
    if package.is_metapackage() {
        bail!(
            "{} is a metapackage and has no files to patch",
            package.name
        );
    }

    let package_name = canonical_package_name(&package.name).into_owned();
    let pretty_version = package
        .pretty_version
        .as_deref()
        .unwrap_or(package.version.as_str())
        .to_string();
    let matching_declarations: Vec<_> = native_declarations(extra)?
        .into_iter()
        .filter(|declaration| {
            declaration.package == package_name
                && (declaration.version == package.version.as_str()
                    || declaration.version == pretty_version)
        })
        .collect();
    if matching_declarations.len() > 1 {
        bail!(
            "multiple native patch selectors match {} {}; remove the duplicate declarations first",
            package.name,
            pretty_version
        );
    }
    let selector = matching_declarations
        .first()
        .map(|declaration| declaration.selector.clone())
        .unwrap_or_else(|| format!("{package_name}@{pretty_version}"));
    let version = selector
        .rsplit_once('@')
        .map(|(_, version)| version.to_string())
        .unwrap_or_else(|| pretty_version.clone());

    let package_dir = vendor_dir.join(&package_name);
    let metadata = fs::symlink_metadata(&package_dir).with_context(|| {
        format!(
            "{} is not installed at {}; run `riff install` first",
            package.name,
            package_dir.display()
        )
    })?;
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to patch symlinked package directory {}",
            package_dir.display()
        );
    }
    if !metadata.is_dir() {
        bail!("package path {} is not a directory", package_dir.display());
    }

    let parent = match edit_parent {
        Some(parent) => {
            let parent = if parent.is_absolute() {
                parent.to_path_buf()
            } else {
                project.join(parent)
            };
            fs::create_dir_all(&parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
            parent
        }
        None => {
            let edit_cache = runtime_cache_dir().join("patch-edits");
            fs::create_dir_all(&edit_cache)
                .with_context(|| format!("Failed to create {}", edit_cache.display()))?;
            let safe_name = package_name.replace('/', "+");
            tempfile::Builder::new()
                .prefix(&format!("{safe_name}-{pretty_version}-"))
                .tempdir_in(&edit_cache)
                .context("Failed to create Riff patch edit directory")?
                .keep()
        }
    };
    let source_dir = parent.join("source");
    let user_dir = parent.join("user");
    let state_path = parent.join(PATCH_EDIT_STATE_FILE);
    if source_dir.exists() || user_dir.exists() || state_path.exists() {
        bail!(
            "patch edit directory {} already contains Riff patch state; choose an empty directory",
            parent.display()
        );
    }

    if let Err(error) = copy_tree(&package_dir, &source_dir, false)
        .and_then(|()| copy_tree(&package_dir, &user_dir, true))
    {
        let _ = fs::remove_dir_all(&source_dir);
        let _ = fs::remove_dir_all(&user_dir);
        return Err(error);
    }
    let source_digest = match tree_digest(&source_dir) {
        Ok(digest) => digest,
        Err(error) => {
            let _ = fs::remove_dir_all(&source_dir);
            let _ = fs::remove_dir_all(&user_dir);
            return Err(error);
        }
    };
    let stored = StoredPatchEdit {
        format_version: PATCH_EDIT_FORMAT_VERSION,
        selector: selector.clone(),
        package: package_name.clone(),
        version: version.clone(),
        version_normalized: package.version.to_string(),
        project: project.clone(),
        source_digest,
    };
    if let Err(error) = serde_json::to_value(stored)
        .map_err(anyhow::Error::from)
        .and_then(|stored| write_json_atomic(&state_path, &stored))
    {
        let _ = fs::remove_dir_all(&source_dir);
        let _ = fs::remove_dir_all(&user_dir);
        let _ = fs::remove_file(&state_path);
        return Err(error);
    }

    Ok(PatchEdit {
        selector,
        package: package_name,
        version,
        version_normalized: package.version.to_string(),
        project,
        source_dir,
        user_dir,
    })
}

pub fn read_patch_edit(edit_dir: &Path) -> Result<PatchEdit> {
    let (parent, user_dir) =
        if edit_dir.join(PATCH_EDIT_STATE_FILE).is_file() && edit_dir.join("user").is_dir() {
            (edit_dir.to_path_buf(), edit_dir.join("user"))
        } else {
            let parent = edit_dir.parent().with_context(|| {
                format!("patch edit directory {} has no parent", edit_dir.display())
            })?;
            (parent.to_path_buf(), edit_dir.to_path_buf())
        };
    let state_path = parent.join(PATCH_EDIT_STATE_FILE);
    let content = fs::read_to_string(&state_path).with_context(|| {
        format!(
            "{} is not a directory created by `riff patch` (missing {})",
            edit_dir.display(),
            state_path.display()
        )
    })?;
    let stored: StoredPatchEdit = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", state_path.display()))?;
    if stored.format_version != PATCH_EDIT_FORMAT_VERSION {
        bail!(
            "unsupported patch edit format {} in {}",
            stored.format_version,
            state_path.display()
        );
    }
    let expected_user_dir = parent.join("user");
    if user_dir != expected_user_dir {
        bail!(
            "{} is not the user edit tree recorded by `riff patch`; expected {}",
            edit_dir.display(),
            expected_user_dir.display()
        );
    }
    let source_dir = parent.join("source");
    if !source_dir.is_dir() || !user_dir.is_dir() {
        bail!("patch edit snapshot at {} is incomplete", parent.display());
    }
    let actual_digest = tree_digest(&source_dir)?;
    if actual_digest != stored.source_digest {
        bail!(
            "the immutable source snapshot at {} was modified; start a new `riff patch` edit",
            source_dir.display()
        );
    }

    Ok(PatchEdit {
        selector: stored.selector,
        package: stored.package,
        version: stored.version,
        version_normalized: stored.version_normalized,
        project: stored.project,
        source_dir,
        user_dir,
    })
}

/// Generate and record a native patch, updating both composer.json and Riff's patch lock.
pub fn commit_patch_edit(
    edit_dir: &Path,
    patches_dir: &Path,
    packages: &[Package],
) -> Result<PatchCommitResult> {
    let edit = read_patch_edit(edit_dir)?;
    let patch = create_patch(&edit.source_dir, &edit.user_dir)
        .map_err(anyhow::Error::new)
        .context("Failed to generate package patch")?;
    if patch.is_empty() {
        bail!(
            "no changes detected between {} and {}",
            edit.source_dir.display(),
            edit.user_dir.display()
        );
    }

    let manifest_path = edit.project.join("composer.json");
    let previous_manifest = fs::read(&manifest_path)
        .with_context(|| format!("Failed to read {}", manifest_path.display()))?;
    let mut document: Value = serde_json::from_slice(&previous_manifest)
        .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;
    let extra = document
        .get("extra")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let declarations = native_declarations(&extra)?;
    let existing = declarations
        .iter()
        .find(|declaration| declaration.selector == edit.selector);

    let relative_path = match existing {
        Some(declaration) => declaration.path.clone(),
        None => default_patch_path(patches_dir, &edit.package, &edit.version)?,
    };
    let relative_text = portable_relative_path(&relative_path, "patch path")?;
    validate_native_patch_path(&relative_text)?;
    let patch_path = edit.project.join(&relative_path);
    let previous_patch = read_optional(&patch_path)?;
    if existing.is_none() && previous_patch.is_some() {
        bail!(
            "refusing to overwrite unreferenced patch file {}; choose another --patches-dir or move the file",
            patch_path.display()
        );
    }
    let appended = existing.is_some();
    let mut combined = if appended {
        let previous = previous_patch.as_ref().with_context(|| {
            format!(
                "native patch {} refers to missing file {}",
                edit.selector,
                patch_path.display()
            )
        })?;
        String::from_utf8(previous.clone())
            .with_context(|| format!("Existing patch {} is not UTF-8", patch_path.display()))?
    } else {
        String::new()
    };
    if !combined.is_empty() && !combined.ends_with('\n') {
        combined.push('\n');
    }
    if !combined.ends_with(&patch) {
        combined.push_str(&patch);
    }

    upsert_native_declaration(&mut document, &edit.selector, &relative_text)?;
    let lockfile = prepare_lockfile_mutation(&edit.project, &document)?;
    let updated_extra = document
        .get("extra")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let lock_path = edit.project.join(NATIVE_PATCH_LOCK_FILE);
    let previous_lock = read_optional(&lock_path)?;

    let mutation = (|| -> Result<()> {
        write_atomic(&patch_path, combined.as_bytes())?;
        write_json_atomic(&manifest_path, &document)?;
        write_lockfile_mutation(lockfile.as_ref())?;
        relock_native(&edit.project, &updated_extra, packages)?;
        Ok(())
    })();
    if let Err(error) = mutation {
        let rollback = rollback_all(vec![
            restore_optional(&lock_path, previous_lock.as_deref()),
            restore_lockfile_mutation(lockfile.as_ref()),
            restore_optional(&manifest_path, Some(&previous_manifest)),
            restore_optional(&patch_path, previous_patch.as_deref()),
        ]);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => {
                Err(error.context(format!("rollback also failed: {rollback_error:#}")))
            }
        };
    }

    Ok(PatchCommitResult {
        selector: edit.selector,
        patch_path,
        appended,
    })
}

/// Remove native declarations and any patch files no longer referenced by the manifest.
pub fn remove_native_patches(
    project: &Path,
    requested: &[String],
    packages: &[Package],
) -> Result<PatchRemoveResult> {
    let project = project
        .canonicalize()
        .with_context(|| format!("Failed to resolve project root {}", project.display()))?;
    let manifest_path = project.join("composer.json");
    let previous_manifest = fs::read(&manifest_path)
        .with_context(|| format!("Failed to read {}", manifest_path.display()))?;
    let mut document: Value = serde_json::from_slice(&previous_manifest)
        .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;
    let extra = document
        .get("extra")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let declarations = native_declarations(&extra)?;
    if declarations.is_empty() {
        bail!("no native Riff patches are declared");
    }

    let selectors: Vec<String> = if requested.is_empty() {
        declarations
            .iter()
            .map(|declaration| declaration.selector.clone())
            .collect()
    } else {
        requested
            .iter()
            .map(|selector| resolve_declared_selector(selector, &declarations))
            .collect::<Result<_>>()?
    };
    let selected: BTreeSet<_> = selectors.iter().cloned().collect();
    let removed_paths: BTreeSet<_> = declarations
        .iter()
        .filter(|declaration| selected.contains(&declaration.selector))
        .map(|declaration| declaration.path.clone())
        .collect();
    remove_native_declarations(&mut document, &selected)?;
    let lockfile = prepare_lockfile_mutation(&project, &document)?;

    let updated_extra = document
        .get("extra")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let lock_path = project.join(NATIVE_PATCH_LOCK_FILE);
    let previous_lock = read_optional(&lock_path)?;
    let mutation = (|| -> Result<()> {
        write_json_atomic(&manifest_path, &document)?;
        write_lockfile_mutation(lockfile.as_ref())?;
        relock_native(&project, &updated_extra, packages)?;
        Ok(())
    })();
    if let Err(error) = mutation {
        let rollback = rollback_all(vec![
            restore_optional(&lock_path, previous_lock.as_deref()),
            restore_lockfile_mutation(lockfile.as_ref()),
            restore_optional(&manifest_path, Some(&previous_manifest)),
        ]);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => {
                Err(error.context(format!("rollback also failed: {rollback_error:#}")))
            }
        };
    }

    let mut result = PatchRemoveResult {
        selectors,
        ..PatchRemoveResult::default()
    };
    for relative in removed_paths {
        let path = project.join(&relative);
        let relative_text = relative.to_string_lossy().replace('\\', "/");
        if json_contains_string(&document, &relative_text) {
            result.preserved_files.push(path);
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                result.preserved_files.push(path.clone());
                result.warnings.push(format!(
                    "did not delete non-regular patch path {}",
                    path.display()
                ));
            }
            Ok(_) => match fs::remove_file(&path) {
                Ok(()) => result.deleted_files.push(path),
                Err(error) => {
                    result.preserved_files.push(path.clone());
                    result.warnings.push(format!(
                        "failed to delete unreferenced patch {}: {error}",
                        path.display()
                    ));
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => result.warnings.push(format!(
                "failed to inspect unreferenced patch {}: {error}",
                path.display()
            )),
        }
    }
    Ok(result)
}

/// Remove the edit snapshot after its patch has been installed successfully.
pub fn cleanup_patch_edit(edit_dir: &Path) -> Result<()> {
    let edit = read_patch_edit(edit_dir)?;
    let parent = edit
        .user_dir
        .parent()
        .context("patch edit directory has no parent")?;
    fs::remove_dir_all(&edit.source_dir)
        .with_context(|| format!("Failed to remove {}", edit.source_dir.display()))?;
    fs::remove_dir_all(&edit.user_dir)
        .with_context(|| format!("Failed to remove {}", edit.user_dir.display()))?;
    let state = parent.join(PATCH_EDIT_STATE_FILE);
    fs::remove_file(&state).with_context(|| format!("Failed to remove {}", state.display()))?;
    if fs::read_dir(parent)?.next().is_none() {
        fs::remove_dir(parent)?;
    }
    Ok(())
}

fn split_optional_package_spec(input: &str) -> Result<(String, Option<String>)> {
    let input = input.trim();
    if input.is_empty() {
        bail!("package name cannot be empty");
    }
    let (name, version) = match input.rsplit_once('@') {
        Some((name, version)) if name.contains('/') && !version.is_empty() => {
            (name, Some(version.to_string()))
        }
        _ => (input, None),
    };
    if !name.contains('/') || name.starts_with('/') || name.ends_with('/') {
        bail!("package must use the vendor/package form, optionally followed by @version");
    }
    Ok((canonical_package_name(name).into_owned(), version))
}

fn select_installed_package<'a>(
    packages: &'a [Package],
    name: &str,
    version: Option<&str>,
) -> Result<&'a Package> {
    let matching: Vec<_> = packages
        .iter()
        .filter(|package| {
            canonical_package_name(&package.name) == name
                && version.is_none_or(|version| {
                    package.version.as_str() == version
                        || package.pretty_version.as_deref() == Some(version)
                })
        })
        .collect();
    match matching.as_slice() {
        [] => match version {
            Some(version) => bail!("package {name}@{version} is not installed"),
            None => bail!("package {name} is not installed"),
        },
        [package] => Ok(*package),
        _ => bail!("multiple installed versions of {name} match; specify @version"),
    }
}

fn default_patch_path(patches_dir: &Path, package: &str, version: &str) -> Result<PathBuf> {
    let directory = portable_relative_path(patches_dir, "patches directory")?;
    validate_native_patch_path(&directory)?;
    let safe_package = package.replace('/', "+");
    let safe_version: String = version
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    Ok(PathBuf::from(directory).join(format!("{safe_package}@{safe_version}.patch")))
}

fn portable_relative_path(path: &Path, description: &str) -> Result<String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.has_root()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("{description} must be a non-empty project-relative path");
    }
    let text = path
        .to_str()
        .with_context(|| format!("{description} must be UTF-8"))?;
    #[cfg(not(windows))]
    if text.contains('\\') {
        bail!("{description} must use portable forward slashes");
    }
    Ok(text.replace('\\', "/"))
}

fn upsert_native_declaration(document: &mut Value, selector: &str, path: &str) -> Result<()> {
    let root = document
        .as_object_mut()
        .context("composer.json must contain a JSON object")?;
    let extra = object_entry(root, "extra", "composer.json extra")?;
    let riff = object_entry(extra, "riff", "extra.riff")?;
    let patched = object_entry(
        riff,
        "patched-dependencies",
        "extra.riff.patched-dependencies",
    )?;
    patched.insert(selector.to_string(), Value::String(path.to_string()));
    Ok(())
}

fn object_entry<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
    description: &str,
) -> Result<&'a mut Map<String, Value>> {
    let value = parent
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if value.is_null() {
        *value = Value::Object(Map::new());
    }
    value
        .as_object_mut()
        .with_context(|| format!("{description} must be an object"))
}

fn remove_native_declarations(document: &mut Value, selectors: &BTreeSet<String>) -> Result<()> {
    let root = document
        .as_object_mut()
        .context("composer.json must contain a JSON object")?;
    let Some(extra) = root.get_mut("extra").and_then(Value::as_object_mut) else {
        bail!("composer.json has no extra object");
    };
    let Some(riff) = extra.get_mut("riff").and_then(Value::as_object_mut) else {
        bail!("composer.json has no extra.riff object");
    };
    let Some(patched) = riff
        .get_mut("patched-dependencies")
        .and_then(Value::as_object_mut)
    else {
        bail!("composer.json has no native patched-dependencies object");
    };
    for selector in selectors {
        patched.remove(selector);
    }
    if patched.is_empty() {
        riff.remove("patched-dependencies");
    }
    if riff.is_empty() {
        extra.remove("riff");
    }
    if extra.is_empty() {
        root.remove("extra");
    }
    Ok(())
}

fn resolve_declared_selector(
    requested: &str,
    declarations: &[super::NativePatchDeclaration],
) -> Result<String> {
    if declarations
        .iter()
        .any(|declaration| declaration.selector == requested)
    {
        return Ok(requested.to_string());
    }
    let (name, version) = requested
        .rsplit_once('@')
        .with_context(|| format!("patch selector {requested:?} must use vendor/package@version"))?;
    let name = canonical_package_name(name);
    let matches: Vec<_> = declarations
        .iter()
        .filter(|declaration| declaration.package == name && declaration.version == version)
        .collect();
    match matches.as_slice() {
        [declaration] => Ok(declaration.selector.clone()),
        [] => bail!("no native patch is declared for {requested}"),
        _ => bail!("multiple native patch declarations match {requested}"),
    }
}

fn json_contains_string(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(value) => value == needle,
        Value::Array(values) => values
            .iter()
            .any(|value| json_contains_string(value, needle)),
        Value::Object(values) => values
            .values()
            .any(|value| json_contains_string(value, needle)),
        _ => false,
    }
}

fn prepare_lockfile_mutation(
    project: &Path,
    manifest: &Value,
) -> Result<Option<RiffLockfileMutation>> {
    let path = project.join("composer.lock");
    let Some(previous) = read_optional(&path)? else {
        return Ok(None);
    };
    let mut document: Value = serde_json::from_slice(&previous)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let object = document
        .as_object_mut()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    let manifest = serde_json::to_string(manifest)?;
    object.insert(
        "content-hash".to_string(),
        Value::String(crate::compute_content_hash(&manifest)),
    );
    Ok(Some(RiffLockfileMutation {
        path,
        previous,
        document,
    }))
}

fn write_lockfile_mutation(mutation: Option<&RiffLockfileMutation>) -> Result<()> {
    if let Some(mutation) = mutation {
        write_json_atomic(&mutation.path, &mutation.document)?;
    }
    Ok(())
}

fn restore_lockfile_mutation(mutation: Option<&RiffLockfileMutation>) -> Result<()> {
    if let Some(mutation) = mutation {
        write_atomic(&mutation.path, &mutation.previous)?;
    }
    Ok(())
}

fn rollback_all(results: Vec<Result<()>>) -> Result<()> {
    let errors = results
        .into_iter()
        .filter_map(Result::err)
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        bail!(errors.join("; "))
    }
}

fn copy_tree(source: &Path, destination: &Path, writable: bool) -> Result<()> {
    let mut directories = Vec::new();
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry.with_context(|| format!("Failed to walk {}", source.display()))?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .context("Failed to resolve patch snapshot path")?;
        let target = destination.join(relative);
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            fs::create_dir_all(&target)?;
            directories.push((
                target,
                editable_permissions(metadata.permissions(), true, writable),
            ));
        } else if metadata.file_type().is_symlink() {
            let link = fs::read_link(entry.path())?;
            create_symlink(&link, &target, entry.path())?;
        } else if metadata.is_file() {
            let parent = target.parent().context("snapshot file has no parent")?;
            fs::create_dir_all(parent)?;
            fs::copy(entry.path(), &target)?;
            fs::set_permissions(
                &target,
                editable_permissions(metadata.permissions(), false, writable),
            )?;
        } else {
            bail!(
                "unsupported special file in package: {}",
                entry.path().display()
            );
        }
    }
    for (directory, permissions) in directories.into_iter().rev() {
        fs::set_permissions(directory, permissions)?;
    }
    Ok(())
}

#[cfg(unix)]
fn editable_permissions(
    permissions: fs::Permissions,
    directory: bool,
    writable: bool,
) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    if !writable {
        return permissions;
    }
    let owner_access = if directory { 0o300 } else { 0o200 };
    fs::Permissions::from_mode(permissions.mode() | owner_access)
}

#[cfg(not(unix))]
fn editable_permissions(
    mut permissions: fs::Permissions,
    _directory: bool,
    writable: bool,
) -> fs::Permissions {
    if writable {
        permissions.set_readonly(false);
    }
    permissions
}

#[cfg(unix)]
fn create_symlink(link: &Path, target: &Path, _source: &Path) -> Result<()> {
    std::os::unix::fs::symlink(link, target)?;
    Ok(())
}

#[cfg(windows)]
fn create_symlink(link: &Path, target: &Path, source: &Path) -> Result<()> {
    let points_to_directory = fs::metadata(source).is_ok_and(|metadata| metadata.is_dir());
    if points_to_directory {
        std::os::windows::fs::symlink_dir(link, target)?;
    } else {
        std::os::windows::fs::symlink_file(link, target)?;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_link: &Path, target: &Path, _source: &Path) -> Result<()> {
    bail!(
        "symlinks are unsupported on this platform: {}",
        target.display()
    )
}

fn tree_digest(root: &Path) -> Result<String> {
    let mut entries = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.with_context(|| format!("Failed to walk {}", root.display()))?;
        if entry.path() != root {
            entries.push(entry.path().to_path_buf());
        }
    }
    entries.sort();
    let mut hasher = Sha256::new();
    for path in entries {
        let relative = path
            .strip_prefix(root)
            .context("Failed to hash patch snapshot path")?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        let metadata = fs::symlink_metadata(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            hasher.update(metadata.permissions().mode().to_le_bytes());
        }
        #[cfg(not(unix))]
        hasher.update([u8::from(metadata.permissions().readonly())]);
        if metadata.file_type().is_symlink() {
            hasher.update(b"link\0");
            hasher.update(fs::read_link(&path)?.to_string_lossy().as_bytes());
        } else if metadata.is_dir() {
            hasher.update(b"dir\0");
        } else if metadata.is_file() {
            hasher.update(b"file\0");
            hasher.update(fs::read(&path)?);
        }
        hasher.update([0xff]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, formatter);
    value.serialize(&mut serializer)?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)
            .map(|metadata| metadata.permissions().mode())
            .unwrap_or(0o644);
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Failed to atomically write {}", path.display()))?;
    Ok(())
}

fn restore_optional(path: &Path, contents: Option<&[u8]>) -> Result<()> {
    match contents {
        Some(contents) => write_atomic(path, contents),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("Failed to remove {}", path.display()))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::{apply_patch, read_native_lock};
    use serde_json::json;

    fn package() -> Package {
        let mut package = Package::new("vendor/package", "1.2.3.0");
        package.pretty_version = Some("1.2.3".into());
        package
    }

    fn project() -> tempfile::TempDir {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("composer.json"), "{}\n").unwrap();
        fs::write(
            project.path().join("composer.lock"),
            r#"{"content-hash":"stale","packages":[]}"#,
        )
        .unwrap();
        fs::create_dir_all(project.path().join("vendor/vendor/package")).unwrap();
        fs::write(
            project.path().join("vendor/vendor/package/example.txt"),
            "before\n",
        )
        .unwrap();
        project
    }

    #[test]
    fn edit_commit_records_lock_and_round_trips() {
        let project = project();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                project.path().join("vendor/vendor/package/example.txt"),
                fs::Permissions::from_mode(0o444),
            )
            .unwrap();
        }
        let edit_parent = project.path().join("edit");
        let edit = begin_patch_edit(
            project.path(),
            &project.path().join("vendor"),
            &[package()],
            &json!({}),
            "vendor/package",
            Some(&edit_parent),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(
                fs::metadata(edit.user_dir.join("example.txt"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o200,
                0
            );
        }
        fs::write(edit.user_dir.join("example.txt"), "after\n").unwrap();

        let result = commit_patch_edit(&edit.user_dir, Path::new("patches"), &[package()]).unwrap();
        assert_eq!(result.selector, "vendor/package@1.2.3");
        assert!(result.patch_path.is_file());
        assert!(project.path().join(NATIVE_PATCH_LOCK_FILE).is_file());
        let manifest: Value =
            serde_json::from_slice(&fs::read(project.path().join("composer.json")).unwrap())
                .unwrap();
        assert_eq!(
            manifest["extra"]["riff"]["patched-dependencies"]["vendor/package@1.2.3"],
            "patches/vendor+package@1.2.3.patch"
        );
        let manifest_content = fs::read_to_string(project.path().join("composer.json")).unwrap();
        let lockfile: Value =
            serde_json::from_slice(&fs::read(project.path().join("composer.lock")).unwrap())
                .unwrap();
        assert_eq!(
            lockfile["content-hash"],
            crate::compute_content_hash(&manifest_content)
        );

        let pristine = project.path().join("pristine");
        fs::create_dir(&pristine).unwrap();
        fs::write(pristine.join("example.txt"), "before\n").unwrap();
        let patch = fs::read_to_string(result.patch_path).unwrap();
        apply_patch(&pristine, &patch, 1).unwrap();
        assert_eq!(
            fs::read_to_string(pristine.join("example.txt")).unwrap(),
            "after\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(project.path().join("composer.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o644
            );
        }
    }

    #[test]
    fn source_snapshot_changes_are_rejected() {
        let project = project();
        let edit = begin_patch_edit(
            project.path(),
            &project.path().join("vendor"),
            &[package()],
            &json!({}),
            "vendor/package@1.2.3",
            Some(&project.path().join("edit")),
        )
        .unwrap();
        fs::write(edit.source_dir.join("example.txt"), "tampered\n").unwrap();
        assert!(read_patch_edit(&edit.user_dir).is_err());
    }

    #[test]
    fn failed_commit_restores_manifest_lock_and_patch_file() {
        let project = project();
        let original_manifest = fs::read(project.path().join("composer.json")).unwrap();
        let original_lock = fs::read(project.path().join("composer.lock")).unwrap();
        let edit = begin_patch_edit(
            project.path(),
            &project.path().join("vendor"),
            &[package()],
            &json!({}),
            "vendor/package",
            Some(&project.path().join("edit")),
        )
        .unwrap();
        fs::write(edit.user_dir.join("example.txt"), "after\n").unwrap();

        assert!(commit_patch_edit(&edit.user_dir, Path::new("patches"), &[]).is_err());

        assert_eq!(
            fs::read(project.path().join("composer.json")).unwrap(),
            original_manifest
        );
        assert_eq!(
            fs::read(project.path().join("composer.lock")).unwrap(),
            original_lock
        );
        assert!(!project
            .path()
            .join("patches/vendor+package@1.2.3.patch")
            .exists());
        assert!(!project.path().join(NATIVE_PATCH_LOCK_FILE).exists());
    }

    #[test]
    fn remove_declaration_deletes_unshared_patch() {
        let project = project();
        fs::create_dir(project.path().join("patches")).unwrap();
        fs::write(project.path().join("patches/fix.patch"), "patch\n").unwrap();
        fs::write(
            project.path().join("composer.json"),
            serde_json::to_vec_pretty(&json!({"extra": {"riff": {
                "patched-dependencies": {"vendor/package@1.2.3": "patches/fix.patch"}
            }}}))
            .unwrap(),
        )
        .unwrap();
        relock_native(
            project.path(),
            &json!({"riff": {"patched-dependencies": {
                "vendor/package@1.2.3": "patches/fix.patch"
            }}}),
            &[package()],
        )
        .unwrap();

        let result = remove_native_patches(
            project.path(),
            &["vendor/package@1.2.3".to_string()],
            &[package()],
        )
        .unwrap();
        assert_eq!(result.deleted_files.len(), 1);
        assert!(!project.path().join("patches/fix.patch").exists());
        assert!(read_native_lock(project.path())
            .unwrap()
            .unwrap()
            .patches
            .is_empty());
    }
}
