//! Installation manager - orchestrates package installation.

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::stream::{self, StreamExt};

use crate::cache::runtime_cache_dir;
use crate::downloader::{DownloadConfig, DownloadManager};
use crate::http::HttpClient;
use crate::output::Output;
use crate::package::Package;
use crate::plugin::manager::PackageLayouts;
use crate::solver::{Operation, Transaction};
use crate::Result;

use super::binary::{BinaryCompatibility, BinaryInstaller};
use super::hook::{PackageInstallHook, PackageInstallObserver};
use super::library::LibraryInstaller;
use super::metapackage::MetapackageInstaller;

/// Installation configuration
#[derive(Debug, Clone)]
pub struct InstallConfig {
    /// Project directory used to resolve project-relative paths
    pub base_dir: PathBuf,
    /// Vendor directory
    pub vendor_dir: PathBuf,
    /// Bin directory
    pub bin_dir: PathBuf,
    /// Composer-compatible binary proxy mode
    pub bin_compat: String,
    /// Cache directory
    pub cache_dir: PathBuf,
    /// Prefer source over dist
    pub prefer_source: bool,
    /// Prefer dist over source
    pub prefer_dist: bool,
    /// Run in dry-run mode (no actual changes)
    pub dry_run: bool,
    /// Skip dev dependencies
    pub no_dev: bool,
    /// Prefer lowest versions (useful for testing compatibility)
    pub prefer_lowest: bool,
    /// Prefer stable versions over unstable versions
    pub prefer_stable: bool,
    /// Download package data into Riff's cache without changing vendor
    pub download_only: bool,
}

impl Default for InstallConfig {
    fn default() -> Self {
        Self {
            base_dir: PathBuf::from("."),
            vendor_dir: PathBuf::from("vendor"),
            bin_dir: PathBuf::from("vendor/bin"),
            bin_compat: "auto".to_string(),
            cache_dir: runtime_cache_dir(),
            prefer_source: false,
            prefer_dist: true,
            dry_run: false,
            no_dev: false,
            prefer_lowest: false,
            prefer_stable: false,
            download_only: false,
        }
    }
}

// Keep enough package operations in flight to overlap network transfers with
// archive extraction. Extraction itself runs on Tokio's blocking pool.
const MAX_CONCURRENT_INSTALLS: usize = 64;

/// Installation manager
pub struct InstallationManager {
    library_installer: Arc<LibraryInstaller>,
    binary_installer: Arc<BinaryInstaller>,
    metapackage_installer: MetapackageInstaller,
    config: InstallConfig,
}

/// Result of an installation operation
#[derive(Debug)]
pub struct InstallResult {
    /// Packages that were installed
    pub installed: Vec<Package>,
    /// Packages that were updated (from, to)
    pub updated: Vec<(Package, Package)>,
    /// Packages reinstalled at the same identity.
    pub reinstalled: Vec<Package>,
    /// Packages that were removed
    pub removed: Vec<Package>,
    /// Binaries that were linked
    pub binaries: Vec<PathBuf>,
}

impl InstallationManager {
    /// Create a new installation manager
    pub fn new(http_client: Arc<HttpClient>, config: InstallConfig) -> Self {
        Self::new_with_output(http_client, config, Output::silent())
    }

    pub fn new_with_output(
        http_client: Arc<HttpClient>,
        config: InstallConfig,
        output: Output,
    ) -> Self {
        let download_config = DownloadConfig {
            base_dir: config.base_dir.clone(),
            vendor_dir: config.vendor_dir.clone(),
            cache_dir: config.cache_dir.clone(),
            prefer_source: config.prefer_source,
            prefer_dist: config.prefer_dist,
        };

        let download_manager = Arc::new(DownloadManager::new_with_output(
            http_client,
            download_config,
            output,
        ));

        let library_installer = Arc::new(LibraryInstaller::new(
            download_manager,
            config.vendor_dir.clone(),
        ));

        let binary_installer = Arc::new(BinaryInstaller::with_compatibility(
            config.bin_dir.clone(),
            config.vendor_dir.clone(),
            BinaryCompatibility::from_config(&config.bin_compat),
        ));

        let metapackage_installer = MetapackageInstaller::new();

        Self {
            library_installer,
            binary_installer,
            metapackage_installer,
            config,
        }
    }

    /// Execute a transaction (install/update/remove packages)
    pub async fn execute(&self, transaction: &Transaction) -> Result<InstallResult> {
        self.execute_with_hook(transaction, None).await
    }

    pub(crate) async fn execute_with_hook(
        &self,
        transaction: &Transaction,
        hook: Option<Arc<dyn PackageInstallHook>>,
    ) -> Result<InstallResult> {
        self.execute_with_layouts(transaction, hook, PackageLayouts::default())
            .await
    }

    pub(crate) async fn execute_with_layouts(
        &self,
        transaction: &Transaction,
        hook: Option<Arc<dyn PackageInstallHook>>,
        layouts: PackageLayouts,
    ) -> Result<InstallResult> {
        self.execute_with_layouts_and_observer(transaction, hook, layouts, None)
            .await
    }

    pub(crate) async fn execute_with_layouts_and_observer(
        &self,
        transaction: &Transaction,
        hook: Option<Arc<dyn PackageInstallHook>>,
        layouts: PackageLayouts,
        observer: Option<Arc<dyn PackageInstallObserver>>,
    ) -> Result<InstallResult> {
        let mut result = InstallResult {
            installed: Vec::new(),
            updated: Vec::new(),
            reinstalled: Vec::new(),
            removed: Vec::new(),
            binaries: Vec::new(),
        };

        if self.config.dry_run {
            // In dry-run mode, just collect what would be done
            for op in &transaction.operations {
                match op {
                    Operation::Install(pkg) => {
                        result.installed.push(pkg.as_ref().clone());
                    }
                    Operation::Update { from, to } => {
                        result
                            .updated
                            .push((from.as_ref().clone(), to.as_ref().clone()));
                    }
                    Operation::Reinstall(package) => {
                        result.reinstalled.push(package.as_ref().clone());
                    }
                    Operation::Uninstall(pkg) => {
                        result.removed.push(pkg.as_ref().clone());
                    }
                    Operation::MarkUnneeded(_) => {}
                    // Alias operations don't need any file system changes
                    Operation::MarkAliasInstalled(_) | Operation::MarkAliasUninstalled(_) => {}
                }
            }
            return Ok(result);
        }

        if self.config.download_only {
            let mut downloads = Vec::new();
            for op in &transaction.operations {
                match op {
                    Operation::Install(package) => {
                        result.installed.push(package.as_ref().clone());
                        if !package.is_platform_package() && !layouts.is_fileless(package) {
                            downloads.push(package.clone());
                        }
                    }
                    Operation::Update { from, to } => {
                        result
                            .updated
                            .push((from.as_ref().clone(), to.as_ref().clone()));
                        if !to.is_platform_package() && !layouts.is_fileless(to) {
                            downloads.push(to.clone());
                        }
                    }
                    Operation::Reinstall(package) => {
                        result.reinstalled.push(package.as_ref().clone());
                        if !package.is_platform_package() && !layouts.is_fileless(package) {
                            downloads.push(package.clone());
                        }
                    }
                    Operation::Uninstall(_)
                    | Operation::MarkUnneeded(_)
                    | Operation::MarkAliasInstalled(_)
                    | Operation::MarkAliasUninstalled(_) => {}
                }
            }

            let download_results: Vec<_> = stream::iter(downloads.iter())
                .map(|package| self.library_installer.download_only(package))
                .buffer_unordered(MAX_CONCURRENT_INSTALLS)
                .collect()
                .await;
            for download_result in download_results {
                download_result?;
            }
            return Ok(result);
        }

        // Create vendor directory
        tokio::fs::create_dir_all(&self.config.vendor_dir).await?;

        // Separate operations into phases for parallel execution:
        // 1. Uninstalls must happen first (sequential - usually few)
        // 2. Updates can be parallelized (remove old, install new)
        // 3. Installs can be parallelized

        let mut uninstalls = Vec::new();
        let mut updates = Vec::new();
        let mut installs = Vec::new();

        for op in &transaction.operations {
            match op {
                Operation::Uninstall(pkg) => {
                    if !pkg.is_platform_package() {
                        uninstalls.push(pkg.clone());
                    }
                }
                Operation::Update { from, to } => {
                    if !to.is_platform_package() {
                        updates.push((from.clone(), to.clone(), false));
                    }
                }
                Operation::Reinstall(package) => {
                    if !package.is_platform_package() {
                        updates.push((package.clone(), package.clone(), true));
                    }
                }
                Operation::Install(pkg) => {
                    if !pkg.is_platform_package() {
                        installs.push(pkg.clone());
                    }
                }
                Operation::MarkUnneeded(_)
                | Operation::MarkAliasInstalled(_)
                | Operation::MarkAliasUninstalled(_) => {}
            }
        }

        // Phase 1: Process uninstalls (sequential, usually few)
        for pkg in &uninstalls {
            if layouts.is_fileless(pkg) {
                self.metapackage_installer.uninstall(pkg).await?;
            } else {
                self.binary_installer.uninstall(pkg).await?;
                self.uninstall_package(pkg).await?;
            }
            result.removed.push(pkg.as_ref().clone());
        }

        // Phase 2: Process updates in parallel
        let update_results: Vec<_> = stream::iter(updates.iter())
            .map(|(from, to, reinstall)| {
                let library_installer = self.library_installer.clone();
                let binary_installer = self.binary_installer.clone();
                let hook = hook.clone();
                let observer = observer.clone();
                let layouts = layouts.clone();
                async move {
                    // Handle metapackage transitions
                    if layouts.is_fileless(to) {
                        if !layouts.is_fileless(from) {
                            binary_installer.uninstall(from).await?;
                            library_installer.uninstall(from).await?;
                        }
                        // Metapackages have no files to install
                        return Ok::<_, crate::RiffError>((
                            from.clone(),
                            to.clone(),
                            Vec::new(),
                            *reinstall,
                        ));
                    }

                    let download_result = if layouts.is_fileless(from) {
                        // Downgrading from metapackage to regular
                        library_installer.install(to).await?
                    } else {
                        // Regular update
                        let result = library_installer.update(from, to).await?;
                        binary_installer.uninstall(from).await?;
                        result
                    };
                    if !download_result.skipped {
                        if let Some(hook) = &hook {
                            let install_path = library_installer.get_install_path(to);
                            if let Err(error) = hook.after_install(to, &install_path).await {
                                if let Err(cleanup_error) = library_installer.uninstall(to).await {
                                    log::warn!(
                                        "Failed to clean up {} after patch failure: {}",
                                        to.name,
                                        cleanup_error
                                    );
                                }
                                return Err(error);
                            }
                        }
                        if let Some(observer) = &observer {
                            observer.package_ready(to, &library_installer.get_install_path(to));
                        }
                    }
                    let bins = binary_installer.install(to).await?;
                    Ok((from.clone(), to.clone(), bins, *reinstall))
                }
            })
            .buffer_unordered(MAX_CONCURRENT_INSTALLS)
            .collect()
            .await;

        for update_result in update_results {
            let (from, to, bins, reinstall) = update_result?;
            if reinstall {
                result.reinstalled.push(to.as_ref().clone());
            } else {
                result
                    .updated
                    .push((from.as_ref().clone(), to.as_ref().clone()));
            }
            result.binaries.extend(bins);
        }

        // Phase 3: Process installs in parallel
        let install_results: Vec<_> = stream::iter(installs.iter())
            .map(|pkg| {
                let library_installer = self.library_installer.clone();
                let binary_installer = self.binary_installer.clone();
                let hook = hook.clone();
                let observer = observer.clone();
                let layouts = layouts.clone();
                async move {
                    if layouts.is_fileless(pkg) {
                        // Metapackages have no files to install
                        return Ok::<_, crate::RiffError>((pkg.clone(), Vec::new()));
                    }

                    let download_result = library_installer.install(pkg).await?;
                    if !download_result.skipped {
                        if let Some(hook) = &hook {
                            let install_path = library_installer.get_install_path(pkg);
                            if let Err(error) = hook.after_install(pkg, &install_path).await {
                                if let Err(cleanup_error) = library_installer.uninstall(pkg).await {
                                    log::warn!(
                                        "Failed to clean up {} after patch failure: {}",
                                        pkg.name,
                                        cleanup_error
                                    );
                                }
                                return Err(error);
                            }
                        }
                        if let Some(observer) = &observer {
                            observer.package_ready(pkg, &library_installer.get_install_path(pkg));
                        }
                    }
                    let bins = binary_installer.install(pkg).await?;
                    Ok((pkg.clone(), bins))
                }
            })
            .buffer_unordered(MAX_CONCURRENT_INSTALLS)
            .collect()
            .await;

        for install_result in install_results {
            let (pkg, bins) = install_result?;
            result.installed.push(pkg.as_ref().clone());
            result.binaries.extend(bins);
        }

        Ok(result)
    }

    /// Uninstall a package
    async fn uninstall_package(&self, package: &Package) -> Result<()> {
        self.library_installer.uninstall(package).await
    }

    /// Install from a list of packages (without a transaction)
    pub async fn install_packages(&self, packages: &[Package]) -> Result<InstallResult> {
        self.install_packages_with_hook(packages, None).await
    }

    pub(crate) async fn install_packages_with_hook(
        &self,
        packages: &[Package],
        hook: Option<Arc<dyn PackageInstallHook>>,
    ) -> Result<InstallResult> {
        let mut result = InstallResult {
            installed: Vec::new(),
            updated: Vec::new(),
            reinstalled: Vec::new(),
            removed: Vec::new(),
            binaries: Vec::new(),
        };

        if self.config.dry_run {
            result.installed = packages.to_vec();
            return Ok(result);
        }

        if self.config.download_only {
            result.installed = packages
                .iter()
                .filter(|package| !package.is_platform_package())
                .cloned()
                .collect();
            let regular_packages: Vec<_> = packages
                .iter()
                .filter(|package| !package.is_platform_package() && !package.is_metapackage())
                .collect();
            let download_results: Vec<_> = stream::iter(regular_packages)
                .map(|package| self.library_installer.download_only(package))
                .buffer_unordered(MAX_CONCURRENT_INSTALLS)
                .collect()
                .await;
            for download_result in download_results {
                download_result?;
            }
            return Ok(result);
        }

        // Create vendor directory
        tokio::fs::create_dir_all(&self.config.vendor_dir).await?;

        // Filter out platform packages and separate metapackages
        let mut metapackages = Vec::new();
        let mut regular_packages = Vec::new();

        for package in packages {
            if package.is_platform_package() {
                continue;
            }
            if package.is_metapackage() {
                metapackages.push(package);
            } else {
                regular_packages.push(package);
            }
        }

        for package in metapackages {
            self.metapackage_installer.install(package).await?;
        }

        // Install regular packages in parallel
        let install_results: Vec<_> = stream::iter(regular_packages.iter())
            .map(|package| {
                let library_installer = self.library_installer.clone();
                let binary_installer = self.binary_installer.clone();
                let hook = hook.clone();
                async move {
                    let download_result = library_installer.install(package).await?;
                    if !download_result.skipped {
                        if let Some(hook) = &hook {
                            let install_path = library_installer.get_install_path(package);
                            if let Err(error) = hook.after_install(package, &install_path).await {
                                if let Err(cleanup_error) =
                                    library_installer.uninstall(package).await
                                {
                                    log::warn!(
                                        "Failed to clean up {} after patch failure: {}",
                                        package.name,
                                        cleanup_error
                                    );
                                }
                                return Err(error);
                            }
                        }
                    }
                    let bins = binary_installer.install(package).await?;
                    Ok::<_, crate::RiffError>(((*package).clone(), bins, download_result.skipped))
                }
            })
            .buffer_unordered(MAX_CONCURRENT_INSTALLS)
            .collect()
            .await;

        for install_result in install_results {
            let (pkg, bins, skipped) = install_result?;
            if !skipped {
                result.installed.push(pkg);
            }
            result.binaries.extend(bins);
        }

        Ok(result)
    }

    /// Get the config
    pub fn config(&self) -> &InstallConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    struct CountingHook {
        calls: AtomicUsize,
        fail: bool,
    }

    struct EditingHook;

    #[async_trait]
    impl PackageInstallHook for CountingHook {
        async fn after_install(
            &self,
            _package: &Package,
            _install_path: &std::path::Path,
        ) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(crate::RiffError::InstallationFailed(
                    "test hook failure".to_string(),
                ));
            }
            Ok(())
        }
    }

    #[async_trait]
    impl PackageInstallHook for EditingHook {
        async fn after_install(
            &self,
            _package: &Package,
            install_path: &std::path::Path,
        ) -> Result<()> {
            tokio::fs::write(install_path.join("file.txt"), "patched").await?;
            Ok(())
        }
    }

    fn test_manager(directory: &TempDir) -> InstallationManager {
        InstallationManager::new(
            Arc::new(HttpClient::new().unwrap()),
            InstallConfig {
                base_dir: directory.path().to_path_buf(),
                vendor_dir: directory.path().join("vendor"),
                bin_dir: directory.path().join("vendor/bin"),
                cache_dir: directory.path().join("cache"),
                ..InstallConfig::default()
            },
        )
    }

    #[test]
    fn test_install_config_default() {
        let config = InstallConfig::default();
        assert_eq!(config.vendor_dir, PathBuf::from("vendor"));
        assert_eq!(config.bin_dir, PathBuf::from("vendor/bin"));
        assert_eq!(config.bin_compat, "auto");
        assert!(config.prefer_dist);
        assert!(!config.prefer_source);
        assert!(!config.dry_run);
    }

    #[tokio::test]
    async fn test_installation_manager_creation() {
        let http_client = Arc::new(HttpClient::new().unwrap());
        let config = InstallConfig::default();
        let _manager = InstallationManager::new(http_client, config);
    }

    // Ported from Composer\Test\Installer\LibraryInstallerTest::
    // testInstallerCreationShouldNotCreateBinDirectory.
    #[test]
    fn composer_library_installer_creation_does_not_create_bin_directory() {
        let directory = TempDir::new().unwrap();
        let bin_dir = directory.path().join("bin");
        let _manager = InstallationManager::new(
            Arc::new(HttpClient::new().unwrap()),
            InstallConfig {
                vendor_dir: directory.path().join("vendor"),
                bin_dir: bin_dir.clone(),
                cache_dir: directory.path().join("cache"),
                ..InstallConfig::default()
            },
        );

        assert!(!bin_dir.exists());
    }

    #[tokio::test]
    async fn test_dry_run_install() {
        let http_client = Arc::new(HttpClient::new().unwrap());
        let config = InstallConfig {
            dry_run: true,
            ..Default::default()
        };
        let manager = InstallationManager::new(http_client, config);

        let packages = vec![
            Package::new("vendor/a", "1.0.0"),
            Package::new("vendor/b", "2.0.0"),
        ];

        let result = manager.install_packages(&packages).await.unwrap();
        assert_eq!(result.installed.len(), 2);
        assert!(result.updated.is_empty());
        assert!(result.removed.is_empty());
    }

    #[tokio::test]
    async fn download_only_does_not_create_vendor() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("composer.json"), r#"{"name":"vendor/package"}"#).unwrap();
        let manager = InstallationManager::new(
            Arc::new(HttpClient::new().unwrap()),
            InstallConfig {
                base_dir: directory.path().to_path_buf(),
                vendor_dir: directory.path().join("vendor"),
                bin_dir: directory.path().join("vendor/bin"),
                cache_dir: directory.path().join("cache"),
                download_only: true,
                ..InstallConfig::default()
            },
        );
        let mut package = Package::new("vendor/package", "1.0.0");
        package.dist = Some(crate::package::Dist::new(
            "path",
            source.to_string_lossy().as_ref(),
        ));

        let result = manager.install_packages(&[package]).await.unwrap();
        assert_eq!(result.installed.len(), 1);
        assert!(!directory.path().join("vendor").exists());
    }

    #[tokio::test]
    async fn install_hook_failure_removes_the_partial_package() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("file.txt"), "original").unwrap();
        let mut package = Package::new("vendor/package", "1.0.0");
        package.dist = Some(crate::package::Dist::path(
            source.to_string_lossy().into_owned(),
        ));
        let manager = test_manager(&directory);
        let hook = Arc::new(CountingHook {
            calls: AtomicUsize::new(0),
            fail: true,
        });

        let error = manager
            .install_packages_with_hook(&[package], Some(hook.clone()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("test hook failure"));
        assert_eq!(hook.calls.load(Ordering::SeqCst), 1);
        assert!(!directory.path().join("vendor/vendor/package").exists());
    }

    #[tokio::test]
    async fn install_hook_is_not_called_for_an_existing_package() {
        let directory = TempDir::new().unwrap();
        let install_path = directory.path().join("vendor/vendor/package");
        std::fs::create_dir_all(&install_path).unwrap();
        let package = Package::new("vendor/package", "1.0.0");
        let manager = test_manager(&directory);
        let hook = Arc::new(CountingHook {
            calls: AtomicUsize::new(0),
            fail: false,
        });

        let result = manager
            .install_packages_with_hook(&[package], Some(hook.clone()))
            .await
            .unwrap();
        assert!(result.installed.is_empty());
        assert_eq!(hook.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn reinstall_restores_pristine_files_before_running_the_hook() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("source");
        let install_path = directory.path().join("vendor/vendor/package");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&install_path).unwrap();
        std::fs::write(source.join("file.txt"), "pristine").unwrap();
        std::fs::write(install_path.join("file.txt"), "old patch").unwrap();

        let mut package = Package::new("vendor/package", "1.0.0");
        package.dist = Some(
            crate::package::Dist::path(source.to_string_lossy().into_owned())
                .with_transport_options(std::collections::HashMap::from([(
                    "symlink".to_string(),
                    serde_json::Value::Bool(false),
                )])),
        );
        let package = Arc::new(package);
        let mut transaction = Transaction::new();
        transaction.reinstall(package.clone());

        let result = test_manager(&directory)
            .execute_with_hook(&transaction, Some(Arc::new(EditingHook)))
            .await
            .unwrap();

        assert_eq!(result.reinstalled, [package.as_ref().clone()]);
        assert_eq!(
            std::fs::read_to_string(install_path.join("file.txt")).unwrap(),
            "patched"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("file.txt")).unwrap(),
            "pristine"
        );
    }

    // Ported from Composer\Test\Installer\InstallationManagerTest::testExecute.
    #[tokio::test]
    async fn composer_installation_manager_executes_each_transaction_operation_kind() {
        let manager = InstallationManager::new(
            Arc::new(HttpClient::new().unwrap()),
            InstallConfig {
                dry_run: true,
                ..InstallConfig::default()
            },
        );
        let installed = Arc::new(Package::new("vendor/install", "1.0.0"));
        let update_from = Arc::new(Package::new("vendor/update", "1.0.0"));
        let update_to = Arc::new(Package::new("vendor/update", "2.0.0"));
        let removed = Arc::new(Package::new("vendor/remove", "1.0.0"));
        let mut transaction = Transaction::new();
        transaction.install(installed.clone());
        transaction.update(update_from.clone(), update_to.clone());
        transaction.uninstall(removed.clone());

        let result = manager.execute(&transaction).await.unwrap();

        assert_eq!(result.installed, [installed.as_ref().clone()]);
        assert_eq!(
            result.updated,
            [(update_from.as_ref().clone(), update_to.as_ref().clone())]
        );
        assert_eq!(result.removed, [removed.as_ref().clone()]);
    }

    // Ported from Composer\Test\Installer\InstallationManagerTest::testInstall.
    #[tokio::test]
    async fn composer_installation_manager_routes_package_installs() {
        let manager = InstallationManager::new(
            Arc::new(HttpClient::new().unwrap()),
            InstallConfig {
                dry_run: true,
                ..InstallConfig::default()
            },
        );
        let package = Package::new("vendor/package", "1.0.0");

        let result = manager
            .install_packages(std::slice::from_ref(&package))
            .await
            .unwrap();

        assert_eq!(result.installed, [package]);
        assert!(result.updated.is_empty());
        assert!(result.removed.is_empty());
    }

    // Ported from Composer\Test\Installer\InstallationManagerTest::testUpdateWithEqualTypes.
    #[tokio::test]
    async fn composer_installation_manager_routes_same_type_updates() {
        let manager = InstallationManager::new(
            Arc::new(HttpClient::new().unwrap()),
            InstallConfig {
                dry_run: true,
                ..InstallConfig::default()
            },
        );
        let from = Arc::new(Package::new("vendor/package", "1.0.0"));
        let to = Arc::new(Package::new("vendor/package", "2.0.0"));
        let mut transaction = Transaction::new();
        transaction.update(from.clone(), to.clone());

        let result = manager.execute(&transaction).await.unwrap();

        assert_eq!(
            result.updated,
            [(from.as_ref().clone(), to.as_ref().clone())]
        );
    }

    // Ported from Composer\Test\Installer\InstallationManagerTest::testUpdateWithNotEqualTypes.
    #[tokio::test]
    async fn composer_installation_manager_handles_library_to_metapackage_updates() {
        let directory = TempDir::new().unwrap();
        let install_path = directory.path().join("vendor/vendor/package");
        std::fs::create_dir_all(&install_path).unwrap();
        std::fs::write(install_path.join("old.txt"), "old").unwrap();
        let from = Arc::new(Package::new("vendor/package", "1.0.0"));
        let mut target = Package::new("vendor/package", "2.0.0");
        target.package_type = crate::package::package_type::METAPACKAGE.into();
        let to = Arc::new(target);
        let mut transaction = Transaction::new();
        transaction.update(from.clone(), to.clone());

        let result = test_manager(&directory)
            .execute(&transaction)
            .await
            .unwrap();

        assert_eq!(
            result.updated,
            [(from.as_ref().clone(), to.as_ref().clone())]
        );
        assert!(!install_path.exists());
    }

    // Ported from Composer\Test\Installer\InstallationManagerTest::testUninstall.
    #[tokio::test]
    async fn composer_installation_manager_routes_package_uninstalls() {
        let directory = TempDir::new().unwrap();
        let install_path = directory.path().join("vendor/vendor/package");
        std::fs::create_dir_all(&install_path).unwrap();
        std::fs::write(install_path.join("installed.txt"), "installed").unwrap();
        let package = Arc::new(Package::new("vendor/package", "1.0.0"));
        let mut transaction = Transaction::new();
        transaction.uninstall(package.clone());

        let result = test_manager(&directory)
            .execute(&transaction)
            .await
            .unwrap();

        assert_eq!(result.removed, [package.as_ref().clone()]);
        assert!(!install_path.exists());
    }

    // Ported from Composer\Test\Installer\InstallationManagerTest::testInstallBinary.
    #[tokio::test]
    async fn composer_installation_manager_ensures_package_binaries_are_linked() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("source");
        std::fs::create_dir_all(source.join("bin")).unwrap();
        std::fs::write(source.join("bin/tool.php"), "#!/usr/bin/env php\n").unwrap();
        let mut package = Package::new("vendor/package", "1.0.0");
        package.dist = Some(
            crate::package::Dist::path(source.to_string_lossy().into_owned())
                .with_transport_options(std::collections::HashMap::from([(
                    "symlink".to_string(),
                    serde_json::Value::Bool(false),
                )])),
        );
        package.bin.push("bin/tool.php".into());

        let result = test_manager(&directory)
            .install_packages(std::slice::from_ref(&package))
            .await
            .unwrap();

        let binary = directory.path().join("vendor/bin/tool");
        assert_eq!(result.installed, [package]);
        assert_eq!(result.binaries.len(), 1);
        assert_eq!(result.binaries[0], binary);
        assert!(binary.exists());
    }
}
