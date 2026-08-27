use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use riff_core::config::Config;
use riff_core::json::{RiffLockfile, RiffManifest};
use riff_core::{Riff, RiffBuilder};
use serde_json::Value;

use crate::CommandContext;

#[derive(Debug, usage_rs::Args)]
pub struct RecipesArgs {
    /// Package to inspect; all recipe entries are shown when omitted
    #[usage(arg)]
    pub package: Option<String>,

    /// Show only recipes whose latest compatible definition differs
    #[usage(short = 'o', long)]
    pub outdated: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug, usage_rs::Args)]
pub struct RecipesInstallArgs {
    /// Recipes to install; all missing recipes are installed when omitted
    #[usage(arg)]
    pub packages: Vec<String>,

    /// Overwrite files managed by the selected recipes
    #[usage(long)]
    pub force: bool,

    /// Reset selected recipes to their latest compatible definitions
    #[usage(long)]
    pub reset: bool,

    /// Accept recipe configuration prompts
    #[usage(long)]
    pub yes: bool,

    /// Disable all plugins
    #[usage(long)]
    pub no_plugins: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug, usage_rs::Args)]
pub struct RecipesUpdateArgs {
    /// Installed recipe to update
    #[usage(arg)]
    pub package: String,

    /// Do not display changelog information
    #[usage(long)]
    pub no_changelog: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug, usage_rs::Args)]
pub struct DumpEnvArgs {
    /// Application environment to compile, for example prod
    #[usage(arg)]
    pub env: Option<String>,

    /// Ignore dotenv contents and only write the environment name
    #[usage(long)]
    pub empty: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub async fn recipes(args: RecipesArgs, context: &CommandContext) -> Result<i32> {
    let riff = load_riff(&args.working_dir, context, true)?;
    let entries = riff_core::plugin::flex::inspect_recipes(&riff, args.package.as_deref()).await?;
    if entries.is_empty() {
        riff_core::errln!(context.output(), "No recipe found");
        return Ok(1);
    }

    if entries.len() == 1 {
        let entry = &entries[0];
        let status = if entry.auto_generated {
            "auto-generated recipe"
        } else if entry.installed_recipe_ref.is_none() {
            "recipe not installed"
        } else if entry.is_outdated() {
            "update available"
        } else {
            "up to date"
        };
        riff_core::outln!(context.output(), "name             : {}", entry.name);
        riff_core::outln!(
            context.output(),
            "version          : {}",
            entry.package_version
        );
        riff_core::outln!(context.output(), "status           : {status}");
        if let Some(url) = &entry.installed_recipe_url {
            riff_core::outln!(context.output(), "installed recipe : {url}");
        }
        if entry.is_outdated() {
            if let Some(url) = &entry.latest_recipe_url {
                riff_core::outln!(context.output(), "latest recipe    : {url}");
            }
        }
        if !entry.files.is_empty() {
            riff_core::outln!(context.output(), "files            :");
            for file in &entry.files {
                riff_core::outln!(context.output(), "  - {file}");
            }
        }
        return Ok(0);
    }

    let mut shown = 0;
    for entry in entries {
        if args.outdated && !entry.is_outdated() {
            continue;
        }
        shown += 1;
        let status = if entry.auto_generated {
            " (auto-generated recipe)"
        } else if entry.installed_recipe_ref.is_none() {
            " (recipe not installed)"
        } else if entry.is_outdated() {
            " (update available)"
        } else {
            ""
        };
        riff_core::outln!(context.output(), " * {}{status}", entry.name);
    }
    Ok(if args.outdated && shown > 0 { 1 } else { 0 })
}

pub async fn install_recipes(args: RecipesInstallArgs, context: &CommandContext) -> Result<i32> {
    let force = args.force || args.reset;
    let riff = load_riff(&args.working_dir, context, !args.no_plugins)?;
    if args.yes {
        std::env::set_var("SYMFONY_ALLOW_CONTRIB", "1");
    }
    let count = riff_core::plugin::flex::install_recipes(&riff, &args.packages, force).await?;
    if count == 0 {
        riff_core::outln!(context.output(), "No recipes to install.");
    } else {
        riff_core::successln!(context.output(), "Success: {count} recipes installed.");
    }
    Ok(0)
}

pub async fn update_recipe(args: RecipesUpdateArgs, context: &CommandContext) -> Result<i32> {
    let working_dir = canonical_dir(&args.working_dir)?;
    ensure_clean_git_index(&working_dir)?;
    let riff = load_riff(&args.working_dir, context, true)?;
    let result = riff_core::plugin::flex::update_recipe(&riff, &args.package).await?;
    if result.up_to_date {
        riff_core::outln!(
            context.output(),
            "The recipe for {} is already up to date.",
            args.package
        );
        return Ok(0);
    }
    riff_core::successln!(
        context.output(),
        "Success: Recipe for {} updated.",
        args.package
    );
    if result.conflicted_files.is_empty() {
        stage_recipe_update(&working_dir, &result.changed_files)?;
        if result.changed_files.is_empty() {
            riff_core::outln!(
                context.output(),
                "No project files changed as a result of the update."
            );
        } else {
            riff_core::outln!(
                context.output(),
                "Use git diff --cached to review the recipe changes."
            );
        }
    } else {
        riff_core::errln!(
            context.output(),
            "The recipe was updated with conflicts in:"
        );
        for file in &result.conflicted_files {
            riff_core::errln!(context.output(), "  - {file}");
        }
        riff_core::errln!(
            context.output(),
            "Resolve the conflict markers, then stage the files normally."
        );
    }
    if !result.skipped_deleted_files.is_empty() {
        riff_core::outln!(
            context.output(),
            "The following locally deleted or shared files were not updated:"
        );
        for file in &result.skipped_deleted_files {
            riff_core::outln!(context.output(), "  - {file}");
        }
    }
    if result.copies_from_package {
        riff_core::outln!(
            context.output(),
            "Note: copy-from-package paths are not changed automatically by recipe updates."
        );
    }
    if !args.no_changelog && !result.changed_files.is_empty() {
        riff_core::outln!(
            context.output(),
            "Review the staged diff for the recipe changelog."
        );
    }
    Ok(0)
}

fn ensure_clean_git_index(working_dir: &Path) -> Result<()> {
    let inside = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(working_dir)
        .output()
        .context("Cannot run recipes:update: git was not found")?;
    if !inside.status.success() {
        bail!("Cannot run recipes:update outside a Git working tree");
    }
    let status = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(working_dir)
        .status()
        .context("Failed to inspect the Git index")?;
    match status.code() {
        Some(0) => Ok(()),
        Some(1) => bail!(
            "Cannot run recipes:update: the Git index contains uncommitted changes; commit or stash them first"
        ),
        _ => bail!("Cannot run recipes:update: failed to inspect the Git index"),
    }
}

fn stage_recipe_update(working_dir: &Path, changed_files: &[String]) -> Result<()> {
    let mut files = changed_files.to_vec();
    files.push("symfony.lock".to_owned());
    let status = Command::new("git")
        .arg("add")
        .arg("--")
        .args(&files)
        .current_dir(working_dir)
        .status()
        .context("Failed to stage the recipe update")?;
    if !status.success() {
        bail!("Failed to stage the recipe update");
    }
    Ok(())
}

pub fn dump_env(args: DumpEnvArgs, context: &CommandContext) -> Result<i32> {
    let working_dir = canonical_dir(&args.working_dir)?;
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(working_dir.join("composer.json"))?)?;
    let root = manifest
        .pointer("/extra/symfony/root-dir")
        .and_then(Value::as_str)
        .unwrap_or(".");
    let dotenv = manifest
        .pointer("/extra/runtime/dotenv_path")
        .and_then(Value::as_str)
        .unwrap_or(".env");
    let env_key = manifest
        .pointer("/extra/runtime/env_var_name")
        .and_then(Value::as_str)
        .unwrap_or("APP_ENV");
    let env = args.env.or_else(|| {
        manifest
            .pointer("/extra/runtime/env")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    if args.empty && env.is_none() {
        bail!("Please provide the environment name when using --empty");
    }
    let php = r#"
$root = getenv('RIFF_DUMP_ROOT');
$path = $root.'/'.getenv('RIFF_DOTENV_PATH');
$envKey = getenv('RIFF_ENV_KEY');
$env = getenv('RIFF_ENV') ?: null;
$empty = getenv('RIFF_EMPTY') === '1';
require getenv('RIFF_VENDOR_AUTOLOAD');
if (!class_exists('Symfony\\Component\\Dotenv\\Dotenv')) {
    fwrite(STDERR, "symfony/dotenv is required to dump the environment\n"); exit(1);
}
if ($empty) {
    $vars = [$envKey => $env];
} else {
    $server = $_SERVER; $environment = $_ENV;
    unset($_SERVER[$envKey]); $_ENV = [$envKey => $env];
    try {
        $dotenv = new Symfony\Component\Dotenv\Dotenv();
        if (!$env && is_file($path.'.local')) {
            $env = $_ENV[$envKey] = $dotenv->parse(file_get_contents($path.'.local'), $path.'.local')[$envKey] ?? null;
        }
        if (!$env) { fwrite(STDERR, "Please provide an environment name or define $envKey in .env.local\n"); exit(1); }
        $dotenv->loadEnv($path, $envKey, 'dev', ['test']);
        unset($_ENV['SYMFONY_DOTENV_VARS'], $_ENV['SYMFONY_DOTENV_PATH']);
        $vars = $_ENV;
    } finally { $_SERVER = $server; $_ENV = $environment; }
}
$export = var_export($vars, true);
$contents = "<?php\n\n// This file was generated by running \"riff dump-env $env\"\n\nreturn $export;\n";
file_put_contents($path.'.local.php', $contents, LOCK_EX);
"#;
    let config = Config::build(Some(&working_dir), true)?;
    let root = if Path::new(root).is_absolute() {
        PathBuf::from(root)
    } else {
        working_dir.join(root)
    };
    let status = Command::new(&context.runtime().php_binary)
        .arg("-r")
        .arg(php)
        .env("RIFF_DUMP_ROOT", &root)
        .env("RIFF_DOTENV_PATH", dotenv)
        .env("RIFF_ENV_KEY", env_key)
        .env("RIFF_ENV", env.as_deref().unwrap_or_default())
        .env("RIFF_EMPTY", if args.empty { "1" } else { "0" })
        .env(
            "RIFF_VENDOR_AUTOLOAD",
            config.get_vendor_dir().join("autoload.php"),
        )
        .current_dir(&working_dir)
        .status()
        .context("Failed to execute PHP for dump-env")?;
    if !status.success() {
        return Ok(status.code().unwrap_or(1));
    }
    riff_core::successln!(
        context.output(),
        "Successfully dumped .env files in .env.local.php"
    );
    Ok(0)
}

fn load_riff(working_dir: &Path, context: &CommandContext, plugins_enabled: bool) -> Result<Riff> {
    let working_dir = canonical_dir(working_dir)?;
    let manifest: RiffManifest =
        serde_json::from_slice(&std::fs::read(working_dir.join("composer.json"))?)?;
    let lock: RiffLockfile = serde_json::from_slice(
        &std::fs::read(working_dir.join("composer.lock")).context("No composer.lock file found")?,
    )?;
    let config = Config::build(Some(&working_dir), true)?;
    RiffBuilder::new(working_dir)
        .with_config(config)
        .with_manifest(manifest)
        .with_lockfile(Some(lock))
        .with_platform(context.platform().clone())
        .with_runtime(context.runtime().clone())
        .with_output(context.output().clone())
        .plugins_enabled(plugins_enabled)
        .build()
}

fn canonical_dir(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .context("Failed to resolve working directory")
}

#[cfg(test)]
mod tests {
    #[test]
    fn generic_cli_commands_do_not_depend_on_flex() {
        let generic_sources = [
            include_str!("../add.rs"),
            include_str!("../update.rs"),
            include_str!("run.rs"),
            include_str!("archive.rs"),
            include_str!("status.rs"),
        ];
        for source in generic_sources {
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            for forbidden in ["symfony/flex", "symfony_flex", "FlexPlan", "SYMFONY_FLEX"] {
                assert!(
                    !production.contains(forbidden),
                    "generic CLI module contains Flex-specific identifier {forbidden}"
                );
            }
        }
    }
}
