use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Result, RiffError};

/// Represents the source of a configuration value
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Default built-in value
    Default,
    /// From global config (~/.composer/config.json or COMPOSER_HOME)
    Global,
    /// From project composer.json
    Project,
    /// From environment variable
    Environment(String),
    /// Programmatically set
    Command,
    /// A caller-provided configuration source name
    Named(String),
    /// Unknown source
    Unknown,
}

impl ConfigSource {
    pub fn as_str(&self) -> &str {
        match self {
            ConfigSource::Default => "default",
            ConfigSource::Global => "global",
            ConfigSource::Project => "project",
            ConfigSource::Environment(var) => var,
            ConfigSource::Command => "command",
            ConfigSource::Named(name) => name,
            ConfigSource::Unknown => "unknown",
        }
    }
}

/// Raw configuration data that can be loaded from JSON files
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<HashMap<String, serde_json::Value>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repositories: Option<serde_json::Value>,
}

/// Loads configuration from various sources
#[derive(Debug)]
pub struct ConfigLoader {
    use_environment: bool,
}

impl ConfigLoader {
    pub fn new(use_environment: bool) -> Self {
        Self { use_environment }
    }

    /// Get COMPOSER_* environment variable
    pub fn get_composer_env(&self, var: &str) -> Option<String> {
        if !self.use_environment {
            return None;
        }

        env::var(var).ok().filter(|s| !s.is_empty())
    }

    /// Resolve the manifest selected by `COMPOSER`, relative to a project directory.
    pub fn get_composer_file<P: AsRef<Path>>(&self, project_dir: P) -> Result<PathBuf> {
        let project_dir = project_dir.as_ref();
        let configured = self.get_composer_env("COMPOSER");
        let file = resolve_composer_file(configured.as_deref())?;
        let resolved = if file.is_absolute() {
            file
        } else {
            project_dir.join(file)
        };
        if configured.is_some() && resolved.is_dir() {
            return Err(RiffError::Config(format!(
                "The COMPOSER environment variable is set to {} which is a directory, this variable should point to a composer.json or be left unset.",
                resolved.display()
            )));
        }
        Ok(resolved)
    }

    /// Get the composer home directory
    pub fn get_composer_home(&self) -> PathBuf {
        // Check COMPOSER_HOME env var first
        if let Some(home) = self.get_composer_env("COMPOSER_HOME") {
            return PathBuf::from(home);
        }

        // Fallback to platform-specific directories
        if let Some(proj_dirs) = directories::ProjectDirs::from("", "", "composer") {
            proj_dirs.config_dir().to_path_buf()
        } else {
            // Ultimate fallback to ~/.composer
            if let Some(home_dir) = directories::BaseDirs::new() {
                home_dir.home_dir().join(".composer")
            } else {
                PathBuf::from(".composer")
            }
        }
    }

    /// Get the cache directory
    pub fn get_cache_dir(&self) -> PathBuf {
        // Check COMPOSER_CACHE_DIR env var first
        if let Some(cache) = self.get_composer_env("COMPOSER_CACHE_DIR") {
            return PathBuf::from(cache);
        }

        // Use XDG cache dir on Unix, AppData on Windows
        if let Some(proj_dirs) = directories::ProjectDirs::from("", "", "composer") {
            proj_dirs.cache_dir().to_path_buf()
        } else {
            // Fallback to home/.composer/cache
            self.get_composer_home().join("cache")
        }
    }

    /// Load configuration from a JSON file
    pub fn load_config_file<P: AsRef<Path>>(&self, path: P) -> Result<RawConfig> {
        let path = path.as_ref();

        if !path.exists() {
            return Ok(RawConfig::default());
        }

        let contents = fs::read_to_string(path)
            .map_err(|e| RiffError::Config(format!("Failed to read {}: {}", path.display(), e)))?;

        let config: RawConfig = serde_json::from_str(&contents)
            .map_err(|e| RiffError::Config(format!("Failed to parse {}: {}", path.display(), e)))?;

        Ok(config)
    }

    /// Load global configuration from ~/.composer/config.json
    pub fn load_global_config(&self) -> Result<RawConfig> {
        let home = self.get_composer_home();
        let config_file = home.join("config.json");
        self.load_config_file(config_file)
    }

    /// Load project configuration from composer.json
    pub fn load_project_config<P: AsRef<Path>>(&self, project_dir: P) -> Result<RawConfig> {
        let manifest = self.get_composer_file(project_dir)?;

        if !manifest.exists() {
            return Ok(RawConfig::default());
        }

        self.load_config_file(manifest)
    }

    /// Get a configuration value from environment variable
    /// Converts "foo-bar" to "COMPOSER_FOO_BAR"
    pub fn get_env_config(&self, key: &str) -> Option<String> {
        let env_var = format!("COMPOSER_{}", key.replace('-', "_").to_uppercase());
        self.get_composer_env(&env_var)
    }

    /// Get boolean value from environment variable
    pub fn get_env_bool(&self, key: &str) -> Option<bool> {
        self.get_env_config(key)
            .map(|val| !matches!(val.to_lowercase().as_str(), "false" | "0" | ""))
    }

    /// Get integer value from environment variable
    pub fn get_env_int(&self, key: &str) -> Option<i64> {
        self.get_env_config(key).and_then(|val| val.parse().ok())
    }

    /// Get unsigned integer value from environment variable
    pub fn get_env_u64(&self, key: &str) -> Option<u64> {
        self.get_env_config(key).and_then(|val| val.parse().ok())
    }

    /// Get a path value from environment variable
    pub fn get_env_path(&self, key: &str) -> Option<PathBuf> {
        self.get_env_config(key).map(PathBuf::from)
    }
}

/// Resolve Composer's manifest filename without mutating or reading process environment.
pub fn resolve_composer_file(configured: Option<&str>) -> Result<PathBuf> {
    let Some(configured) = configured.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(PathBuf::from("./composer.json"));
    };
    let path = PathBuf::from(configured);
    if path.is_dir() {
        return Err(RiffError::Config(format!(
            "The COMPOSER environment variable is set to {configured} which is a directory, this variable should point to a composer.json or be left unset."
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_source_as_str() {
        assert_eq!(ConfigSource::Default.as_str(), "default");
        assert_eq!(ConfigSource::Global.as_str(), "global");
        assert_eq!(ConfigSource::Project.as_str(), "project");
        assert_eq!(ConfigSource::Command.as_str(), "command");
        assert_eq!(
            ConfigSource::Named("phpunit-test".into()).as_str(),
            "phpunit-test"
        );
        assert_eq!(ConfigSource::Unknown.as_str(), "unknown");
        assert_eq!(
            ConfigSource::Environment("COMPOSER_HOME".to_string()).as_str(),
            "COMPOSER_HOME"
        );
    }

    #[test]
    fn test_config_loader_new() {
        let loader = ConfigLoader::new(true);
        assert!(loader.use_environment);

        let loader = ConfigLoader::new(false);
        assert!(!loader.use_environment);
    }

    #[test]
    fn test_get_composer_home() {
        let loader = ConfigLoader::new(false);
        let home = loader.get_composer_home();
        assert!(home.is_absolute() || home.starts_with(".composer"));
    }

    #[test]
    fn test_get_cache_dir() {
        let loader = ConfigLoader::new(false);
        let cache = loader.get_cache_dir();
        assert!(cache.is_absolute() || cache.ends_with("cache"));
    }

    // Ported from Composer\Test\FactoryTest::testGetComposerJsonPath.
    #[test]
    fn composer_factory_uses_default_manifest_path() {
        assert_eq!(
            resolve_composer_file(None).unwrap().display().to_string(),
            "./composer.json"
        );
    }

    // Ported from Composer\Test\FactoryTest::testGetComposerJsonPathFromEnv.
    #[test]
    fn composer_factory_trims_manifest_path_from_environment() {
        assert_eq!(
            resolve_composer_file(Some(" foo.json ")).unwrap(),
            PathBuf::from("foo.json")
        );
    }

    // Ported from Composer\Test\FactoryTest::testGetComposerJsonPathFailsIfDir.
    #[test]
    fn composer_factory_rejects_directory_manifest_path() {
        let directory = tempfile::tempdir().unwrap();
        let error = resolve_composer_file(directory.path().to_str()).unwrap_err();
        assert!(error.to_string().contains(&format!(
            "The COMPOSER environment variable is set to {} which is a directory",
            directory.path().display()
        )));
    }
}
