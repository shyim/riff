use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

use super::package_metadata::{PackageMetadata, ProjectPackageMetadata};

#[derive(Debug, usage_rs::Args)]
pub struct HomeArgs {
    /// Package or packages to browse to
    #[usage(
        arg,
        name = "PACKAGE",
        complete = crate::commands::completion::complete_installed_package
    )]
    pub packages: Vec<String>,

    /// Open the homepage instead of the repository URL
    #[usage(short = 'H', long)]
    pub homepage: bool,

    /// Only show the homepage or repository URL
    #[usage(short = 's', long)]
    pub show: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub async fn execute(args: HomeArgs, context: &crate::CommandContext) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let metadata = ProjectPackageMetadata::load(&working_dir).await?;
    let mut packages = args.packages;
    if packages.is_empty() {
        riff_core::warnln!(
            context.output(),
            "No package specified, opening homepage for the root package"
        );
        packages.push(metadata.root.name.clone());
    }

    let mut exit_code = 0;
    for package_name in packages {
        let matching = metadata.matching(&package_name).collect::<Vec<_>>();
        if matching.is_empty() {
            riff_core::warnln!(context.output(), "Package {package_name} not found");
            exit_code = 1;
        }

        let url = matching
            .into_iter()
            .filter_map(|package| package_url(package, args.homepage))
            .find(|url| valid_url(url));
        if let Some(url) = url {
            if args.show {
                riff_core::outln!(context.output(), "{url}");
            } else if let Err(error) = open_browser(url) {
                riff_core::warnln!(
                    context.output(),
                    "No suitable browser opening command found, open yourself: {url} ({error})"
                );
            }
        } else {
            let property = if args.homepage {
                "homepage"
            } else {
                "repository URL"
            };
            riff_core::warnln!(
                context.output(),
                "Invalid or missing {property} for {package_name}"
            );
            exit_code = 1;
        }
    }

    Ok(exit_code)
}

fn package_url(package: &PackageMetadata, homepage: bool) -> Option<&str> {
    if homepage {
        return package.homepage.as_deref();
    }
    package
        .support_source
        .as_deref()
        .or(package.source_url.as_deref())
        .or(package.homepage.as_deref())
}

fn valid_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok()
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> Result<()> {
    let status = Command::new("cmd")
        .args(["/C", "start", "", url])
        .status()
        .context("failed to start the Windows browser command")?;
    anyhow::ensure!(status.success(), "browser command exited with {status}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> Result<()> {
    let status = Command::new("open")
        .arg(url)
        .status()
        .context("failed to start open")?;
    anyhow::ensure!(status.success(), "open exited with {status}");
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_browser(url: &str) -> Result<()> {
    let status = Command::new("xdg-open")
        .arg(url)
        .status()
        .context("failed to start xdg-open")?;
    anyhow::ensure!(status.success(), "xdg-open exited with {status}");
    Ok(())
}

#[cfg(not(any(unix, target_os = "windows")))]
fn open_browser(_url: &str) -> Result<()> {
    anyhow::bail!("browser opening is not supported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package() -> PackageMetadata {
        PackageMetadata {
            name: "vendor/package".to_string(),
            homepage: Some("https://example.org/home".to_string()),
            source_url: Some("https://example.org/repository".to_string()),
            support_source: Some("https://example.org/support-source".to_string()),
            funding: Vec::new(),
            default_branch: false,
        }
    }

    #[test]
    fn repository_source_precedes_source_url_and_homepage() {
        let package = package();
        assert_eq!(
            package_url(&package, false),
            Some("https://example.org/support-source")
        );
        assert_eq!(
            package_url(&package, true),
            Some("https://example.org/home")
        );
    }
}
