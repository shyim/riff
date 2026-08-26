//! Package installation system.
//!
//! This module handles the installation, update, and removal of packages
//! into the vendor directory.

mod binary;
mod dependency_policy;
mod hook;
#[allow(clippy::module_inception)]
mod installer;
mod library;
mod manager;
mod metapackage;
mod suggestions;

pub use binary::{BinaryCompatibility, BinaryInstaller};
pub use dependency_policy::{PackagePolicy, PolicyPhase, PolicyViolation};
pub(crate) use hook::{PackageInstallHook, PackageInstallObserver};
pub use installer::{
    DumpAutoloadOptions, InstallOptions, Installer, PlatformRequirementFilter, UpdateOptions,
    UpdateResult,
};
pub use library::LibraryInstaller;
pub use manager::{InstallConfig, InstallationManager};
pub use metapackage::{MetapackageInstaller, MetapackageResult};
pub use suggestions::{
    SuggestedPackage, SuggestedPackagesReporter, MODE_BY_PACKAGE, MODE_BY_SUGGESTION, MODE_LIST,
};
