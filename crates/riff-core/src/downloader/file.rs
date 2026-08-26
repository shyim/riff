//! File downloader for HTTP/HTTPS archives.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use riff_semver::Comparator;
use sha1::{Digest, Sha1};

use crate::cache::Cache;
use crate::http::HttpClient;
use crate::{Result, RiffError};

use super::archive::{ArchiveExtractor, ArchiveType};
use super::checksum::{verify_checksum, ChecksumType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDownloadRequest {
    original_url: String,
    processed_url: String,
    custom_cache_key: Option<String>,
}

impl FileDownloadRequest {
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            processed_url: url.clone(),
            original_url: url,
            custom_cache_key: None,
        }
    }

    pub fn with_processed_url(mut self, url: impl Into<String>) -> Self {
        self.processed_url = url.into();
        self
    }

    pub fn with_custom_cache_key(mut self, key: impl Into<String>) -> Self {
        self.custom_cache_key = Some(key.into());
        self
    }

    pub fn original_url(&self) -> &str {
        &self.original_url
    }

    pub fn processed_url(&self) -> &str {
        &self.processed_url
    }

    pub fn cache_key(&self, package_name: &str) -> String {
        let material = self
            .custom_cache_key
            .as_deref()
            .unwrap_or(&self.processed_url);
        let digest = Sha1::digest(material.as_bytes());
        format!("{package_name}/{digest:x}.")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileUpdateDirection {
    Upgrade,
    Downgrade,
}

impl FileUpdateDirection {
    pub const fn progress_verb(self) -> &'static str {
        match self {
            Self::Upgrade => "Upgrading",
            Self::Downgrade => "Downgrading",
        }
    }
}

/// File downloader for HTTP archives
pub struct FileDownloader {
    http_client: Arc<HttpClient>,
}

impl FileDownloader {
    /// Create a new file downloader
    pub fn new(http_client: Arc<HttpClient>) -> Self {
        Self { http_client }
    }

    pub fn new_with_cache_maintenance(
        http_client: Arc<HttpClient>,
        cache: &Cache,
        ttl: Duration,
        max_size: u64,
    ) -> Result<Self> {
        Self::garbage_collect_cache(cache, ttl, max_size)?;
        Ok(Self::new(http_client))
    }

    /// Download a file to the specified path
    pub async fn download<F>(&self, url: &str, dest: &Path, progress: Option<F>) -> Result<()>
    where
        F: Fn(u64, u64),
    {
        if dest.is_dir() {
            return Err(RiffError::DownloadFailed {
                package: url.to_owned(),
                reason: format!("download destination '{}' is a directory", dest.display()),
            });
        }
        let parent = dest.parent().unwrap_or_else(|| Path::new("."));
        if parent.exists() && !parent.is_dir() {
            return Err(RiffError::DownloadFailed {
                package: url.to_string(),
                reason: format!(
                    "download destination '{}' exists and is not a directory",
                    parent.display()
                ),
            });
        }
        tokio::fs::create_dir_all(parent).await?;
        let temporary = tempfile::Builder::new()
            .prefix(".riff-download-")
            .suffix(".part")
            .tempfile_in(parent)?
            .into_temp_path()
            .keep()
            .map_err(|error| error.error)?;

        let result = self.http_client.download(url, &temporary, progress).await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(RiffError::DownloadFailed {
                package: url.to_string(),
                reason: error.to_string(),
            });
        }
        Self::ensure_saved(&temporary, url)?;
        match tokio::fs::rename(&temporary, dest).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                tokio::fs::remove_file(dest).await?;
                tokio::fs::rename(&temporary, dest).await?;
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(error.into());
            }
        }
        Self::ensure_saved(dest, url)
    }

    pub async fn download_request<F>(
        &self,
        request: &FileDownloadRequest,
        dest: &Path,
        progress: Option<F>,
    ) -> Result<()>
    where
        F: Fn(u64, u64),
    {
        self.download(request.processed_url(), dest, progress).await
    }

    pub async fn download_into_directory<F>(
        &self,
        url: &str,
        directory: &Path,
        file_name: &str,
        progress: Option<F>,
    ) -> Result<PathBuf>
    where
        F: Fn(u64, u64),
    {
        if directory.exists() && !directory.is_dir() {
            return Err(RiffError::DownloadFailed {
                package: url.to_owned(),
                reason: format!(
                    "download path '{}' exists and is not a directory",
                    directory.display()
                ),
            });
        }
        tokio::fs::create_dir_all(directory).await?;
        let destination = directory.join(file_name);
        self.download(url, &destination, progress).await?;
        Ok(destination)
    }

    pub fn temporary_download_path(vendor_dir: &Path, url: &str) -> Result<PathBuf> {
        let directory = vendor_dir.join("composer");
        std::fs::create_dir_all(&directory)?;
        let suffix = url::Url::parse(url)
            .ok()
            .and_then(|url| {
                Path::new(url.path())
                    .extension()
                    .map(|extension| format!(".{}", extension.to_string_lossy()))
            })
            .unwrap_or_default();
        let path = tempfile::Builder::new()
            .prefix("tmp-")
            .suffix(&suffix)
            .tempfile_in(directory)?
            .into_temp_path()
            .keep()
            .map_err(|error| error.error)?;
        Ok(path)
    }

    pub fn ensure_saved(path: &Path, url: &str) -> Result<()> {
        if path.is_file() {
            Ok(())
        } else {
            Err(RiffError::DownloadFailed {
                package: url.to_owned(),
                reason: format!("file could not be saved to '{}'", path.display()),
            })
        }
    }

    pub fn garbage_collect_cache(cache: &Cache, ttl: Duration, max_size: u64) -> Result<u64> {
        Ok(cache.gc_with_max_size(ttl, max_size)?)
    }

    pub fn update_direction(old_version: &str, new_version: &str) -> FileUpdateDirection {
        if Comparator::greater_than(old_version, new_version) {
            FileUpdateDirection::Downgrade
        } else {
            FileUpdateDirection::Upgrade
        }
    }

    /// Download and verify checksum
    pub async fn download_verified<F>(
        &self,
        url: &str,
        dest: &Path,
        expected_checksum: &str,
        progress: Option<F>,
    ) -> Result<()>
    where
        F: Fn(u64, u64),
    {
        // Download the file
        self.download(url, dest, progress).await?;

        // Verify checksum
        let checksum_type =
            ChecksumType::from_hex_length(expected_checksum.len()).ok_or_else(|| {
                RiffError::ChecksumMismatch {
                    package: url.to_string(),
                }
            })?;

        let valid = verify_checksum(dest, expected_checksum, checksum_type).await?;

        if !valid {
            // Remove the downloaded file
            let _ = tokio::fs::remove_file(dest).await;
            return Err(RiffError::ChecksumMismatch {
                package: url.to_string(),
            });
        }

        Ok(())
    }

    /// Download and extract an archive
    pub async fn download_and_extract<F>(
        &self,
        url: &str,
        dest_dir: &Path,
        expected_checksum: Option<&str>,
        progress: Option<F>,
    ) -> Result<()>
    where
        F: Fn(u64, u64),
    {
        // Determine archive type from URL
        let archive_type = ArchiveType::from_path(Path::new(url)).unwrap_or(ArchiveType::Zip);

        // Create temp file for download
        let temp_dir = tempfile::tempdir()?;
        let temp_file = temp_dir.path().join(format!(
            "download.{}",
            match archive_type {
                ArchiveType::Zip => "zip",
                ArchiveType::Tar => "tar",
                ArchiveType::TarGz => "tar.gz",
                ArchiveType::TarBz2 => "tar.bz2",
                ArchiveType::TarXz => "tar.xz",
            }
        ));

        // Download
        if let Some(checksum) = expected_checksum {
            self.download_verified(url, &temp_file, checksum, progress)
                .await?;
        } else {
            self.download(url, &temp_file, progress).await?;
        }

        // Extract
        ArchiveExtractor::extract(&temp_file, dest_dir)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn serve_once(body: &'static [u8]) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });
        (format!("http://{address}/archive.zip"), handle)
    }

    #[tokio::test]
    async fn test_file_downloader_creation() {
        let client = Arc::new(HttpClient::new().unwrap());
        let _downloader = FileDownloader::new(client);
    }

    #[tokio::test]
    async fn composer_file_downloader_rejects_invalid_checksum_and_removes_file() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("archive.zip");
        let (url, server) = serve_once(b"downloaded archive");
        let downloader = FileDownloader::new(Arc::new(HttpClient::new().unwrap()));

        let result = downloader
            .download_verified(
                &url,
                &destination,
                "0000000000000000000000000000000000000000",
                None::<fn(u64, u64)>,
            )
            .await;
        server.join().unwrap();

        assert!(matches!(result, Err(RiffError::ChecksumMismatch { .. })));
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn composer_file_downloader_rejects_existing_file_as_download_directory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("package");
        fs::write(&path, "existing file").unwrap();
        let downloader = FileDownloader::new(Arc::new(HttpClient::new().unwrap()));

        let error = downloader
            .download_into_directory(
                "http://example.test/script.js",
                &path,
                "script.js",
                None::<fn(u64, u64)>,
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("exists and is not a directory"));
        assert_eq!(fs::read_to_string(path).unwrap(), "existing file");
    }

    #[test]
    fn composer_file_downloader_allocates_temporary_name_under_vendor_composer() {
        let temp = tempfile::tempdir().unwrap();
        let vendor = temp.path().join("vendor");
        let path =
            FileDownloader::temporary_download_path(&vendor, "http://example.test/script.js")
                .unwrap();

        assert_eq!(path.parent(), Some(vendor.join("composer").as_path()));
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("tmp-"));
        assert!(name.ends_with(".js"));
        assert!(path.is_file());
    }

    #[test]
    fn composer_file_downloader_reports_when_download_was_not_saved() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("script.js");

        let error =
            FileDownloader::ensure_saved(&missing, "http://example.test/script.js").unwrap_err();

        assert!(error.to_string().contains("could not be saved to"));
        assert!(error.to_string().contains("script.js"));
    }

    #[tokio::test]
    async fn composer_file_downloader_uses_processed_url_and_derived_cache_key() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("script.js");
        let (processed_url, server) = serve_once(b"processed");
        let request = FileDownloadRequest::new("http://example.test/original.js")
            .with_processed_url(&processed_url);
        let downloader = FileDownloader::new(Arc::new(HttpClient::new().unwrap()));

        downloader
            .download_request(&request, &destination, None::<fn(u64, u64)>)
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(request.original_url(), "http://example.test/original.js");
        assert_eq!(request.processed_url(), processed_url);
        assert_eq!(fs::read(destination).unwrap(), b"processed");
        assert_eq!(
            request.cache_key("dummy/pkg"),
            format!("dummy/pkg/{:x}.", Sha1::digest(processed_url.as_bytes()))
        );
    }

    #[test]
    fn composer_file_downloader_uses_custom_cache_key() {
        let request = FileDownloadRequest::new("http://example.test/original.js")
            .with_custom_cache_key("xyzzy");

        assert_eq!(
            request.cache_key("dummy/pkg"),
            format!("dummy/pkg/{:x}.", Sha1::digest(b"xyzzy"))
        );
    }

    #[test]
    fn composer_file_downloader_runs_configured_cache_garbage_collection() {
        let temp = tempfile::tempdir().unwrap();
        let cache = Cache::new(temp.path().to_path_buf());
        cache.write("expired.zip", b"archive").unwrap();

        let _downloader = FileDownloader::new_with_cache_maintenance(
            Arc::new(HttpClient::new().unwrap()),
            &cache,
            Duration::MAX,
            0,
        )
        .unwrap();

        assert!(!cache.has("expired.zip"));
    }

    #[test]
    fn composer_file_downloader_labels_semver_downgrades() {
        let direction = FileDownloader::update_direction("1.2.0", "1.0.0");
        assert_eq!(direction, FileUpdateDirection::Downgrade);
        assert_eq!(direction.progress_verb(), "Downgrading");
    }

    #[tokio::test]
    #[ignore] // Requires network access
    async fn test_download_file() {
        use tempfile::TempDir;

        let client = Arc::new(HttpClient::new().unwrap());
        let downloader = FileDownloader::new(client);

        let temp_dir = TempDir::new().unwrap();
        let dest = temp_dir.path().join("test.bin");

        let result = downloader
            .download("https://httpbin.org/bytes/100", &dest, None::<fn(u64, u64)>)
            .await;

        assert!(result.is_ok());
        assert!(dest.exists());
    }
}
