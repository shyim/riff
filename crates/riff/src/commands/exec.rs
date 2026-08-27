use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use riff_core::config::Config;
use riff_core::json::RiffManifest;

#[derive(Debug, usage_rs::Args)]
pub struct ExecArgs {
    /// List available binaries
    #[usage(short = 'l', long)]
    pub list: bool,

    /// Binary to execute
    #[usage(complete = crate::commands::completion::complete_binary)]
    pub binary: Option<String>,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,

    /// Arguments passed to the binary
    #[usage(arg, double_dash = "automatic")]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BinaryEntry {
    display: String,
    executable: PathBuf,
}

pub fn execute(args: ExecArgs, context: &crate::CommandContext) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let manifest_path = working_dir.join("composer.json");
    let manifest: RiffManifest = serde_json::from_slice(
        &fs::read(&manifest_path)
            .with_context(|| format!("Failed to read {}", manifest_path.display()))?,
    )?;
    let config = Config::build(Some(&working_dir), true)?;
    let binaries = available_binaries(&working_dir, &config.get_bin_dir(), &manifest)?;

    if args.list || args.binary.is_none() {
        if binaries.is_empty() {
            bail!(
                "No binaries found in composer.json or in bin-dir ({})",
                config.get_bin_dir().display()
            );
        }
        riff_core::outln!(context.output(), "Available binaries:");
        for binary in &binaries {
            riff_core::outln!(context.output(), "- {}", binary.display);
        }
        return Ok(0);
    }

    let requested = args.binary.as_deref().unwrap_or_default();
    let executable = binaries
        .iter()
        .find(|entry| {
            entry.display.trim_end_matches(" (local)") == requested
                || entry.executable == Path::new(requested)
        })
        .map(|entry| entry.executable.clone())
        .unwrap_or_else(|| config.get_bin_dir().join(requested));
    if !executable.is_file() {
        bail!("Binary {requested:?} was not found");
    }
    let status = Command::new(&executable)
        .args(args.args)
        .current_dir(&working_dir)
        .status()
        .with_context(|| format!("Failed to execute {}", executable.display()))?;
    Ok(status.code().unwrap_or(1))
}

fn available_binaries(
    working_dir: &Path,
    bin_dir: &Path,
    manifest: &RiffManifest,
) -> Result<Vec<BinaryEntry>> {
    let mut binaries = Vec::new();
    let mut names = HashSet::new();
    if bin_dir.is_dir() {
        let mut entries = fs::read_dir(bin_dir)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if !entry.file_type()?.is_file() && !entry.file_type()?.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".bat") || !names.insert(name.clone()) {
                continue;
            }
            binaries.push(BinaryEntry {
                display: name,
                executable: entry.path(),
            });
        }
    }
    for local in &manifest.bin {
        let path = working_dir.join(local.as_str());
        let Some(name) = Path::new(local.as_str())
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        binaries.push(BinaryEntry {
            display: format!("{name} (local)"),
            executable: path,
        });
    }
    Ok(binaries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_windows_proxy_copies_and_marks_root_binaries() {
        let project = tempfile::tempdir().unwrap();
        let bin_dir = project.path().join("vendor/bin");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("tool"), "").unwrap();
        fs::write(bin_dir.join("tool.bat"), "").unwrap();
        let mut manifest = RiffManifest::default();
        manifest.bin.push("bin/local".into());

        let binaries = available_binaries(project.path(), &bin_dir, &manifest).unwrap();
        assert_eq!(
            binaries
                .iter()
                .map(|binary| binary.display.as_str())
                .collect::<Vec<_>>(),
            ["tool", "local (local)"]
        );
    }
}
