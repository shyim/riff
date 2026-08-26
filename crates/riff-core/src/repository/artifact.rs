//! Artifact repository - discovers packages from archive files in a directory.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use sha1::{Digest, Sha1};
use walkdir::WalkDir;

use super::traits::{ProviderInfo, Repository, SearchMode, SearchResult};
use crate::archive::{read_composer_json_from_archive, ComposerArchiveFormat};
use crate::package::{Autoload, AutoloadPath, Dist, Package};

/// Artifact repository - provides packages from archive files in a directory
///
/// This repository type scans a directory for archive files (zip, tar, tar.gz, tgz)
/// and extracts package information from their composer.json files.
///
/// ```json
/// {
///     "repositories": [
///         {
///             "type": "artifact",
///             "url": "path/to/directory/with/zips/"
///         }
///     ]
/// }
/// ```
#[derive(Debug)]
pub struct ArtifactRepository {
    /// Repository name
    name: String,
    /// Directory path to scan for archives
    path: PathBuf,
    /// Configured path used to keep dist URLs relative when requested.
    dist_base: PathBuf,
    /// Discovered packages
    packages: Vec<Arc<Package>>,
}

impl ArtifactRepository {
    /// Create a new artifact repository
    ///
    /// # Arguments
    /// * `path` - Path to directory containing archive files
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let name = format!("artifact ({})", path.display());

        let mut repo = Self {
            name,
            dist_base: path.clone(),
            path,
            packages: Vec::new(),
        };

        repo.scan_directory();
        repo
    }

    /// Create a repository whose relative URL is resolved from a project
    /// directory without turning package dist URLs into absolute paths.
    pub fn new_with_base(path: impl Into<PathBuf>, base_dir: impl AsRef<Path>) -> Self {
        let dist_base = path.into();
        let path = if dist_base.is_absolute() {
            dist_base.clone()
        } else {
            base_dir.as_ref().join(&dist_base)
        };
        let name = format!("artifact ({})", dist_base.display());
        let mut repo = Self {
            name,
            path,
            dist_base,
            packages: Vec::new(),
        };
        repo.scan_directory();
        repo
    }

    /// Scan the directory for archive files and extract package information
    fn scan_directory(&mut self) {
        if !self.path.exists() || !self.path.is_dir() {
            return;
        }

        for entry in WalkDir::new(&self.path)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();

            if !path.is_file() {
                continue;
            }

            // Check for supported archive extensions
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase());

            let archive_type = match ext.as_deref() {
                Some("zip") => ComposerArchiveFormat::Zip,
                Some("tar") => ComposerArchiveFormat::Tar,
                Some("gz") => {
                    // Check if it's .tar.gz
                    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                    if stem.ends_with(".tar") {
                        ComposerArchiveFormat::TarGz
                    } else {
                        continue; // Skip plain .gz files
                    }
                }
                Some("tgz") => ComposerArchiveFormat::TarGz,
                _ => continue,
            };

            if let Some(package) = self.load_package_from_archive(path, archive_type) {
                self.packages.push(Arc::new(package));
            }
        }
    }

    /// Load package information from an archive file
    fn load_package_from_archive(
        &self,
        path: &Path,
        archive_type: ComposerArchiveFormat,
    ) -> Option<Package> {
        let manifest = read_composer_json_from_archive(path, archive_type)
            .ok()
            .flatten()?;

        let json: serde_json::Value = serde_json::from_str(&manifest).ok()?;

        // Required fields
        let name = json.get("name")?.as_str()?;
        let version = json.get("version")?.as_str()?;

        let mut pkg = Package::new(name, version);

        let shasum = calculate_sha1(path).ok();

        let dist_type = match archive_type {
            ComposerArchiveFormat::Zip => "zip",
            ComposerArchiveFormat::Tar | ComposerArchiveFormat::TarGz => "tar",
        };

        let relative = path.strip_prefix(&self.path).unwrap_or(path);
        let dist_path = self.dist_base.join(relative);
        let dist_url = dist_path.to_string_lossy();
        let dist_url = if dist_path.is_absolute() {
            dist_url.into_owned()
        } else {
            dist_url.replace('\\', "/")
        };
        let mut dist = Dist::new(dist_type, &dist_url);
        if let Some(sha) = shasum {
            dist = dist.with_shasum(&sha);
        }
        pkg.dist = Some(dist);

        if let Some(desc) = json.get("description").and_then(|v| v.as_str()) {
            pkg.description = Some(desc.to_string());
        }

        if let Some(t) = json.get("type").and_then(|v| v.as_str()) {
            pkg.package_type = t.into();
        }

        if let Some(license) = json.get("license") {
            pkg.license = parse_license(license).into_iter().map(Into::into).collect();
        }

        if let Some(require) = json.get("require").and_then(|v| v.as_object()) {
            pkg.require = require
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("*").to_string()))
                .collect();
        }

        if let Some(require_dev) = json.get("require-dev").and_then(|v| v.as_object()) {
            pkg.require_dev = require_dev
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("*").to_string()))
                .collect();
        }

        if let Some(autoload) = json.get("autoload") {
            pkg.autoload = Some(parse_autoload(autoload));
        }

        if let Some(autoload_dev) = json.get("autoload-dev") {
            pkg.autoload_dev = Some(parse_autoload(autoload_dev));
        }

        if let Some(bin) = json.get("bin").and_then(|v| v.as_array()) {
            pkg.bin = bin
                .iter()
                .filter_map(|v| v.as_str().map(Into::into))
                .collect();
        }

        Some(pkg)
    }
}

/// Calculate SHA1 hash of a file
fn calculate_sha1(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// Parse license from JSON value
fn parse_license(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(Into::into))
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse autoload from JSON value
fn parse_autoload(value: &serde_json::Value) -> Autoload {
    let mut autoload = Autoload::default();

    if let Some(psr4) = value.get("psr-4").and_then(|v| v.as_object()) {
        for (namespace, paths) in psr4 {
            let path = json_to_autoload_path(paths);
            autoload.psr4.insert(namespace.clone(), path);
        }
    }

    if let Some(psr0) = value.get("psr-0").and_then(|v| v.as_object()) {
        for (namespace, paths) in psr0 {
            let path = json_to_autoload_path(paths);
            autoload.psr0.insert(namespace.clone(), path);
        }
    }

    if let Some(classmap) = value.get("classmap").and_then(|v| v.as_array()) {
        autoload.classmap = classmap
            .iter()
            .filter_map(|v| v.as_str().map(Into::into))
            .collect();
    }

    if let Some(files) = value.get("files").and_then(|v| v.as_array()) {
        autoload.files = files
            .iter()
            .filter_map(|v| v.as_str().map(Into::into))
            .collect();
    }

    autoload
}

/// Convert JSON value to AutoloadPath
fn json_to_autoload_path(value: &serde_json::Value) -> AutoloadPath {
    match value {
        serde_json::Value::String(s) => AutoloadPath::Single(s.as_str().into()),
        serde_json::Value::Array(arr) => {
            let paths: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            if paths.len() == 1 {
                AutoloadPath::Single(paths[0].as_str().into())
            } else {
                AutoloadPath::Multiple(paths.into_iter().map(Into::into).collect())
            }
        }
        _ => AutoloadPath::Single("".into()),
    }
}

#[async_trait]
impl Repository for ArtifactRepository {
    fn name(&self) -> &str {
        &self.name
    }

    async fn has_package(&self, name: &str) -> bool {
        self.packages
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(name))
    }

    async fn find_packages(&self, name: &str) -> Vec<Arc<Package>> {
        self.packages
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case(name))
            .cloned()
            .collect()
    }

    async fn find_package(&self, name: &str, version: &str) -> Option<Arc<Package>> {
        self.packages
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name) && p.version == version)
            .cloned()
    }

    async fn find_packages_with_constraint(
        &self,
        name: &str,
        _constraint: &str,
    ) -> Vec<Arc<Package>> {
        // Return all versions, let the solver filter
        self.find_packages(name).await
    }

    async fn get_packages(&self) -> Vec<Arc<Package>> {
        self.packages.clone()
    }

    async fn search(&self, query: &str, _mode: SearchMode) -> Vec<SearchResult> {
        self.packages
            .iter()
            .filter(|p| {
                p.name.contains(query)
                    || p.description
                        .as_ref()
                        .map(|d| d.contains(query))
                        .unwrap_or(false)
            })
            .map(|p| SearchResult {
                name: p.name.clone(),
                description: p.description.clone(),
                url: None,
                abandoned: None,
                downloads: None,
                favers: None,
            })
            .collect()
    }

    async fn get_providers(&self, _package_name: &str) -> Vec<ProviderInfo> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn create_test_zip(dir: &Path, name: &str, pkg_name: &str, version: &str) -> PathBuf {
        let zip_path = dir.join(name);
        let file = File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);

        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        let manifest = serde_json::json!({
            "name": pkg_name,
            "version": version,
            "description": "Test package"
        });

        zip.start_file("composer.json", options).unwrap();
        zip.write_all(manifest.to_string().as_bytes()).unwrap();

        zip.finish().unwrap();
        zip_path
    }

    fn create_test_zip_with_subdir(
        dir: &Path,
        name: &str,
        pkg_name: &str,
        version: &str,
    ) -> PathBuf {
        let zip_path = dir.join(name);
        let file = File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);

        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        let manifest = serde_json::json!({
            "name": pkg_name,
            "version": version,
            "description": "Test package in subdirectory"
        });

        // Add directory entry
        zip.add_directory("package/", options).unwrap();

        // Add composer.json in subdirectory
        zip.start_file("package/composer.json", options).unwrap();
        zip.write_all(manifest.to_string().as_bytes()).unwrap();

        zip.finish().unwrap();
        zip_path
    }

    fn create_test_tar(dir: &Path, name: &str, pkg_name: &str, version: &str) -> PathBuf {
        let tar_path = dir.join(name);
        let file = File::create(&tar_path).unwrap();
        let mut tar = tar::Builder::new(file);
        let manifest = serde_json::to_vec(&serde_json::json!({
            "name": pkg_name,
            "version": version
        }))
        .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "composer.json", manifest.as_slice())
            .unwrap();
        tar.finish().unwrap();
        tar_path
    }

    // Ported from Composer\Test\Repository\ArtifactRepositoryTest::
    // testExtractsConfigsFromZipArchives.
    #[tokio::test]
    async fn composer_artifact_repository_extracts_valid_root_and_first_level_manifests() {
        let temp = TempDir::new().unwrap();
        create_test_zip(temp.path(), "root.zip", "vendor/root", "1.0.0");
        create_test_zip_with_subdir(temp.path(), "nested.zip", "vendor/nested", "2.0.0");
        create_test_tar(temp.path(), "archive.tar", "vendor/tar", "3.0.0");
        let invalid = File::create(temp.path().join("invalid.zip")).unwrap();
        let mut invalid = zip::ZipWriter::new(invalid);
        invalid
            .start_file(
                "nested/too-deep/readme",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        invalid.write_all(b"not a package").unwrap();
        invalid.finish().unwrap();

        let repository = ArtifactRepository::new(temp.path());
        let mut packages = repository.get_packages().await;
        packages.sort_by(|left, right| left.name.cmp(&right.name));
        assert_eq!(
            packages
                .iter()
                .map(|package| (package.name.as_str(), package.pretty_version()))
                .collect::<Vec<_>>(),
            [
                ("vendor/nested", "2.0.0"),
                ("vendor/root", "1.0.0"),
                ("vendor/tar", "3.0.0")
            ]
        );
        assert_eq!(
            packages
                .iter()
                .find(|package| package.name == "vendor/tar")
                .unwrap()
                .dist
                .as_ref()
                .unwrap()
                .dist_type,
            "tar"
        );
    }

    // Ported from Composer\Test\Repository\ArtifactRepositoryTest::
    // testAbsoluteRepoUrlCreatesAbsoluteUrlPackages.
    #[tokio::test]
    async fn composer_artifact_repository_keeps_absolute_dist_urls() {
        let temp = TempDir::new().unwrap();
        let artifacts = temp.path().join("artifacts");
        std::fs::create_dir(&artifacts).unwrap();
        create_test_zip(&artifacts, "package.zip", "vendor/package", "1.0.0");

        let packages = ArtifactRepository::new(&artifacts).get_packages().await;
        let url = &packages[0].dist.as_ref().unwrap().url;
        assert!(Path::new(url).is_absolute());
        assert!(url.starts_with(&artifacts.to_string_lossy().to_string()));
    }

    // Ported from Composer\Test\Repository\ArtifactRepositoryTest::
    // testRelativeRepoUrlCreatesRelativeUrlPackages.
    #[tokio::test]
    async fn composer_artifact_repository_keeps_relative_dist_urls() {
        let temp = TempDir::new().unwrap();
        let artifacts = temp.path().join("artifacts");
        std::fs::create_dir(&artifacts).unwrap();
        create_test_zip(&artifacts, "package.zip", "vendor/package", "1.0.0");

        let packages = ArtifactRepository::new_with_base("artifacts", temp.path())
            .get_packages()
            .await;
        assert_eq!(
            packages[0].dist.as_ref().unwrap().url,
            "artifacts/package.zip"
        );
    }

    #[tokio::test]
    async fn test_artifact_repository_zip() {
        let temp = TempDir::new().unwrap();
        create_test_zip(temp.path(), "package-1.0.0.zip", "vendor/package", "1.0.0");

        let repo = ArtifactRepository::new(temp.path());
        let packages = repo.get_packages().await;

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "vendor/package");
        assert_eq!(packages[0].version, "1.0.0");
        assert!(packages[0].dist.is_some());
    }

    #[tokio::test]
    async fn test_artifact_repository_zip_with_subdir() {
        let temp = TempDir::new().unwrap();
        create_test_zip_with_subdir(temp.path(), "package-1.0.0.zip", "vendor/package", "1.0.0");

        let repo = ArtifactRepository::new(temp.path());
        let packages = repo.get_packages().await;

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "vendor/package");
    }

    #[tokio::test]
    async fn test_artifact_repository_multiple_packages() {
        let temp = TempDir::new().unwrap();
        create_test_zip(
            temp.path(),
            "package-a-1.0.0.zip",
            "vendor/package-a",
            "1.0.0",
        );
        create_test_zip(
            temp.path(),
            "package-b-2.0.0.zip",
            "vendor/package-b",
            "2.0.0",
        );

        let repo = ArtifactRepository::new(temp.path());
        let packages = repo.get_packages().await;

        assert_eq!(packages.len(), 2);
    }

    #[tokio::test]
    async fn test_artifact_repository_multiple_versions() {
        let temp = TempDir::new().unwrap();
        create_test_zip(temp.path(), "package-1.0.0.zip", "vendor/package", "1.0.0");
        create_test_zip(temp.path(), "package-2.0.0.zip", "vendor/package", "2.0.0");

        let repo = ArtifactRepository::new(temp.path());
        let packages = repo.find_packages("vendor/package").await;

        assert_eq!(packages.len(), 2);
    }

    #[tokio::test]
    async fn test_artifact_repository_find_package() {
        let temp = TempDir::new().unwrap();
        create_test_zip(temp.path(), "package-1.0.0.zip", "vendor/package", "1.0.0");
        create_test_zip(temp.path(), "package-2.0.0.zip", "vendor/package", "2.0.0");

        let repo = ArtifactRepository::new(temp.path());

        let found = repo.find_package("vendor/package", "1.0.0").await;
        assert!(found.is_some());
        assert_eq!(found.unwrap().version, "1.0.0");

        let not_found = repo.find_package("vendor/package", "3.0.0").await;
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn test_artifact_repository_with_metadata() {
        let temp = TempDir::new().unwrap();
        let zip_path = temp.path().join("package.zip");
        let file = File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);

        let options = zip::write::SimpleFileOptions::default();

        let manifest = serde_json::json!({
            "name": "vendor/package",
            "version": "1.0.0",
            "description": "A test package",
            "type": "library",
            "license": "MIT",
            "require": {
                "php": ">=8.0"
            },
            "autoload": {
                "psr-4": {
                    "Vendor\\Package\\": "src/"
                }
            }
        });

        zip.start_file("composer.json", options).unwrap();
        zip.write_all(manifest.to_string().as_bytes()).unwrap();
        zip.finish().unwrap();

        let repo = ArtifactRepository::new(temp.path());
        let packages = repo.get_packages().await;

        assert_eq!(packages.len(), 1);
        let pkg = &packages[0];
        assert_eq!(pkg.description, Some("A test package".to_string()));
        assert_eq!(pkg.package_type, "library");
        assert_eq!(pkg.license, vec!["MIT".to_string()]);
        assert!(pkg.require.contains_key("php"));
        assert!(pkg.autoload.is_some());
    }

    #[tokio::test]
    async fn test_artifact_repository_skips_invalid() {
        let temp = TempDir::new().unwrap();

        // Create a valid package
        create_test_zip(temp.path(), "valid.zip", "vendor/valid", "1.0.0");

        // Create an invalid zip (no composer.json)
        let invalid_path = temp.path().join("invalid.zip");
        let file = File::create(&invalid_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("readme.txt", options).unwrap();
        zip.write_all(b"No composer.json here").unwrap();
        zip.finish().unwrap();

        let repo = ArtifactRepository::new(temp.path());
        let packages = repo.get_packages().await;

        // Should only have the valid package
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "vendor/valid");
    }

    #[tokio::test]
    async fn test_artifact_repository_sha1_checksum() {
        let temp = TempDir::new().unwrap();
        create_test_zip(temp.path(), "package.zip", "vendor/package", "1.0.0");

        let repo = ArtifactRepository::new(temp.path());
        let packages = repo.get_packages().await;

        assert_eq!(packages.len(), 1);
        let dist = packages[0].dist.as_ref().unwrap();
        assert!(dist.shasum.is_some());
        // SHA1 is 40 hex characters
        assert_eq!(dist.shasum.as_ref().unwrap().len(), 40);
    }

    #[tokio::test]
    async fn test_artifact_repository_empty_directory() {
        let temp = TempDir::new().unwrap();

        let repo = ArtifactRepository::new(temp.path());
        let packages = repo.get_packages().await;

        assert!(packages.is_empty());
    }

    #[tokio::test]
    async fn test_artifact_repository_nonexistent_directory() {
        let repo = ArtifactRepository::new("/nonexistent/path");
        let packages = repo.get_packages().await;

        assert!(packages.is_empty());
    }

    #[tokio::test]
    async fn test_artifact_repository_subdirectories() {
        let temp = TempDir::new().unwrap();

        // Create a subdirectory with packages
        let subdir = temp.path().join("packages");
        std::fs::create_dir(&subdir).unwrap();
        create_test_zip(&subdir, "package.zip", "vendor/package", "1.0.0");

        let repo = ArtifactRepository::new(temp.path());
        let packages = repo.get_packages().await;

        // Should find packages in subdirectories
        assert_eq!(packages.len(), 1);
    }

    #[tokio::test]
    async fn test_search() {
        let temp = TempDir::new().unwrap();
        create_test_zip(temp.path(), "foo.zip", "vendor/foo-package", "1.0.0");
        create_test_zip(temp.path(), "bar.zip", "vendor/bar-package", "1.0.0");

        let repo = ArtifactRepository::new(temp.path());

        let results = repo.search("foo", SearchMode::Name).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "vendor/foo-package");
    }
}
