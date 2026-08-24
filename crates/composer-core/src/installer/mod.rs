//! Package installation system.
//!
//! This module handles the installation, update, and removal of packages
//! into the vendor directory.

mod binary;
mod installer;
mod library;
mod manager;
mod metapackage;

pub use binary::BinaryInstaller;
pub use installer::{InstallOptions, Installer, UpdateOptions, UpdateResult};
pub use library::LibraryInstaller;
pub use manager::{InstallConfig, InstallationManager};
pub use metapackage::{MetapackageInstaller, MetapackageResult};
