use std::time::Duration;

use anyhow::Result;
use riff_core::cache::{runtime_cache_dir, Cache};
use riff_core::config::Config;

#[derive(usage_rs::Args, Debug)]
pub struct ClearCacheArgs {
    /// Only run garbage collection, preserving fresh cache entries
    #[usage(long)]
    pub gc: bool,

    /// Treat the cache as disabled for this invocation
    #[usage(long)]
    pub no_cache: bool,
}

pub fn execute(args: ClearCacheArgs) -> Result<i32> {
    let root = runtime_cache_dir();
    if args.no_cache {
        riff_core::outln!("Cache is not enabled: {}", root.display());
        return Ok(0);
    }

    if args.gc {
        garbage_collect(&root)?;
        riff_core::successln!("All caches garbage-collected.");
    } else {
        if root.is_dir() {
            Cache::new(root).clear()?;
        }
        riff_core::successln!("All caches cleared.");
    }
    Ok(0)
}

fn garbage_collect(root: &std::path::Path) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    let config = Config::default();
    let general_ttl = Duration::from_secs(config.cache_ttl);
    let file_ttl = Duration::from_secs(config.cache_files_ttl.unwrap_or(config.cache_ttl));

    let files = root.join("files");
    if files.is_dir() {
        Cache::new(files).gc_with_max_size(file_ttl, config.cache_files_maxsize)?;
    }
    let repositories = root.join("repo");
    if repositories.is_dir() {
        Cache::new(repositories).gc(general_ttl)?;
    }
    let vcs = root.join("vcs");
    if vcs.is_dir() {
        Cache::new(vcs).gc_vcs(general_ttl)?;
    }
    Ok(())
}
