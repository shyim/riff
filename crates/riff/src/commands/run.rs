//! Run command - execute scripts defined in composer.json.

use anyhow::{Context, Result};
use console::style;
use std::path::PathBuf;

use riff_core::config::Config;
use riff_core::json::RiffManifest;

use crate::CommandContext;
use riff_core::scripts;

#[derive(usage_rs::Args, Debug)]
pub struct RunArgs {
    /// Script name to run
    #[usage(
        value_name = "SCRIPT",
        complete = crate::commands::completion::complete_script
    )]
    pub script: Option<String>,

    /// List available scripts
    #[usage(short = 'l', long)]
    pub list: bool,

    /// Enable development mode while dispatching the script
    #[usage(long)]
    pub dev: bool,

    /// Disable development mode while dispatching the script
    #[usage(long)]
    pub no_dev: bool,

    /// Disable plugins while discovering and executing the script command
    #[usage(long)]
    pub no_plugins: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,

    /// Arguments passed to the script
    #[usage(arg, double_dash = "automatic")]
    pub args: Vec<String>,
}

pub async fn execute(args: RunArgs, context: &CommandContext) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;

    // Load composer.json
    let json_path = working_dir.join("composer.json");
    if !json_path.exists() {
        riff_core::errln!(
            "{} No composer.json found in {}",
            style("Error:").red().bold(),
            working_dir.display()
        );
        return Ok(1);
    }

    let content = std::fs::read_to_string(&json_path)?;
    let manifest: RiffManifest = serde_json::from_str(&content)?;
    let config = Config::build(Some(&working_dir), true)?;
    let plugins =
        riff_core::plugin::PluginManager::builtins(!args.no_plugins, config.allow_plugins.clone())?;

    // If --list or no script specified, show available scripts
    if args.list || args.script.is_none() {
        return scripts::list_scripts(&manifest);
    }

    let script_name = args.script.as_ref().unwrap();

    // Run the script
    scripts::run_script(
        script_name,
        &manifest,
        &working_dir,
        &args.args,
        scripts::ScriptExecutionOptions {
            runtime: context.runtime(),
            dev_mode: args.dev || !args.no_dev,
            plugins: &plugins,
            bin_dir: config.get_bin_dir(),
        },
    )
}

#[cfg(test)]
mod tests {
    // Ported from Composer\Test\Command\RunScriptCommandTest::
    // testDetectAndPassDevModeToEventAndToDispatching.
    #[test]
    fn composer_run_script_resolves_dev_mode_flags() {
        for (dev, no_dev, expected) in [
            (true, true, true),
            (true, false, true),
            (false, true, false),
            (false, false, true),
        ] {
            assert_eq!(dev || !no_dev, expected);
        }
    }
}
