pub mod autoload;
pub mod cache;
pub mod composer;
pub mod config;
pub mod dependency_graph;
pub mod downloader;
pub mod error;
pub mod event;
pub mod http;
pub mod installer;
pub mod json;
pub mod package;
pub mod platform;
pub mod plugin;
pub mod repository;
pub mod runtime;
pub mod scripts;
pub mod solver;
pub mod util;

pub use autoload::{AutoloadConfig, AutoloadGenerator};
pub use composer::{Composer, ComposerBuilder};
pub use dependency_graph::{
    find_packages_with_replacers_and_providers, get_dependents, DependencyResult,
};
pub use downloader::{DownloadManager, DownloadResult};
pub use error::{ComposerError, Result};
pub use event::{
    ComposerEvent, EventDispatcher, EventListener, EventType, PostAutoloadDumpEvent,
    PostInstallEvent, PostUpdateEvent, PreAutoloadDumpEvent, PreInstallEvent, PreUpdateEvent,
};
pub use installer::{InstallConfig, InstallationManager};
pub use json::{ComposerJson, ComposerLock};
pub use package::Package;
pub use platform::PlatformSnapshot;
pub use plugin::{register_plugins, BinConfig};
pub use repository::{Repository, RepositoryManager};
pub use runtime::RuntimeContext;
pub use solver::{Policy, Pool, Request, Solver, Transaction};
pub use util::{compute_content_hash, is_platform_package};
#[cfg(test)]
mod test_content_hash;
