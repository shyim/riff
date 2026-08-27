//! Download manager for orchestrating package downloads.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cache::runtime_cache_dir;
use crate::http::HttpClient;
use crate::output::Output;
use crate::package::{Dist, Source};
use crate::{Package, Result, RiffError};

use super::archive::{process_dist_url, ArchiveExtractor};
use super::checksum::{verify_checksum, ChecksumType};
use super::file::FileDownloader;
use super::git::GitDownloader;
use super::path::{PathDownloader, PathStrategy};
use super::vcs::VcsDownloader;

/// Result of a download operation
#[derive(Debug)]
pub struct DownloadResult {
    /// Path where the package was extracted
    pub path: PathBuf,
    /// Whether the download was from cache
    pub from_cache: bool,
    /// Whether the download was skipped (already installed)
    pub skipped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadSource {
    Source,
    Dist,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadPreference {
    Auto,
    Source,
    Dist,
}

/// Configuration for the download manager
#[derive(Debug, Clone)]
pub struct DownloadConfig {
    /// Project directory used to resolve relative path repositories
    pub base_dir: PathBuf,
    /// Prefer source over dist
    pub prefer_source: bool,
    /// Prefer dist over source
    pub prefer_dist: bool,
    /// Cache directory for downloaded archives
    pub cache_dir: PathBuf,
    /// Vendor directory for extracted packages
    pub vendor_dir: PathBuf,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            base_dir: PathBuf::from("."),
            prefer_source: false,
            prefer_dist: true,
            cache_dir: runtime_cache_dir(),
            vendor_dir: PathBuf::from("vendor"),
        }
    }
}

/// Download manager for package installation
pub struct DownloadManager {
    file_downloader: FileDownloader,
    git_downloader: GitDownloader,
    path_downloader: PathDownloader,
    extraction_semaphore: tokio::sync::Semaphore,
    config: DownloadConfig,
    preferences: Vec<(String, DownloadPreference)>,
    source_fallback: bool,
    output: Output,
}

const MAX_CONCURRENT_EXTRACTIONS: usize = 10;

impl DownloadManager {
    /// Create a new download manager
    pub fn new(http_client: Arc<HttpClient>, config: DownloadConfig) -> Self {
        Self::new_with_output(http_client, config, Output::silent())
    }

    pub fn new_with_output(
        http_client: Arc<HttpClient>,
        config: DownloadConfig,
        output: Output,
    ) -> Self {
        Self {
            file_downloader: FileDownloader::new(http_client),
            git_downloader: GitDownloader::new(),
            path_downloader: PathDownloader::new(),
            extraction_semaphore: tokio::sync::Semaphore::new(MAX_CONCURRENT_EXTRACTIONS),
            config,
            preferences: Vec::new(),
            source_fallback: false,
            output,
        }
    }

    pub fn with_preferences(
        mut self,
        preferences: impl IntoIterator<Item = (String, DownloadPreference)>,
    ) -> Self {
        self.preferences = preferences.into_iter().collect();
        self
    }

    pub fn with_source_fallback(mut self, enabled: bool) -> Self {
        self.source_fallback = enabled;
        self
    }

    pub fn installed_download_source(package: &Package) -> Result<Option<DownloadSource>> {
        if package.is_metapackage() {
            return Ok(None);
        }
        match package.installation_source.as_deref() {
            Some("source") if package.source.is_some() => {
                let source = package.source.as_ref().expect("checked above");
                if is_source_type(&source.source_type) {
                    Ok(Some(DownloadSource::Source))
                } else {
                    Err(invalid_installed_source(package, "source"))
                }
            }
            Some("dist") if package.dist.is_some() => {
                let dist = package.dist.as_ref().expect("checked above");
                if !is_source_type(&dist.dist_type) {
                    Ok(Some(DownloadSource::Dist))
                } else {
                    Err(invalid_installed_source(package, "dist"))
                }
            }
            Some(source @ ("source" | "dist")) => Err(invalid_installed_source(package, source)),
            Some(source) => Err(invalid_installed_source(package, source)),
            None => Err(RiffError::DownloadFailed {
                package: package.name.clone(),
                reason: "Package has no installation source".to_owned(),
            }),
        }
    }

    pub fn available_sources(
        &self,
        package: &Package,
        previous: Option<&Package>,
    ) -> Vec<DownloadSource> {
        let source_available = package.source.is_some();
        let dist_available = package.dist.is_some();
        let mut sources = match self.preference_for(package) {
            DownloadPreference::Source => [DownloadSource::Source, DownloadSource::Dist],
            DownloadPreference::Dist => [DownloadSource::Dist, DownloadSource::Source],
            DownloadPreference::Auto if package.is_dev() => {
                [DownloadSource::Source, DownloadSource::Dist]
            }
            DownloadPreference::Auto => [DownloadSource::Dist, DownloadSource::Source],
        }
        .into_iter()
        .filter(|source| match source {
            DownloadSource::Source => source_available,
            DownloadSource::Dist => dist_available,
        })
        .collect::<Vec<_>>();

        if let Some(previous) = previous {
            let keep_previous = !(previous.installation_source.as_deref() == Some("dist")
                && !previous.is_dev()
                && package.is_dev());
            if keep_previous {
                let previous = match previous.installation_source.as_deref() {
                    Some("source") => Some(DownloadSource::Source),
                    Some("dist") => Some(DownloadSource::Dist),
                    _ => None,
                };
                if let Some(index) = previous
                    .and_then(|source| sources.iter().position(|candidate| *candidate == source))
                {
                    sources.swap(0, index);
                }
            }
        }

        sources
    }

    /// Download and install a package
    pub async fn download(&self, package: &Package) -> Result<DownloadResult> {
        let dest_dir = self.package_path(package);

        if package.is_metapackage() {
            return Ok(DownloadResult {
                path: dest_dir,
                from_cache: false,
                skipped: true,
            });
        }

        if let Some(dist) = &package.dist {
            if dist.dist_type == "path" {
                log::debug!(
                    "Installing {} ({}) from path",
                    package.name,
                    package.version
                );
                return self.download_from_path(package, dist, &dest_dir).await;
            }
        }

        let sources = self.available_sources(package, None);
        let mut last_error = None;
        for (index, download_source) in sources.iter().copied().enumerate() {
            let result = self
                .download_from_selected_source(package, download_source, &dest_dir)
                .await;
            match result {
                Ok(result) => return Ok(result),
                Err(error) => {
                    let next = sources.get(index + 1).copied();
                    let may_fallback = match (download_source, next) {
                        (DownloadSource::Source, Some(DownloadSource::Dist)) => true,
                        (DownloadSource::Dist, Some(DownloadSource::Source)) => {
                            self.source_fallback
                        }
                        _ => false,
                    };
                    if !may_fallback {
                        return Err(error);
                    }
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| RiffError::DownloadFailed {
            package: package.name.clone(),
            reason: "No source or dist available".to_string(),
        }))
    }

    async fn download_from_selected_source(
        &self,
        package: &Package,
        source: DownloadSource,
        dest_dir: &Path,
    ) -> Result<DownloadResult> {
        match source {
            DownloadSource::Source => {
                let source = package
                    .source
                    .as_ref()
                    .ok_or_else(|| RiffError::DownloadFailed {
                        package: package.name.clone(),
                        reason: "No source available".to_owned(),
                    })?;
                self.download_from_source(package, source, dest_dir).await?;
                Ok(DownloadResult {
                    path: dest_dir.to_path_buf(),
                    from_cache: false,
                    skipped: false,
                })
            }
            DownloadSource::Dist => {
                let dist = package
                    .dist
                    .as_ref()
                    .ok_or_else(|| RiffError::DownloadFailed {
                        package: package.name.clone(),
                        reason: "No dist available".to_owned(),
                    })?;
                let from_cache = self.download_from_dist(package, dist, dest_dir).await?;
                Ok(DownloadResult {
                    path: dest_dir.to_path_buf(),
                    from_cache,
                    skipped: false,
                })
            }
        }
    }

    /// Download multiple packages in parallel
    pub async fn download_many(&self, packages: &[Package]) -> Vec<Result<DownloadResult>> {
        use futures_util::stream::{self, StreamExt};

        const MAX_CONCURRENT_DOWNLOADS: usize = 10;

        stream::iter(packages)
            .map(|package| self.download(package))
            .buffer_unordered(MAX_CONCURRENT_DOWNLOADS)
            .collect()
            .await
    }

    /// Populate Riff's package cache without extracting into vendor.
    pub async fn download_only(&self, package: &Package) -> Result<DownloadResult> {
        if let Some(dist) = &package.dist {
            if dist.dist_type == "path" {
                return Ok(DownloadResult {
                    path: PathBuf::from(&dist.url),
                    from_cache: true,
                    skipped: true,
                });
            }
        }

        let use_source = self.should_use_source(package);
        if !use_source {
            if let Some(dist) = &package.dist {
                let (path, from_cache) = self.cache_dist_archive(package, dist).await?;
                return Ok(DownloadResult {
                    path,
                    from_cache,
                    skipped: false,
                });
            }
        }

        if let Some(source) = &package.source {
            let safe_reference: String = source
                .reference
                .chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                        character
                    } else {
                        '_'
                    }
                })
                .collect();
            let path = self
                .config
                .cache_dir
                .join("vcs")
                .join(&package.name)
                .join(safe_reference);
            let completion_marker = path.join(".riff-download-complete");
            if completion_marker.exists() {
                return Ok(DownloadResult {
                    path,
                    from_cache: true,
                    skipped: false,
                });
            }
            if path.exists() {
                tokio::fs::remove_dir_all(&path).await?;
            }
            self.download_from_source(package, source, &path).await?;
            tokio::fs::write(&completion_marker, []).await?;
            return Ok(DownloadResult {
                path,
                from_cache: false,
                skipped: false,
            });
        }

        if let Some(dist) = &package.dist {
            let (path, from_cache) = self.cache_dist_archive(package, dist).await?;
            return Ok(DownloadResult {
                path,
                from_cache,
                skipped: false,
            });
        }

        Err(RiffError::DownloadFailed {
            package: package.name.clone(),
            reason: "No source or dist available".to_string(),
        })
    }

    async fn cache_dist_archive(&self, package: &Package, dist: &Dist) -> Result<(PathBuf, bool)> {
        let cache_file = self.cache_path(package, &dist.dist_type);
        if let Some(parent) = cache_file.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let checksum = dist
            .sha256
            .as_ref()
            .filter(|value| !value.is_empty())
            .or_else(|| dist.shasum.as_ref().filter(|value| !value.is_empty()));
        if cache_file.exists() {
            let valid = if let Some(checksum) = checksum {
                let checksum_type =
                    ChecksumType::from_hex_length(checksum.len()).unwrap_or(ChecksumType::Sha256);
                verify_checksum(&cache_file, checksum, checksum_type).await?
            } else {
                true
            };
            if valid {
                return Ok((cache_file, true));
            }
            let _ = tokio::fs::remove_file(&cache_file).await;
        }

        for url in dist.urls() {
            let url = process_dist_url(&url, dist.reference.as_deref());
            if let Err(error) = self
                .file_downloader
                .download(&url, &cache_file, None::<fn(u64, u64)>)
                .await
            {
                crate::warnln!(
                    self.output,
                    "Warning: Failed to download from {}: {}",
                    url,
                    error
                );
                continue;
            }
            if let Some(checksum) = checksum {
                let checksum_type =
                    ChecksumType::from_hex_length(checksum.len()).unwrap_or(ChecksumType::Sha256);
                if !verify_checksum(&cache_file, checksum, checksum_type).await? {
                    let _ = tokio::fs::remove_file(&cache_file).await;
                    return Err(RiffError::ChecksumMismatch {
                        package: package.name.clone(),
                    });
                }
            }
            return Ok((cache_file, false));
        }
        Err(RiffError::DownloadFailed {
            package: package.name.clone(),
            reason: "All download URLs failed".to_string(),
        })
    }

    /// Download from dist (archive)
    /// Returns true if the download was from cache
    async fn download_from_dist(
        &self,
        package: &Package,
        dist: &Dist,
        dest_dir: &Path,
    ) -> Result<bool> {
        let cache_file = self.cache_path(package, &dist.dist_type);
        if let Some(parent) = cache_file.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Try URLs in order (primary + mirrors)
        let urls = dist.urls();

        let checksum = dist
            .sha256
            .as_ref()
            .filter(|s| !s.is_empty())
            .or_else(|| dist.shasum.as_ref().filter(|s| !s.is_empty()));

        for url in &urls {
            let url = process_dist_url(url, dist.reference.as_deref());
            if cache_file.exists() {
                // Verify checksum if available
                if let Some(checksum) = checksum {
                    let checksum_type = ChecksumType::from_hex_length(checksum.len())
                        .unwrap_or(ChecksumType::Sha256);

                    if verify_checksum(&cache_file, checksum, checksum_type).await? {
                        self.extract_archive(&cache_file, dest_dir).await?;
                        return Ok(true);
                    }
                    let _ = tokio::fs::remove_file(&cache_file).await;
                } else {
                    self.extract_archive(&cache_file, dest_dir).await?;
                    return Ok(true);
                }
            }

            let result = self
                .file_downloader
                .download(&url, &cache_file, None::<fn(u64, u64)>)
                .await;

            if let Err(e) = result {
                crate::warnln!(
                    self.output,
                    "Warning: Failed to download from {}: {}",
                    url,
                    e
                );
                continue;
            }

            // Verify checksum if available
            if let Some(checksum) = checksum {
                let checksum_type =
                    ChecksumType::from_hex_length(checksum.len()).unwrap_or(ChecksumType::Sha256);

                if !verify_checksum(&cache_file, checksum, checksum_type).await? {
                    let _ = tokio::fs::remove_file(&cache_file).await;
                    return Err(RiffError::ChecksumMismatch {
                        package: package.name.clone(),
                    });
                }
            }

            // Extract the archive
            self.extract_archive(&cache_file, dest_dir).await?;
            return Ok(false);
        }

        Err(RiffError::DownloadFailed {
            package: package.name.clone(),
            reason: "All download URLs failed".to_string(),
        })
    }

    /// Download from source (git)
    async fn download_from_source(
        &self,
        package: &Package,
        source: &Source,
        dest_dir: &Path,
    ) -> Result<()> {
        // Create destination directory
        if let Some(parent) = dest_dir.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        match source.source_type.as_str() {
            "git" => {
                // Try URLs in order
                for url in source.urls() {
                    let result = self
                        .git_downloader
                        .clone(&url, dest_dir, Some(&source.reference));

                    if result.is_ok() {
                        return Ok(());
                    }
                }

                Err(RiffError::DownloadFailed {
                    package: package.name.clone(),
                    reason: "Git clone failed for all URLs".to_string(),
                })
            }
            "hg" | "mercurial" | "svn" | "fossil" | "perforce" | "p4" => {
                let mut last_error = None;
                for url in source.urls() {
                    if dest_dir.exists() {
                        if dest_dir.is_dir() {
                            std::fs::remove_dir_all(dest_dir)?;
                        } else {
                            std::fs::remove_file(dest_dir)?;
                        }
                    }
                    match VcsDownloader::clone(
                        source.source_type.as_str(),
                        &url,
                        dest_dir,
                        Some(&source.reference),
                    ) {
                        Ok(()) => return Ok(()),
                        Err(error) => last_error = Some(error),
                    }
                }
                Err(RiffError::DownloadFailed {
                    package: package.name.clone(),
                    reason: last_error.map_or_else(
                        || format!("{} checkout failed for all URLs", source.source_type),
                        |error| error.to_string(),
                    ),
                })
            }
            other => Err(RiffError::DownloadFailed {
                package: package.name.clone(),
                reason: format!("Unsupported source type: {}", other),
            }),
        }
    }

    /// Download from path (local directory)
    async fn download_from_path(
        &self,
        _package: &Package,
        dist: &Dist,
        dest_dir: &Path,
    ) -> Result<DownloadResult> {
        let source_path = PathBuf::from(&dist.url);
        let source_path = if source_path.is_absolute() {
            source_path
        } else {
            self.config.base_dir.join(source_path)
        };

        // Determine strategy from transport options
        let strategy = dist
            .transport_options
            .as_ref()
            .and_then(|opts| opts.get("symlink"))
            .and_then(|v| v.as_bool())
            .map(|symlink| {
                if symlink {
                    PathStrategy::Symlink
                } else {
                    PathStrategy::Mirror
                }
            });

        let relative = dist
            .transport_options
            .as_ref()
            .and_then(|opts| opts.get("relative"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Create parent directory if needed
        if let Some(parent) = dest_dir.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        self.path_downloader
            .install(&source_path, dest_dir, strategy, relative)?;

        Ok(DownloadResult {
            path: dest_dir.to_path_buf(),
            from_cache: false,
            skipped: false,
        })
    }

    /// Extract an archive to destination
    async fn extract_archive(&self, archive_path: &Path, dest_dir: &Path) -> Result<()> {
        let _permit = self.extraction_semaphore.acquire().await.map_err(|error| {
            RiffError::InstallationFailed(format!("Archive extraction scheduler failed: {error}"))
        })?;
        let archive_path = archive_path.to_path_buf();
        let dest_dir = dest_dir.to_path_buf();

        tokio::task::spawn_blocking(move || {
            Self::extract_archive_blocking(&archive_path, &dest_dir)
        })
        .await
        .map_err(|error| {
            RiffError::InstallationFailed(format!("Archive extraction task failed: {error}"))
        })?
    }

    fn extract_archive_blocking(archive_path: &Path, dest_dir: &Path) -> Result<()> {
        // Clean destination if it exists
        if dest_dir.exists() {
            std::fs::remove_dir_all(dest_dir)?;
        }
        std::fs::create_dir_all(dest_dir)?;

        ArchiveExtractor::extract(archive_path, dest_dir)
    }

    /// Get the path where a package should be installed
    fn package_path(&self, package: &Package) -> PathBuf {
        self.config.vendor_dir.join(&package.name)
    }

    /// Get the cache path for a package archive
    fn cache_path(&self, package: &Package, archive_type: &str) -> PathBuf {
        let safe_name = package.name.replace('/', "-");
        let filename = format!("{}-{}.{}", safe_name, package.version, archive_type);
        self.config
            .cache_dir
            .join("files")
            .join(&package.name)
            .join(filename)
    }

    /// Determine if source should be used for a package
    fn should_use_source(&self, package: &Package) -> bool {
        self.available_sources(package, None).first() == Some(&DownloadSource::Source)
    }

    fn preference_for(&self, package: &Package) -> DownloadPreference {
        if let Some((_, preference)) = self
            .preferences
            .iter()
            .find(|(pattern, _)| package_pattern_matches(pattern, &package.name))
        {
            return *preference;
        }
        if self.config.prefer_source {
            DownloadPreference::Source
        } else if self.config.prefer_dist {
            DownloadPreference::Dist
        } else {
            DownloadPreference::Auto
        }
    }

    /// Remove a package
    pub async fn remove(&self, package: &Package) -> Result<()> {
        if package.is_metapackage() {
            return Ok(());
        }
        let dest_dir = self.package_path(package);

        if dest_dir.exists() {
            tokio::fs::remove_dir_all(&dest_dir).await?;
        }
        if package
            .source
            .as_ref()
            .is_some_and(|source| source.source_type == "fossil")
        {
            let repository = dest_dir.with_extension("fossil");
            if repository.exists() {
                tokio::fs::remove_file(repository).await?;
            }
        }

        Ok(())
    }

    /// Update a package (remove old, install new)
    pub async fn update(&self, old: &Package, new: &Package) -> Result<DownloadResult> {
        self.guard_source_removal(old)?;
        // Remove old package
        self.remove(old).await?;

        // Download new package
        self.download(new).await
    }

    fn guard_source_removal(&self, package: &Package) -> Result<()> {
        if package.installation_source.as_deref() != Some("source") {
            return Ok(());
        }
        let Some(source) = &package.source else {
            return Ok(());
        };
        let path = self.package_path(package);
        if source.source_type == "git"
            && path.exists()
            && GitDownloader::is_git_repo(&path)
            && GitDownloader::has_local_changes(&path)?
        {
            return Err(RiffError::InstallationFailed(
                "Source directory has uncommitted changes.".to_owned(),
            ));
        }
        Ok(())
    }
}

fn invalid_installed_source(package: &Package, source: &str) -> RiffError {
    RiffError::DownloadFailed {
        package: package.name.clone(),
        reason: format!("Package is not correctly installed from {source}"),
    }
}

fn is_source_type(package_type: &str) -> bool {
    matches!(
        package_type,
        "git" | "hg" | "mercurial" | "svn" | "fossil" | "perforce" | "p4"
    )
}

fn package_pattern_matches(pattern: &str, package: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let package = package.to_ascii_lowercase();
    let (mut pattern_index, mut package_index) = (0usize, 0usize);
    let (mut wildcard, mut retry) = (None, 0usize);
    let pattern = pattern.as_bytes();
    let package = package.as_bytes();
    while package_index < package.len() {
        if pattern.get(pattern_index) == package.get(package_index) {
            pattern_index += 1;
            package_index += 1;
        } else if pattern.get(pattern_index) == Some(&b'*') {
            wildcard = Some(pattern_index);
            pattern_index += 1;
            retry = package_index;
        } else if let Some(index) = wildcard {
            pattern_index = index + 1;
            retry += 1;
            package_index = retry;
        } else {
            return false;
        }
    }
    pattern[pattern_index..].iter().all(|byte| *byte == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Command;
    use tempfile::TempDir;

    fn test_manager(
        directory: &TempDir,
        prefer_source: bool,
        prefer_dist: bool,
    ) -> DownloadManager {
        DownloadManager::new(
            Arc::new(HttpClient::new().unwrap()),
            DownloadConfig {
                base_dir: directory.path().to_path_buf(),
                cache_dir: directory.path().join("cache"),
                vendor_dir: directory.path().join("vendor"),
                prefer_source,
                prefer_dist,
            },
        )
    }

    fn zip_bytes(path: &str, contents: &[u8]) -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        archive
            .start_file(path, zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(contents).unwrap();
        archive.finish().unwrap().into_inner()
    }

    fn serve_once(status: &str, body: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!("http://{address}/package.zip");
        let status = status.to_owned();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).unwrap();
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        });
        (url, server)
    }

    fn git(directory: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn source_repository(filename: &str, contents: &str) -> TempDir {
        let repository = TempDir::new().unwrap();
        git(repository.path(), &["init", "--quiet"]);
        git(
            repository.path(),
            &["config", "user.email", "test@example.com"],
        );
        git(repository.path(), &["config", "user.name", "Test"]);
        git(repository.path(), &["config", "commit.gpgsign", "false"]);
        fs::write(repository.path().join(filename), contents).unwrap();
        git(repository.path(), &["add", filename]);
        git(repository.path(), &["commit", "--quiet", "-m", "fixture"]);
        repository
    }

    fn both_sources(name: &str, version: &str) -> Package {
        let mut package = Package::new(name, version);
        package.source = Some(Source::git("/does/not/exist", "HEAD"));
        package.dist = Some(Dist::zip("http://127.0.0.1:9/package.zip"));
        package
    }

    #[test]
    fn test_download_config_default() {
        let config = DownloadConfig::default();
        assert!(config.prefer_dist);
        assert!(!config.prefer_source);
    }

    #[test]
    fn test_package_path() {
        let client = Arc::new(HttpClient::new().unwrap());
        let config = DownloadConfig {
            vendor_dir: PathBuf::from("/app/vendor"),
            ..Default::default()
        };
        let manager = DownloadManager::new(client, config);

        let package = Package::new("vendor/package", "1.0.0");
        let path = manager.package_path(&package);

        assert_eq!(path, PathBuf::from("/app/vendor/vendor/package"));
    }

    #[test]
    fn test_cache_path() {
        let client = Arc::new(HttpClient::new().unwrap());
        let config = DownloadConfig {
            cache_dir: PathBuf::from("/cache"),
            ..Default::default()
        };
        let manager = DownloadManager::new(client, config);

        let package = Package::new("vendor/package", "1.0.0");
        let path = manager.cache_path(&package, "zip");

        assert_eq!(
            path,
            PathBuf::from("/cache/files/vendor/package/vendor-package-1.0.0.zip")
        );
    }

    #[test]
    fn composer_download_manager_rejects_missing_installed_source() {
        let package = Package::new("vendor/package", "1.0.0");
        assert!(matches!(
            DownloadManager::installed_download_source(&package),
            Err(RiffError::DownloadFailed { reason, .. })
                if reason == "Package has no installation source"
        ));
    }

    #[test]
    fn composer_download_manager_resolves_installed_dist_downloader() {
        let mut package = Package::new("vendor/package", "1.0.0");
        package.installation_source = Some("dist".into());
        package.dist = Some(Dist::zip("https://example.test/package.zip"));
        assert_eq!(
            DownloadManager::installed_download_source(&package).unwrap(),
            Some(DownloadSource::Dist)
        );
    }

    #[test]
    fn composer_download_manager_rejects_dist_with_source_downloader_type() {
        let mut package = Package::new("vendor/package", "1.0.0");
        package.installation_source = Some("dist".into());
        package.dist = Some(Dist::new("git", "https://example.test/package.git"));
        assert!(DownloadManager::installed_download_source(&package).is_err());
    }

    #[test]
    fn composer_download_manager_resolves_installed_source_downloader() {
        let mut package = Package::new("vendor/package", "1.0.0");
        package.installation_source = Some("source".into());
        package.source = Some(Source::git("https://example.test/package.git", "HEAD"));
        assert_eq!(
            DownloadManager::installed_download_source(&package).unwrap(),
            Some(DownloadSource::Source)
        );
    }

    #[test]
    fn composer_download_manager_rejects_source_with_dist_downloader_type() {
        let mut package = Package::new("vendor/package", "1.0.0");
        package.installation_source = Some("source".into());
        package.source = Some(Source::new(
            "zip",
            "https://example.test/package.zip",
            "reference",
        ));
        assert!(DownloadManager::installed_download_source(&package).is_err());
    }

    #[test]
    fn composer_download_manager_has_no_downloader_for_installed_metapackage() {
        let mut package = Package::new("vendor/package", "1.0.0");
        package.package_type = "metapackage".into();
        assert_eq!(
            DownloadManager::installed_download_source(&package).unwrap(),
            None
        );
    }

    #[test]
    fn test_should_use_source_dev() {
        let client = Arc::new(HttpClient::new().unwrap());
        let config = DownloadConfig {
            prefer_dist: false,
            ..DownloadConfig::default()
        };
        let manager = DownloadManager::new(client, config);

        let mut package = Package::new("vendor/package", "dev-main");
        package.source = Some(Source::git(
            "https://github.com/vendor/package.git",
            "abc123",
        ));

        assert!(manager.should_use_source(&package));
    }

    #[test]
    fn test_should_use_source_prefer_source() {
        let client = Arc::new(HttpClient::new().unwrap());
        let config = DownloadConfig {
            prefer_source: true,
            prefer_dist: false,
            ..Default::default()
        };
        let manager = DownloadManager::new(client, config);

        let mut package = Package::new("vendor/package", "1.0.0");
        package.source = Some(Source::git(
            "https://github.com/vendor/package.git",
            "abc123",
        ));

        assert!(manager.should_use_source(&package));
    }

    #[tokio::test]
    async fn composer_file_downloader_rejects_package_without_dist_reference() {
        let client = Arc::new(HttpClient::new().unwrap());
        let manager = DownloadManager::new(client, DownloadConfig::default());
        let package = Package::new("dummy/pkg", "1.0.0");

        let result = manager.download(&package).await;

        assert!(matches!(
            result,
            Err(RiffError::DownloadFailed { package, reason })
                if package == "dummy/pkg" && reason == "No source or dist available"
        ));
    }

    #[tokio::test]
    async fn composer_download_manager_rejects_package_without_source_or_dist() {
        let client = Arc::new(HttpClient::new().unwrap());
        let manager = DownloadManager::new(client, DownloadConfig::default());
        let package = Package::new("dummy/pkg", "1.0.0");

        let result = manager.download(&package).await;

        assert!(matches!(
            result,
            Err(RiffError::DownloadFailed { package, reason })
                if package == "dummy/pkg" && reason == "No source or dist available"
        ));
    }

    #[test]
    fn composer_download_manager_automatically_prefers_source_for_dev_packages() {
        let client = Arc::new(HttpClient::new().unwrap());
        let manager = DownloadManager::new(
            client,
            DownloadConfig {
                prefer_dist: false,
                prefer_source: false,
                ..Default::default()
            },
        );
        let mut package = Package::new("dummy/pkg", "dev-main");
        package.source = Some(Source::git("https://example.test/dummy.git", "reference"));
        package.dist = Some(Dist::zip("https://example.test/dummy.zip"));

        assert!(manager.should_use_source(&package));
    }

    #[test]
    fn composer_download_manager_automatically_prefers_dist_for_stable_packages() {
        let client = Arc::new(HttpClient::new().unwrap());
        let manager = DownloadManager::new(
            client,
            DownloadConfig {
                prefer_dist: false,
                prefer_source: false,
                ..Default::default()
            },
        );
        let mut package = Package::new("dummy/pkg", "1.0.0");
        package.source = Some(Source::git("https://example.test/dummy.git", "reference"));
        package.dist = Some(Dist::zip("https://example.test/dummy.zip"));

        assert!(!manager.should_use_source(&package));
    }

    fn preferred_source(
        name: &str,
        version: &str,
        preferences: Vec<(String, DownloadPreference)>,
    ) -> DownloadSource {
        let directory = TempDir::new().unwrap();
        let manager = test_manager(&directory, false, false).with_preferences(preferences);
        manager.available_sources(&both_sources(name, version), None)[0]
    }

    #[test]
    fn composer_download_manager_uses_auto_for_unmatched_dev_package() {
        assert_eq!(
            preferred_source(
                "bar/package",
                "dev-main",
                vec![("foo/*".into(), DownloadPreference::Source)]
            ),
            DownloadSource::Source
        );
    }

    #[test]
    fn composer_download_manager_uses_auto_for_unmatched_stable_package() {
        assert_eq!(
            preferred_source(
                "bar/package",
                "1.0.0",
                vec![("foo/*".into(), DownloadPreference::Source)]
            ),
            DownloadSource::Dist
        );
    }

    #[test]
    fn composer_download_manager_matching_auto_prefers_source_for_dev() {
        assert_eq!(
            preferred_source(
                "foo/package",
                "dev-main",
                vec![("foo/*".into(), DownloadPreference::Auto)]
            ),
            DownloadSource::Source
        );
    }

    #[test]
    fn composer_download_manager_matching_auto_prefers_dist_for_stable() {
        assert_eq!(
            preferred_source(
                "foo/package",
                "1.0.0",
                vec![("foo/*".into(), DownloadPreference::Auto)]
            ),
            DownloadSource::Dist
        );
    }

    #[test]
    fn composer_download_manager_matching_source_overrides_auto() {
        assert_eq!(
            preferred_source(
                "foo/package",
                "1.0.0",
                vec![("foo/*".into(), DownloadPreference::Source)]
            ),
            DownloadSource::Source
        );
    }

    #[test]
    fn composer_download_manager_matching_dist_overrides_auto() {
        assert_eq!(
            preferred_source(
                "foo/package",
                "dev-main",
                vec![("foo/*".into(), DownloadPreference::Dist)]
            ),
            DownloadSource::Dist
        );
    }

    #[test]
    fn composer_download_manager_updates_stick_to_compatible_installed_source() {
        let directory = TempDir::new().unwrap();
        let manager = test_manager(&directory, false, false);
        let cases = [
            (
                Some(("source", "1.0.0")),
                "1.1.0",
                true,
                true,
                vec![DownloadSource::Source, DownloadSource::Dist],
            ),
            (
                Some(("dist", "1.0.0")),
                "1.1.0",
                true,
                true,
                vec![DownloadSource::Dist, DownloadSource::Source],
            ),
            (
                Some(("source", "1.0.0")),
                "1.1.0",
                false,
                true,
                vec![DownloadSource::Dist],
            ),
            (
                Some(("dist", "1.0.0")),
                "1.1.0",
                true,
                false,
                vec![DownloadSource::Source],
            ),
            (
                Some(("source", "1.0.0")),
                "dev-main",
                true,
                true,
                vec![DownloadSource::Source, DownloadSource::Dist],
            ),
            (
                Some(("dist", "1.0.0")),
                "dev-main",
                true,
                true,
                vec![DownloadSource::Source, DownloadSource::Dist],
            ),
            (
                None,
                "dev-main",
                true,
                true,
                vec![DownloadSource::Source, DownloadSource::Dist],
            ),
            (None, "dev-main", false, true, vec![DownloadSource::Dist]),
            (None, "dev-main", true, false, vec![DownloadSource::Source]),
            (
                None,
                "1.0.0",
                true,
                true,
                vec![DownloadSource::Dist, DownloadSource::Source],
            ),
            (None, "1.0.0", false, true, vec![DownloadSource::Dist]),
            (None, "1.0.0", true, false, vec![DownloadSource::Source]),
        ];

        for (previous, target_version, has_source, has_dist, expected) in cases {
            let mut target = Package::new("vendor/package", target_version);
            target.source = has_source.then(|| Source::git("/source", "HEAD"));
            target.dist = has_dist.then(|| Dist::zip("http://example.test/dist.zip"));
            let previous = previous.map(|(source, version)| {
                let mut package = Package::new("vendor/package", version);
                package.installation_source = Some(source.into());
                package
            });
            assert_eq!(
                manager.available_sources(&target, previous.as_ref()),
                expected,
                "previous={previous:?}, target={target_version}"
            );
        }
    }

    #[tokio::test]
    async fn composer_download_manager_downloads_dist_and_reuses_cache() {
        let directory = TempDir::new().unwrap();
        let (url, server) = serve_once("200 OK", zip_bytes("dist.txt", b"dist"));
        let manager = test_manager(&directory, false, true);
        let mut package = Package::new("vendor/package", "1.0.0");
        package.source = Some(Source::git("/unused/source", "HEAD"));
        package.dist = Some(Dist::zip(url));

        let first = manager.download(&package).await.unwrap();
        assert!(!first.from_cache);
        assert_eq!(fs::read(first.path.join("dist.txt")).unwrap(), b"dist");
        server.join().unwrap();

        fs::remove_dir_all(&first.path).unwrap();
        let second = manager.download(&package).await.unwrap();
        assert!(second.from_cache);
        assert_eq!(fs::read(second.path.join("dist.txt")).unwrap(), b"dist");
    }

    async fn assert_dist_to_source_fallback() {
        let directory = TempDir::new().unwrap();
        let source = source_repository("source.txt", "source");
        let (url, server) = serve_once("404 Not Found", Vec::new());
        let manager = test_manager(&directory, false, true).with_source_fallback(true);
        let mut package = Package::new("vendor/package", "1.0.0");
        package.dist = Some(Dist::zip(url));
        package.source = Some(Source::git(
            source.path().to_string_lossy().as_ref(),
            "HEAD",
        ));

        let result = manager.download(&package).await.unwrap();
        assert_eq!(fs::read(result.path.join("source.txt")).unwrap(), b"source");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn composer_download_manager_full_package_failover_uses_source() {
        assert_dist_to_source_fallback().await;
    }

    #[tokio::test]
    async fn composer_download_manager_downloads_dist_only_package() {
        let directory = TempDir::new().unwrap();
        let (url, server) = serve_once("200 OK", zip_bytes("dist.txt", b"dist"));
        let manager = test_manager(&directory, false, true);
        let mut package = Package::new("vendor/package", "1.0.0");
        package.dist = Some(Dist::zip(url));
        let result = manager.download(&package).await.unwrap();
        assert!(result.path.join("dist.txt").is_file());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn composer_download_manager_downloads_source_only_package() {
        let directory = TempDir::new().unwrap();
        let source = source_repository("source.txt", "source");
        let manager = test_manager(&directory, false, true);
        let mut package = Package::new("vendor/package", "1.0.0");
        package.source = Some(Source::git(
            source.path().to_string_lossy().as_ref(),
            "HEAD",
        ));
        let result = manager.download(&package).await.unwrap();
        assert!(result.path.join("source.txt").is_file());
    }

    #[tokio::test]
    async fn composer_download_manager_skips_metapackage_download() {
        let directory = TempDir::new().unwrap();
        let manager = test_manager(&directory, false, true);
        let mut package = both_sources("vendor/meta", "1.0.0");
        package.package_type = "metapackage".into();
        let result = manager.download(&package).await.unwrap();
        assert!(result.skipped);
        assert!(!result.path.exists());
    }

    #[tokio::test]
    async fn composer_download_manager_prefer_source_selects_source() {
        let directory = TempDir::new().unwrap();
        let source = source_repository("source.txt", "source");
        let manager = test_manager(&directory, true, false);
        let mut package = Package::new("vendor/package", "1.0.0");
        package.source = Some(Source::git(
            source.path().to_string_lossy().as_ref(),
            "HEAD",
        ));
        package.dist = Some(Dist::zip("http://127.0.0.1:9/unused.zip"));
        let result = manager.download(&package).await.unwrap();
        assert!(result.path.join("source.txt").is_file());
    }

    #[test]
    fn composer_download_manager_prefer_source_still_selects_dist_only_package() {
        let directory = TempDir::new().unwrap();
        let manager = test_manager(&directory, true, false);
        let mut package = Package::new("vendor/package", "1.0.0");
        package.dist = Some(Dist::zip("http://example.test/package.zip"));
        assert_eq!(
            manager.available_sources(&package, None),
            vec![DownloadSource::Dist]
        );
    }

    #[test]
    fn composer_download_manager_prefer_source_selects_source_only_package() {
        let directory = TempDir::new().unwrap();
        let manager = test_manager(&directory, true, false);
        let mut package = Package::new("vendor/package", "1.0.0");
        package.source = Some(Source::git("/source", "HEAD"));
        assert_eq!(
            manager.available_sources(&package, None),
            vec![DownloadSource::Source]
        );
    }

    #[tokio::test]
    async fn composer_download_manager_prefer_source_rejects_package_without_sources() {
        let directory = TempDir::new().unwrap();
        let manager = test_manager(&directory, true, false);
        let package = Package::new("vendor/package", "1.0.0");
        assert!(manager.download(&package).await.is_err());
    }

    #[tokio::test]
    async fn composer_download_manager_does_not_fallback_from_dist_when_disabled() {
        let directory = TempDir::new().unwrap();
        let source = source_repository("source.txt", "source");
        let (url, server) = serve_once("404 Not Found", Vec::new());
        let manager = test_manager(&directory, false, true);
        let mut package = Package::new("vendor/package", "1.0.0");
        package.dist = Some(Dist::zip(url));
        package.source = Some(Source::git(
            source.path().to_string_lossy().as_ref(),
            "HEAD",
        ));
        assert!(manager.download(&package).await.is_err());
        assert!(!directory
            .path()
            .join("vendor/vendor/package/source.txt")
            .exists());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn composer_download_manager_falls_back_from_dist_when_enabled() {
        assert_dist_to_source_fallback().await;
    }

    #[tokio::test]
    async fn composer_download_manager_falls_back_from_source_to_dist_by_default() {
        let directory = TempDir::new().unwrap();
        let (url, server) = serve_once("200 OK", zip_bytes("dist.txt", b"dist"));
        let manager = test_manager(&directory, true, false);
        let mut package = Package::new("vendor/package", "1.0.0");
        package.source = Some(Source::git("/does/not/exist", "HEAD"));
        package.dist = Some(Dist::zip(url));
        let result = manager.download(&package).await.unwrap();
        assert!(result.path.join("dist.txt").is_file());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn download_only_caches_dist_without_extracting_vendor() {
        let directory = TempDir::new().unwrap();
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        archive
            .start_file("fixture.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"fixture").unwrap();
        let archive = archive.finish().unwrap().into_inner();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                archive.len()
            )
            .unwrap();
            stream.write_all(&archive).unwrap();
        });

        let manager = DownloadManager::new(
            Arc::new(HttpClient::new().unwrap()),
            DownloadConfig {
                base_dir: directory.path().to_path_buf(),
                cache_dir: directory.path().join("cache"),
                vendor_dir: directory.path().join("vendor"),
                ..Default::default()
            },
        );
        let mut package = Package::new("fixture/package", "1.0.0");
        package.dist = Some(Dist::new("zip", format!("http://{address}/fixture.zip")));

        let first = manager.download_only(&package).await.unwrap();
        assert!(!first.from_cache);
        assert!(first.path.is_file());
        assert!(!directory.path().join("vendor").exists());

        let second = manager.download_only(&package).await.unwrap();
        assert!(second.from_cache);
        assert_eq!(second.path, first.path);
        server.join().unwrap();
    }

    async fn update_from_installed_fixture(old_type: &str) {
        let directory = TempDir::new().unwrap();
        let manager = test_manager(&directory, false, true);
        let mut old = Package::new("vendor/package", "1.0.0");
        old.installation_source = Some("dist".into());
        old.dist = Some(Dist::new(old_type, "http://example.test/old"));
        let installed = directory.path().join("vendor/vendor/package");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join("old.txt"), "old").unwrap();

        let (url, server) = serve_once("200 OK", zip_bytes("new.txt", b"new"));
        let mut new = Package::new("vendor/package", "2.0.0");
        new.installation_source = Some("dist".into());
        new.dist = Some(Dist::zip(url));

        let result = manager.update(&old, &new).await.unwrap();
        assert!(!result.path.join("old.txt").exists());
        assert_eq!(fs::read(result.path.join("new.txt")).unwrap(), b"new");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn composer_download_manager_updates_equal_dist_types() {
        update_from_installed_fixture("zip").await;
    }

    #[tokio::test]
    async fn composer_download_manager_updates_different_downloader_types() {
        update_from_installed_fixture("tar").await;
    }

    fn initialize_installed_git_repository(path: &Path, dirty: bool) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init", "--quiet"]);
        git(path, &["config", "user.email", "test@example.com"]);
        git(path, &["config", "user.name", "Test"]);
        git(path, &["config", "commit.gpgsign", "false"]);
        fs::write(path.join("tracked.txt"), "tracked").unwrap();
        git(path, &["add", "tracked.txt"]);
        git(path, &["commit", "--quiet", "-m", "installed"]);
        if dirty {
            fs::write(path.join("uncommitted.txt"), "do not lose").unwrap();
        }
    }

    #[tokio::test]
    async fn composer_download_manager_runs_removal_guard_when_type_changes() {
        let directory = TempDir::new().unwrap();
        let manager = test_manager(&directory, false, true);
        let installed = directory.path().join("vendor/vendor/package");
        initialize_installed_git_repository(&installed, false);
        let mut old = Package::new("vendor/package", "dev-main");
        old.installation_source = Some("source".into());
        old.source = Some(Source::git("/source", "HEAD"));

        let (url, server) = serve_once("200 OK", zip_bytes("dist.txt", b"dist"));
        let mut new = Package::new("vendor/package", "1.0.0");
        new.installation_source = Some("dist".into());
        new.dist = Some(Dist::zip(url));
        let result = manager.update(&old, &new).await.unwrap();
        assert!(result.path.join("dist.txt").is_file());
        assert!(!result.path.join(".git").exists());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn composer_download_manager_preserves_dirty_source_when_guard_aborts() {
        let directory = TempDir::new().unwrap();
        let manager = test_manager(&directory, false, true);
        let installed = directory.path().join("vendor/vendor/package");
        initialize_installed_git_repository(&installed, true);
        let mut old = Package::new("vendor/package", "dev-main");
        old.installation_source = Some("source".into());
        old.source = Some(Source::git("/source", "HEAD"));
        let mut new = Package::new("vendor/package", "1.0.0");
        new.installation_source = Some("dist".into());
        new.dist = Some(Dist::zip("http://127.0.0.1:9/unused.zip"));

        let error = manager.update(&old, &new).await.unwrap_err();
        assert!(error.to_string().contains("uncommitted changes"));
        assert_eq!(
            fs::read_to_string(installed.join("uncommitted.txt")).unwrap(),
            "do not lose"
        );
    }

    #[tokio::test]
    async fn composer_download_manager_updates_metapackage_without_files() {
        let directory = TempDir::new().unwrap();
        let manager = test_manager(&directory, false, true);
        let mut old = Package::new("vendor/meta", "1.0.0");
        old.package_type = "metapackage".into();
        let mut new = Package::new("vendor/meta", "2.0.0");
        new.package_type = "metapackage".into();
        let result = manager.update(&old, &new).await.unwrap();
        assert!(result.skipped);
        assert!(!result.path.exists());
    }

    #[tokio::test]
    async fn composer_download_manager_removes_installed_package_directory() {
        let directory = TempDir::new().unwrap();
        let manager = test_manager(&directory, false, true);
        let package = Package::new("vendor/package", "1.0.0");
        let installed = directory.path().join("vendor/vendor/package");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join("file.txt"), "content").unwrap();
        manager.remove(&package).await.unwrap();
        assert!(!installed.exists());
    }

    #[tokio::test]
    async fn composer_download_manager_metapackage_remove_is_noop() {
        let directory = TempDir::new().unwrap();
        let manager = test_manager(&directory, false, true);
        let mut package = Package::new("vendor/meta", "1.0.0");
        package.package_type = "metapackage".into();
        let sentinel = directory.path().join("vendor/vendor/meta/sentinel.txt");
        fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        fs::write(&sentinel, "sentinel").unwrap();
        manager.remove(&package).await.unwrap();
        assert!(sentinel.exists());
    }

    #[tokio::test]
    async fn relative_path_dist_is_resolved_from_project_directory() {
        let project = TempDir::new().unwrap();
        let source = project.path().join("custom/plugins/FixturePlugin");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("composer.json"), "{}").unwrap();

        let client = Arc::new(HttpClient::new().unwrap());
        let manager = DownloadManager::new(
            client,
            DownloadConfig {
                base_dir: project.path().to_path_buf(),
                vendor_dir: project.path().join("vendor"),
                ..Default::default()
            },
        );
        let destination = project.path().join("vendor/vendor/fixture-plugin");

        manager
            .download_from_path(
                &Package::new("vendor/fixture-plugin", "dev-main"),
                &Dist::path("custom/plugins/FixturePlugin"),
                &destination,
            )
            .await
            .unwrap();

        assert_eq!(
            destination.canonicalize().unwrap(),
            source.canonicalize().unwrap()
        );
    }
}
