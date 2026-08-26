use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

const APPLIED_PATCH_STATE_FILE: &str = "riff-patches.json";

pub fn read_applied_patch_state(vendor_dir: &Path) -> BTreeMap<String, String> {
    let path = vendor_dir.join("composer").join(APPLIED_PATCH_STATE_FILE);
    let Ok(content) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

pub fn changed_patch_packages(
    previous: &BTreeMap<String, String>,
    desired: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    previous
        .keys()
        .chain(desired.keys())
        .filter(|package| previous.get(*package) != desired.get(*package))
        .cloned()
        .collect()
}

pub fn write_applied_patch_state(
    vendor_dir: &Path,
    desired: &BTreeMap<String, String>,
) -> Result<()> {
    let composer_dir = vendor_dir.join("composer");
    fs::create_dir_all(&composer_dir)
        .with_context(|| format!("Failed to create {}", composer_dir.display()))?;
    let path = composer_dir.join(APPLIED_PATCH_STATE_FILE);
    if desired.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context(format!("Failed to remove {}", path.display()))
            }
        }
        return Ok(());
    }
    let mut bytes = serde_json::to_vec_pretty(desired)?;
    bytes.push(b'\n');
    let mut temporary = tempfile::NamedTempFile::new_in(&composer_dir)?;
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

/// Invalidate selected applied fingerprints so the next install downloads a
/// pristine package and applies its current patch set again. An empty package
/// list invalidates every recorded package.
pub fn invalidate_applied_patch_state(vendor_dir: &Path, packages: &[String]) -> Result<usize> {
    let mut state = read_applied_patch_state(vendor_dir);
    let removed = if packages.is_empty() {
        let removed = state.len();
        state.clear();
        removed
    } else {
        packages
            .iter()
            .filter(|package| state.remove(&package.to_lowercase()).is_some())
            .count()
    };
    write_applied_patch_state(vendor_dir, &state)?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_added_changed_and_removed_fingerprints() {
        let previous = BTreeMap::from([
            ("a/a".to_string(), "old".to_string()),
            ("b/b".to_string(), "removed".to_string()),
        ]);
        let desired = BTreeMap::from([
            ("a/a".to_string(), "new".to_string()),
            ("c/c".to_string(), "added".to_string()),
        ]);
        assert_eq!(
            changed_patch_packages(&previous, &desired),
            BTreeSet::from(["a/a".to_string(), "b/b".to_string(), "c/c".to_string()])
        );
    }

    #[test]
    fn invalidates_selected_fingerprints() {
        let directory = tempfile::tempdir().unwrap();
        let state = BTreeMap::from([
            ("a/a".to_string(), "one".to_string()),
            ("b/b".to_string(), "two".to_string()),
        ]);
        write_applied_patch_state(directory.path(), &state).unwrap();
        assert_eq!(
            invalidate_applied_patch_state(directory.path(), &["A/A".to_string()]).unwrap(),
            1
        );
        assert_eq!(
            read_applied_patch_state(directory.path()),
            BTreeMap::from([("b/b".to_string(), "two".to_string())])
        );
    }

    #[test]
    fn empty_state_does_not_leave_riff_metadata_in_vendor() {
        let directory = tempfile::tempdir().unwrap();
        let state = BTreeMap::from([("a/a".to_string(), "one".to_string())]);
        write_applied_patch_state(directory.path(), &state).unwrap();
        let path = directory.path().join("composer/riff-patches.json");
        assert!(path.exists());

        write_applied_patch_state(directory.path(), &BTreeMap::new()).unwrap();
        assert!(!path.exists());

        write_applied_patch_state(directory.path(), &BTreeMap::new()).unwrap();
        assert!(!path.exists());
    }
}
