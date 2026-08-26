use anyhow::Result;
use riff_core::config::Config;
use riff_core::{Package, Platform, RuntimeContext};

/// Runtime and platform information supplied to Riff command execution.
#[derive(Debug, Clone)]
pub struct CommandContext {
    runtime: RuntimeContext,
    platform: Platform,
}

impl CommandContext {
    pub fn new(runtime: RuntimeContext, platform: Platform) -> Self {
        Self { runtime, platform }
    }

    pub fn runtime(&self) -> &RuntimeContext {
        &self.runtime
    }

    pub fn platform(&self) -> &Platform {
        &self.platform
    }

    pub fn packages(&self, config: &Config) -> Result<Vec<Package>> {
        self.platform.to_packages(&config.platform)
    }

    pub(crate) fn with_php_binary(mut self, php_binary: std::path::PathBuf) -> Self {
        self.runtime.php_binary = php_binary;
        self
    }
}
