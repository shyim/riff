use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;
use walkdir::WalkDir;

#[derive(Debug, Error)]
pub enum PatchApplyError {
    #[error("patch contained no file sections")]
    Empty,
    #[error("unsupported patch record: {0}")]
    Unsupported(String),
    #[error("unsafe patch path {0:?}")]
    UnsafePath(String),
    #[error("patch target contains a symlink or junction: {0}")]
    Symlink(PathBuf),
    #[error("patch target is not UTF-8 text: {0}")]
    Binary(PathBuf),
    #[error("failed to apply patch to {path}: {message}")]
    Apply { path: PathBuf, message: String },
    #[error("failed to access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug)]
struct PatchSection {
    old_path: Option<String>,
    new_path: Option<String>,
    body: String,
}

/// Apply a UTF-8 multi-file unified diff beneath `package_dir`.
///
/// Every hunk is applied in memory before any filesystem mutation begins. Path
/// components are stripped according to `depth`, then validated without ever
/// following symlinks or platform reparse points.
pub fn apply_patch(
    package_dir: &Path,
    patch_text: &str,
    depth: u32,
) -> Result<(), PatchApplyError> {
    reject_unsupported_records(patch_text)?;
    ensure_root_is_directory(package_dir)?;
    let sections = split_patch_sections(patch_text)?;
    if sections.is_empty() {
        return Err(PatchApplyError::Empty);
    }

    let mut staged: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
    for section in sections {
        let header_path = section
            .new_path
            .as_deref()
            .or(section.old_path.as_deref())
            .ok_or(PatchApplyError::Empty)?;
        let relative = strip_and_validate_path(header_path, depth)?;
        ensure_no_symlink_in_chain(package_dir, &relative)?;
        let target = package_dir.join(&relative);

        let original = match staged.get(&relative) {
            Some(Some(value)) => value.clone(),
            Some(None) => String::new(),
            None => read_text_or_empty(&target)?,
        };
        let was_crlf = original.contains("\r\n");
        let normalized = if was_crlf {
            original.replace("\r\n", "\n")
        } else {
            original
        };
        let parsed =
            diffy::Patch::from_str(&section.body).map_err(|error| PatchApplyError::Apply {
                path: relative.clone(),
                message: error.to_string(),
            })?;
        let patched =
            diffy::apply(&normalized, &parsed).map_err(|error| PatchApplyError::Apply {
                path: relative.clone(),
                message: error.to_string(),
            })?;

        if section.new_path.is_none() {
            staged.insert(relative, None);
        } else {
            let patched = if was_crlf {
                patched.replace('\n', "\r\n").replace("\r\r\n", "\r\n")
            } else {
                patched
            };
            staged.insert(relative, Some(patched));
        }
    }

    for (relative, content) in staged {
        let target = package_dir.join(&relative);
        match content {
            Some(content) => atomic_write(&target, content.as_bytes())?,
            None => match fs::remove_file(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(PatchApplyError::Io {
                        path: target,
                        source,
                    })
                }
            },
        }
    }
    Ok(())
}

/// Create a Git-style, text-only multi-file patch between two directory trees.
pub fn create_patch(source_dir: &Path, edited_dir: &Path) -> Result<String, PatchApplyError> {
    let mut paths = BTreeSet::new();
    collect_paths(source_dir, &mut paths)?;
    collect_paths(edited_dir, &mut paths)?;

    let mut output = String::new();
    for relative in paths {
        let source = source_dir.join(&relative);
        let edited = edited_dir.join(&relative);
        let source_metadata = fs::symlink_metadata(&source).ok();
        let edited_metadata = fs::symlink_metadata(&edited).ok();
        let source_link = source_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_symlink());
        let edited_link = edited_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_symlink());
        if source_link || edited_link {
            let same = source_link
                && edited_link
                && fs::read_link(&source).ok() == fs::read_link(&edited).ok();
            if same {
                continue;
            }
            return Err(PatchApplyError::Symlink(relative));
        }
        let source_is_dir = source_metadata.as_ref().is_some_and(|value| value.is_dir());
        let edited_is_dir = edited_metadata.as_ref().is_some_and(|value| value.is_dir());
        if source_is_dir && edited_is_dir {
            continue;
        }
        if source_is_dir != edited_is_dir {
            return Err(PatchApplyError::Unsupported(format!(
                "file/directory type change at {}",
                relative.display()
            )));
        }
        if source_metadata
            .as_ref()
            .is_some_and(|value| !value.is_file())
            || edited_metadata
                .as_ref()
                .is_some_and(|value| !value.is_file())
        {
            return Err(PatchApplyError::Unsupported(format!(
                "special file at {}",
                relative.display()
            )));
        }

        #[cfg(unix)]
        if let (Some(source), Some(edited)) = (&source_metadata, &edited_metadata) {
            use std::os::unix::fs::PermissionsExt;
            if source.permissions().mode() & 0o111 != edited.permissions().mode() & 0o111 {
                return Err(PatchApplyError::Unsupported(format!(
                    "executable mode change at {}",
                    relative.display()
                )));
            }
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if source_metadata.is_none()
                && edited_metadata
                    .as_ref()
                    .is_some_and(|edited| edited.permissions().mode() & 0o111 != 0)
            {
                return Err(PatchApplyError::Unsupported(format!(
                    "executable file addition at {}",
                    relative.display()
                )));
            }
        }

        let source_bytes = read_bytes_or_empty(&source)?;
        let edited_bytes = read_bytes_or_empty(&edited)?;
        let source_exists = source_metadata.is_some();
        let edited_exists = edited_metadata.is_some();
        if source_exists == edited_exists && source_bytes == edited_bytes {
            continue;
        }
        if source_bytes.is_empty() && edited_bytes.is_empty() {
            return Err(PatchApplyError::Unsupported(format!(
                "empty file addition or deletion at {}",
                relative.display()
            )));
        }
        let source_text = std::str::from_utf8(&source_bytes)
            .map_err(|_| PatchApplyError::Binary(relative.clone()))?;
        let edited_text = std::str::from_utf8(&edited_bytes)
            .map_err(|_| PatchApplyError::Binary(relative.clone()))?;
        let relative_text = relative
            .to_str()
            .ok_or_else(|| PatchApplyError::UnsafePath(relative.display().to_string()))?
            .replace('\\', "/");
        let patch = diffy::create_patch(source_text, edited_text).to_string();
        let body = strip_diffy_headers(&patch);
        output.push_str(&format!("diff --git a/{relative_text} b/{relative_text}\n"));
        if source.exists() {
            output.push_str(&format!("--- a/{relative_text}\n"));
        } else {
            output.push_str("--- /dev/null\n");
        }
        if edited.exists() {
            output.push_str(&format!("+++ b/{relative_text}\n"));
        } else {
            output.push_str("+++ /dev/null\n");
        }
        output.push_str(body);
        if !output.ends_with('\n') {
            output.push('\n');
        }
    }
    Ok(output)
}

fn reject_unsupported_records(text: &str) -> Result<(), PatchApplyError> {
    for line in text.lines() {
        let unsupported = [
            "GIT binary patch",
            "Binary files ",
            "rename from ",
            "rename to ",
            "copy from ",
            "copy to ",
            "old mode ",
            "new mode ",
        ];
        if unsupported.iter().any(|prefix| line.starts_with(prefix)) {
            return Err(PatchApplyError::Unsupported(line.to_string()));
        }
    }
    Ok(())
}

fn split_patch_sections(text: &str) -> Result<Vec<PatchSection>, PatchApplyError> {
    let mut sections = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0usize;
    while index < lines.len() {
        if lines[index].starts_with("diff --git ") {
            index += 1;
            while index < lines.len()
                && !lines[index].starts_with("--- ")
                && !lines[index].starts_with("diff --git ")
            {
                index += 1;
            }
            if index >= lines.len() || lines[index].starts_with("diff --git ") {
                return Err(PatchApplyError::Unsupported(
                    "git diff section without unified hunks".to_string(),
                ));
            }
        } else if !lines[index].starts_with("--- ") {
            index += 1;
            continue;
        }

        let old_header = lines[index]
            .strip_prefix("--- ")
            .ok_or(PatchApplyError::Empty)?;
        index += 1;
        let new_header = lines
            .get(index)
            .and_then(|line| line.strip_prefix("+++ "))
            .ok_or_else(|| PatchApplyError::Unsupported("missing +++ header".to_string()))?;
        index += 1;
        let old_path = header_path(old_header)?;
        let new_path = header_path(new_header)?;
        let mut body = format!("--- {old_header}\n+++ {new_header}\n");
        let mut saw_hunk = false;
        let mut hunk_remaining: Option<(usize, usize)> = None;

        while index < lines.len() {
            if lines[index].starts_with("diff --git ") {
                break;
            }
            if lines[index].starts_with("--- ") && saw_hunk && hunk_remaining.is_none() {
                break;
            }
            if let Some(counts) = hunk_line_counts(lines[index]) {
                saw_hunk = true;
                hunk_remaining = (counts != (0, 0)).then_some(counts);
            } else if let Some((old, new)) = hunk_remaining.as_mut() {
                match lines[index].as_bytes().first() {
                    Some(b' ') => {
                        *old = old.saturating_sub(1);
                        *new = new.saturating_sub(1);
                    }
                    Some(b'-') => *old = old.saturating_sub(1),
                    Some(b'+') => *new = new.saturating_sub(1),
                    _ => {}
                }
                if *old == 0 && *new == 0 {
                    hunk_remaining = None;
                }
            }
            body.push_str(lines[index]);
            body.push('\n');
            index += 1;
        }
        if !saw_hunk {
            return Err(PatchApplyError::Unsupported(
                "file section without hunks".to_string(),
            ));
        }
        sections.push(PatchSection {
            old_path,
            new_path,
            body,
        });
    }
    Ok(sections)
}

fn hunk_line_counts(header: &str) -> Option<(usize, usize)> {
    let ranges = header.strip_prefix("@@ -")?.split_once(" @@")?.0;
    let (old, new) = ranges.split_once(" +")?;
    let count = |range: &str| {
        range
            .split_once(',')
            .map_or(Some(1), |(_, count)| count.parse().ok())
    };
    Some((count(old)?, count(new)?))
}

fn header_path(header: &str) -> Result<Option<String>, PatchApplyError> {
    let header = header
        .split_once('\t')
        .map_or(header, |(path, _timestamp)| path)
        .trim();
    if header == "/dev/null" {
        return Ok(None);
    }
    if let Some(quoted) = header
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        return unescape_git_quoted(quoted)
            .map(Some)
            .ok_or_else(|| PatchApplyError::UnsafePath(header.to_string()));
    }
    Ok(Some(header.to_string()))
}

fn unescape_git_quoted(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            output.push(bytes[index]);
            index += 1;
            continue;
        }
        let escaped = *bytes.get(index + 1)?;
        match escaped {
            b'\\' | b'"' => output.push(escaped),
            b'n' => output.push(b'\n'),
            b'r' => output.push(b'\r'),
            b't' => output.push(b'\t'),
            b'0'..=b'3' => {
                let second = *bytes.get(index + 2)?;
                let third = *bytes.get(index + 3)?;
                if !(b'0'..=b'7').contains(&second) || !(b'0'..=b'7').contains(&third) {
                    return None;
                }
                output.push(((escaped - b'0') << 6) | ((second - b'0') << 3) | (third - b'0'));
                index += 2;
            }
            _ => return None,
        }
        index += 2;
    }
    String::from_utf8(output).ok()
}

fn strip_and_validate_path(raw: &str, depth: u32) -> Result<PathBuf, PatchApplyError> {
    if raw.is_empty() || raw.contains('\0') || raw.contains('\\') {
        return Err(PatchApplyError::UnsafePath(raw.to_string()));
    }
    let components: Vec<_> = Path::new(raw).components().collect();
    let depth = usize::try_from(depth).unwrap_or(usize::MAX);
    if components.len() <= depth {
        return Err(PatchApplyError::UnsafePath(raw.to_string()));
    }
    let mut output = PathBuf::new();
    for component in &components[depth..] {
        match component {
            Component::Normal(value) => output.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(PatchApplyError::UnsafePath(raw.to_string()))
            }
        }
    }
    if output.as_os_str().is_empty() {
        return Err(PatchApplyError::UnsafePath(raw.to_string()));
    }
    Ok(output)
}

fn ensure_no_symlink_in_chain(root: &Path, relative: &Path) -> Result<(), PatchApplyError> {
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        cursor.push(component);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(PatchApplyError::Symlink(cursor))
            }
            #[cfg(windows)]
            Ok(metadata) => {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Err(PatchApplyError::Symlink(cursor));
                }
            }
            #[cfg(not(windows))]
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => {
                return Err(PatchApplyError::Io {
                    path: cursor,
                    source,
                })
            }
        }
    }
    Ok(())
}

fn ensure_root_is_directory(root: &Path) -> Result<(), PatchApplyError> {
    let metadata = fs::symlink_metadata(root).map_err(|source| PatchApplyError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(PatchApplyError::Symlink(root.to_path_buf()));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(PatchApplyError::Symlink(root.to_path_buf()));
        }
    }
    if !metadata.is_dir() {
        return Err(PatchApplyError::UnsafePath(root.display().to_string()));
    }
    Ok(())
}

fn read_text_or_empty(path: &Path) -> Result<String, PatchApplyError> {
    match fs::read(path) {
        Ok(bytes) => {
            String::from_utf8(bytes).map_err(|_| PatchApplyError::Binary(path.to_path_buf()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(source) => Err(PatchApplyError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn read_bytes_or_empty(path: &Path) -> Result<Vec<u8>, PatchApplyError> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(PatchApplyError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn collect_paths(root: &Path, output: &mut BTreeSet<PathBuf>) -> Result<(), PatchApplyError> {
    if !root.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| PatchApplyError::Io {
            path: error.path().unwrap_or(root).to_path_buf(),
            source: error
                .into_io_error()
                .unwrap_or_else(|| std::io::Error::other("failed to walk patch tree")),
        })?;
        if entry.path() == root {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| PatchApplyError::UnsafePath(entry.path().display().to_string()))?;
        output.insert(relative.to_path_buf());
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), PatchApplyError> {
    let parent = path
        .parent()
        .ok_or_else(|| PatchApplyError::UnsafePath(path.display().to_string()))?;
    fs::create_dir_all(parent).map_err(|source| PatchApplyError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| PatchApplyError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    let existing_permissions = fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.permissions());
    if let Some(permissions) = existing_permissions {
        temporary
            .as_file()
            .set_permissions(permissions)
            .map_err(|source| PatchApplyError::Io {
                path: path.to_path_buf(),
                source,
            })?;
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o644))
                .map_err(|source| PatchApplyError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
        }
    }
    use std::io::Write;
    temporary
        .write_all(bytes)
        .map_err(|source| PatchApplyError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| PatchApplyError::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    Ok(())
}

fn strip_diffy_headers(patch: &str) -> &str {
    let mut lines = patch.splitn(3, '\n');
    let _ = lines.next();
    let _ = lines.next();
    lines.next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_applies_multi_file_patch() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let edited = directory.path().join("edited");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&edited).unwrap();
        fs::write(source.join("changed.txt"), "old\n").unwrap();
        fs::write(edited.join("changed.txt"), "new\n").unwrap();
        fs::write(edited.join("added.txt"), "added\n").unwrap();

        let patch = create_patch(&source, &edited).unwrap();
        apply_patch(&source, &patch, 1).unwrap();

        assert_eq!(
            fs::read_to_string(source.join("changed.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            fs::read_to_string(source.join("added.txt")).unwrap(),
            "added\n"
        );
    }

    #[test]
    fn creates_and_applies_changes_without_trailing_newlines() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let edited = directory.path().join("edited");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&edited).unwrap();
        fs::write(source.join("file.txt"), "old").unwrap();
        fs::write(edited.join("file.txt"), "new").unwrap();

        let patch = create_patch(&source, &edited).unwrap();
        apply_patch(&source, &patch, 1).unwrap();

        assert_eq!(fs::read_to_string(source.join("file.txt")).unwrap(), "new");
    }

    #[test]
    fn applying_patch_preserves_crlf_line_endings() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("file.txt"), "old\r\nkeep\r\n").unwrap();
        let patch =
            "--- a/file.txt\r\n+++ b/file.txt\r\n@@ -1,2 +1,2 @@\r\n-old\r\n+new\r\n keep\r\n";

        apply_patch(directory.path(), patch, 1).unwrap();

        assert_eq!(
            fs::read_to_string(directory.path().join("file.txt")).unwrap(),
            "new\r\nkeep\r\n"
        );
    }

    #[test]
    fn strips_legacy_parent_component_before_validation() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/file.php"), "old\n").unwrap();
        let patch = "--- ../src/file.php\n+++ ../src/file.php\n@@ -1 +1 @@\n-old\n+new\n";
        apply_patch(directory.path(), patch, 1).unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join("src/file.php")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn refuses_path_escape_after_depth() {
        let directory = tempfile::tempdir().unwrap();
        let patch = "--- a/../../outside\n+++ b/../../outside\n@@ -0,0 +1 @@\n+pwned\n";
        assert!(matches!(
            apply_patch(directory.path(), patch, 1),
            Err(PatchApplyError::UnsafePath(_))
        ));
    }

    #[test]
    fn binary_edit_aborts_patch_creation() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let edited = directory.path().join("edited");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&edited).unwrap();
        fs::write(source.join("binary"), [0xff]).unwrap();
        fs::write(edited.join("binary"), [0xfe]).unwrap();
        assert!(matches!(
            create_patch(&source, &edited),
            Err(PatchApplyError::Binary(_))
        ));
    }

    #[test]
    fn plain_diff_content_that_starts_with_header_marker_stays_in_hunk() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("file.txt"), "--- old marker\nkeep\n").unwrap();
        let patch = "--- a/file.txt\n+++ b/file.txt\n@@ -1,2 +1,2 @@\n---- old marker\n+new marker\n keep\n";
        apply_patch(directory.path(), patch, 1).unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join("file.txt")).unwrap(),
            "new marker\nkeep\n"
        );
    }

    #[test]
    fn empty_file_additions_fail_clearly() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let edited = directory.path().join("edited");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&edited).unwrap();
        fs::write(edited.join("empty.txt"), "").unwrap();
        assert!(matches!(
            create_patch(&source, &edited),
            Err(PatchApplyError::Unsupported(message)) if message.contains("empty file")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn executable_file_additions_fail_clearly() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let edited = directory.path().join("edited");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&edited).unwrap();
        let executable = edited.join("script");
        fs::write(&executable, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            create_patch(&source, &edited),
            Err(PatchApplyError::Unsupported(message)) if message.contains("executable file")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn applying_content_patch_preserves_executable_mode() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("script");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let patch = "--- a/script\n+++ b/script\n@@ -1 +1 @@\n-old\n+new\n";
        apply_patch(directory.path(), patch, 1).unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
