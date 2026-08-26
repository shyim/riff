#[allow(clippy::module_inception)]
mod cache;
mod repo_cache;

use std::ffi::OsString;
use std::path::PathBuf;

pub use cache::Cache;
pub use repo_cache::{CacheMetadata, RepoCache};

/// Return the shared cache root used by Riff at runtime.
///
/// Composer cache configuration is intentionally not consulted: Riff keeps
/// its cached data in its own namespace. `RIFF_CACHE_DIR` can be used to move
/// that namespace without mixing it with Composer's cache.
pub fn runtime_cache_dir() -> PathBuf {
    let override_dir = std::env::var_os("RIFF_CACHE_DIR").filter(|value| !value.is_empty());
    let platform_dir = directories::ProjectDirs::from("", "", "riff")
        .map(|directories| directories.cache_dir().to_path_buf());

    select_runtime_cache_dir(override_dir, platform_dir)
}

fn select_runtime_cache_dir(
    override_dir: Option<OsString>,
    platform_dir: Option<PathBuf>,
) -> PathBuf {
    override_dir
        .map(PathBuf::from)
        .or(platform_dir)
        .unwrap_or_else(|| PathBuf::from(".riff/cache"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_riff_cache_directory_wins() {
        assert_eq!(
            select_runtime_cache_dir(
                Some(OsString::from("/custom/riff-cache")),
                Some(PathBuf::from("/platform/riff")),
            ),
            PathBuf::from("/custom/riff-cache")
        );
    }

    #[test]
    fn platform_cache_directory_is_the_default() {
        assert_eq!(
            select_runtime_cache_dir(None, Some(PathBuf::from("/platform/riff"))),
            PathBuf::from("/platform/riff")
        );
    }

    #[test]
    fn local_riff_cache_is_the_last_resort() {
        assert_eq!(
            select_runtime_cache_dir(None, None),
            PathBuf::from(".riff/cache")
        );
    }
}
