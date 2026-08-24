use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::package::Package;

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
        let mut versions = BTreeMap::new();
        versions.insert("php".to_string(), self.php_version.clone());
        if self.int_size >= 8 {
            versions.insert("php-64bit".to_string(), self.php_version.clone());
        }
        if self.ipv6 {
            versions.insert("php-ipv6".to_string(), self.php_version.clone());
        }
        if self.zts {
            versions.insert("php-zts".to_string(), self.php_version.clone());
        }
        if self.debug {
            versions.insert("php-debug".to_string(), self.php_version.clone());
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
                versions.insert(format!("lib-{name}"), normalized(version));
            }
        }

        versions.insert("composer".to_string(), "2.99.99".to_string());
        versions.insert("composer-runtime-api".to_string(), "2.2.2".to_string());
        versions.insert("composer-plugin-api".to_string(), "2.6.0".to_string());

        for (name, value) in overrides {
            let name = name.to_ascii_lowercase();
            match value {
                serde_json::Value::String(version) => {
                    versions.insert(name, version.clone());
                }
                serde_json::Value::Bool(false) if name == "php" => {
                    bail!("config.platform.php cannot be disabled");
                }
                serde_json::Value::Bool(false) => {
                    versions.remove(&name);
                }
                _ => bail!("config.platform.{name} must be a version string or false"),
            }
        }

        Ok(versions
            .into_iter()
            .map(|(name, version)| Package::new(&name, &version))
            .collect())
    }
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
    fn php_cannot_be_disabled() {
        let overrides = HashMap::from([("php".to_string(), serde_json::json!(false))]);
        assert!(snapshot().to_packages(&overrides).is_err());
    }
}
