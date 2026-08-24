//! Repository cache with HTTP metadata support (Last-Modified, ETag)

use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::Cache;

/// Cache entry metadata stored alongside cached content
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMetadata {
    /// HTTP Last-Modified header value
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    /// HTTP ETag header value
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_sha256: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_modified_ns: Option<u64>,
}

impl Default for CacheMetadata {
    fn default() -> Self {
        Self {
            last_modified: None,
            etag: None,
            content_sha256: None,
            content_length: None,
            content_modified_ns: None,
        }
    }
}

/// Repository cache that stores metadata alongside cached content
///
/// This cache stores two files for each entry:
/// - `<key>` - The actual cached content
/// - `<key>.meta` - JSON metadata (Last-Modified, ETag, etc.)
pub struct RepoCache {
    /// Underlying filesystem cache
    cache: Cache,
}

impl RepoCache {
    /// Create a new repository cache
    ///
    /// # Arguments
    /// * `cache_dir` - Base cache directory
    /// * `repo_url` - Repository URL (used to create unique cache subdirectory)
    pub fn new(cache_dir: PathBuf, repo_url: &str) -> Self {
        // Sanitize repo URL to create cache subdirectory
        let sanitized = Self::sanitize_url(repo_url);
        let cache_path = cache_dir.join("repo").join(sanitized);

        Self {
            cache: Cache::new(cache_path),
        }
    }

    /// Sanitize a URL for use as a directory name
    fn sanitize_url(url: &str) -> String {
        // Remove protocol
        let url = url
            .trim_start_matches("https://")
            .trim_start_matches("http://");

        let mut sanitized = String::with_capacity(url.len());
        for character in url.chars() {
            if character.is_ascii_alphanumeric() {
                sanitized.push(character.to_ascii_lowercase());
            } else {
                sanitized.push('-');
            }
        }
        sanitized
    }

    /// Set read-only mode
    pub fn set_read_only(&mut self, read_only: bool) {
        self.cache.set_read_only(read_only);
    }

    /// Check if cache is enabled
    pub fn is_enabled(&self) -> bool {
        self.cache.is_enabled()
    }

    /// Check if cache is read-only
    pub fn is_read_only(&self) -> bool {
        self.cache.is_read_only()
    }

    /// Get the metadata key for a cache key
    fn meta_key(key: &str) -> String {
        format!("{}.meta", key)
    }

    /// Read cached content with metadata
    ///
    /// # Returns
    /// Tuple of (content, metadata) if cached, None otherwise
    pub fn read(&self, key: &str) -> io::Result<Option<(Vec<u8>, CacheMetadata)>> {
        // Read main content
        let content = match self.cache.read(key)? {
            Some(c) => c,
            None => return Ok(None),
        };

        // Read metadata (optional)
        let metadata = match self.cache.read(&Self::meta_key(key))? {
            Some(meta_bytes) => serde_json::from_slice(&meta_bytes).unwrap_or_default(),
            None => CacheMetadata::default(),
        };

        Ok(Some((content, metadata)))
    }

    /// Read an auxiliary cache entry without looking for HTTP metadata.
    pub fn read_data(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        self.cache.read(key)
    }

    /// Read only the metadata for a cache key
    pub fn read_metadata(&self, key: &str) -> io::Result<Option<CacheMetadata>> {
        match self.cache.read(&Self::meta_key(key))? {
            Some(meta_bytes) => {
                let metadata: CacheMetadata =
                    serde_json::from_slice(&meta_bytes).unwrap_or_default();
                Ok(Some(metadata))
            }
            None => Ok(None),
        }
    }

    /// Write content with metadata to cache
    pub fn write(&self, key: &str, content: &[u8], metadata: &CacheMetadata) -> io::Result<()> {
        // Write main content
        self.cache.write(key, content)?;

        self.write_metadata_for_content(key, content, metadata)
    }

    pub fn write_metadata_for_content(
        &self,
        key: &str,
        content: &[u8],
        metadata: &CacheMetadata,
    ) -> io::Result<()> {
        let Some((content_length, modified)) = self.cache.identity(key)? else {
            return Ok(());
        };
        let mut metadata = metadata.clone();
        metadata.content_sha256 = Some(Sha256::digest(content).into());
        metadata.content_length = Some(content_length);
        metadata.content_modified_ns = modified_ns(modified);

        // Write metadata
        let meta_bytes = serde_json::to_vec(&metadata)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.cache.write(&Self::meta_key(key), &meta_bytes)?;

        Ok(())
    }

    pub fn fresh_content_sha256(
        &self,
        key: &str,
        max_age: Duration,
    ) -> io::Result<Option<[u8; 32]>> {
        let Some((content_length, modified)) = self.cache.identity(key)? else {
            return Ok(None);
        };
        if SystemTime::now()
            .duration_since(modified)
            .map_or(true, |age| age >= max_age)
        {
            return Ok(None);
        }

        let Some(metadata) = self.read_metadata(key)? else {
            return Ok(None);
        };
        if metadata.content_length != Some(content_length)
            || metadata.content_modified_ns != modified_ns(modified)
        {
            return Ok(None);
        }

        Ok(metadata.content_sha256)
    }

    /// Write an auxiliary cache entry without HTTP metadata.
    pub fn write_data(&self, key: &str, content: &[u8]) -> io::Result<()> {
        self.cache.write(key, content)
    }

    /// Check if a cached entry exists
    pub fn has(&self, key: &str) -> bool {
        self.cache.has(key)
    }

    /// Get age of cached entry
    pub fn age(&self, key: &str) -> io::Result<Option<Duration>> {
        self.cache.age(key)
    }

    /// Remove a cached entry
    pub fn remove(&self, key: &str) -> io::Result<()> {
        self.cache.remove(key)?;
        self.cache.remove(&Self::meta_key(key))?;
        Ok(())
    }

    /// Clear all cached entries
    pub fn clear(&self) -> io::Result<()> {
        self.cache.clear()
    }

    /// Garbage collect old entries
    pub fn gc(&self, ttl: Duration) -> io::Result<u64> {
        self.cache.gc(ttl)
    }

    /// Get SHA256 hash of cached content
    pub fn sha256(&self, key: &str) -> io::Result<Option<String>> {
        self.cache.sha256(key)
    }
}

fn modified_ns(modified: SystemTime) -> Option<u64> {
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_nanos()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_repo_cache_new() {
        let temp = TempDir::new().unwrap();
        let cache = RepoCache::new(temp.path().to_path_buf(), "https://repo.packagist.org");

        assert!(cache.is_enabled());
        assert!(!cache.is_read_only());
    }

    #[test]
    fn test_repo_cache_write_read() {
        let temp = TempDir::new().unwrap();
        let cache = RepoCache::new(temp.path().to_path_buf(), "https://repo.packagist.org");

        let content = b"test content";
        let metadata = CacheMetadata {
            last_modified: Some("Wed, 24 Dec 2025 10:00:00 GMT".to_string()),
            etag: None,
            ..CacheMetadata::default()
        };

        cache.write("test-key", content, &metadata).unwrap();

        let (read_content, read_metadata) = cache.read("test-key").unwrap().unwrap();
        assert_eq!(read_content, content);
        assert_eq!(read_metadata.last_modified, metadata.last_modified);
    }

    #[test]
    fn test_repo_cache_read_metadata_only() {
        let temp = TempDir::new().unwrap();
        let cache = RepoCache::new(temp.path().to_path_buf(), "https://repo.packagist.org");

        let metadata = CacheMetadata {
            last_modified: Some("Wed, 24 Dec 2025 10:00:00 GMT".to_string()),
            etag: Some("\"abc123\"".to_string()),
            ..CacheMetadata::default()
        };

        cache.write("test-key", b"content", &metadata).unwrap();

        let read_metadata = cache.read_metadata("test-key").unwrap().unwrap();
        assert_eq!(read_metadata.last_modified, metadata.last_modified);
        assert_eq!(read_metadata.etag, metadata.etag);
    }

    #[test]
    fn test_content_hash_requires_matching_file_identity() {
        let temp = TempDir::new().unwrap();
        let cache = RepoCache::new(temp.path().to_path_buf(), "https://repo.packagist.org");
        let metadata = CacheMetadata::default();

        cache.write("test-key", b"original", &metadata).unwrap();
        assert_eq!(
            cache
                .fresh_content_sha256("test-key", Duration::MAX)
                .unwrap(),
            Some(Sha256::digest(b"original").into())
        );

        cache.cache.write("test-key", b"changed content").unwrap();
        assert!(cache
            .fresh_content_sha256("test-key", Duration::MAX)
            .unwrap()
            .is_none());

        cache
            .write_metadata_for_content("test-key", b"changed content", &metadata)
            .unwrap();
        assert_eq!(
            cache
                .fresh_content_sha256("test-key", Duration::MAX)
                .unwrap(),
            Some(Sha256::digest(b"changed content").into())
        );
        assert!(cache
            .fresh_content_sha256("test-key", Duration::ZERO)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_sanitize_url() {
        assert_eq!(
            RepoCache::sanitize_url("https://repo.packagist.org"),
            "repo-packagist-org"
        );
        assert_eq!(
            RepoCache::sanitize_url("https://packages.example.com/composer"),
            "packages-example-com-composer"
        );
        assert_eq!(
            RepoCache::sanitize_url("http://PACKAGES.example.com/a--b?x=1"),
            "packages-example-com-a--b-x-1"
        );
        assert_eq!(
            RepoCache::sanitize_url("https://café.example"),
            "caf--example"
        );
    }
}
