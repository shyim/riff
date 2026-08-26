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
mod vcs;

pub use archive::{ArchiveExtractor, ArchiveType};
pub use checksum::{verify_checksum, ChecksumType};
pub use file::{FileDownloadRequest, FileDownloader, FileUpdateDirection};
pub use git::{
    strip_url_credentials, GitAuthentication, GitDownloader, GitProcess, GitProcessCommand,
    GitProcessOutput, GitRemoteExecutor, SystemGitProcess,
};
pub use manager::{DownloadConfig, DownloadManager, DownloadResult};
pub use path::{PathDownloader, PathInstallResult, PathStrategy};
pub use vcs::{
    PerforceCheckout, PerforceInstallPlan, PerforceSession, VcsCommandSpec, VcsDownloader,
};
