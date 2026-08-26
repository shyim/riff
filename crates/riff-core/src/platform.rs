use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::package::Package;
use crate::util::is_platform_package;

const COMPOSER_VERSION: &str = "2.99.99";
const COMPOSER_RUNTIME_API_VERSION: &str = "2.2.2";
const COMPOSER_PLUGIN_API_VERSION: &str = "2.6.0";

/// Complete platform information supplied by a Riff embedding application.
///
/// A platform can contain a typed PHP snapshot, explicit virtual packages, or
/// both. Explicit packages override packages derived from the snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Platform {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    snapshot: Option<PlatformSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    packages: Vec<Package>,
}

impl Platform {
    /// Create a platform from typed PHP runtime facts.
    pub fn from_snapshot(snapshot: PlatformSnapshot) -> Self {
        Self {
            snapshot: Some(snapshot),
            packages: Vec::new(),
        }
    }

    /// Create an explicitly empty runtime platform.
    ///
    /// Riff's Composer capability packages are still included when this
    /// platform is resolved.
    pub fn empty() -> Self {
        Self {
            snapshot: None,
            packages: Vec::new(),
        }
    }

    /// Create a platform from already materialized virtual packages.
    pub fn from_packages(packages: Vec<Package>) -> Self {
        Self {
            snapshot: None,
            packages,
        }
    }

    /// Add or replace one virtual platform package.
    pub fn with_package(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.packages.push(Package::new(name, version.into()));
        self
    }

    /// Return the supplied PHP snapshot, if this platform has one.
    pub fn snapshot(&self) -> Option<&PlatformSnapshot> {
        self.snapshot.as_ref()
    }

    /// Convert all supplied facts to the virtual packages consumed by Riff.
    ///
    /// Precedence is Riff capabilities, snapshot facts, explicit packages,
    /// then Composer's `config.platform` overrides.
    pub fn to_packages(
        &self,
        overrides: &HashMap<String, serde_json::Value>,
    ) -> Result<Vec<Package>> {
        let mut packages = BTreeMap::new();
        for (name, version) in [
            ("composer", COMPOSER_VERSION),
            ("composer-runtime-api", COMPOSER_RUNTIME_API_VERSION),
            ("composer-plugin-api", COMPOSER_PLUGIN_API_VERSION),
        ] {
            packages.insert(name.to_string(), platform_package(name, version));
        }

        if let Some(snapshot) = &self.snapshot {
            for package in snapshot.runtime_packages() {
                packages.insert(package.name.clone(), package);
            }
        }

        for package in &self.packages {
            let name = package.name.to_ascii_lowercase();
            if !is_platform_package(&name) {
                bail!(
                    "supplied platform package {name} is not a PHP, extension, library, or Composer virtual package"
                );
            }
            let mut package = package.clone();
            package.name.clone_from(&name);
            package.package_type = "platform".into();
            packages.insert(name, package);
        }

        for (name, value) in overrides {
            let name = name.to_ascii_lowercase();
            match value {
                serde_json::Value::String(version) => {
                    packages.insert(name.clone(), platform_package(&name, version));
                }
                serde_json::Value::Bool(false) if name == "php" => {
                    bail!("config.platform.php cannot be disabled");
                }
                serde_json::Value::Bool(false) => {
                    packages.remove(&name);
                }
                _ => bail!("config.platform.{name} must be a version string or false"),
            }
        }

        Ok(packages.into_values().collect())
    }
}

/// Runtime facts returned by the small external PHP probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformSnapshot {
    pub php_version: String,
    pub php_version_id: u64,
    pub int_size: u64,
    pub zts: bool,
    pub debug: bool,
    pub ipv6: bool,
    #[serde(default)]
    pub extensions: BTreeMap<String, String>,
    #[serde(default)]
    pub libraries: BTreeMap<String, String>,
}

impl PlatformSnapshot {
    /// Convert detected facts to the virtual packages consumed by the solver.
    pub fn to_packages(
        &self,
        overrides: &HashMap<String, serde_json::Value>,
    ) -> Result<Vec<Package>> {
        Platform::from_snapshot(self.clone()).to_packages(overrides)
    }

    fn runtime_packages(&self) -> Vec<Package> {
        let mut versions = BTreeMap::new();
        let php_version = normalized_php_version(&self.php_version);
        versions.insert("php".to_string(), php_version.clone());
        if self.int_size >= 8 {
            versions.insert("php-64bit".to_string(), php_version.clone());
        }
        if self.ipv6 {
            versions.insert("php-ipv6".to_string(), php_version.clone());
        }
        if self.zts {
            versions.insert("php-zts".to_string(), php_version.clone());
        }
        if self.debug {
            versions.insert("php-debug".to_string(), php_version);
        }

        for (name, version) in &self.extensions {
            let name = name.to_ascii_lowercase();
            if name != "core" && name != "standard" {
                versions.insert(
                    format!("ext-{}", name.replace(' ', "-")),
                    normalized(version),
                );
            }
        }
        for (name, version) in &self.libraries {
            if !version.is_empty() {
                let version = match name.as_str() {
                    "openssl" => parse_openssl_version(version)
                        .map(|(version, _)| version)
                        .unwrap_or_else(|| normalized(version)),
                    "jpeg" | "gd-libjpeg" | "imagick-libjpeg" => {
                        parse_libjpeg_version(version).unwrap_or_else(|| normalized(version))
                    }
                    "icu-zoneinfo" => {
                        parse_zoneinfo_version(version).unwrap_or_else(|| normalized(version))
                    }
                    _ => normalized(version),
                };
                versions.insert(format!("lib-{name}"), version);
            }
        }

        versions
            .into_iter()
            .map(|(name, version)| platform_package(&name, &version))
            .collect()
    }
}

fn platform_package(name: &str, version: &str) -> Package {
    let mut package = Package::new(name, version);
    package.package_type = "platform".into();
    package
}

fn normalized(version: &str) -> String {
    if version.is_empty()
        || version.eq_ignore_ascii_case("false")
        || !version.starts_with(|character: char| character.is_ascii_digit())
    {
        "0".to_string()
    } else {
        version.to_string()
    }
}

fn normalized_php_version(version: &str) -> String {
    let bytes = version.as_bytes();
    let mut end = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if end == 0 {
        return normalized(version);
    }

    for _ in 0..3 {
        if bytes.get(end) != Some(&b'.') {
            break;
        }
        let dot = end;
        end += 1;
        let digits = bytes[end..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits == 0 {
            end = dot;
            break;
        }
        end += digits;
    }

    version[..end].to_string()
}

/// Convert OpenSSL's pre-3 patch-letter scheme to a numeric version Composer's
/// constraint parser can compare. The boolean indicates a FIPS build.
pub fn parse_openssl_version(version: &str) -> Option<(String, bool)> {
    let base_end = version
        .as_bytes()
        .iter()
        .take_while(|byte| byte.is_ascii_digit() || **byte == b'.')
        .count();
    if base_end == 0 {
        return None;
    }
    let base = &version[..base_end];
    let tail = &version[base_end..];
    let patch_limit = tail
        .as_bytes()
        .iter()
        .take(2)
        .take_while(|byte| byte.is_ascii_lowercase())
        .count();
    let (patch, suffix) = (0..=patch_limit).rev().find_map(|patch_len| {
        openssl_suffix(&tail[patch_len..]).map(|suffix| (&tail[..patch_len], suffix))
    })?;
    let is_fips = suffix.contains("fips");
    let major = base
        .split('.')
        .next()
        .and_then(|part| part.parse::<u64>().ok())?;

    let mut parsed = base.to_string();
    if major < 3 {
        parsed.push('.');
        parsed.push_str(&alpha_version_number(patch).to_string());
    }

    let normalized_suffix = format!("-{}", suffix.trim_start_matches('-'))
        .replace("-fips", "")
        .replace("-pre", "-alpha");
    parsed.push_str(normalized_suffix.trim_end_matches('-'));
    Some((parsed, is_fips))
}

fn openssl_suffix(tail: &str) -> Option<&str> {
    let bytes = tail.as_bytes();
    let mut offset = 0;

    loop {
        let token_start = offset;
        if bytes.get(offset) == Some(&b'-') {
            offset += 1;
        }
        let remainder = &tail[offset..];
        let Some(token) = ["alpha", "beta", "fips", "dev", "pre", "rc"]
            .into_iter()
            .find(|token| remainder.starts_with(token))
        else {
            offset = token_start;
            break;
        };
        offset += token.len();
        offset += bytes[offset..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
    }

    let suffix_end = offset;
    if bytes.get(offset) == Some(&b'-') {
        offset += 1;
        let word_len = bytes[offset..]
            .iter()
            .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
            .count();
        if word_len == 0 {
            return None;
        }
        offset += word_len;
    }

    let remainder = &tail[offset..];
    if !remainder.is_empty()
        && !(remainder.starts_with(" (")
            && remainder.ends_with(')')
            && remainder.len() > 3
            && !remainder[2..remainder.len() - 1].contains('\n'))
    {
        return None;
    }

    Some(&tail[..suffix_end])
}

/// Convert libjpeg's letter suffix to a numeric minor version.
pub fn parse_libjpeg_version(version: &str) -> Option<String> {
    parse_alpha_suffix_version(version, false)
}

/// Convert an IANA zoneinfo release suffix to a numeric revision.
pub fn parse_zoneinfo_version(version: &str) -> Option<String> {
    parse_alpha_suffix_version(version, true)
}

fn parse_alpha_suffix_version(version: &str, four_digit_prefix: bool) -> Option<String> {
    let split = version
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(version.len());
    let (prefix, suffix) = version.split_at(split);
    if prefix.is_empty()
        || (four_digit_prefix && prefix.len() != 4)
        || !suffix.bytes().all(|byte| byte.is_ascii_lowercase())
    {
        return None;
    }
    Some(format!("{prefix}.{}", alpha_version_number(suffix)))
}

fn alpha_version_number(alpha: &str) -> u64 {
    alpha.bytes().map(|byte| u64::from(byte - b'a' + 1)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> PlatformSnapshot {
        PlatformSnapshot {
            php_version: "8.3.4".to_string(),
            php_version_id: 80304,
            int_size: 8,
            zts: true,
            debug: false,
            ipv6: true,
            extensions: BTreeMap::from([("json".to_string(), "8.3.4".to_string())]),
            libraries: BTreeMap::from([("openssl".to_string(), "3.2.1".to_string())]),
        }
    }

    #[test]
    fn platform_overrides_add_replace_and_disable() {
        let overrides = HashMap::from([
            ("php".to_string(), serde_json::json!("8.2.0")),
            ("ext-json".to_string(), serde_json::json!(false)),
            ("ext-demo".to_string(), serde_json::json!("1.0")),
        ]);
        let packages = snapshot().to_packages(&overrides).unwrap();
        assert!(packages
            .iter()
            .any(|p| p.name == "php" && p.version == "8.2.0"));
        assert!(!packages.iter().any(|p| p.name == "ext-json"));
        assert!(packages.iter().any(|p| p.name == "ext-demo"));
    }

    #[test]
    fn explicit_packages_override_snapshot_before_project_config() {
        let platform = Platform::from_snapshot(snapshot())
            .with_package("PHP", "8.4.1")
            .with_package("ext-demo", "2.0");
        let overrides = HashMap::from([("php".to_string(), serde_json::json!("8.2.7"))]);

        let packages = platform.to_packages(&overrides).unwrap();
        assert!(packages
            .iter()
            .any(|package| package.name == "php" && package.version == "8.2.7"));
        assert!(packages
            .iter()
            .any(|package| package.name == "ext-demo" && package.version == "2.0"));
    }

    #[test]
    fn empty_platform_keeps_overridable_riff_capabilities() {
        let overrides = HashMap::from([(
            "composer-runtime-api".to_string(),
            serde_json::json!("2.3.0"),
        )]);
        let packages = Platform::empty().to_packages(&overrides).unwrap();

        assert!(!packages.iter().any(|package| package.name == "php"));
        assert!(packages.iter().any(|package| {
            package.name == "composer-runtime-api" && package.version == "2.3.0"
        }));
    }

    #[test]
    fn rejects_non_platform_extra_packages() {
        let platform = Platform::empty().with_package("vendor/package", "1.0");
        let error = platform.to_packages(&HashMap::new()).unwrap_err();
        assert!(error.to_string().contains("is not a PHP"));
    }

    #[test]
    fn php_cannot_be_disabled() {
        let overrides = HashMap::from([("php".to_string(), serde_json::json!(false))]);
        assert!(snapshot().to_packages(&overrides).is_err());
    }

    #[test]
    fn composer_platform_repository_php_version_data_provider() {
        struct Case {
            php_version: &'static str,
            int_size: u64,
            debug: bool,
            zts: bool,
            ipv6: bool,
            expected: &'static [(&'static str, &'static str)],
        }

        let cases = [
            Case {
                php_version: "7.1.33",
                int_size: 4,
                debug: false,
                zts: false,
                ipv6: false,
                expected: &[("php", "7.1.33")],
            },
            Case {
                php_version: "7.2.31-1+ubuntu16.04.1+deb.sury.org+1",
                int_size: 4,
                debug: true,
                zts: false,
                ipv6: false,
                expected: &[("php", "7.2.31"), ("php-debug", "7.2.31")],
            },
            Case {
                php_version: "7.2.31-1+ubuntu16.04.1+deb.sury.org+1",
                int_size: 4,
                debug: false,
                zts: true,
                ipv6: false,
                expected: &[("php", "7.2.31"), ("php-zts", "7.2.31")],
            },
            Case {
                php_version: "7.2.31-1+ubuntu16.04.1+deb.sury.org+1",
                int_size: 8,
                debug: false,
                zts: false,
                ipv6: false,
                expected: &[("php", "7.2.31"), ("php-64bit", "7.2.31")],
            },
            Case {
                php_version: "7.2.31-1+ubuntu16.04.1+deb.sury.org+1",
                int_size: 4,
                debug: false,
                zts: false,
                ipv6: true,
                expected: &[("php", "7.2.31"), ("php-ipv6", "7.2.31")],
            },
        ];

        for case in cases {
            let snapshot = PlatformSnapshot {
                php_version: case.php_version.to_string(),
                php_version_id: 0,
                int_size: case.int_size,
                zts: case.zts,
                debug: case.debug,
                ipv6: case.ipv6,
                extensions: BTreeMap::new(),
                libraries: BTreeMap::new(),
            };
            let packages = snapshot.to_packages(&HashMap::new()).unwrap();
            for (name, version) in case.expected {
                assert!(
                    packages.iter().any(|package| {
                        package.name == *name && package.pretty_version.as_deref() == Some(*version)
                    }),
                    "missing {name} {version} for {}",
                    case.php_version
                );
            }
        }
    }

    #[test]
    fn composer_platform_repository_inet_pton_regression() {
        let mut snapshot = snapshot();
        snapshot.ipv6 = false;
        assert!(!snapshot
            .to_packages(&HashMap::new())
            .unwrap()
            .iter()
            .any(|package| package.name == "php-ipv6"));
    }

    #[test]
    fn composer_platform_repository_exposes_package_manager_version() {
        assert!(snapshot()
            .to_packages(&HashMap::new())
            .unwrap()
            .iter()
            .any(|package| package.name == "composer" && package.version == "2.99.99"));
    }

    #[test]
    fn composer_platform_version_parses_openssl_versions() {
        let cases = [
            ("1.2.3", "1.2.3.0", false),
            ("1.2.3-beta3", "1.2.3.0-beta3", false),
            ("1.2.3-beta3-dev", "1.2.3.0-beta3-dev", false),
            ("1.2.3-beta3-fips", "1.2.3.0-beta3", true),
            ("1.2.3-beta3-fips-dev", "1.2.3.0-beta3-dev", true),
            ("1.2.3-dev", "1.2.3.0-dev", false),
            ("1.2.3-fips", "1.2.3.0", true),
            ("1.2.3-fips-beta3", "1.2.3.0-beta3", true),
            ("1.2.3-fips-beta3-dev", "1.2.3.0-beta3-dev", true),
            ("1.2.3-fips-dev", "1.2.3.0-dev", true),
            ("1.2.3-pre2", "1.2.3.0-alpha2", false),
            ("1.2.3-pre2-dev", "1.2.3.0-alpha2-dev", false),
            ("1.2.3-pre2-fips", "1.2.3.0-alpha2", true),
            ("1.2.3-pre2-fips-dev", "1.2.3.0-alpha2-dev", true),
            ("1.2.3a", "1.2.3.1", false),
            ("1.2.3a-beta3", "1.2.3.1-beta3", false),
            ("1.2.3a-beta3-dev", "1.2.3.1-beta3-dev", false),
            ("1.2.3a-dev", "1.2.3.1-dev", false),
            ("1.2.3a-dev-fips", "1.2.3.1-dev", true),
            ("1.2.3a-fips", "1.2.3.1", true),
            ("1.2.3a-fips-beta3", "1.2.3.1-beta3", true),
            ("1.2.3a-fips-dev", "1.2.3.1-dev", true),
            ("1.2.3beta3", "1.2.3.0-beta3", false),
            ("1.2.3beta3-dev", "1.2.3.0-beta3-dev", false),
            ("1.2.3zh", "1.2.3.34", false),
            ("1.2.3zh-dev", "1.2.3.34-dev", false),
            ("1.2.3zh-fips", "1.2.3.34", true),
            ("1.2.3zh-fips-dev", "1.2.3.34-dev", true),
            ("1.2.3zh-fips-rc3", "1.2.3.34-rc3", true),
            ("1.2.3zh-alpha10-fips", "1.2.3.34-alpha10", true),
            ("1.1.1l (Schannel)", "1.1.1.12", false),
            ("1.2.3z", "1.2.3.26", false),
            ("1.2.3za", "1.2.3.27", false),
            ("1.2.3zy", "1.2.3.51", false),
            ("1.2.3zz", "1.2.3.52", false),
            ("3.0.0", "3.0.0", false),
            ("3.2.4-dev", "3.2.4-dev", false),
        ];

        for (input, expected, expected_fips) in cases {
            assert_eq!(
                parse_openssl_version(input),
                Some((expected.to_string(), expected_fips)),
                "unexpected OpenSSL conversion for {input}"
            );
        }
    }

    #[test]
    fn composer_platform_version_parses_libjpeg_versions() {
        for (input, expected) in [("9", "9.0"), ("9a", "9.1"), ("9b", "9.2"), ("9za", "9.27")] {
            assert_eq!(parse_libjpeg_version(input).as_deref(), Some(expected));
        }
    }

    #[test]
    fn composer_platform_version_parses_zoneinfo_versions() {
        for (input, expected) in [
            ("2019c", "2019.3"),
            ("2020a", "2020.1"),
            ("2020za", "2020.27"),
        ] {
            assert_eq!(parse_zoneinfo_version(input).as_deref(), Some(expected));
        }
    }

    #[test]
    fn detected_openssl_versions_use_numeric_patch_levels() {
        let packages = snapshot().to_packages(&HashMap::new()).unwrap();
        assert!(packages
            .iter()
            .any(|package| package.name == "lib-openssl" && package.version == "3.2.1"));

        let mut legacy = snapshot();
        legacy
            .libraries
            .insert("openssl".to_string(), "1.1.1g".to_string());
        let packages = legacy.to_packages(&HashMap::new()).unwrap();
        assert!(packages
            .iter()
            .any(|package| package.name == "lib-openssl" && package.version == "1.1.1.7"));
    }

    // Ported from Composer\Test\Repository\PlatformRepositoryTest::
    // testLibraryInformation. Riff's PHP probe performs runtime-specific
    // discovery, while this typed boundary turns all detected libraries into
    // solver-visible virtual packages and normalizes vendor version schemes.
    #[test]
    fn composer_platform_snapshot_exposes_detected_library_information() {
        let mut snapshot = snapshot();
        snapshot.libraries.extend([
            ("curl".to_string(), "8.6.0".to_string()),
            ("icu".to_string(), "74.2".to_string()),
            ("jpeg".to_string(), "9za".to_string()),
            ("icu-zoneinfo".to_string(), "2020a".to_string()),
            ("zlib".to_string(), "1.3.1".to_string()),
        ]);

        let packages = snapshot.to_packages(&HashMap::new()).unwrap();
        let versions: BTreeMap<_, _> = packages
            .iter()
            .filter(|package| package.name.starts_with("lib-"))
            .map(|package| (package.name.as_str(), package.version.as_str()))
            .collect();
        assert_eq!(versions["lib-curl"], "8.6.0");
        assert_eq!(versions["lib-icu"], "74.2");
        assert_eq!(versions["lib-jpeg"], "9.27");
        assert_eq!(versions["lib-icu-zoneinfo"], "2020.1");
        assert_eq!(versions["lib-openssl"], "3.2.1");
        assert_eq!(versions["lib-zlib"], "1.3.1");
    }
}
