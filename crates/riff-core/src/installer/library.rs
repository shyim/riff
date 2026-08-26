//! Library installer - installs packages to vendor directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::downloader::{DownloadManager, DownloadResult};
use crate::package::Package;
use crate::Result;

/// Library installer for standard Composer packages
pub struct LibraryInstaller {
    download_manager: Arc<DownloadManager>,
    vendor_dir: PathBuf,
}

impl LibraryInstaller {
    /// Create a new library installer
    pub fn new(download_manager: Arc<DownloadManager>, vendor_dir: impl Into<PathBuf>) -> Self {
        Self {
            download_manager,
            vendor_dir: vendor_dir.into(),
        }
    }

    /// Get the install path for a package
    pub fn get_install_path(&self, package: &Package) -> PathBuf {
        let mut path = self.vendor_dir.join(package.pretty_name());
        if let Some(target_dir) = package.target_dir.as_deref() {
            path.push(target_dir);
        }
        path
    }

    /// Check if a package is installed
    pub fn is_installed(&self, package: &Package) -> bool {
        let install_path = self.get_install_path(package);
        install_path.exists()
    }

    /// Composer considers a package installed only when both its repository
    /// record and its installation directory exist.
    pub fn is_installed_with_repository(
        &self,
        package: &Package,
        repository_contains: bool,
    ) -> bool {
        repository_contains && self.is_installed(package)
    }

    /// Install a package
    ///
    /// If the package is already installed, this is a no-op and returns Ok with skipped flag.
    pub async fn install(&self, package: &Package) -> Result<DownloadResult> {
        let install_path = self.get_install_path(package);

        // Check if already installed - skip if so
        if install_path.exists() {
            return Ok(DownloadResult {
                path: install_path,
                from_cache: false,
                skipped: true,
            });
        }

        // Download and extract
        self.download_manager.download(package).await
    }

    pub async fn download_only(&self, package: &Package) -> Result<DownloadResult> {
        self.download_manager.download_only(package).await
    }

    /// Update a package
    pub async fn update(&self, from: &Package, to: &Package) -> Result<DownloadResult> {
        // Remove old version
        self.uninstall(from).await?;

        // Install new version
        self.download_manager.download(to).await
    }

    /// Uninstall a package
    pub async fn uninstall(&self, package: &Package) -> Result<()> {
        let install_path = self.get_install_path(package);

        if install_path.exists() {
            tokio::fs::remove_dir_all(&install_path).await?;
        }

        Ok(())
    }

    /// Get the vendor directory
    pub fn vendor_dir(&self) -> &Path {
        &self.vendor_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::DownloadConfig;
    use crate::http::HttpClient;
    use tempfile::TempDir;

    fn create_test_installer() -> (LibraryInstaller, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let http_client = Arc::new(HttpClient::new().unwrap());
        let download_config = DownloadConfig {
            vendor_dir: temp_dir.path().join("vendor"),
            cache_dir: temp_dir.path().join("cache"),
            ..Default::default()
        };
        let download_manager = Arc::new(DownloadManager::new(http_client, download_config));
        let installer = LibraryInstaller::new(download_manager, temp_dir.path().join("vendor"));
        (installer, temp_dir)
    }

    #[test]
    fn test_get_install_path() {
        let (installer, _temp) = create_test_installer();
        let package = Package::new("vendor/package", "1.0.0");

        let path = installer.get_install_path(&package);
        assert!(path.ends_with("vendor/vendor/package"));
    }

    #[test]
    fn composer_installer_creation_does_not_create_vendor_directory() {
        let temp_dir = TempDir::new().unwrap();
        let vendor_dir = temp_dir.path().join("vendor");
        let http_client = Arc::new(HttpClient::new().unwrap());
        let download_manager = Arc::new(DownloadManager::new(
            http_client,
            DownloadConfig {
                vendor_dir: vendor_dir.clone(),
                cache_dir: temp_dir.path().join("cache"),
                ..Default::default()
            },
        ));

        let _installer = LibraryInstaller::new(download_manager, &vendor_dir);

        assert!(!vendor_dir.exists());
    }

    #[test]
    fn composer_install_path_uses_the_pretty_package_name() {
        let (installer, _temp) = create_test_installer();
        let package = Package::new("Vendor/Pkg", "1.0.0");

        assert!(installer
            .get_install_path(&package)
            .ends_with("vendor/Vendor/Pkg"));
    }

    #[test]
    fn composer_install_path_appends_the_target_directory() {
        let (installer, _temp) = create_test_installer();
        let mut package = Package::new("Foo/Bar", "1.0.0");
        package.target_dir = Some("Some/Namespace".to_string());

        assert!(installer
            .get_install_path(&package)
            .ends_with("vendor/Foo/Bar/Some/Namespace"));
    }

    // Ported from Composer\Test\Installer\LibraryInstallerTest::testIsInstalled.
    #[test]
    fn composer_library_installer_requires_repository_and_install_path() {
        let (installer, _temp) = create_test_installer();
        let package = Package::new("test/pkg", "1.0.0");

        assert!(!installer.is_installed_with_repository(&package, false));
        assert!(!installer.is_installed_with_repository(&package, true));
        std::fs::create_dir_all(installer.get_install_path(&package)).unwrap();
        assert!(installer.is_installed_with_repository(&package, true));
        assert!(!installer.is_installed_with_repository(&package, false));
    }

    // Ported from Composer\Test\Installer\LibraryInstallerTest::testInstall.
    #[tokio::test]
    async fn composer_library_installer_installs_package_files() {
        let (installer, temp) = create_test_installer();
        let source = temp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("installed.txt"), "installed").unwrap();
        let mut package = Package::new("some/package", "1.0.0");
        package.dist = Some(
            crate::package::Dist::path(source.to_string_lossy().into_owned())
                .with_transport_options(std::collections::HashMap::from([(
                    "symlink".to_string(),
                    serde_json::Value::Bool(false),
                )])),
        );

        let result = installer.install(&package).await.unwrap();

        assert!(!result.skipped);
        assert_eq!(
            std::fs::read_to_string(installer.get_install_path(&package).join("installed.txt"))
                .unwrap(),
            "installed"
        );
    }

    // Ported from Composer\Test\Installer\LibraryInstallerTest::testUpdate.
    #[tokio::test]
    async fn composer_library_installer_replaces_package_on_update() {
        let (installer, temp) = create_test_installer();
        let old = Package::new("vendor/package1", "1.0.0");
        std::fs::create_dir_all(installer.get_install_path(&old)).unwrap();
        std::fs::write(installer.get_install_path(&old).join("old.txt"), "old").unwrap();

        let source = temp.path().join("new-source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("new.txt"), "new").unwrap();
        let mut target = Package::new("vendor/package1", "2.0.0");
        target.dist = Some(
            crate::package::Dist::path(source.to_string_lossy().into_owned())
                .with_transport_options(std::collections::HashMap::from([(
                    "symlink".to_string(),
                    serde_json::Value::Bool(false),
                )])),
        );

        installer.update(&old, &target).await.unwrap();

        let path = installer.get_install_path(&target);
        assert!(!path.join("old.txt").exists());
        assert_eq!(
            std::fs::read_to_string(path.join("new.txt")).unwrap(),
            "new"
        );
    }

    // Ported from Composer\Test\Installer\LibraryInstallerTest::testUninstall.
    #[tokio::test]
    async fn composer_library_installer_removes_installed_package() {
        let (installer, _temp) = create_test_installer();
        let package = Package::new("vendor/pkg", "1.0.0");
        let path = installer.get_install_path(&package);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("installed.txt"), "installed").unwrap();

        installer.uninstall(&package).await.unwrap();
        assert!(!path.exists());
        installer.uninstall(&package).await.unwrap();
    }

    #[test]
    fn test_is_not_installed() {
        let (installer, _temp) = create_test_installer();
        let package = Package::new("vendor/package", "1.0.0");

        assert!(!installer.is_installed(&package));
    }

    #[tokio::test]
    async fn test_uninstall_nonexistent() {
        let (installer, _temp) = create_test_installer();
        let package = Package::new("vendor/package", "1.0.0");

        // Should not error when uninstalling non-existent package
        let result = installer.uninstall(&package).await;
        assert!(result.is_ok());
    }
}
