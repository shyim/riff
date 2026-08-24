use anyhow::{bail, Result};
use glob::Pattern;

use crate::config::AllowPlugins;
use crate::package::Package;

const NATIVE_PLUGINS: &[&str] = &[
    "bamarni/composer-bin-plugin",
    "phpstan/extension-installer",
    "symfony/runtime",
];

#[derive(Debug, Clone)]
pub struct PluginPolicy {
    enabled: bool,
    allow: AllowPlugins,
}

impl PluginPolicy {
    pub fn new(enabled: bool, allow: AllowPlugins) -> Self {
        Self { enabled, allow }
    }

    pub fn allows(&self, package: &str) -> bool {
        if !self.enabled {
            return false;
        }
        match &self.allow {
            AllowPlugins::Bool(value) => *value,
            AllowPlugins::Map(patterns) => patterns.iter().any(|(pattern, allowed)| {
                *allowed
                    && Pattern::new(pattern)
                        .map(|pattern| pattern.matches(package))
                        .unwrap_or(false)
            }),
        }
    }

    pub fn validate<'a>(&self, packages: impl IntoIterator<Item = &'a Package>) -> Result<()> {
        for package in packages {
            if package.is_composer_plugin()
                && self.allows(&package.name)
                && !NATIVE_PLUGINS.contains(&package.name.as_str())
            {
                bail!(
                    "Composer plugin {} is enabled but cannot run in sonata; disable it with config.allow-plugins or --no-plugins",
                    package.name
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn explicit_patterns_control_plugins() {
        let policy = PluginPolicy::new(
            true,
            AllowPlugins::Map(HashMap::from([("phpstan/*".to_string(), true)])),
        );
        assert!(policy.allows("phpstan/extension-installer"));
        assert!(!policy.allows("vendor/plugin"));
    }

    #[test]
    fn unknown_enabled_plugin_is_rejected() {
        let policy = PluginPolicy::new(true, AllowPlugins::Bool(true));
        let mut package = Package::new("vendor/plugin", "1.0.0");
        package.package_type = "composer-plugin".into();
        assert!(policy.validate([&package]).is_err());
        assert!(PluginPolicy::new(false, AllowPlugins::Bool(true))
            .validate([&package])
            .is_ok());
    }

    #[test]
    fn native_plugin_is_accepted_when_enabled() {
        let policy = PluginPolicy::new(true, AllowPlugins::Bool(true));
        let mut package = Package::new("phpstan/extension-installer", "1.4.3");
        package.package_type = "composer-plugin".into();
        assert!(policy.validate([&package]).is_ok());
    }
}
