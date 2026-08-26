//! Loading Composer array-package metadata into Riff's typed package model.

use riff_semver::VersionParser;

use super::Package;

/// Controls metadata which is only safe/relevant when loading repository config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PackageLoadOptions {
    /// Retain downloader transport options from the input package metadata.
    pub load_transport_options: bool,
}

/// Load package metadata using Composer's default ArrayLoader behavior.
pub fn load_package_config(config: &serde_json::Value) -> Result<Package, String> {
    load_package_config_with_options(config, PackageLoadOptions::default())
}

/// Load package metadata with explicit repository-config handling.
pub fn load_package_config_with_options(
    config: &serde_json::Value,
    options: PackageLoadOptions,
) -> Result<Package, String> {
    let mut config = config.clone();
    let object = config
        .as_object_mut()
        .ok_or_else(|| "Package config must be an object".to_owned())?;
    let pretty_version = object
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Package config must contain a string version".to_owned())?
        .to_owned();
    let normalized_version = object
        .remove("version_normalized")
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .map(Ok)
        .unwrap_or_else(|| VersionParser::new().normalize(&pretty_version))
        .map_err(|error| format!("Failed to normalize package version: {error}"))?;
    object.insert(
        "version".to_owned(),
        serde_json::Value::String(normalized_version),
    );
    object.insert(
        "pretty_version".to_owned(),
        serde_json::Value::String(pretty_version),
    );
    if !options.load_transport_options {
        object.remove("transport-options");
    }

    serde_json::from_value(config).map_err(|error| format!("Invalid package config: {error}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::package::dump_package;

    fn composer_array_loader_config() -> serde_json::Value {
        json!({
            "name": "a/b",
            "version": "1.2.3",
            "version_normalized": "1.2.3.0",
            "description": "Foo bar",
            "type": "library",
            "keywords": ["a", "b", "c"],
            "homepage": "https://example.com",
            "license": ["MIT", "GPL-3.0-only"],
            "authors": [{"name": "Bob", "email": "bob@example.org"}],
            "funding": [{"type": "custom", "url": "https://example.org/fund"}],
            "require": {"foo/bar": "1.0"},
            "require-dev": {"foo/baz": "1.0"},
            "replace": {"foo/qux": "1.0"},
            "conflict": {"foo/quux": "1.0"},
            "provide": {"foo/quuux": "1.0"},
            "autoload": {"psr-0": {"Ns\\Prefix": "path"}, "classmap": ["path"]},
            "include-path": ["path3", "path4"],
            "target-dir": "some/prefix",
            "extra": {"random": {"things": "of any shape"}},
            "bin": ["bin1", "bin/foo"],
            "archive": {"exclude": ["/foo/bar", "baz", "!/foo/bar/baz"]},
            "transport-options": {"ssl": {"local_cert": "/opt/certs/test.pem"}},
            "abandoned": "foo/bar"
        })
    }

    fn assert_roundtrip(load_transport_options: bool) {
        let config = composer_array_loader_config();
        let package = load_package_config_with_options(
            &config,
            PackageLoadOptions {
                load_transport_options,
            },
        )
        .unwrap();
        let dumped = dump_package(&package).unwrap();

        let mut expected = config;
        if !load_transport_options {
            expected
                .as_object_mut()
                .unwrap()
                .remove("transport-options");
        }
        assert_eq!(dumped, expected);
    }

    #[test]
    fn composer_array_loader_default_omits_transport_config() {
        let package = load_package_config(&composer_array_loader_config()).unwrap();
        assert!(package.transport_options.is_none());
        assert_roundtrip(false);
    }

    #[test]
    fn composer_array_loader_can_retain_transport_config() {
        assert_roundtrip(true);
    }

    #[test]
    fn composer_array_loader_can_explicitly_omit_transport_config() {
        assert_roundtrip(false);
    }
}
