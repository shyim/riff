//! Run command - execute scripts defined in composer.json.

use anyhow::{Context, Result};
use console::style;
use std::path::PathBuf;

use sonata_core::json::ComposerJson;

use crate::platform::AppContext;
use sonata_core::scripts;

#[derive(usage_rs::Args, Debug)]
pub struct RunArgs {
    /// Script name to run
    #[usage(value_name = "SCRIPT")]
    pub script: Option<String>,

    /// List available scripts
    #[usage(short = 'l', long)]
    pub list: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,

    /// Arguments passed to the script
    #[usage(arg, double_dash = "automatic")]
    pub args: Vec<String>,
}

pub async fn execute(args: RunArgs, context: &AppContext) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;

    // Load composer.json
    let json_path = working_dir.join("composer.json");
    if !json_path.exists() {
        eprintln!(
            "{} No composer.json found in {}",
            style("Error:").red().bold(),
            working_dir.display()
        );
        return Ok(1);
    }

    let content = std::fs::read_to_string(&json_path)?;
    let composer_json: ComposerJson = serde_json::from_str(&content)?;

    // If --list or no script specified, show available scripts
    if args.list || args.script.is_none() {
        return scripts::list_scripts(&composer_json);
    }

    let script_name = args.script.as_ref().unwrap();

    // Run the script
    scripts::run_script(
        script_name,
        &composer_json,
        &working_dir,
        &args.args,
        context.runtime(),
    )
}
