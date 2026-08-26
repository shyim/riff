// Package model for Composer packages
//
// This module provides structs and types for representing Composer packages,
// including dependencies, autoload configuration, source/dist information, etc.

mod alias;
mod autoload;
mod convert;
mod dumper;
mod link;
mod loader;
#[allow(clippy::module_inception)]
mod package;
mod pattern;
mod root_version;
mod security;
mod source;
pub mod version_bumper;

pub use alias::{
    branch_alias_is_valid, parse_branch_aliases, parse_inline_alias, AliasPackage,
    DEFAULT_BRANCH_ALIAS,
};
pub use autoload::{Autoload, AutoloadPath};
pub use dumper::dump_package;
pub use link::{Link, LinkType};
pub use loader::{load_package_config, load_package_config_with_options, PackageLoadOptions};
pub use package::{
    package_type, Abandoned, ArchiveConfig, Author, DependencyMap, Funding, Package, ScriptHandler,
    Scripts, Stability, Support,
};
pub use pattern::{package_name_matches, package_names_to_regex};
pub use root_version::{
    detect_root_version, detect_root_version_with_non_feature_branches, get_git_branch,
    RootVersion, RootVersionSource, SystemVersionGuessProcess, VersionGuess,
    VersionGuessCommandOutput, VersionGuessOptions, VersionGuessProcess, VersionGuesser,
    DEFAULT_ROOT_PRETTY_VERSION, DEFAULT_ROOT_VERSION,
};
pub use security::{validate_package_metadata, PackageMetadataError};
pub use source::{Dist, Mirror, Source};
