use std::collections::BTreeMap;
use std::path::Path;

use async_trait::async_trait;

use crate::package::Package;
use crate::Result;

/// Internal extension point for native Composer plugins that need to modify an
/// extracted package before its binaries and autoload metadata are installed.
#[async_trait]
pub(crate) trait PackageInstallHook: Send + Sync {
    async fn after_install(&self, package: &Package, install_path: &Path) -> Result<()>;

    fn fingerprints(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
}

/// Observes packages after extraction and patching have completed. Observers
/// must defer failures until their results are consumed by the caller.
pub(crate) trait PackageInstallObserver: Send + Sync {
    fn package_ready(&self, package: &Package, install_path: &Path);
}
