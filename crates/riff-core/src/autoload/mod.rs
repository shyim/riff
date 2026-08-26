//! Autoload generation for PHP packages.
//!
//! This module generates the vendor/autoload.php and related files
//! that enable automatic class loading in PHP.

mod classmap;
mod generator;

pub use classmap::ClassMapGenerator;
pub use generator::{
    AutoloadConfig, AutoloadGenerationEvent, AutoloadGenerationResult, AutoloadGenerator,
    PackageAutoload, PlatformCheckRequirements, RootPackageInfo,
};

use std::path::Path;

/// Get the current git HEAD commit hash.
pub fn get_head_commit(path: &Path) -> Option<String> {
    let git_dir = path.join(".git");
    if !git_dir.exists() {
        return None;
    }

    let head_path = git_dir.join("HEAD");
    if !head_path.exists() {
        return None;
    }

    let head_content = std::fs::read_to_string(head_path).ok()?;
    let head = head_content.trim();

    if let Some(stripped) = head.strip_prefix("ref: ") {
        // Reference to another file
        let ref_path = git_dir.join(stripped);
        if ref_path.exists() {
            let ref_content = std::fs::read_to_string(ref_path).ok()?;
            Some(ref_content.trim().to_owned())
        } else {
            let packed_refs = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
            packed_refs.lines().find_map(|line| {
                let (reference, name) = line.split_once(' ')?;
                (name == stripped).then(|| reference.to_owned())
            })
        }
    } else {
        // Detached HEAD or hash
        Some(head.to_owned())
    }
}
