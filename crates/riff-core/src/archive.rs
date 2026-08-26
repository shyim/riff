//! Native archive inspection shared by artifact repositories and archive utilities.

use flate2::read::GzDecoder;
use globset::{GlobBuilder, GlobMatcher};
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tar::Archive as TarArchive;
use tempfile::NamedTempFile;
use walkdir::WalkDir;
use zip::ZipArchive;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerArchiveFormat {
    Zip,
    Tar,
    TarGz,
}

#[derive(Debug)]
pub enum ComposerArchiveError {
    ComposerJsonNotFound,
    MultipleTopLevelPaths { paths: Vec<String> },
    Read(std::io::Error),
}

impl fmt::Display for ComposerArchiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ComposerJsonNotFound => formatter.write_str(
                "No composer.json found either at the top level or within the topmost directory",
            ),
            Self::MultipleTopLevelPaths { paths } => write!(
                formatter,
                "Archive has more than one top level directories, and no composer.json was found on the top level, so it's an invalid archive. Top level paths found were: {}",
                paths.join(",")
            ),
            Self::Read(error) => write!(formatter, "Failed to read composer.json: {error}"),
        }
    }
}

impl std::error::Error for ComposerArchiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ComposerArchiveError {
    fn from(error: std::io::Error) -> Self {
        Self::Read(error)
    }
}

/// Formats Riff can create for the Composer `archive` workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageArchiveFormat {
    Zip,
    Tar,
}

impl PackageArchiveFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Tar => "tar",
        }
    }
}

impl FromStr for PackageArchiveFormat {
    type Err = PackageArchiveError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "zip" => Ok(Self::Zip),
            "tar" => Ok(Self::Tar),
            value => Err(PackageArchiveError::UnsupportedFormat(value.to_owned())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PackageArchiveError {
    #[error("No archiver found to support {0} format")]
    UnsupportedFormat(String),
    #[error("Could not archive {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Could not create zip archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("Invalid archive exclude pattern {pattern:?}: {message}")]
    InvalidExcludePattern { pattern: String, message: String },
}

/// Ordered filename components used by Composer package archives.
pub fn package_archive_filename_parts(
    package: &crate::package::Package,
) -> Vec<(&'static str, String)> {
    let base = package
        .archive
        .as_ref()
        .and_then(|archive| archive.name.clone())
        .unwrap_or_else(|| {
            package
                .name
                .chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                        character
                    } else {
                        '-'
                    }
                })
                .collect()
        });
    let mut parts = vec![("base", base)];

    let dist_reference = package
        .dist
        .as_ref()
        .and_then(|dist| dist.reference.as_deref());
    let is_commit = dist_reference.is_some_and(|reference| {
        reference.len() == 40 && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    if is_commit {
        parts.push(("dist_reference", dist_reference.unwrap().replace('/', "-")));
        if let Some(dist_type) = package.dist.as_ref().map(|dist| dist.dist_type.to_string()) {
            parts.push(("dist_type", dist_type.replace('/', "-")));
        }
    } else {
        parts.push(("version", package.pretty_version().replace('/', "-")));
        if let Some(reference) = dist_reference {
            parts.push(("dist_reference", reference.replace('/', "-")));
        }
    }

    if let Some(reference) = package
        .source
        .as_ref()
        .map(|source| source.reference.as_str())
    {
        let digest = format!("{:x}", Sha1::digest(reference.as_bytes()));
        parts.push(("source_reference", digest[..6].to_owned()));
    }

    parts
}

pub fn package_archive_filename(package: &crate::package::Package) -> String {
    package_archive_filename_parts(package)
        .into_iter()
        .map(|(_, value)| value)
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Debug)]
struct ArchiveExcludeRule {
    negated: bool,
    matches_any_segment: bool,
    allow_leading_slash: bool,
    matcher: GlobMatcher,
}

impl ArchiveExcludeRule {
    fn parse(pattern: &str) -> Result<Self, PackageArchiveError> {
        let (negated, pattern) = pattern
            .strip_prefix('!')
            .map_or((false, pattern), |pattern| (true, pattern));
        let anchored = pattern.starts_with('/');
        let pattern = pattern.trim_matches('/');
        let matches_any_segment = !anchored && !pattern.contains('/');
        let allow_leading_slash = !anchored && pattern.contains('/');
        let matcher = GlobBuilder::new(pattern)
            .literal_separator(true)
            .backslash_escape(true)
            .build()
            .map_err(|error| PackageArchiveError::InvalidExcludePattern {
                pattern: pattern.to_owned(),
                message: error.to_string(),
            })?
            .compile_matcher();
        Ok(Self {
            negated,
            matches_any_segment,
            allow_leading_slash,
            matcher,
        })
    }

    fn matches(&self, path: &str) -> bool {
        let segments: Vec<_> = path.split('/').collect();
        if self.matches_any_segment {
            return segments
                .iter()
                .any(|segment| self.matcher.is_match(Path::new(segment)));
        }
        (1..=segments.len()).any(|end| {
            let prefix = segments[..end].join("/");
            self.matcher.is_match(&prefix)
                || (self.allow_leading_slash && self.matcher.is_match(format!("/{prefix}")))
        })
    }
}

fn git_attribute_excludes(source: &Path) -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(source.join(".gitattributes")) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(parse_git_attribute_exclude)
        .collect()
}

/// Parse a single `.gitattributes` export-ignore rule.
pub fn parse_git_attribute_exclude(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields = line.split_whitespace();
    let pattern = fields.next()?;
    let attribute = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    match attribute {
        "export-ignore" => Some(pattern.to_owned()),
        "-export-ignore" => Some(format!("!{pattern}")),
        _ => None,
    }
}

/// Find source entries eligible for a package archive.
pub fn archivable_files(
    source: &Path,
    excludes: &[String],
    ignore_filters: bool,
) -> Result<Vec<PathBuf>, PackageArchiveError> {
    let source = source
        .canonicalize()
        .map_err(|source_error| PackageArchiveError::Io {
            path: source.to_owned(),
            source: source_error,
        })?;
    let patterns = if ignore_filters {
        Vec::new()
    } else {
        git_attribute_excludes(&source)
            .into_iter()
            .chain(excludes.iter().cloned())
            .map(|pattern| ArchiveExcludeRule::parse(&pattern))
            .collect::<Result<Vec<_>, _>>()?
    };

    let mut files = Vec::new();
    for entry in WalkDir::new(&source).follow_links(false).into_iter() {
        let entry = entry.map_err(|error| PackageArchiveError::Io {
            path: error.path().unwrap_or(&source).to_owned(),
            source: error.into_io_error().unwrap_or_else(|| {
                std::io::Error::other("failed to traverse package archive source")
            }),
        })?;
        if entry.path() == source {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(&source)
            .expect("walked source entry");
        let relative_string = relative.to_string_lossy().replace('\\', "/");
        if relative_string == ".git" || relative_string.starts_with(".git/") {
            continue;
        }
        if entry.file_type().is_symlink() {
            let Ok(target) = entry.path().canonicalize() else {
                continue;
            };
            if !target.starts_with(&source) {
                continue;
            }
        }
        let mut excluded = false;
        for pattern in &patterns {
            if pattern.matches(&relative_string) {
                excluded = !pattern.negated;
            }
        }
        if !excluded && entry.file_type().is_file() {
            files.push(relative.to_owned());
        }
    }
    files.sort_by(|left, right| {
        left.to_string_lossy()
            .replace('\\', "/")
            .cmp(&right.to_string_lossy().replace('\\', "/"))
    });
    Ok(files)
}

/// Create a local package archive atomically in `target_dir`.
pub fn create_package_archive(
    package: &crate::package::Package,
    source: &Path,
    target_dir: &Path,
    format: &str,
    file_name: Option<&str>,
    ignore_filters: bool,
) -> Result<PathBuf, PackageArchiveError> {
    let format = PackageArchiveFormat::from_str(format)?;
    std::fs::create_dir_all(target_dir).map_err(|source| PackageArchiveError::Io {
        path: target_dir.to_owned(),
        source,
    })?;
    let name = file_name
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| package_archive_filename(package));
    let target = target_dir.join(format!("{name}.{}", format.extension()));
    let excludes = package
        .archive
        .as_ref()
        .map(|archive| archive.exclude.as_slice())
        .unwrap_or_default();
    let files = archivable_files(source, excludes, ignore_filters)?;
    let temporary =
        NamedTempFile::new_in(target_dir).map_err(|source| PackageArchiveError::Io {
            path: target_dir.to_owned(),
            source,
        })?;
    match format {
        PackageArchiveFormat::Zip => {
            let writer = temporary
                .reopen()
                .map_err(|source_error| PackageArchiveError::Io {
                    path: temporary.path().to_owned(),
                    source: source_error,
                })?;
            let mut archive = zip::ZipWriter::new(writer);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for relative in files {
                let archive_name = if cfg!(windows) {
                    relative.to_string_lossy().replace('\\', "/")
                } else {
                    relative.to_string_lossy().into_owned()
                };
                archive.start_file(archive_name, options)?;
                let mut input = File::open(source.join(&relative)).map_err(|source_error| {
                    PackageArchiveError::Io {
                        path: source.join(&relative),
                        source: source_error,
                    }
                })?;
                std::io::copy(&mut input, &mut archive).map_err(|source_error| {
                    PackageArchiveError::Io {
                        path: source.join(&relative),
                        source: source_error,
                    }
                })?;
            }
            archive.finish()?;
        }
        PackageArchiveFormat::Tar => {
            let writer = temporary
                .reopen()
                .map_err(|source_error| PackageArchiveError::Io {
                    path: temporary.path().to_owned(),
                    source: source_error,
                })?;
            let mut archive = tar::Builder::new(writer);
            for relative in files {
                archive
                    .append_path_with_name(source.join(&relative), &relative)
                    .map_err(|source_error| PackageArchiveError::Io {
                        path: source.join(&relative),
                        source: source_error,
                    })?;
            }
            archive
                .finish()
                .map_err(|source_error| PackageArchiveError::Io {
                    path: temporary.path().to_owned(),
                    source: source_error,
                })?;
        }
    }
    temporary
        .persist(&target)
        .map_err(|error| PackageArchiveError::Io {
            path: target.clone(),
            source: error.error,
        })?;
    Ok(target)
}

/// Read the root package manifest from a supported archive.
///
/// Missing, malformed, and truly empty archives return `Ok(None)`. A non-empty
/// archive is accepted only when `composer.json` is at its root or immediately
/// inside its sole top-level directory.
pub fn read_composer_json_from_archive(
    path: &Path,
    format: ComposerArchiveFormat,
) -> Result<Option<String>, ComposerArchiveError> {
    match format {
        ComposerArchiveFormat::Zip => read_zip_composer_json(path),
        ComposerArchiveFormat::Tar => read_tar_composer_json(path, false),
        ComposerArchiveFormat::TarGz => read_tar_composer_json(path, true),
    }
}

#[derive(Debug)]
struct ArchiveMember {
    path: String,
    is_file: bool,
}

fn locate_composer_json(members: &[ArchiveMember]) -> Result<Option<String>, ComposerArchiveError> {
    if members.is_empty() {
        return Ok(None);
    }

    if members
        .iter()
        .any(|member| member.is_file && member.path == "composer.json")
    {
        return Ok(Some("composer.json".to_owned()));
    }

    let top_level_paths: BTreeSet<String> = members
        .iter()
        .filter(|member| !member.path.contains("__MACOSX"))
        .filter_map(top_level_path)
        .collect();

    if top_level_paths.len() > 1 {
        return Err(ComposerArchiveError::MultipleTopLevelPaths {
            paths: top_level_paths.into_iter().collect(),
        });
    }

    if let Some(top_level) = top_level_paths.into_iter().next() {
        let candidate = format!("{}/composer.json", top_level.trim_end_matches('/'));
        if members
            .iter()
            .any(|member| member.is_file && member.path == candidate)
        {
            return Ok(Some(candidate));
        }
    }

    Err(ComposerArchiveError::ComposerJsonNotFound)
}

fn top_level_path(member: &ArchiveMember) -> Option<String> {
    let path = member.path.trim_matches('/');
    if path.is_empty() {
        return None;
    }
    let first = path.split('/').next()?;
    if member.path.contains('/') || !member.is_file {
        Some(format!("{first}/"))
    } else {
        Some(first.to_owned())
    }
}

fn normalize_member_path(path: &str) -> String {
    let mut normalized = path.replace('\\', "/");
    while let Some(path) = normalized.strip_prefix("./") {
        normalized = path.to_owned();
    }
    normalized
}

fn read_zip_composer_json(path: &Path) -> Result<Option<String>, ComposerArchiveError> {
    let Ok(file) = File::open(path) else {
        return Ok(None);
    };
    let Ok(mut archive) = ZipArchive::new(BufReader::new(file)) else {
        return Ok(None);
    };

    let mut members = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let Ok(member) = archive.by_index(index) else {
            return Ok(None);
        };
        members.push(ArchiveMember {
            path: normalize_member_path(member.name()),
            is_file: member.is_file(),
        });
    }

    let Some(manifest_path) = locate_composer_json(&members)? else {
        return Ok(None);
    };
    let mut manifest = archive
        .by_name(&manifest_path)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut contents = String::new();
    manifest.read_to_string(&mut contents)?;
    Ok(Some(contents))
}

fn read_tar_composer_json(
    path: &Path,
    gzipped: bool,
) -> Result<Option<String>, ComposerArchiveError> {
    let Ok(file) = File::open(path) else {
        return Ok(None);
    };
    if gzipped {
        read_tar_composer_json_from_reader(GzDecoder::new(BufReader::new(file)))
    } else {
        read_tar_composer_json_from_reader(BufReader::new(file))
    }
}

fn read_tar_composer_json_from_reader<R: Read>(
    reader: R,
) -> Result<Option<String>, ComposerArchiveError> {
    let mut archive = TarArchive::new(reader);
    let Ok(entries) = archive.entries() else {
        return Ok(None);
    };
    let mut members = Vec::new();
    let mut manifests = BTreeMap::new();

    for entry in entries {
        let Ok(mut entry) = entry else {
            return Ok(None);
        };
        let Ok(path) = entry.path() else {
            return Ok(None);
        };
        let path = normalize_member_path(&path.to_string_lossy());
        let is_file = entry.header().entry_type().is_file();
        if is_file && path.rsplit('/').next() == Some("composer.json") {
            let mut contents = String::new();
            entry.read_to_string(&mut contents)?;
            manifests.insert(path.clone(), contents);
        }
        members.push(ArchiveMember { path, is_file });
    }

    let Some(manifest_path) = locate_composer_json(&members)? else {
        return Ok(None);
    };
    Ok(manifests.remove(&manifest_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Package, Source};
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    use tar::{Builder as TarBuilder, Header};
    use tempfile::TempDir;
    use zip::write::SimpleFileOptions;

    const MANIFEST: &str = "{\n    \"name\": \"foo/bar\"\n}\n";

    enum FixtureEntry<'a> {
        Directory(&'a str),
        File(&'a str, &'a str),
    }

    fn package_archive_fixture() -> Package {
        let mut package = Package::new("archivertest/archivertest", "master");
        package.pretty_version = Some("master".into());
        package.source = Some(Source::new("git", ".", "master"));
        package
    }

    fn source_fixture(temp: &TempDir, entries: &[(&str, &str)]) {
        for (relative, contents) in entries {
            let path = temp.path().join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, contents).unwrap();
        }
    }

    fn archive_relative_files(
        source: &Path,
        excludes: &[&str],
        ignore_filters: bool,
    ) -> Vec<String> {
        archivable_files(
            source,
            &excludes
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            ignore_filters,
        )
        .unwrap()
        .into_iter()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect()
    }

    fn zip_contents(path: &Path) -> BTreeMap<String, String> {
        let mut archive = ZipArchive::new(File::open(path).unwrap()).unwrap();
        let mut contents = BTreeMap::new();
        for index in 0..archive.len() {
            let mut file = archive.by_index(index).unwrap();
            let name = file.name().to_owned();
            let mut value = String::new();
            file.read_to_string(&mut value).unwrap();
            contents.insert(name, value);
        }
        contents
    }

    #[test]
    fn composer_archive_manager_rejects_unknown_formats_before_writing() {
        let source = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        let error = create_package_archive(
            &package_archive_fixture(),
            source.path(),
            target.path(),
            "__unknown_format__",
            None,
            false,
        )
        .unwrap_err();

        assert!(matches!(error, PackageArchiveError::UnsupportedFormat(_)));
        assert!(target.path().read_dir().unwrap().next().is_none());
    }

    #[test]
    fn composer_archive_manager_creates_tar_archives_without_temporary_artifacts() {
        let source = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        source_fixture(
            &source,
            &[("composer.json", "{}"), ("src/lib.php", "<?php")],
        );

        let archive = create_package_archive(
            &package_archive_fixture(),
            source.path(),
            target.path(),
            "tar",
            None,
            false,
        )
        .unwrap();

        assert_eq!(
            archive.file_name().unwrap(),
            "archivertest-archivertest-master-4f26ae.tar"
        );
        let mut tar = TarArchive::new(File::open(archive).unwrap());
        let names = tar
            .entries()
            .unwrap()
            .map(|entry| {
                entry
                    .unwrap()
                    .path()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(names, ["composer.json", "src/lib.php"]);
        assert_eq!(target.path().read_dir().unwrap().count(), 1);
    }

    #[test]
    fn composer_archive_manager_honors_custom_file_names() {
        let source = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        source_fixture(&source, &[("composer.json", "{}")]);

        let archive = create_package_archive(
            &package_archive_fixture(),
            source.path(),
            target.path(),
            "tar",
            Some("testArchiveName"),
            false,
        )
        .unwrap();

        assert_eq!(archive, target.path().join("testArchiveName.tar"));
    }

    #[test]
    fn composer_archive_manager_builds_filename_parts() {
        assert_eq!(
            package_archive_filename_parts(&package_archive_fixture()),
            [
                ("base", "archivertest-archivertest".to_owned()),
                ("version", "master".to_owned()),
                ("source_reference", "4f26ae".to_owned()),
            ]
        );
    }

    #[test]
    fn composer_archive_manager_builds_package_filename() {
        assert_eq!(
            package_archive_filename(&package_archive_fixture()),
            "archivertest-archivertest-master-4f26ae"
        );
    }

    #[test]
    fn composer_zip_archiver_preserves_simple_files_and_paths() {
        let source = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        source_fixture(
            &source,
            &[
                ("file.txt", "content"),
                ("foo/bar/baz", "nested"),
                ("x/includeme", "included"),
            ],
        );

        let path = create_package_archive(
            &package_archive_fixture(),
            source.path(),
            target.path(),
            "zip",
            Some("simple"),
            false,
        )
        .unwrap();

        assert_eq!(
            zip_contents(&path),
            BTreeMap::from([
                ("file.txt".to_owned(), "content".to_owned()),
                ("foo/bar/baz".to_owned(), "nested".to_owned()),
                ("x/includeme".to_owned(), "included".to_owned()),
            ])
        );
    }

    #[test]
    fn composer_zip_archiver_does_not_treat_gitignore_as_export_ignore() {
        for include in ["!/docs", "!/docs/"] {
            let source = TempDir::new().unwrap();
            let target = TempDir::new().unwrap();
            source_fixture(
                &source,
                &[
                    (".gitignore", &format!("/*\n.*\n!.git*\n{include}")),
                    ("docs/README.md", "# The doc"),
                ],
            );

            let path = create_package_archive(
                &package_archive_fixture(),
                source.path(),
                target.path(),
                "zip",
                Some("gitignore"),
                false,
            )
            .unwrap();
            assert_eq!(
                zip_contents(&path).keys().cloned().collect::<Vec<_>>(),
                [".gitignore", "docs/README.md"]
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn composer_zip_archiver_preserves_backslashes_in_unix_filenames() {
        let source = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        source_fixture(&source, &[("folder\\with\\backslashes/README.md", "# doc")]);

        let path = create_package_archive(
            &package_archive_fixture(),
            source.path(),
            target.path(),
            "zip",
            Some("backslash"),
            false,
        )
        .unwrap();

        assert_eq!(
            zip_contents(&path),
            BTreeMap::from([(
                "folder\\with\\backslashes/README.md".to_owned(),
                "# doc".to_owned()
            )])
        );
    }

    #[test]
    fn composer_archivable_files_apply_ordered_manual_excludes_and_negations() {
        let source = TempDir::new().unwrap();
        source_fixture(
            &source,
            &[
                (".foo", ""),
                ("prefixA.foo", ""),
                ("prefixB.foo", ""),
                ("prefixC.foo", ""),
                ("A/prefixA.foo", ""),
                ("A/prefixB.foo", ""),
                ("A/prefixC.foo", ""),
                ("A/prefixD.foo", ""),
                ("B/sub/prefixC.foo", ""),
            ],
        );

        assert_eq!(
            archive_relative_files(
                source.path(),
                &[
                    "prefixB.foo",
                    "!/prefixB.foo",
                    "/prefixA.foo",
                    "prefixC.*",
                    "!*/*/*/prefixC.foo",
                    ".*",
                ],
                false,
            ),
            [
                "A/prefixA.foo",
                "A/prefixD.foo",
                "B/sub/prefixC.foo",
                "prefixB.foo",
            ]
        );
    }

    #[test]
    fn composer_archivable_files_apply_gitattributes_export_ignore_rules() {
        let source = TempDir::new().unwrap();
        source_fixture(
            &source,
            &[
                (
                    ".gitattributes",
                    "prefixB.foo export-ignore\n/prefixA.foo export-ignore\nprefixE.foo export-ignore\n/prefixE.foo -export-ignore\n\\!important.txt export-ignore\n",
                ),
                ("prefixA.foo", ""),
                ("A/prefixA.foo", ""),
                ("prefixB.foo", ""),
                ("A/prefixB.foo", ""),
                ("prefixE.foo", ""),
                ("A/prefixE.foo", ""),
                ("!important.txt", ""),
                ("keep.txt", ""),
            ],
        );

        assert_eq!(
            archive_relative_files(source.path(), &[], false),
            [".gitattributes", "A/prefixA.foo", "keep.txt", "prefixE.foo",]
        );
    }

    #[test]
    fn composer_archivable_files_can_skip_all_exclude_filters() {
        let source = TempDir::new().unwrap();
        source_fixture(
            &source,
            &[
                (".gitattributes", "keep.txt export-ignore\n"),
                ("keep.txt", ""),
                ("prefixB.foo", ""),
            ],
        );

        assert_eq!(
            archive_relative_files(source.path(), &["prefixB.foo"], true),
            [".gitattributes", "keep.txt", "prefixB.foo"]
        );
    }

    #[test]
    fn composer_git_exclude_filter_parses_export_ignore_polarity() {
        assert_eq!(
            parse_git_attribute_exclude("app/config/parameters.yml export-ignore"),
            Some("app/config/parameters.yml".to_owned())
        );
        assert_eq!(
            parse_git_attribute_exclude("app/config/parameters.yml -export-ignore"),
            Some("!app/config/parameters.yml".to_owned())
        );
        assert_eq!(parse_git_attribute_exclude("# comment"), None);
        assert_eq!(parse_git_attribute_exclude("README.md text"), None);
    }

    fn zip_fixture(temp: &TempDir, name: &str, entries: &[FixtureEntry<'_>]) -> std::path::PathBuf {
        let path = temp.path().join(name);
        let mut archive = zip::ZipWriter::new(File::create(&path).unwrap());
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for entry in entries {
            match entry {
                FixtureEntry::Directory(path) => archive.add_directory(*path, options).unwrap(),
                FixtureEntry::File(path, contents) => {
                    archive.start_file(*path, options).unwrap();
                    archive.write_all(contents.as_bytes()).unwrap();
                }
            }
        }
        archive.finish().unwrap();
        path
    }

    fn append_tar_entries<W: Write>(archive: &mut TarBuilder<W>, entries: &[FixtureEntry<'_>]) {
        for entry in entries {
            match entry {
                FixtureEntry::Directory(path) => {
                    let mut header = Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_size(0);
                    header.set_mode(0o755);
                    header.set_cksum();
                    archive.append_data(&mut header, *path, &[][..]).unwrap();
                }
                FixtureEntry::File(path, contents) => {
                    let mut header = Header::new_gnu();
                    header.set_size(contents.len() as u64);
                    header.set_mode(0o644);
                    header.set_cksum();
                    archive
                        .append_data(&mut header, *path, contents.as_bytes())
                        .unwrap();
                }
            }
        }
    }

    fn tar_fixture(
        temp: &TempDir,
        name: &str,
        entries: &[FixtureEntry<'_>],
        gzipped: bool,
    ) -> std::path::PathBuf {
        let path = temp.path().join(name);
        let file = File::create(&path).unwrap();
        if gzipped {
            let mut archive = TarBuilder::new(GzEncoder::new(file, Compression::default()));
            append_tar_entries(&mut archive, entries);
            archive.into_inner().unwrap().finish().unwrap();
        } else {
            let mut archive = TarBuilder::new(file);
            append_tar_entries(&mut archive, entries);
            archive.into_inner().unwrap();
        }
        path
    }

    fn assert_tar_variants(
        entries: &[FixtureEntry<'_>],
        assertion: impl Fn(Result<Option<String>, ComposerArchiveError>),
    ) {
        let temp = TempDir::new().unwrap();
        let raw = tar_fixture(&temp, "fixture.tar", entries, false);
        assertion(read_composer_json_from_archive(
            &raw,
            ComposerArchiveFormat::Tar,
        ));
        let gzip = tar_fixture(&temp, "fixture.tar.gz", entries, true);
        assertion(read_composer_json_from_archive(
            &gzip,
            ComposerArchiveFormat::TarGz,
        ));
    }

    #[test]
    fn composer_zip_returns_none_for_missing_archive() {
        let temp = TempDir::new().unwrap();
        assert!(read_composer_json_from_archive(
            &temp.path().join("missing.zip"),
            ComposerArchiveFormat::Zip
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn composer_zip_returns_none_for_empty_archive() {
        let temp = TempDir::new().unwrap();
        let path = zip_fixture(&temp, "empty.zip", &[]);
        assert!(
            read_composer_json_from_archive(&path, ComposerArchiveFormat::Zip)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn composer_zip_rejects_archive_without_composer_json() {
        let temp = TempDir::new().unwrap();
        let path = zip_fixture(
            &temp,
            "nojson.zip",
            &[FixtureEntry::File("README.md", "read me")],
        );
        let error = read_composer_json_from_archive(&path, ComposerArchiveFormat::Zip).unwrap_err();
        assert!(matches!(error, ComposerArchiveError::ComposerJsonNotFound));
        assert_eq!(
            error.to_string(),
            "No composer.json found either at the top level or within the topmost directory"
        );
    }

    #[test]
    fn composer_zip_rejects_composer_json_in_nested_subfolder() {
        let temp = TempDir::new().unwrap();
        let path = zip_fixture(
            &temp,
            "nested.zip",
            &[FixtureEntry::File("top/nested/composer.json", MANIFEST)],
        );
        assert!(matches!(
            read_composer_json_from_archive(&path, ComposerArchiveFormat::Zip),
            Err(ComposerArchiveError::ComposerJsonNotFound)
        ));
    }

    #[test]
    fn composer_zip_reads_composer_json_from_root() {
        let temp = TempDir::new().unwrap();
        let path = zip_fixture(
            &temp,
            "root.zip",
            &[FixtureEntry::File("composer.json", MANIFEST)],
        );
        assert_eq!(
            read_composer_json_from_archive(&path, ComposerArchiveFormat::Zip).unwrap(),
            Some(MANIFEST.into())
        );
    }

    #[test]
    fn composer_zip_reads_composer_json_from_first_folder() {
        let temp = TempDir::new().unwrap();
        let path = zip_fixture(
            &temp,
            "folder.zip",
            &[
                FixtureEntry::Directory("package/"),
                FixtureEntry::File("package/composer.json", MANIFEST),
            ],
        );
        assert_eq!(
            read_composer_json_from_archive(&path, ComposerArchiveFormat::Zip).unwrap(),
            Some(MANIFEST.into())
        );
    }

    #[test]
    fn composer_zip_rejects_multiple_top_level_directories() {
        let temp = TempDir::new().unwrap();
        let path = zip_fixture(
            &temp,
            "multiple.zip",
            &[
                FixtureEntry::File("folder1/composer.json", MANIFEST),
                FixtureEntry::File("folder2/composer.json", MANIFEST),
            ],
        );
        let error = read_composer_json_from_archive(&path, ComposerArchiveFormat::Zip).unwrap_err();
        assert!(matches!(
            error,
            ComposerArchiveError::MultipleTopLevelPaths { .. }
        ));
        assert!(error.to_string().ends_with("folder1/,folder2/"));
    }

    #[test]
    fn composer_zip_reads_composer_json_from_implicit_first_subfolder() {
        let temp = TempDir::new().unwrap();
        let path = zip_fixture(
            &temp,
            "implicit.zip",
            &[FixtureEntry::File("package/composer.json", MANIFEST)],
        );
        assert_eq!(
            read_composer_json_from_archive(&path, ComposerArchiveFormat::Zip).unwrap(),
            Some(MANIFEST.into())
        );
    }

    #[test]
    fn composer_tar_returns_none_for_missing_archive() {
        let temp = TempDir::new().unwrap();
        for format in [ComposerArchiveFormat::Tar, ComposerArchiveFormat::TarGz] {
            assert!(
                read_composer_json_from_archive(&temp.path().join("missing.tar"), format)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn composer_tar_returns_none_for_empty_archive() {
        assert_tar_variants(&[], |result| assert!(result.unwrap().is_none()));
    }

    #[test]
    fn composer_tar_rejects_archive_without_composer_json() {
        assert_tar_variants(&[FixtureEntry::File("README.md", "read me")], |result| {
            assert!(matches!(
                result,
                Err(ComposerArchiveError::ComposerJsonNotFound)
            ));
        });
    }

    #[test]
    fn composer_tar_rejects_composer_json_in_nested_subfolder() {
        assert_tar_variants(
            &[FixtureEntry::File("top/nested/composer.json", MANIFEST)],
            |result| {
                assert!(matches!(
                    result,
                    Err(ComposerArchiveError::ComposerJsonNotFound)
                ));
            },
        );
    }

    #[test]
    fn composer_tar_reads_composer_json_from_root() {
        assert_tar_variants(&[FixtureEntry::File("composer.json", MANIFEST)], |result| {
            assert_eq!(result.unwrap(), Some(MANIFEST.into()))
        });
    }

    #[test]
    fn composer_tar_reads_composer_json_from_first_folder() {
        assert_tar_variants(
            &[
                FixtureEntry::Directory("package/"),
                FixtureEntry::File("package/composer.json", MANIFEST),
            ],
            |result| assert_eq!(result.unwrap(), Some(MANIFEST.into())),
        );
    }

    #[test]
    fn composer_tar_rejects_multiple_top_level_directories() {
        assert_tar_variants(
            &[
                FixtureEntry::File("folder1/composer.json", MANIFEST),
                FixtureEntry::File("folder2/composer.json", MANIFEST),
            ],
            |result| {
                let error = result.unwrap_err();
                assert!(matches!(
                    error,
                    ComposerArchiveError::MultipleTopLevelPaths { .. }
                ));
            },
        );
    }
}
