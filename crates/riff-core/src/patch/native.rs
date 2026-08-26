use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::package::Package;
use crate::util::canonical_package_name;

pub const NATIVE_PATCH_LOCK_FILE: &str = "riff-patches.lock.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativePatchDeclaration {
    pub selector: String,
    pub package: String,
    pub version: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NativePatchLock {
    #[serde(rename = "lock-version")]
    pub lock_version: u32,
    #[serde(rename = "_hash")]
    pub hash: String,
    pub patches: BTreeMap<String, NativePatchLockEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NativePatchLockEntry {
    pub path: String,
    #[serde(rename = "version-normalized")]
    pub version_normalized: String,
    pub sha256: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedNativePatch {
    pub selector: String,
    pub package: String,
    pub path: PathBuf,
    pub sha256: String,
}

pub fn native_declarations(extra: &Value) -> Result<Vec<NativePatchDeclaration>> {
    let Some(riff) = extra.get("riff") else {
        return Ok(Vec::new());
    };
    let riff = riff.as_object().context("extra.riff must be an object")?;
    let Some(patches) = riff.get("patched-dependencies") else {
        return Ok(Vec::new());
    };
    let patches = patches
        .as_object()
        .context("extra.riff.patched-dependencies must be an object")?;

    let mut declarations = Vec::with_capacity(patches.len());
    for (selector, path) in patches {
        let path = path.as_str().with_context(|| {
            format!("extra.riff.patched-dependencies.{selector} must be a string")
        })?;
        let (package, version) = split_selector(selector)?;
        validate_native_patch_path(path)
            .with_context(|| format!("invalid native patch path for {selector}"))?;
        declarations.push(NativePatchDeclaration {
            selector: selector.clone(),
            package,
            version,
            path: PathBuf::from(path),
        });
    }
    Ok(declarations)
}

pub fn read_native_lock(root: &Path) -> Result<Option<NativePatchLock>> {
    let path = root.join(NATIVE_PATCH_LOCK_FILE);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()))
        }
    };
    let lock: NativePatchLock = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    if lock.lock_version != 1 {
        bail!(
            "Unsupported {} lock version {}; expected 1",
            NATIVE_PATCH_LOCK_FILE,
            lock.lock_version
        );
    }
    let expected_hash = lock_hash(&lock.patches)?;
    if lock.hash != expected_hash {
        bail!(
            "{} has an invalid _hash; run `riff patches-relock`",
            NATIVE_PATCH_LOCK_FILE
        );
    }
    Ok(Some(lock))
}

pub fn relock_native(root: &Path, extra: &Value, packages: &[Package]) -> Result<NativePatchLock> {
    let declarations = native_declarations(extra)?;
    let mut patches = BTreeMap::new();
    for declaration in declarations {
        let package = find_matching_package(&declaration, packages).with_context(|| {
            format!(
                "native patch selector {} does not match a locked or installed package",
                declaration.selector
            )
        })?;
        let absolute = root.join(&declaration.path);
        let sha256 = sha256_file(&absolute).with_context(|| {
            format!(
                "Failed to hash native patch {} for {}",
                absolute.display(),
                declaration.selector
            )
        })?;
        patches.insert(
            declaration.selector,
            NativePatchLockEntry {
                path: declaration.path.to_string_lossy().replace('\\', "/"),
                version_normalized: package.version.to_string(),
                sha256,
            },
        );
    }
    let lock = NativePatchLock {
        lock_version: 1,
        hash: lock_hash(&patches)?,
        patches,
    };
    write_native_lock(root, &lock)?;
    Ok(lock)
}

pub(crate) fn resolve_native_patches(
    root: &Path,
    extra: &Value,
    packages: &[Package],
) -> Result<Vec<ResolvedNativePatch>> {
    let declarations = native_declarations(extra)?;
    let lock = read_native_lock(root)?;
    if declarations.is_empty() {
        if lock.as_ref().is_some_and(|lock| !lock.patches.is_empty()) {
            bail!(
                "{} contains patches that are no longer declared; run `riff patches-relock`",
                NATIVE_PATCH_LOCK_FILE
            );
        }
        return Ok(Vec::new());
    }
    let lock = lock.with_context(|| {
        format!(
            "native patches are declared but {} is missing; run `riff patches-relock`",
            NATIVE_PATCH_LOCK_FILE
        )
    })?;
    if lock.patches.len() != declarations.len() {
        bail!(
            "native patch declarations differ from {}; run `riff patches-relock`",
            NATIVE_PATCH_LOCK_FILE
        );
    }

    let mut resolved = Vec::new();
    for declaration in declarations {
        let entry = lock.patches.get(&declaration.selector).with_context(|| {
            format!(
                "native patch {} is not locked; run `riff patches-relock`",
                declaration.selector
            )
        })?;
        let declared_path = declaration.path.to_string_lossy().replace('\\', "/");
        if entry.path != declared_path {
            bail!(
                "native patch path for {} differs from {}; run `riff patches-relock`",
                declaration.selector,
                NATIVE_PATCH_LOCK_FILE
            );
        }
        let absolute = root.join(&declaration.path);
        let actual_sha256 = sha256_file(&absolute)
            .with_context(|| format!("Failed to hash native patch {}", absolute.display()))?;
        if !entry.sha256.eq_ignore_ascii_case(&actual_sha256) {
            bail!(
                "native patch {} has changed since {}; run `riff patches-relock`",
                declaration.selector,
                NATIVE_PATCH_LOCK_FILE
            );
        }
        let Some(package) = find_matching_package(&declaration, packages) else {
            continue;
        };
        if package.version.as_str() != entry.version_normalized {
            bail!(
                "native patch {} was locked for normalized version {}, but {} is selected; run `riff patches-relock`",
                declaration.selector,
                entry.version_normalized,
                package.version
            );
        }
        resolved.push(ResolvedNativePatch {
            selector: declaration.selector,
            package: declaration.package,
            path: absolute,
            sha256: actual_sha256,
        });
    }
    Ok(resolved)
}

fn write_native_lock(root: &Path, lock: &NativePatchLock) -> Result<()> {
    let path = root.join(NATIVE_PATCH_LOCK_FILE);
    let mut bytes =
        serde_json::to_vec_pretty(lock).context("Failed to serialize native patch lock")?;
    bytes.push(b'\n');
    let mut temporary = tempfile::NamedTempFile::new_in(root)
        .context("Failed to create temporary native patch lock")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path)
            .map(|metadata| metadata.permissions().mode())
            .unwrap_or(0o644);
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&path)
        .map_err(|error| error.error)
        .with_context(|| format!("Failed to atomically write {}", path.display()))?;
    Ok(())
}

fn split_selector(selector: &str) -> Result<(String, String)> {
    let (name, version) = selector.rsplit_once('@').with_context(|| {
        format!("native patch selector {selector:?} must use vendor/package@version")
    })?;
    if !name.contains('/') || name.is_empty() || version.is_empty() {
        bail!("native patch selector {selector:?} must use vendor/package@version");
    }
    Ok((
        canonical_package_name(name).into_owned(),
        version.to_string(),
    ))
}

pub fn validate_native_patch_path(path: &str) -> Result<()> {
    if path.is_empty() || path.contains('\0') || path.contains('\\') {
        bail!("path must be a non-empty portable project-relative path");
    }
    let path = Path::new(path);
    if path.is_absolute()
        || path.has_root()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("path must stay within the project root");
    }
    Ok(())
}

fn find_matching_package<'a>(
    declaration: &NativePatchDeclaration,
    packages: &'a [Package],
) -> Option<&'a Package> {
    packages.iter().find(|package| {
        canonical_package_name(&package.name) == declaration.package
            && (package.version.as_str() == declaration.version
                || package.pretty_version.as_deref() == Some(declaration.version.as_str()))
    })
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn lock_hash(patches: &BTreeMap<String, NativePatchLockEntry>) -> Result<String> {
    let canonical = serde_json::to_vec(patches)?;
    let mut hasher = Sha256::new();
    hasher.update(canonical);
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn package() -> Package {
        let mut package = Package::new("vendor/package", "1.2.3.0");
        package.pretty_version = Some("1.2.3".into());
        package
    }

    #[test]
    fn parses_exact_native_selector_map() {
        let declarations = native_declarations(&json!({
            "riff": {"patched-dependencies": {
                "Vendor/Package@1.2.3": "patches/vendor+package@1.2.3.patch"
            }}
        }))
        .unwrap();
        assert_eq!(declarations[0].package, "vendor/package");
        assert_eq!(declarations[0].version, "1.2.3");
    }

    #[test]
    fn relock_and_resolve_detect_file_drift() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("patches")).unwrap();
        fs::write(directory.path().join("patches/fix.patch"), "patch one\n").unwrap();
        let extra = json!({"riff": {"patched-dependencies": {
            "vendor/package@1.2.3": "patches/fix.patch"
        }}});
        relock_native(directory.path(), &extra, &[package()]).unwrap();
        assert_eq!(
            resolve_native_patches(directory.path(), &extra, &[package()])
                .unwrap()
                .len(),
            1
        );

        fs::write(directory.path().join("patches/fix.patch"), "patch two\n").unwrap();
        assert!(resolve_native_patches(directory.path(), &extra, &[package()]).is_err());
    }

    #[test]
    fn refuses_patch_paths_outside_project() {
        assert!(native_declarations(&json!({
            "riff": {"patched-dependencies": {
                "vendor/package@1.2.3": "../outside.patch"
            }}
        }))
        .is_err());
    }
}
