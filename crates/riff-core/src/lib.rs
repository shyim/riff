pub mod advisory;
pub mod archive;
pub mod autoload;
pub mod cache;
pub mod config;
pub mod dependency_graph;
pub mod downloader;
pub mod error;
pub mod event;
pub mod filesystem;
pub mod filter_list;
pub mod http;
pub mod installer;
pub mod json;
pub mod output;
pub mod package;
pub mod patch;
pub mod platform;
pub mod plugin;
pub mod policy_config;
pub mod process;
pub mod repository;
pub mod riff;
pub mod runtime;
pub mod scripts;
pub mod session;
pub mod solver;
pub mod url_utils;
pub mod util;

pub use autoload::{AutoloadConfig, AutoloadGenerator};
pub use dependency_graph::{
    find_packages_with_replacers_and_providers, get_dependents, query_dependencies,
    query_dependencies_with_candidates, DependencyQuery, DependencyQueryError,
    DependencyQueryResult, DependencyResult,
};
pub use downloader::{DownloadManager, DownloadResult};
pub use error::{Result, RiffError};
pub use event::{
    EventDispatcher, EventListener, EventType, PostAutoloadDumpEvent, PostInstallEvent,
    PostUpdateEvent, PreAutoloadDumpEvent, PreInstallEvent, PreUpdateEvent, RiffEvent,
};
pub use installer::{InstallConfig, InstallationManager};
pub use json::{RiffLockfile, RiffManifest};
pub use output::{
    AnsiMode, Output, OutputEvent, OutputLevel, OutputMode, OutputOptions, OutputSink, OutputStream,
};
pub use package::Package;
pub use platform::{Platform, PlatformSnapshot};
pub use plugin::{BinConfig, PackageOperation, PluginManager};
pub use repository::{Repository, RepositoryManager};
pub use riff::{Riff, RiffBuilder};
pub use runtime::RuntimeContext;
pub use session::{
    BatchOptions, ProjectInstallRequest, ProjectInstallResult, RiffSession, RiffSessionBuilder,
};
pub use solver::{Policy, Pool, Request, Solver, Transaction};
pub use util::{compute_content_hash, is_platform_package};

#[cfg(test)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};

    static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) fn environment_lock() -> MutexGuard<'static, ()> {
        ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) struct EnvironmentGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvironmentGuard {
        pub(crate) fn set(key: &'static str, value: Option<&str>) -> Self {
            let previous = std::env::var_os(key);
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}

#[cfg(test)]
mod test_content_hash;
