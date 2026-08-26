//! Binary installer - creates executable links for package binaries.

use std::path::{Path, PathBuf};

use crate::package::Package;
use crate::Result;

/// Compatibility mode used for generated package binary entry points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BinaryCompatibility {
    /// Generate the native Composer-compatible proxy for the current platform.
    #[default]
    Auto,
    /// Generate both the Unix proxy and its Windows batch counterpart.
    Full,
    /// Generate a portable Unix proxy without a Windows batch file.
    Proxy,
    /// Link directly to the package binary where the platform supports it.
    Symlink,
}

impl BinaryCompatibility {
    pub fn from_config(value: &str) -> Self {
        match value {
            "full" => Self::Full,
            "proxy" => Self::Proxy,
            "symlink" => Self::Symlink,
            _ => Self::Auto,
        }
    }

    fn effective(self) -> Self {
        match self {
            Self::Auto if cfg!(windows) => Self::Full,
            Self::Auto => Self::Proxy,
            mode => mode,
        }
    }
}

/// Binary installer for creating executable links
pub struct BinaryInstaller {
    /// Directory where binaries are linked
    bin_dir: PathBuf,
    /// Vendor directory where packages are installed
    vendor_dir: PathBuf,
    /// Compatibility mode used for generated binary entry points
    compatibility: BinaryCompatibility,
}

impl BinaryInstaller {
    /// Create a new binary installer
    pub fn new(bin_dir: impl Into<PathBuf>, vendor_dir: impl Into<PathBuf>) -> Self {
        Self {
            bin_dir: bin_dir.into(),
            vendor_dir: vendor_dir.into(),
            compatibility: BinaryCompatibility::Auto,
        }
    }

    /// Create a binary installer with an explicit Composer `bin-compat` mode.
    pub fn with_compatibility(
        bin_dir: impl Into<PathBuf>,
        vendor_dir: impl Into<PathBuf>,
        compatibility: BinaryCompatibility,
    ) -> Self {
        Self {
            bin_dir: bin_dir.into(),
            vendor_dir: vendor_dir.into(),
            compatibility,
        }
    }

    /// Install binaries for a package
    pub async fn install(&self, package: &Package) -> Result<Vec<PathBuf>> {
        if package.bin.is_empty() {
            return Ok(Vec::new());
        }

        tokio::fs::create_dir_all(&self.bin_dir).await?;

        let mut installed = Vec::new();
        let package_dir = self.vendor_dir.join(&package.name);

        for bin_path in &package.bin {
            let source = package_dir.join(bin_path);
            let bin_name = Path::new(bin_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| bin_path.to_string());

            let link_name = bin_name.strip_suffix(".php").unwrap_or(&bin_name);
            let link_path = self.bin_dir.join(link_name);

            if source.is_file() {
                self.create_bin_link(&source, &link_path).await?;
                installed.push(link_path);
            }
        }

        Ok(installed)
    }

    /// Remove binaries for a package
    pub async fn uninstall(&self, package: &Package) -> Result<()> {
        for bin_path in &package.bin {
            let bin_name = Path::new(bin_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| bin_path.to_string());

            let link_name = bin_name.strip_suffix(".php").unwrap_or(&bin_name);
            let link_path = self.bin_dir.join(link_name);

            if link_path.exists() {
                tokio::fs::remove_file(&link_path).await?;
            }
            let batch_path = batch_path(&link_path);
            if batch_path.exists() {
                tokio::fs::remove_file(batch_path).await?;
            }
        }

        Ok(())
    }

    async fn create_bin_link(&self, source: &Path, link: &Path) -> Result<()> {
        if link.exists() {
            tokio::fs::remove_file(link).await?;
        }

        match self.compatibility.effective() {
            BinaryCompatibility::Full => {
                self.create_unix_proxy(source, link).await?;
                self.create_windows_proxy(source, link).await?;
            }
            BinaryCompatibility::Proxy => self.create_unix_proxy(source, link).await?,
            BinaryCompatibility::Symlink => self.create_symlink(source, link).await?,
            BinaryCompatibility::Auto => unreachable!("auto compatibility mode is resolved above"),
        }

        make_executable(source).await?;

        Ok(())
    }

    async fn create_unix_proxy(&self, source: &Path, link: &Path) -> Result<()> {
        let contents = tokio::fs::read(source).await?;
        let proxy = if let Some(shebang) = php_binary_shebang(&contents) {
            let target = php_path_expression(link, source);
            let autoload = php_path_expression(link, &self.vendor_dir.join("autoload.php"));
            let target_display = link
                .parent()
                .and_then(|directory| pathdiff::diff_paths(source, directory))
                .map(|path| portable_path(&path))
                .unwrap_or_else(|| portable_path(source));
            let phpunit = portable_path(source)
                == portable_path(&self.vendor_dir.join("phpunit/phpunit/phpunit"));
            composer_php_proxy(
                &shebang,
                &target_display,
                &target,
                &autoload,
                contents.starts_with(b"#!"),
                phpunit,
            )
        } else {
            let target = shell_path_expression(link, source);
            format!(
                "#!/usr/bin/env sh\n\nexport COMPOSER_RUNTIME_BIN_DIR=\"$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\"\nexec {target} \"$@\"\n"
            )
        };

        tokio::fs::write(link, proxy).await?;
        make_executable(link).await
    }

    async fn create_windows_proxy(&self, source: &Path, link: &Path) -> Result<()> {
        let contents = tokio::fs::read(source).await?;
        let batch_path = batch_path(link);
        let (caller, target) = if php_binary_shebang(&contents).is_some() {
            ("php", format!("%~dp0/{}", file_name(link)))
        } else {
            ("call", windows_relative_path(link, source))
        };
        let proxy = format!(
            "@ECHO OFF\r\nsetlocal DISABLEDELAYEDEXPANSION\r\nSET \"BIN_TARGET={target}\"\r\nSET COMPOSER_RUNTIME_BIN_DIR=%~dp0\r\n{caller} \"%BIN_TARGET%\" %*\r\n"
        );
        tokio::fs::write(batch_path, proxy).await?;
        Ok(())
    }

    #[cfg(unix)]
    async fn create_symlink(&self, source: &Path, link: &Path) -> Result<()> {
        tokio::fs::symlink(source, link).await?;
        Ok(())
    }

    #[cfg(windows)]
    async fn create_symlink(&self, source: &Path, link: &Path) -> Result<()> {
        self.create_windows_proxy(source, link).await
    }

    /// Get the bin directory
    pub fn bin_dir(&self) -> &Path {
        &self.bin_dir
    }
}

fn php_binary_shebang(contents: &[u8]) -> Option<String> {
    let prefix = String::from_utf8_lossy(&contents[..contents.len().min(500)]);
    let (shebang, php) = if prefix.starts_with("#!") {
        let end = prefix.find('\n').unwrap_or(prefix.len());
        (Some(prefix[..end].trim_end_matches('\r')), &prefix[end..])
    } else {
        (None, prefix.as_ref())
    };

    php.trim_start_matches(['\r', '\n', '\t', ' '])
        .starts_with("<?php")
        .then(|| shebang.unwrap_or("#!/usr/bin/env php").to_string())
}

fn php_path_expression(link: &Path, target: &Path) -> String {
    if let Some(relative) = link
        .parent()
        .and_then(|directory| pathdiff::diff_paths(target, directory))
    {
        let relative = portable_path(&relative);
        if let Some(tail) = relative.strip_prefix("../") {
            return format!("__DIR__ . '/..'.'/{}'", php_single_quote_escape(tail));
        }
        return format!("__DIR__ . '/{}'", php_single_quote_escape(&relative));
    }
    format!("'{}'", php_single_quote_escape(&portable_path(target)))
}

fn composer_php_proxy(
    shebang: &str,
    target_display: &str,
    target: &str,
    autoload: &str,
    needs_stream_wrapper: bool,
    phpunit: bool,
) -> String {
    let stream_hint = if needs_stream_wrapper {
        "\n * using a stream wrapper to prevent the shebang from being output on PHP<8"
    } else {
        ""
    };
    let phpunit_global = if phpunit {
        format!(
            "$GLOBALS['__PHPUNIT_ISOLATION_EXCLUDE_LIST'] = $GLOBALS['__PHPUNIT_ISOLATION_BLACKLIST'] = array(realpath({target}));\n"
        )
    } else {
        String::new()
    };
    let stream_proxy = if needs_stream_wrapper {
        COMPOSER_STREAM_PROXY
            .replace(
                "__OPENED_PATH__",
                if phpunit {
                    "'phpvfscomposer://'.$this->realpath"
                } else {
                    "$this->realpath"
                },
            )
            .replace(
                "__PHPUNIT_READ_HACK__",
                if phpunit {
                    "\n                $data = str_replace('__DIR__', var_export(dirname($this->realpath), true), $data);\n                $data = str_replace('__FILE__', var_export($this->realpath, true), $data);"
                } else {
                    ""
                },
            )
            .replace("__TARGET__", target)
    } else {
        String::new()
    };

    format!(
        "{shebang}\n<?php\n\n/**\n * Proxy PHP file generated by Composer\n *\n * This file includes the referenced bin path ({target_display}){stream_hint}\n *\n * @generated\n */\n\nnamespace Composer;\n\n$GLOBALS['_composer_bin_dir'] = __DIR__;\n$GLOBALS['_composer_autoload_path'] = {autoload};\n{phpunit_global}\n{stream_proxy}return include {target};\n"
    )
}

const COMPOSER_STREAM_PROXY: &str = r#"if (PHP_VERSION_ID < 80000) {
    if (!class_exists('Composer\BinProxyWrapper')) {
        /**
         * @internal
         */
        final class BinProxyWrapper
        {
            private $handle;
            private $position;
            private $realpath;

            public function stream_open($path, $mode, $options, &$opened_path)
            {
                // get rid of phpvfscomposer:// prefix for __FILE__ & __DIR__ resolution
                $opened_path = substr($path, 17);
                $this->realpath = realpath($opened_path) ?: $opened_path;
                $opened_path = __OPENED_PATH__;
                $this->handle = fopen($this->realpath, $mode);
                $this->position = 0;

                return (bool) $this->handle;
            }

            public function stream_read($count)
            {
                $data = fread($this->handle, $count);

                if ($this->position === 0) {
                    $data = preg_replace('{^#!.*\r?\n}', '', $data);
                }__PHPUNIT_READ_HACK__

                $this->position += strlen($data);

                return $data;
            }

            public function stream_cast($castAs)
            {
                return $this->handle;
            }

            public function stream_close()
            {
                fclose($this->handle);
            }

            public function stream_lock($operation)
            {
                return $operation ? flock($this->handle, $operation) : true;
            }

            public function stream_seek($offset, $whence)
            {
                if (0 === fseek($this->handle, $offset, $whence)) {
                    $this->position = ftell($this->handle);
                    return true;
                }

                return false;
            }

            public function stream_tell()
            {
                return $this->position;
            }

            public function stream_eof()
            {
                return feof($this->handle);
            }

            public function stream_stat()
            {
                return array();
            }

            public function stream_set_option($option, $arg1, $arg2)
            {
                return true;
            }

            public function url_stat($path, $flags)
            {
                $path = substr($path, 17);
                if (file_exists($path)) {
                    return stat($path);
                }

                return false;
            }
        }
    }

    if (
        (function_exists('stream_get_wrappers') && in_array('phpvfscomposer', stream_get_wrappers(), true))
        || (function_exists('stream_wrapper_register') && stream_wrapper_register('phpvfscomposer', 'Composer\BinProxyWrapper'))
    ) {
        return include("phpvfscomposer://" . __TARGET__);
    }
}

"#;

fn shell_path_expression(link: &Path, target: &Path) -> String {
    if let Some(relative) = link
        .parent()
        .and_then(|directory| pathdiff::diff_paths(target, directory))
    {
        return format!(
            "\"$COMPOSER_RUNTIME_BIN_DIR/{}\"",
            shell_double_quote_escape(&portable_path(&relative))
        );
    }
    format!("\"{}\"", shell_double_quote_escape(&portable_path(target)))
}

fn windows_relative_path(link: &Path, target: &Path) -> String {
    link.parent()
        .and_then(|directory| pathdiff::diff_paths(target, directory))
        .map(|relative| format!("%~dp0/{}", portable_path(&relative)))
        .unwrap_or_else(|| portable_path(target))
}

fn portable_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn php_single_quote_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

fn shell_double_quote_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

fn batch_path(link: &Path) -> PathBuf {
    PathBuf::from(format!("{}.bat", link.to_string_lossy()))
}

#[cfg(unix)]
async fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = tokio::fs::metadata(path).await?;
    let mut perms = metadata.permissions();
    perms.set_mode(perms.mode() | 0o111);
    tokio::fs::set_permissions(path, perms).await?;
    Ok(())
}

#[cfg(windows)]
async fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn test_binary_installer_creation() {
        let installer = BinaryInstaller::new("/app/vendor/bin", "/app/vendor");
        assert_eq!(installer.bin_dir(), Path::new("/app/vendor/bin"));
    }

    #[tokio::test]
    async fn test_install_no_binaries() {
        let temp_dir = TempDir::new().unwrap();
        let installer =
            BinaryInstaller::new(temp_dir.path().join("bin"), temp_dir.path().join("vendor"));

        let package = Package::new("vendor/package", "1.0.0");
        let result = installer.install(&package).await;

        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_uninstall_no_binaries() {
        let temp_dir = TempDir::new().unwrap();
        let installer =
            BinaryInstaller::new(temp_dir.path().join("bin"), temp_dir.path().join("vendor"));

        let package = Package::new("vendor/package", "1.0.0");
        let result = installer.uninstall(&package).await;

        assert!(result.is_ok());
    }

    // Ported from Composer\Test\Installer\BinaryInstallerTest::
    // testInstallAndExecBinaryWithFullCompat.
    #[cfg(unix)]
    #[tokio::test]
    async fn composer_full_compat_binary_proxies_execute_all_php_binary_forms() {
        let directory = TempDir::new().unwrap();
        let vendor_dir = directory.path().join("vendor");
        let package_dir = vendor_dir.join("foo/bar");
        let bin_dir = directory.path().join("bin");
        std::fs::create_dir_all(&package_dir).unwrap();

        let installer =
            BinaryInstaller::with_compatibility(&bin_dir, &vendor_dir, BinaryCompatibility::Full);
        let mut package = Package::new("foo/bar", "1.0.0");
        package.bin.push("binary".into());

        let phar = decode_base64(concat!(
            "IyEvdXNyL2Jpbi9lbnYgcGhwCjw/cGhwCgpQaGFyOjptYXBQaGFyKCd0ZXN0LnBo",
            "YXInKTsKCnJlcXVpcmUgJ3BoYXI6Ly90ZXN0LnBoYXIvcnVuLnBocCc7CgpfX0hB",
            "TFRfQ09NUElMRVIoKTsgPz4NCj4AAAABAAAAEQAAAAEACQAAAHRlc3QucGhhcgAAAAAH",
            "AAAAcnVuLnBocCoAAADb9n9hKgAAAMUDDWGkAQAAAAAAADw/cGhwIGVjaG8gInN1Y2",
            "Nlc3MgIi4kX1NFUlZFUlsiYXJndiJdWzFdO1SOC0IE3+UN0yzrHIwyspp9slhmAgAA",
            "AEdCTUI="
        ));
        let binaries = [
            b"<?php\n\necho 'success '.$_SERVER['argv'][1];".to_vec(),
            b"#!/usr/bin/env php\n<?php\n\necho 'success '.$_SERVER['argv'][1];".to_vec(),
            phar,
            b"#!/usr/bin/env php\n<?php declare(strict_types=1);\n\necho 'success '.$_SERVER['argv'][1];".to_vec(),
        ];

        for contents in binaries {
            std::fs::write(package_dir.join("binary"), contents).unwrap();
            let installed = installer.install(&package).await.unwrap();
            assert_eq!(installed, [bin_dir.join("binary")]);

            let output = Command::new(bin_dir.join("binary"))
                .arg("arg")
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8_lossy(&output.stderr), "");
            assert_eq!(String::from_utf8_lossy(&output.stdout), "success arg");
            assert!(bin_dir.join("binary.bat").is_file());
            assert!(!std::fs::symlink_metadata(bin_dir.join("binary"))
                .unwrap()
                .file_type()
                .is_symlink());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proxy_mode_executes_non_php_binaries_with_forwarded_arguments() {
        let directory = TempDir::new().unwrap();
        let vendor_dir = directory.path().join("vendor");
        let package_dir = vendor_dir.join("foo/bar");
        let bin_dir = directory.path().join("bin");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::write(
            package_dir.join("binary"),
            "#!/usr/bin/env sh\nprintf 'success %s' \"$1\"\n",
        )
        .unwrap();

        let installer =
            BinaryInstaller::with_compatibility(&bin_dir, &vendor_dir, BinaryCompatibility::Proxy);
        let mut package = Package::new("foo/bar", "1.0.0");
        package.bin.push("binary".into());

        installer.install(&package).await.unwrap();
        let output = Command::new(bin_dir.join("binary"))
            .arg("arg")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "success arg");
    }

    #[cfg(unix)]
    fn decode_base64(input: &str) -> Vec<u8> {
        let mut output = Vec::new();
        let mut buffer = 0_u32;
        let mut bits = 0_u8;
        for byte in input.bytes() {
            let value = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => break,
                _ => continue,
            };
            buffer = (buffer << 6) | u32::from(value);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                output.push((buffer >> bits) as u8);
                buffer &= (1 << bits) - 1;
            }
        }
        output
    }
}
