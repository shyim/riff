//! Archive extraction (zip, tar, tar.gz, tar.bz2).

use flate2::read::GzDecoder;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek};
use std::path::{Path, PathBuf};

use crate::{Result, RiffError};

// ZIP readers seek repeatedly while extracting. Buffer small archives in memory
// to avoid that syscall overhead while keeping concurrent memory use bounded.
const MAX_IN_MEMORY_ZIP_SIZE: u64 = 4 * 1024 * 1024;

/// Rewrite generated GitHub and Bitbucket distribution URLs to download the
/// package's selected reference. Other URLs, and packages without a reference,
/// remain untouched.
pub fn process_dist_url(url: &str, reference: Option<&str>) -> String {
    process_dist_url_with_domains(url, reference, &["github.com"], &["gitlab.com"])
}

/// Rewrite generated distribution URLs, including configured GitHub/GitLab
/// enterprise origins, to the selected package reference.
pub fn process_dist_url_with_domains(
    url: &str,
    reference: Option<&str>,
    github_domains: &[&str],
    gitlab_domains: &[&str],
) -> String {
    let Some(reference) = reference else {
        return url.to_string();
    };
    let Ok(parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    let host = parsed.host_str().unwrap_or_default();
    let segments = parsed
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();

    if host.eq_ignore_ascii_case("api.github.com")
        && segments.len() >= 5
        && segments[0] == "repos"
        && matches!(segments[3], "zipball" | "tarball")
    {
        return format!(
            "https://api.github.com/repos/{}/{}/{}/{}",
            segments[1], segments[2], segments[3], reference
        );
    }

    if configured_domain_matches(host, github_domains)
        && segments.len() >= 6
        && segments[0] == "api"
        && segments[1] == "v3"
        && segments[2] == "repos"
        && matches!(segments[5], "zipball" | "tarball")
    {
        return format!(
            "{}://{}/api/v3/repos/{}/{}/{}/{}",
            parsed.scheme(),
            host,
            segments[3],
            segments[4],
            segments[5],
            reference
        );
    }

    if (host.eq_ignore_ascii_case("github.com") || host.eq_ignore_ascii_case("www.github.com"))
        && segments.len() >= 4
    {
        let archive_type = match segments[2] {
            "zipball" => Some("zipball"),
            "tarball" => Some("tarball"),
            "archive" if segments[3].ends_with(".zip") => Some("zipball"),
            "archive" if segments[3].ends_with(".tar.gz") => Some("tarball"),
            _ => None,
        };
        if let Some(archive_type) = archive_type {
            return format!(
                "https://api.github.com/repos/{}/{}/{}/{}",
                segments[0], segments[1], archive_type, reference
            );
        }
    }

    if (host.eq_ignore_ascii_case("bitbucket.org")
        || host.eq_ignore_ascii_case("www.bitbucket.org"))
        && segments.len() >= 4
        && segments[2] == "get"
    {
        let extension = ["tar.bz2", "tar.gz", "zip"]
            .into_iter()
            .find(|extension| segments[3].ends_with(&format!(".{extension}")));
        if let Some(extension) = extension {
            return format!(
                "https://bitbucket.org/{}/{}/get/{}.{}",
                segments[0], segments[1], reference, extension
            );
        }
    }

    let canonical_gitlab = host
        .strip_prefix("www.")
        .filter(|host| host.eq_ignore_ascii_case("gitlab.com"))
        .unwrap_or(host);
    if configured_domain_matches(canonical_gitlab, gitlab_domains)
        && segments.len() >= 6
        && segments[0] == "api"
        && matches!(segments[1], "v3" | "v4")
        && segments[2] == "projects"
        && segments[4] == "repository"
        && segments[5].starts_with("archive.")
    {
        let api_version = if canonical_gitlab.eq_ignore_ascii_case("gitlab.com") {
            "v4"
        } else {
            segments[1]
        };
        return format!(
            "{}://{}/api/{}/projects/{}/repository/{}?sha={}",
            parsed.scheme(),
            canonical_gitlab,
            api_version,
            segments[3],
            segments[5],
            reference
        );
    }

    url.to_string()
}

fn configured_domain_matches(host: &str, configured: &[&str]) -> bool {
    configured.iter().any(|domain| {
        let authority = domain.split('/').next().unwrap_or(domain);
        let configured_host = authority
            .rsplit_once(':')
            .filter(|(_, port)| port.bytes().all(|byte| byte.is_ascii_digit()))
            .map_or(authority, |(host, _)| host);
        host.eq_ignore_ascii_case(configured_host)
    })
}

/// Supported archive types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveType {
    Zip,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
}

impl ArchiveType {
    /// Detect archive type from file extension
    pub fn from_path(path: &Path) -> Option<Self> {
        let path_str = path.to_string_lossy().to_lowercase();

        if path_str.ends_with(".zip") {
            Some(ArchiveType::Zip)
        } else if path_str.ends_with(".tar.gz") || path_str.ends_with(".tgz") {
            Some(ArchiveType::TarGz)
        } else if path_str.ends_with(".tar.bz2") || path_str.ends_with(".tbz2") {
            Some(ArchiveType::TarBz2)
        } else if path_str.ends_with(".tar.xz") || path_str.ends_with(".txz") {
            Some(ArchiveType::TarXz)
        } else if path_str.ends_with(".tar") {
            Some(ArchiveType::Tar)
        } else {
            None
        }
    }

    /// Detect archive type from content type header
    pub fn from_content_type(content_type: &str) -> Option<Self> {
        let ct = content_type.to_lowercase();

        // Check more specific types first
        if ct.contains("gzip") || ct.contains("x-gzip") {
            Some(ArchiveType::TarGz)
        } else if ct.contains("bzip2") || ct.contains("x-bzip2") {
            Some(ArchiveType::TarBz2)
        } else if ct.contains("x-xz") {
            Some(ArchiveType::TarXz)
        } else if ct.contains("x-tar") {
            Some(ArchiveType::Tar)
        } else if ct.contains("zip") {
            Some(ArchiveType::Zip)
        } else {
            None
        }
    }
}

/// Archive extractor
pub struct ArchiveExtractor;

impl ArchiveExtractor {
    /// Extract an archive to the specified directory
    pub fn extract(archive_path: &Path, dest_dir: &Path) -> Result<()> {
        let archive_type = ArchiveType::from_path(archive_path).ok_or_else(|| {
            RiffError::InstallationFailed(format!(
                "Unknown archive type: {}",
                archive_path.display()
            ))
        })?;

        Self::extract_with_type(archive_path, dest_dir, archive_type)
    }

    /// Extract an archive with explicit type
    pub fn extract_with_type(
        archive_path: &Path,
        dest_dir: &Path,
        archive_type: ArchiveType,
    ) -> Result<()> {
        // Create destination directory
        std::fs::create_dir_all(dest_dir)?;

        match archive_type {
            ArchiveType::Zip => Self::extract_zip(archive_path, dest_dir),
            ArchiveType::Tar => Self::extract_tar(archive_path, dest_dir),
            ArchiveType::TarGz => Self::extract_tar_gz(archive_path, dest_dir),
            ArchiveType::TarBz2 => Self::extract_tar_bz2(archive_path, dest_dir),
            ArchiveType::TarXz => Self::extract_tar_xz(archive_path, dest_dir),
        }
    }

    /// Extract a zip archive
    fn extract_zip(archive_path: &Path, dest_dir: &Path) -> Result<()> {
        let result = if archive_path.metadata()?.len() <= MAX_IN_MEMORY_ZIP_SIZE {
            let bytes = std::fs::read(archive_path)?;
            Self::extract_zip_reader(Cursor::new(bytes), dest_dir)
        } else {
            let file = File::open(archive_path)?;
            Self::extract_zip_reader(BufReader::new(file), dest_dir)
        };
        result.map_err(|error| {
            RiffError::InstallationFailed(format!(
                "There was an error extracting the ZIP file: {error}"
            ))
        })
    }

    /// Try a primary ZIP extraction strategy and then a native fallback.
    pub fn extract_zip_with_fallback<P, F>(primary: P, fallback: F) -> Result<()>
    where
        P: FnOnce() -> Result<()>,
        F: FnOnce() -> Result<()>,
    {
        match primary() {
            Ok(()) => Ok(()),
            Err(primary_error) => fallback().map_err(|fallback_error| {
                RiffError::InstallationFailed(format!(
                    "ZIP extraction failed with both strategies; primary: {primary_error}; fallback: {fallback_error}"
                ))
            }),
        }
    }

    fn extract_zip_reader<R: Read + Seek>(reader: R, dest_dir: &Path) -> Result<()> {
        let mut archive = zip::ZipArchive::new(reader)
            .map_err(|e| RiffError::InstallationFailed(format!("Failed to open zip: {}", e)))?;

        Self::validate_zip_entry_names(&archive)?;

        // Find common prefix (GitHub archives have vendor-package-hash/ prefix)
        let common_prefix = Self::find_zip_common_prefix(&archive);
        let mut created_directories = HashSet::new();
        created_directories.insert(dest_dir.to_path_buf());

        for i in 0..archive.len() {
            let mut file = archive.by_index(i).map_err(|e| {
                RiffError::InstallationFailed(format!("Failed to read zip entry: {}", e))
            })?;

            let enclosed_path = file.enclosed_name().ok_or_else(|| {
                RiffError::InstallationFailed(format!(
                    "Path traversal detected in archive: {}",
                    file.name()
                ))
            })?;
            let relative_path = if let Some(ref prefix) = common_prefix {
                enclosed_path
                    .strip_prefix(Path::new(prefix))
                    .unwrap_or(&enclosed_path)
            } else {
                &enclosed_path
            };

            // Skip empty paths
            if relative_path.as_os_str().is_empty() {
                continue;
            }

            let outpath = dest_dir.join(relative_path);

            if file.is_dir() {
                Self::create_zip_directory(&outpath, &mut created_directories)?;
            } else if let Some(parent) = outpath.parent() {
                Self::create_zip_directory(parent, &mut created_directories)?;
            }

            if file.is_dir() {
                // Already created above.
            } else {
                let mut outfile = File::create(&outpath)?;
                std::io::copy(&mut file, &mut outfile)?;

                // Set permissions on Unix
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Some(mode) = file.unix_mode() {
                        std::fs::set_permissions(&outpath, std::fs::Permissions::from_mode(mode))?;
                    }
                }
            }
        }

        Ok(())
    }

    fn validate_zip_entry_names<R: Read + Seek>(archive: &zip::ZipArchive<R>) -> Result<()> {
        let mut names = std::collections::HashMap::<String, String>::new();
        for index in 0..archive.len() {
            let Some(name) = archive.name_for_index(index) else {
                continue;
            };
            let normalized = name.replace('\\', "/").trim_end_matches('/').to_lowercase();
            if let Some(previous) = names.get(&normalized) {
                if previous != name {
                    return Err(RiffError::InstallationFailed(format!(
                        "archive may contain identical file names with different capitalization: {previous} and {name}"
                    )));
                }
            } else {
                names.insert(normalized, name.to_owned());
            }
        }
        Ok(())
    }

    /// Create a ZIP entry's directory once per extraction.
    ///
    /// Archives commonly omit explicit directory entries, so every file still
    /// needs its parent prepared. Remembering directories avoids issuing a
    /// recursive mkdir sequence for every sibling file.
    fn create_zip_directory(path: &Path, created_directories: &mut HashSet<PathBuf>) -> Result<()> {
        if created_directories.insert(path.to_path_buf()) {
            std::fs::create_dir_all(path)?;
        }
        Ok(())
    }

    /// Find common prefix in zip archive (e.g., vendor-package-hash/)
    fn find_zip_common_prefix<R: Read + Seek>(archive: &zip::ZipArchive<R>) -> Option<String> {
        if archive.is_empty() {
            return None;
        }

        // Get first entry's name
        let first_name = archive.name_for_index(0)?;

        // Find the first directory component
        let slash_pos = first_name.find('/')?;
        let prefix = &first_name[..=slash_pos];

        // Check if all entries share this prefix
        for i in 0..archive.len() {
            if let Some(name) = archive.name_for_index(i) {
                if !name.starts_with(prefix) {
                    return None;
                }
            }
        }

        Some(prefix.to_string())
    }

    /// Extract a plain tar archive
    fn extract_tar(archive_path: &Path, dest_dir: &Path) -> Result<()> {
        let file = File::open(archive_path)?;
        let reader = BufReader::new(file);
        Self::extract_tar_reader(reader, dest_dir)
    }

    /// Extract a gzipped tar archive
    fn extract_tar_gz(archive_path: &Path, dest_dir: &Path) -> Result<()> {
        let file = File::open(archive_path)?;
        let reader = BufReader::new(file);
        let decoder = GzDecoder::new(reader);
        Self::extract_tar_reader(decoder, dest_dir)
    }

    /// Extract a bzip2 tar archive
    fn extract_tar_bz2(archive_path: &Path, dest_dir: &Path) -> Result<()> {
        use bzip2::read::BzDecoder;

        let file = File::open(archive_path)?;
        let reader = BufReader::new(file);
        let decoder = BzDecoder::new(reader);
        Self::extract_tar_reader(decoder, dest_dir)
    }

    /// Extract an xz tar archive
    fn extract_tar_xz(archive_path: &Path, dest_dir: &Path) -> Result<()> {
        use xz2::read::XzDecoder;

        let file = File::open(archive_path)?;
        let reader = BufReader::new(file);
        let decoder = XzDecoder::new(reader);
        Self::extract_tar_reader(decoder, dest_dir)
    }

    /// Extract from a tar reader (common implementation)
    /// Strips the first component (GitHub-style vendor-package-ref/ prefix)
    fn extract_tar_reader<R: Read>(reader: R, dest_dir: &Path) -> Result<()> {
        Self::extract_tar_with_strip(reader, dest_dir, 1)
    }

    /// Extract tar with prefix stripping
    pub fn extract_tar_with_strip<R: Read>(
        reader: R,
        dest_dir: &Path,
        strip_components: usize,
    ) -> Result<()> {
        let mut archive = tar::Archive::new(reader);

        // Canonicalize dest_dir for path traversal check
        let dest_dir_canonical = dest_dir.canonicalize().map_err(|e| {
            RiffError::InstallationFailed(format!("Failed to canonicalize destination: {}", e))
        })?;

        for entry in archive
            .entries()
            .map_err(|e| RiffError::InstallationFailed(format!("Failed to read tar: {}", e)))?
        {
            let mut entry = entry.map_err(|e| {
                RiffError::InstallationFailed(format!("Failed to read tar entry: {}", e))
            })?;

            let path = entry.path().map_err(|e| {
                RiffError::InstallationFailed(format!("Invalid path in tar: {}", e))
            })?;

            // Strip leading components
            let components: Vec<_> = path.components().collect();
            if components.len() <= strip_components {
                continue;
            }

            let stripped: std::path::PathBuf = components[strip_components..].iter().collect();
            if stripped.as_os_str().is_empty() {
                continue;
            }

            // Validate path doesn't contain traversal sequences
            let stripped_str = stripped.to_string_lossy();
            if stripped_str.contains("..") {
                return Err(RiffError::InstallationFailed(format!(
                    "Path traversal detected in archive: {}",
                    stripped_str
                )));
            }

            let outpath = dest_dir.join(&stripped);

            // Create parent directories first so we can verify the path
            if entry.header().entry_type().is_dir() {
                std::fs::create_dir_all(&outpath)?;
            } else if let Some(parent) = outpath.parent() {
                std::fs::create_dir_all(parent)?;
            }

            // Verify the path stays within destination directory
            let outpath_canonical = outpath.canonicalize().unwrap_or_else(|_| {
                // For new files, canonicalize the parent and append filename
                if let Some(parent) = outpath.parent() {
                    if let Ok(parent_canonical) = parent.canonicalize() {
                        if let Some(filename) = outpath.file_name() {
                            return parent_canonical.join(filename);
                        }
                    }
                }
                outpath.clone()
            });

            if !outpath_canonical.starts_with(&dest_dir_canonical) {
                return Err(RiffError::InstallationFailed(format!(
                    "Path traversal detected: {} escapes destination directory",
                    stripped_str
                )));
            }

            if entry.header().entry_type().is_dir() {
                // Already created above
            } else {
                entry.unpack(&outpath).map_err(|e| {
                    RiffError::InstallationFailed(format!("Failed to extract: {}", e))
                })?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        for (name, contents) in entries {
            archive
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            archive.write_all(contents).unwrap();
        }
        archive.finish().unwrap();
    }

    #[test]
    fn test_archive_type_from_path() {
        assert_eq!(
            ArchiveType::from_path(Path::new("package.zip")),
            Some(ArchiveType::Zip)
        );
        assert_eq!(
            ArchiveType::from_path(Path::new("package.tar.gz")),
            Some(ArchiveType::TarGz)
        );
        assert_eq!(
            ArchiveType::from_path(Path::new("package.tgz")),
            Some(ArchiveType::TarGz)
        );
        assert_eq!(
            ArchiveType::from_path(Path::new("package.tar.bz2")),
            Some(ArchiveType::TarBz2)
        );
        assert_eq!(
            ArchiveType::from_path(Path::new("package.tar")),
            Some(ArchiveType::Tar)
        );
        assert_eq!(ArchiveType::from_path(Path::new("package.txt")), None);
    }

    #[test]
    fn test_archive_type_from_content_type() {
        assert_eq!(
            ArchiveType::from_content_type("application/zip"),
            Some(ArchiveType::Zip)
        );
        assert_eq!(
            ArchiveType::from_content_type("application/gzip"),
            Some(ArchiveType::TarGz)
        );
        assert_eq!(
            ArchiveType::from_content_type("application/x-tar"),
            Some(ArchiveType::Tar)
        );
    }

    #[test]
    fn composer_archive_downloader_keeps_github_zipball_url_without_reference() {
        let url = "https://github.com/composer/composer/zipball/master";
        assert_eq!(process_dist_url(url, None), url);
    }

    #[test]
    fn composer_archive_downloader_keeps_github_tar_archive_without_reference() {
        let url = "https://github.com/composer/composer/archive/master.tar.gz";
        assert_eq!(process_dist_url(url, None), url);
    }

    #[test]
    fn composer_archive_downloader_keeps_github_api_url_without_reference() {
        let url = "https://api.github.com/repos/composer/composer/zipball/master";
        assert_eq!(process_dist_url(url, None), url);
    }

    #[test]
    fn composer_archive_downloader_rewrites_github_dist_references() {
        for (url, archive_type) in [
            (
                "https://api.github.com/repos/composer/composer/zipball/master",
                "zipball",
            ),
            (
                "https://api.github.com/repos/composer/composer/tarball/master",
                "tarball",
            ),
            (
                "https://github.com/composer/composer/zipball/master",
                "zipball",
            ),
            (
                "https://www.github.com/composer/composer/tarball/master",
                "tarball",
            ),
            (
                "https://github.com/composer/composer/archive/master.zip",
                "zipball",
            ),
            (
                "https://github.com/composer/composer/archive/master.tar.gz",
                "tarball",
            ),
        ] {
            assert_eq!(
                process_dist_url(url, Some("ref")),
                format!("https://api.github.com/repos/composer/composer/{archive_type}/ref")
            );
        }
    }

    #[test]
    fn composer_archive_downloader_rewrites_bitbucket_dist_references() {
        for (url, extension) in [
            (
                "https://bitbucket.org/davereid/drush-virtualhost/get/77ca490c26ac818e024d1138aa8bd3677d1ef21f.zip",
                "zip",
            ),
            (
                "https://bitbucket.org/davereid/drush-virtualhost/get/master.tar.gz",
                "tar.gz",
            ),
            (
                "https://bitbucket.org/davereid/drush-virtualhost/get/v1.0.tar.bz2",
                "tar.bz2",
            ),
        ] {
            assert_eq!(
                process_dist_url(url, Some("ref")),
                format!("https://bitbucket.org/davereid/drush-virtualhost/get/ref.{extension}")
            );
        }
    }

    #[test]
    fn composer_url_updates_dist_references_across_supported_forges() {
        for (url, expected, github, gitlab, reference) in [
            (
                "https://github.com/foo/bar/zipball/abcd",
                "https://api.github.com/repos/foo/bar/zipball/newref",
                vec!["github.com"],
                vec!["gitlab.com"],
                "newref",
            ),
            (
                "https://www.github.com/foo/bar/archive/abcd.tar.gz",
                "https://api.github.com/repos/foo/bar/tarball/newref",
                vec!["github.com"],
                vec!["gitlab.com"],
                "newref",
            ),
            (
                "https://mygithub.com/api/v3/repos/foo/bar/tarball/abcd",
                "https://mygithub.com/api/v3/repos/foo/bar/tarball/newref",
                vec!["mygithub.com"],
                vec!["gitlab.com"],
                "newref",
            ),
            (
                "https://www.bitbucket.org/foo/bar/get/abcd.tar.bz2",
                "https://bitbucket.org/foo/bar/get/newref.tar.bz2",
                vec!["github.com"],
                vec!["gitlab.com"],
                "newref",
            ),
            (
                "https://www.gitlab.com/api/v3/projects/foo%2Fbar/repository/archive.tar.gz?sha=abcd",
                "https://gitlab.com/api/v4/projects/foo%2Fbar/repository/archive.tar.gz?sha=newref",
                vec!["github.com"],
                vec!["gitlab.com"],
                "newref",
            ),
            (
                "https://mygitlab.com/api/v3/projects/foo%2Fbar/repository/archive.tar.bz2?sha=abcd",
                "https://mygitlab.com/api/v3/projects/foo%2Fbar/repository/archive.tar.bz2?sha=65",
                vec!["github.com"],
                vec!["mygitlab.com"],
                "65",
            ),
        ] {
            assert_eq!(
                process_dist_url_with_domains(url, Some(reference), &github, &gitlab),
                expected,
                "{url}"
            );
        }
    }

    #[test]
    fn composer_zip_downloader_reports_invalid_archives() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("composer-test.zip");
        std::fs::write(&archive_path, b"zip").unwrap();

        let error = ArchiveExtractor::extract(&archive_path, &temp.path().join("dest"))
            .expect_err("invalid zip must fail");

        assert!(error.to_string().contains("Failed to open zip"));
    }

    // Ported from Composer\Test\Downloader\XzDownloaderTest::testErrorMessages.
    #[test]
    fn composer_xz_downloader_reports_unrecognized_archive_format() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("composer-test.tar.xz");
        std::fs::write(&archive_path, b"this is not an xz archive").unwrap();

        let error = ArchiveExtractor::extract(&archive_path, &temp.path().join("dest"))
            .expect_err("invalid xz archive must fail");
        let message = error.to_string().to_ascii_lowercase();
        assert!(
            message.contains("format")
                || message.contains("archive")
                || message.contains("failed to read tar"),
            "unexpected extraction error: {error}"
        );
    }

    #[test]
    fn composer_zip_downloader_extracts_valid_archives() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("composer-test.zip");
        let file = File::create(&archive_path).unwrap();
        zip::ZipWriter::new(file).finish().unwrap();
        let destination = temp.path().join("dest");

        ArchiveExtractor::extract(&archive_path, &destination).unwrap();

        assert!(destination.is_dir());
    }

    #[test]
    fn composer_zip_downloader_reports_native_extraction_failure() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("conflict.zip");
        write_zip(
            &archive_path,
            &[
                ("package/conflict", b"file"),
                ("package/conflict/child.txt", b"child"),
            ],
        );

        let error = ArchiveExtractor::extract(&archive_path, &temp.path().join("dest"))
            .expect_err("file/directory conflict must fail extraction");

        assert!(error
            .to_string()
            .contains("There was an error extracting the ZIP file"));
    }

    #[test]
    fn composer_zip_downloader_reports_case_insensitive_name_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("case-collision.zip");
        write_zip(
            &archive_path,
            &[
                ("package/Source.php", b"first"),
                ("package/source.php", b"second"),
            ],
        );

        let error = ArchiveExtractor::extract(&archive_path, &temp.path().join("dest"))
            .expect_err("case-insensitive duplicate names must fail extraction");
        let message = error.to_string();
        assert!(message.contains("identical file names with different capitalization"));
        assert!(message.contains("Source.php"));
        assert!(message.contains("source.php"));
    }

    #[test]
    fn composer_zip_downloader_native_backend_reports_invalid_archive() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("invalid.zip");
        std::fs::write(&archive_path, b"not a ZIP archive").unwrap();

        let error = ArchiveExtractor::extract(&archive_path, &temp.path().join("dest"))
            .expect_err("invalid ZIP must fail");

        assert!(error.to_string().contains("Failed to open zip"));
    }

    #[test]
    fn composer_zip_downloader_native_backend_extracts_archive() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("package.zip");
        write_zip(&archive_path, &[("package/file.txt", b"contents")]);
        let destination = temp.path().join("dest");

        ArchiveExtractor::extract(&archive_path, &destination).unwrap();

        assert_eq!(
            std::fs::read(destination.join("file.txt")).unwrap(),
            b"contents"
        );
    }

    #[test]
    fn composer_zip_downloader_falls_back_to_native_extraction() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("package.zip");
        write_zip(&archive_path, &[("package/file.txt", b"contents")]);
        let destination = temp.path().join("dest");

        ArchiveExtractor::extract_zip_with_fallback(
            || {
                Err(RiffError::InstallationFailed(
                    "system unzip failed".to_owned(),
                ))
            },
            || ArchiveExtractor::extract(&archive_path, &destination),
        )
        .unwrap();

        assert_eq!(
            std::fs::read(destination.join("file.txt")).unwrap(),
            b"contents"
        );
    }

    #[test]
    fn composer_zip_downloader_reports_when_all_extraction_strategies_fail() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("invalid.zip");
        std::fs::write(&archive_path, b"not a ZIP archive").unwrap();

        let error = ArchiveExtractor::extract_zip_with_fallback(
            || {
                Err(RiffError::InstallationFailed(
                    "system unzip failed".to_owned(),
                ))
            },
            || ArchiveExtractor::extract(&archive_path, &temp.path().join("dest")),
        )
        .expect_err("both extraction strategies must fail");
        let message = error.to_string();
        assert!(message.contains("ZIP extraction failed with both strategies"));
        assert!(message.contains("system unzip failed"));
        assert!(message.contains("Failed to open zip"));
    }

    #[test]
    fn test_zip_extraction_rejects_escaping_path() {
        let temp = tempfile::TempDir::new().unwrap();
        let archive_path = temp.path().join("malicious.zip");
        let file = File::create(&archive_path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file(
                "../../escaped.php",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(b"escaped").unwrap();
        archive.finish().unwrap();

        let result = ArchiveExtractor::extract(&archive_path, &temp.path().join("dest"));

        assert!(result.is_err());
        assert!(!temp.path().join("escaped.php").exists());
    }

    #[test]
    fn test_zip_extraction_creates_implicit_nested_directories() {
        let temp = tempfile::TempDir::new().unwrap();
        let archive_path = temp.path().join("nested.zip");
        let file = File::create(&archive_path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();

        archive
            .start_file("package-hash/src/first.php", options)
            .unwrap();
        archive.write_all(b"first").unwrap();
        archive
            .start_file("package-hash/src/nested/second.php", options)
            .unwrap();
        archive.write_all(b"second").unwrap();
        archive
            .start_file("package-hash/src/third.php", options)
            .unwrap();
        archive.write_all(b"third").unwrap();
        archive.finish().unwrap();

        let dest = temp.path().join("dest");
        ArchiveExtractor::extract(&archive_path, &dest).unwrap();

        assert_eq!(std::fs::read(dest.join("src/first.php")).unwrap(), b"first");
        assert_eq!(
            std::fs::read(dest.join("src/nested/second.php")).unwrap(),
            b"second"
        );
        assert_eq!(std::fs::read(dest.join("src/third.php")).unwrap(), b"third");
    }

    #[test]
    fn test_large_zip_extraction_uses_streaming_reader() {
        let temp = tempfile::TempDir::new().unwrap();
        let archive_path = temp.path().join("large.zip");
        let file = File::create(&archive_path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let contents = vec![b'x'; MAX_IN_MEMORY_ZIP_SIZE as usize + 1];

        archive
            .start_file("package-hash/large.bin", options)
            .unwrap();
        archive.write_all(&contents).unwrap();
        archive.finish().unwrap();

        assert!(archive_path.metadata().unwrap().len() > MAX_IN_MEMORY_ZIP_SIZE);
        let dest = temp.path().join("dest");
        ArchiveExtractor::extract(&archive_path, &dest).unwrap();

        assert_eq!(std::fs::read(dest.join("large.bin")).unwrap(), contents);
    }
}
