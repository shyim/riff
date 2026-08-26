//! First-class package patching.

mod author;
mod compat;
mod engine;
mod native;
mod state;

pub use author::{
    begin_patch_edit, cleanup_patch_edit, commit_patch_edit, ensure_applied_patch_state_current,
    read_patch_edit, remove_native_patches, PatchCommitResult, PatchEdit, PatchRemoveResult,
};
pub use compat::{relock_compatibility, CompatibilityRelockResult};
pub use engine::{apply_patch, create_patch, PatchApplyError};
pub use native::{
    native_declarations, read_native_lock, relock_native, validate_native_patch_path,
    NativePatchDeclaration, NativePatchLock, NativePatchLockEntry, NATIVE_PATCH_LOCK_FILE,
};
pub use state::{
    changed_patch_packages, invalidate_applied_patch_state, read_applied_patch_state,
    write_applied_patch_state,
};

pub(crate) use compat::prepare;

pub async fn desired_patch_fingerprints(
    composer: &crate::Riff,
    packages: &[crate::Package],
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    compat::desired_fingerprints(composer, packages).await
}
