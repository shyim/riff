//! Checksum verification for downloaded files.

use md5::Md5;
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::path::Path;

use crate::Result;

/// Supported checksum types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumType {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
    Md5,
}

impl ChecksumType {
    /// Detect checksum type from length of hex string
    pub fn from_hex_length(len: usize) -> Option<Self> {
        match len {
            32 => Some(ChecksumType::Md5),
            40 => Some(ChecksumType::Sha1),
            64 => Some(ChecksumType::Sha256),
            96 => Some(ChecksumType::Sha384),
            128 => Some(ChecksumType::Sha512),
            _ => None,
        }
    }
}

/// Incremental archive checksum, shared by cached reads and streaming downloads.
pub(super) enum ChecksumHasher {
    Sha1(sha1::Sha1),
    Sha256(Sha256),
    Sha384(Sha384),
    Sha512(Sha512),
    Md5(Md5),
}

impl ChecksumHasher {
    pub(super) fn new(kind: ChecksumType) -> Self {
        match kind {
            ChecksumType::Sha1 => Self::Sha1(sha1::Sha1::new()),
            ChecksumType::Sha256 => Self::Sha256(Sha256::new()),
            ChecksumType::Sha384 => Self::Sha384(Sha384::new()),
            ChecksumType::Sha512 => Self::Sha512(Sha512::new()),
            ChecksumType::Md5 => Self::Md5(Md5::new()),
        }
    }

    pub(super) fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Sha1(hash) => hash.update(bytes),
            Self::Sha256(hash) => hash.update(bytes),
            Self::Sha384(hash) => hash.update(bytes),
            Self::Sha512(hash) => hash.update(bytes),
            Self::Md5(hash) => hash.update(bytes),
        }
    }

    pub(super) fn finish(self) -> String {
        match self {
            Self::Sha1(hash) => format!("{:x}", hash.finalize()),
            Self::Sha256(hash) => format!("{:x}", hash.finalize()),
            Self::Sha384(hash) => format!("{:x}", hash.finalize()),
            Self::Sha512(hash) => format!("{:x}", hash.finalize()),
            Self::Md5(hash) => format!("{:x}", hash.finalize()),
        }
    }
}

/// Verify checksum of a file without buffering the entire archive in memory.
pub async fn verify_checksum(
    path: &Path,
    expected: &str,
    checksum_type: ChecksumType,
) -> Result<bool> {
    Ok(compute_checksum(path, checksum_type)
        .await?
        .eq_ignore_ascii_case(expected))
}

async fn compute_checksum(path: &Path, checksum_type: ChecksumType) -> Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut buffer = vec![0; super::archive::ZIP_COPY_BUFFER_SIZE];
        let mut hasher = ChecksumHasher::new(checksum_type);
        loop {
            let read = match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            hasher.update(&buffer[..read]);
        }
        Ok(hasher.finish())
    })
    .await
    .map_err(|error| {
        crate::RiffError::InstallationFailed(format!("Checksum task failed: {error}"))
    })?
}

/// Compute SHA-256 checksum of a file.
#[allow(dead_code)]
pub async fn compute_sha256(path: &Path) -> Result<String> {
    compute_checksum(path, ChecksumType::Sha256).await
}

/// Compute SHA-1 checksum of a file.
#[allow(dead_code)]
pub async fn compute_sha1(path: &Path) -> Result<String> {
    compute_checksum(path, ChecksumType::Sha1).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn test_checksum_type_from_hex_length() {
        assert_eq!(ChecksumType::from_hex_length(32), Some(ChecksumType::Md5));
        assert_eq!(ChecksumType::from_hex_length(40), Some(ChecksumType::Sha1));
        assert_eq!(
            ChecksumType::from_hex_length(64),
            Some(ChecksumType::Sha256)
        );
        assert_eq!(
            ChecksumType::from_hex_length(96),
            Some(ChecksumType::Sha384)
        );
        assert_eq!(
            ChecksumType::from_hex_length(128),
            Some(ChecksumType::Sha512)
        );
        assert_eq!(ChecksumType::from_hex_length(50), None);
    }

    #[tokio::test]
    async fn test_verify_sha256() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        // Write test content
        let mut file = tokio::fs::File::create(path).await.unwrap();
        file.write_all(b"hello world").await.unwrap();
        file.flush().await.unwrap();
        drop(file);

        // SHA-256 of "hello world"
        let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

        let result = verify_checksum(path, expected, ChecksumType::Sha256).await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_verify_sha256_mismatch() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        let mut file = tokio::fs::File::create(path).await.unwrap();
        file.write_all(b"hello world").await.unwrap();
        file.flush().await.unwrap();
        drop(file);

        let wrong_hash = "0000000000000000000000000000000000000000000000000000000000000000";

        let result = verify_checksum(path, wrong_hash, ChecksumType::Sha256).await;
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn test_compute_sha256() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        let mut file = tokio::fs::File::create(path).await.unwrap();
        file.write_all(b"hello world").await.unwrap();
        file.flush().await.unwrap();
        drop(file);

        let hash = compute_sha256(path).await.unwrap();
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[tokio::test]
    async fn cached_checksums_match_known_vectors_across_buffer_boundaries() {
        let file = NamedTempFile::new().unwrap();
        let mut bytes: Vec<u8> = (0..262144).map(|index| (index % 256) as u8).collect();
        bytes.extend_from_slice(b"tail");
        std::fs::write(file.path(), bytes).unwrap();
        // Independently generated with Python hashlib for bytes(range(256))*1024+b"tail".
        for (kind, expected) in [
            (ChecksumType::Sha1, "94a0a2acae0b357ada003f336e466ac0224039ba"),
            (ChecksumType::Sha256, "145a2cf50dd668b2e895180d6455a0ef930907876cb5a53c36de2ceadb081d01"),
            (ChecksumType::Sha384, "9ededf14929c5cd099e65008dae525d4738cc1b06e81dcf8e37337a621a6e1537b0a1e85ecc31baeaa71727a50e6e0d7"),
            (ChecksumType::Sha512, "e10a0887e9e2e2db1ad38f2f0d47a756506441b7c0770c3b1fefbbc25ffaefe043235853c6df1b0f5845e400cee151ea2682caa73b1e9aa851e40ab23a4e1810"),
            (ChecksumType::Md5, "1eb50f84dbd78044c79bb5e19dd34270"),
        ] {
            assert!(verify_checksum(file.path(), &expected.to_uppercase(), kind).await.unwrap());
        }
    }
}
