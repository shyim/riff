use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use riff_core::config::ConfigLoader;
use riff_core::json::RiffManifest;

#[derive(Debug, usage_rs::Args)]
pub struct GlobalArgs {
    /// Command to execute in Composer's global home
    #[usage(value_name = "COMMAND-NAME")]
    pub command_name: String,

    /// Arguments passed to the selected command
    #[usage(arg, double_dash = "automatic")]
    pub args: Vec<String>,

    /// Do not ask interactive questions
    #[usage(short = 'n', long)]
    pub no_interaction: bool,
}

pub fn execute(args: GlobalArgs) -> Result<i32> {
    let home = ConfigLoader::new(true).get_composer_home();
    prepare_home(&home)?;
    riff_core::outln!("Changed current directory to {}", home.display());

    let dynamic_script = is_script_command(&home, &args.command_name)?;
    let executable = std::env::current_exe().context("Failed to locate Riff executable")?;
    let mut command = Command::new(executable);
    command.env_remove("COMPOSER");
    if dynamic_script {
        command.arg("run").arg(&args.command_name);
    } else {
        command.arg(&args.command_name);
    }
    command.args(&args.args);
    if args.no_interaction && accepts_no_interaction(&args.command_name) {
        command.arg("--no-interaction");
    }
    command.arg("-d").arg(&home);
    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("Failed to run global command {}", args.command_name))?;
    Ok(status.code().unwrap_or(1))
}

fn prepare_home(home: &Path) -> Result<()> {
    if home.exists() && !home.is_dir() {
        bail!("{} exists and is not a directory.", home.display());
    }
    fs::create_dir_all(home)
        .with_context(|| format!("Failed to create global home {}", home.display()))?;
    let manifest = home.join("composer.json");
    if !manifest.exists() {
        fs::write(&manifest, "{}\n")
            .with_context(|| format!("Failed to create {}", manifest.display()))?;
    }
    Ok(())
}

fn is_script_command(home: &Path, command: &str) -> Result<bool> {
    let manifest: RiffManifest = serde_json::from_slice(&fs::read(home.join("composer.json"))?)?;
    Ok(manifest.scripts.custom.contains_key(command))
}

fn accepts_no_interaction(command: &str) -> bool {
    matches!(
        command,
        "install" | "update" | "require" | "remove" | "create-project" | "init"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from Composer\Test\Command\GlobalCommandTest::testCannotCreateHome.
    #[test]
    fn composer_global_rejects_a_home_path_that_is_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("file");
        fs::write(&home, "").unwrap();
        let error = prepare_home(&home).unwrap_err().to_string();
        assert_eq!(
            error,
            format!("{} exists and is not a directory.", home.display())
        );
    }
}
