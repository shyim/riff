//! Cross-platform filesystem operations shared by package-management workflows.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Normalize separators, redundant path components, drive letters, and URI prefixes.
///
/// This is a lexical operation and does not access the filesystem.
pub fn normalize_path(path: &str) -> String {
    let mut path = path.replace('\\', "/");
    let mut absolute = "";

    if path.starts_with("//") && path.len() > 2 {
        absolute = "//";
        path.drain(..2);
    }

    let (mut prefix, remaining) = split_prefix(&path);
    path = remaining.to_string();

    if path.starts_with('/') {
        absolute = "/";
        path.drain(..1);
    }

    let mut parts: Vec<&str> = Vec::new();
    let mut up = false;
    for chunk in path.split('/') {
        if chunk == ".." && (!absolute.is_empty() || up) {
            parts.pop();
            up = !parts.is_empty() && parts.last().is_some_and(|part| *part != "..");
        } else if chunk != "." && !chunk.is_empty() {
            parts.push(chunk);
            up = chunk != "..";
        }
    }

    uppercase_drive_letter(&mut prefix);
    format!("{prefix}{absolute}{}", parts.join("/"))
}

/// Return the shortest lexical path from `from` to `to`.
pub fn shortest_path(
    from: &str,
    to: &str,
    directories: bool,
    prefer_relative: bool,
) -> io::Result<String> {
    if !is_absolute_path(from) || !is_absolute_path(to) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("from ({from}) and to ({to}) must be absolute paths"),
        ));
    }

    let mut from = normalize_path(from);
    let to = normalize_path(to);
    if directories {
        from = format!("{}/dummy_file", from.trim_end_matches('/'));
    }

    if dirname(&from) == dirname(&to) {
        return Ok(format!("./{}", basename(&to)));
    }

    let mut common_path = to.clone();
    while !format!("{from}/").starts_with(&format!("{common_path}/"))
        && common_path != "/"
        && !is_drive_root(&common_path)
    {
        common_path = dirname(&common_path).to_string();
    }

    if !from.starts_with(&common_path) {
        return Ok(to);
    }

    let common_path = format!("{}/", common_path.trim_end_matches('/'));
    let source_depth = from
        .get(common_path.len()..)
        .unwrap_or_default()
        .matches('/')
        .count();

    if !prefer_relative && common_path == "/" && source_depth > 1 {
        return Ok(to);
    }

    let result = format!(
        "{}{}",
        "../".repeat(source_depth),
        to.get(common_path.len()..).unwrap_or_default()
    );
    Ok(if result.is_empty() {
        "./".to_string()
    } else {
        result
    })
}

/// Return the byte size of a file or every file below a directory.
pub fn path_size(path: &Path) -> io::Result<u64> {
    let metadata = fs::metadata(path)?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }

    let mut size = 0;
    for entry in fs::read_dir(path)? {
        size += path_size(&entry?.path())?;
    }
    Ok(size)
}

/// Recursively copy a file or directory.
pub fn copy_path(source: &Path, target: &Path) -> io::Result<()> {
    let metadata = fs::metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir_all(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_path(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, target)?;
    }
    Ok(())
}

/// Copy `source` to `target`, deleting the source only after a complete copy.
pub fn copy_then_remove(source: &Path, target: &Path) -> io::Result<()> {
    copy_path(source, target)?;
    remove_path(source)
}

/// Remove a file, directory, or symlink without following a symlinked directory.
pub fn remove_path(path: &Path) -> io::Result<()> {
    let path = trim_trailing_separators(path);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    if metadata.file_type().is_symlink() {
        return remove_symlink(&path);
    }
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn split_prefix(path: &str) -> (String, &str) {
    if path.len() >= 2 && path.as_bytes()[0].is_ascii_alphabetic() && path.as_bytes()[1] == b':' {
        return (path[..2].to_string(), &path[2..]);
    }

    let Some(colon) = path.find(':') else {
        return (String::new(), path);
    };
    if colon < 2
        || !path[..colon]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric())
    {
        return (String::new(), path);
    }

    let mut prefix_end = colon + 1;
    if path[prefix_end..].starts_with("//") {
        prefix_end += 2;
        let rest = &path[prefix_end..];
        if rest.len() >= 2 && rest.as_bytes()[0].is_ascii_alphabetic() && rest.as_bytes()[1] == b':'
        {
            prefix_end += 2;
        }
    }
    (path[..prefix_end].to_string(), &path[prefix_end..])
}

fn uppercase_drive_letter(prefix: &mut String) {
    let bytes = prefix.as_bytes();
    if bytes.len() >= 2 && bytes[bytes.len() - 1] == b':' {
        let letter = bytes[bytes.len() - 2];
        let is_drive = bytes.len() == 2
            || (bytes.len() >= 4 && &bytes[bytes.len() - 4..bytes.len() - 2] == b"//");
        if is_drive && letter.is_ascii_lowercase() {
            prefix.replace_range(
                prefix.len() - 2..prefix.len() - 1,
                &(letter as char).to_ascii_uppercase().to_string(),
            );
        }
    }
}

fn is_absolute_path(path: &str) -> bool {
    let path = path.replace('\\', "/");
    path.starts_with('/')
        || (path.len() >= 3
            && path.as_bytes()[0].is_ascii_alphabetic()
            && path.as_bytes()[1] == b':'
            && path.as_bytes()[2] == b'/')
}

fn is_drive_root(path: &str) -> bool {
    path.len() == 3
        && path.as_bytes()[0].is_ascii_alphabetic()
        && path.as_bytes()[1] == b':'
        && path.as_bytes()[2] == b'/'
}

fn dirname(path: &str) -> &str {
    let trimmed = if path == "/" || is_drive_root(path) {
        path
    } else {
        path.trim_end_matches('/')
    };
    let Some(index) = trimmed.rfind('/') else {
        return ".";
    };
    if index == 0 {
        "/"
    } else if index == 2 && trimmed.as_bytes().get(1) == Some(&b':') {
        &trimmed[..=index]
    } else {
        &trimmed[..index]
    }
}

fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
}

fn trim_trailing_separators(path: &Path) -> PathBuf {
    path.components().collect()
}

#[cfg(unix)]
fn remove_symlink(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

#[cfg(windows)]
fn remove_symlink(path: &Path) -> io::Result<()> {
    if fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_filesystem_normalizes_paths_data_provider() {
        let cases = [
            ("../foo", "../foo"),
            ("C:/foo/bar", "c:/foo//bar"),
            ("C:/foo/bar", "C:/foo/./bar"),
            ("C:/foo/bar", "C://foo//bar"),
            ("C:/foo/bar", "C:///foo//bar"),
            ("C:/bar", "C:/foo/../bar"),
            ("/bar", "/foo/../bar/"),
            ("phar://C:/Foo", "phar://c:/Foo/Bar/.."),
            ("phar://C:/Foo", "phar://c:///Foo/Bar/.."),
            ("phar://C:/", "phar://c:/Foo/Bar/../../../.."),
            ("/", "/Foo/Bar/../../../.."),
            ("/", "/"),
            ("/", "//"),
            ("/", "///"),
            ("/Foo", "///Foo"),
            ("C:/", "c:\\"),
            ("../src", "Foo/Bar/../../../src"),
            ("C:../b", "c:.\\..\\a\\..\\b"),
            ("phar://C:../Foo", "phar://c:../Foo"),
            ("//foo/bar", "\\\\foo\\bar"),
        ];

        for (expected, input) in cases {
            assert_eq!(normalize_path(input), expected, "input: {input}");
        }
    }

    #[test]
    fn composer_filesystem_finds_shortest_paths_data_provider() {
        let cases = [
            ("/foo/bar", "/foo/bar", false, false, "./bar"),
            ("/foo/bar", "/foo/baz", false, false, "./baz"),
            ("/foo/bar/", "/foo/baz", false, false, "./baz"),
            ("/foo/bar", "/foo/bar", true, false, "./"),
            ("/foo/bar", "/foo/baz", true, false, "../baz"),
            ("/foo/bar/", "/foo/baz", true, false, "../baz"),
            ("C:/foo/bar/", "c:/foo/baz", true, false, "../baz"),
            (
                "/foo/bin/run",
                "/foo/vendor/acme/bin/run",
                false,
                false,
                "../vendor/acme/bin/run",
            ),
            ("/foo/bin/run", "/bar/bin/run", false, false, "/bar/bin/run"),
            ("/foo/bin/run", "/bar/bin/run", true, false, "/bar/bin/run"),
            (
                "c:/foo/bin/run",
                "d:/bar/bin/run",
                true,
                false,
                "D:/bar/bin/run",
            ),
            (
                "c:/bin/run",
                "c:/vendor/acme/bin/run",
                false,
                false,
                "../vendor/acme/bin/run",
            ),
            (
                "c:\\bin\\run",
                "c:/vendor/acme/bin/run",
                false,
                false,
                "../vendor/acme/bin/run",
            ),
            (
                "c:/bin/run",
                "d:/vendor/acme/bin/run",
                false,
                false,
                "D:/vendor/acme/bin/run",
            ),
            (
                "c:\\bin\\run",
                "d:/vendor/acme/bin/run",
                false,
                false,
                "D:/vendor/acme/bin/run",
            ),
            ("C:/Temp/test", "C:\\Temp", false, false, "./"),
            ("/tmp/test", "/tmp", false, false, "./"),
            ("C:/Temp/test/sub", "C:\\Temp", false, false, "../"),
            ("/tmp/test/sub", "/tmp", false, false, "../"),
            ("/tmp/test/sub", "/tmp", true, false, "../../"),
            ("c:/tmp/test/sub", "c:/tmp", true, false, "../../"),
            ("/tmp", "/tmp/test", false, false, "test"),
            ("C:/Temp", "c:\\Temp\\test", false, false, "test"),
            ("/tmp/test/./", "/tmp/test", true, false, "./"),
            ("/tmp/test/../vendor", "/tmp/test", true, false, "../test"),
            ("/tmp/test/.././vendor", "/tmp/test", true, false, "../test"),
            ("C:/Temp", "c:\\Temp\\..\\..\\test", true, false, "../test"),
            (
                "C:/Temp/../..",
                "c:\\Temp\\..\\..\\test",
                true,
                false,
                "./test",
            ),
            (
                "C:/Temp/../..",
                "D:\\Temp\\..\\..\\test",
                true,
                false,
                "D:/test",
            ),
            ("/app/vendor/foo/bar", "/lib", true, true, "../../../../lib"),
            ("/tmp", "/tmp/../../test", true, false, "../test"),
            ("/tmp", "/test", true, false, "../test"),
            ("/foo/bar", "/foo/bar_vendor", true, false, "../bar_vendor"),
            ("/foo/bar_vendor", "/foo/bar", true, false, "../bar"),
            ("/foo/bar_vendor", "/foo/bar/src", true, false, "../bar/src"),
            (
                "/foo/bar_vendor/src2",
                "/foo/bar/src/lib",
                true,
                false,
                "../../bar/src/lib",
            ),
            ("C:/", "C:/foo/bar/", true, false, "foo/bar"),
        ];

        for (from, to, directories, prefer_relative, expected) in cases {
            assert_eq!(
                shortest_path(from, to, directories, prefer_relative).unwrap(),
                expected,
                "from: {from}, to: {to}"
            );
        }
    }

    #[test]
    fn composer_filesystem_removes_nested_directory() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("nested");
        fs::create_dir_all(directory.join("level1/level2")).unwrap();
        fs::write(directory.join("level1/level2/hello.txt"), "hello world").unwrap();

        remove_path(&directory).unwrap();

        assert!(!directory.exists());
    }

    #[test]
    fn composer_filesystem_reports_file_size() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        fs::write(&file, "Hello").unwrap();

        assert_eq!(path_size(&file).unwrap(), 5);
    }

    #[test]
    fn composer_filesystem_reports_recursive_directory_size() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("file1.txt"), "Hello").unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        fs::write(temp.path().join("nested/file2.txt"), "World").unwrap();

        assert_eq!(path_size(temp.path()).unwrap(), 10);
    }

    #[test]
    fn composer_filesystem_copies_files_and_directories() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("foo");
        fs::create_dir_all(source.join("bar")).unwrap();
        fs::create_dir_all(source.join("baz")).unwrap();
        fs::write(source.join("foo.file"), "foo").unwrap();
        fs::write(source.join("bar/foobar.file"), "foobar").unwrap();
        fs::write(source.join("baz/foobaz.file"), "foobaz").unwrap();

        let copied = temp.path().join("foop");
        copy_path(&source, &copied).unwrap();
        assert_eq!(fs::read_to_string(copied.join("foo.file")).unwrap(), "foo");
        assert_eq!(
            fs::read_to_string(copied.join("bar/foobar.file")).unwrap(),
            "foobar"
        );
        assert_eq!(
            fs::read_to_string(copied.join("baz/foobaz.file")).unwrap(),
            "foobaz"
        );

        let file = temp.path().join("source.file");
        fs::write(&file, "testfile").unwrap();
        let copied_file = temp.path().join("copied.file");
        copy_path(&file, &copied_file).unwrap();
        assert_eq!(fs::read_to_string(copied_file).unwrap(), "testfile");
    }

    #[test]
    fn composer_filesystem_copies_then_removes_source() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("foo");
        fs::create_dir_all(source.join("bar")).unwrap();
        fs::write(source.join("bar/file"), "content").unwrap();
        let target = temp.path().join("foop");

        copy_then_remove(&source, &target).unwrap();

        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(target.join("bar/file")).unwrap(),
            "content"
        );

        let file = temp.path().join("source.file");
        fs::write(&file, "testfile").unwrap();
        let copied_file = temp.path().join("copied.file");
        copy_then_remove(&file, &copied_file).unwrap();
        assert!(!file.exists());
        assert_eq!(fs::read_to_string(copied_file).unwrap(), "testfile");
    }

    #[cfg(unix)]
    #[test]
    fn composer_filesystem_unlinks_symlinked_directory() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("FILE"), "content").unwrap();
        let linked = temp.path().join("linked");
        symlink(&real, &linked).unwrap();

        remove_path(&linked).unwrap();

        assert!(!linked.exists());
        assert!(real.join("FILE").exists());
    }

    #[cfg(unix)]
    #[test]
    fn composer_filesystem_removes_symlinked_directory_with_trailing_slash() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("FILE"), "content").unwrap();
        let linked = temp.path().join("linked");
        symlink(&real, &linked).unwrap();
        let linked_with_slash = PathBuf::from(format!("{}/", linked.display()));

        remove_path(&linked_with_slash).unwrap();

        assert!(!linked.exists());
        assert!(real.join("FILE").exists());
    }
}
