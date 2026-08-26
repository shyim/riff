//! Plugin system for ported Composer plugins.
//!
//! This module provides native Rust implementations of popular Composer plugins.
//! Since riff cannot execute PHP-based Composer plugins, these are manually
//! ported and registered as native capabilities.
//!
//! Plugins register only the capabilities they implement with the sealed
//! `PluginManager`; generic package-manager flows never depend on a specific
//! plugin implementation.

mod composer_bin;
mod composer_patches;
pub(crate) mod manager;
mod php_http_discovery;
mod phpstan_extension_installer;
mod policy;
mod symfony_flex;
mod symfony_runtime;

pub use composer_bin::BinConfig;
pub use manager::{PackageOperation, PluginManager};
pub use policy::PluginPolicy;

/// Explicit Symfony Flex recipe operations used by the recipe CLI commands.
/// Generic package-manager flows go through `PluginManager` instead.
pub mod flex {
    pub use super::symfony_flex::{
        inspect_recipes, install_recipes, read_recipe_lock, update_recipe, RecipeInspection,
        RecipeUpdateResult,
    };
}
