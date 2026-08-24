//! Package downloading and extraction module.
//!
//! This module provides functionality for downloading packages from
//! various sources (HTTP archives, Git repositories, local paths) and extracting them.

mod archive;
mod checksum;
mod file;
mod git;
mod manager;
mod path;

pub use archive::{ArchiveExtractor, ArchiveType};
pub use checksum::{verify_checksum, ChecksumType};
pub use file::FileDownloader;
pub use git::GitDownloader;
pub use manager::{DownloadConfig, DownloadManager, DownloadResult};
pub use path::{PathDownloader, PathInstallResult, PathStrategy};
