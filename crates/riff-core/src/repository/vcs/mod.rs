//! VCS repository support - discovers packages from version control systems.
//!
//! This module provides repository implementations for:
//! - Generic VCS (auto-detect)
//! - Git repositories
//! - GitHub repositories (with API support)
//! - GitLab repositories (with API support)
//! - Bitbucket repositories (with API support)

mod bitbucket;
mod cli;
mod driver;
mod forgejo;
mod git;
mod github;
mod gitlab;
mod perforce;
mod repository;
mod svn;

pub use bitbucket::BitbucketDriver;
pub use driver::{VcsDist, VcsDriver, VcsDriverError, VcsInfo, VcsSource};
pub use forgejo::{ForgejoDriver, ForgejoRepositoryData, ForgejoUrl};
pub use git::{get_head_commit, GitDriver};
pub use github::{GitHubDriver, GitHubFundingLink, GitHubPrivateAccessStrategy};
pub use gitlab::{GitLabDist, GitLabDriver, GitLabProtocol, GitLabRequestOptions, GitLabSource};
pub use perforce::{
    Perforce, PerforceCommandOutput, PerforceConfig, PerforceCredentialUpdate, PerforceDriver,
    PerforceProcess, SystemPerforceProcess,
};
pub use repository::{VcsRepository, VcsType};
pub use svn::{Svn, SvnCommandOutput, SvnDriver, SvnProcess, SystemSvnProcess};
