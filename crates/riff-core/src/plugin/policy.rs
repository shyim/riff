use glob::Pattern;

use crate::config::AllowPlugins;

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
}
