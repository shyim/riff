//! Create a new project from a Composer package.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use riff_core::cache::runtime_cache_dir;
use riff_core::config::Config;
use riff_core::downloader::{DownloadConfig, DownloadManager};
use riff_core::http::HttpClient;
use riff_core::repository::{ComposerRepository, RepositoryManager};
use riff_core::{json::Repository as JsonRepository, Package};
use riff_semver::{Semver, VersionParser};

use crate::add::select_recommended_package;
use crate::install::{self, InstallArgs};
use crate::CommandContext;

#[derive(usage_rs::Args, Debug)]
pub struct CreateProjectArgs {
    /// Package to install as the project root
    #[usage(
        arg,
        name = "PACKAGE",
        complete = crate::commands::completion::complete_available_package
    )]
    pub package: String,

    /// Directory to create (defaults to the package basename)
    #[usage(arg, name = "DIRECTORY")]
    pub directory: Option<PathBuf>,

    /// Package version or constraint
    #[usage(arg, name = "VERSION")]
    pub version: Option<String>,

    /// Repository URL, local packages.json file, or inline repository JSON
    #[usage(long, value_name = "REPOSITORY")]
    pub repository: Vec<String>,

    /// Prefer source installation (git clone)
    #[usage(long)]
    pub prefer_source: bool,

    /// Prefer dist installation (archive download)
    #[usage(long)]
    pub prefer_dist: bool,

    /// Installation preference: dist, source, or auto
    #[usage(
        long,
        value_name = "PREFERENCE",
        complete = crate::commands::completion::complete_prefer_install
    )]
    pub prefer_install: Option<String>,

    /// Do not install the created project's dependencies
    #[usage(long)]
    pub no_install: bool,

    /// Skip dev dependencies
    #[usage(long)]
    pub no_dev: bool,

    /// Skip script execution
    #[usage(long)]
    pub no_scripts: bool,

    /// Disable all plugins
    #[usage(long)]
    pub no_plugins: bool,

    /// Deprecated alias of --no-blocking
    #[usage(long)]
    pub no_security_blocking: bool,

    /// Disable all dependency policy blocking
    #[usage(long)]
    pub no_blocking: bool,

    /// Do not ask any interactive question
    #[usage(short = 'n', long)]
    pub no_interaction: bool,

    /// Increase verbosity (-v, -vv, -vvv)
    #[usage(short = 'v', long, count)]
    pub verbose: u8,
}

pub async fn execute(args: CreateProjectArgs, context: &CommandContext) -> Result<i32> {
    let current_dir = std::env::current_dir().context("Failed to resolve current directory")?;
    let target = project_target(&current_dir, &args.package, args.directory.as_deref())?;
    validate_target(&target)?;

    let (prefer_source, prefer_dist) = install_preference(
        args.prefer_source,
        args.prefer_dist,
        args.prefer_install.as_deref(),
    )?;
    let mut repositories = RepositoryManager::new();
    if args.repository.is_empty() {
        repositories.add_repository(Arc::new(ComposerRepository::packagist_with_cache(
            runtime_cache_dir(),
        )));
    } else {
        for repository in &args.repository {
            add_repository(&mut repositories, repository, &current_dir)?;
        }
    }

    let config = Config::build(Some(&current_dir), true)?;
    let platform_packages = context.packages(&config)?;
    let package = resolve_project_package(
        &repositories,
        &args.package,
        args.version.as_deref(),
        &platform_packages,
    )
    .await?;
    let package = repositories.hydrate_package(&package);

    let parent = target
        .parent()
        .context("Project directory must have a parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create {}", parent.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".riff-create-project-")
        .tempdir_in(parent)
        .context("Failed to create project staging directory")?;
    let manager = DownloadManager::new(
        Arc::new(HttpClient::new()?),
        DownloadConfig {
            base_dir: current_dir,
            prefer_source,
            prefer_dist,
            cache_dir: runtime_cache_dir(),
            vendor_dir: staging.path().to_path_buf(),
        },
    );

    let reference = package_reference(&package, args.verbose);
    let method = if should_use_source(&package, prefer_source, prefer_dist) {
        "Cloning"
    } else {
        "Extracting archive"
    };
    riff_core::outln!(
        "- Installing {} ({}): {}{}",
        package.pretty_name(),
        package.pretty_version(),
        method,
        reference
            .as_deref()
            .map(|reference| format!(" {reference}"))
            .unwrap_or_default()
    );
    let downloaded = manager.download(&package).await?;

    if target.exists() {
        std::fs::remove_dir(&target)
            .with_context(|| format!("Failed to prepare empty directory {}", target.display()))?;
    }
    tokio::fs::rename(&downloaded.path, &target)
        .await
        .with_context(|| format!("Failed to create project in {}", target.display()))?;

    if args.no_install {
        return Ok(0);
    }

    install::execute(
        InstallArgs {
            packages: Vec::new(),
            dev: false,
            no_suggest: false,
            no_install: false,
            prefer_source: args.prefer_source,
            prefer_dist: args.prefer_dist,
            prefer_install: args.prefer_install,
            dry_run: false,
            download_only: false,
            no_dev: args.no_dev,
            no_autoloader: false,
            no_scripts: args.no_scripts,
            no_plugins: args.no_plugins,
            no_security_blocking: args.no_security_blocking,
            no_blocking: args.no_blocking,
            optimize_autoloader: false,
            classmap_authoritative: false,
            apcu_autoloader: false,
            apcu_autoloader_prefix: None,
            ignore_platform_reqs: false,
            ignore_platform_req: Vec::new(),
            working_dir: target,
            no_interaction: args.no_interaction,
            verbose: args.verbose,
            no_audit: false,
            audit_format: "summary".to_string(),
        },
        context,
    )
    .await
}

fn project_target(current_dir: &Path, package: &str, directory: Option<&Path>) -> Result<PathBuf> {
    let directory = match directory {
        Some(directory) => directory.to_path_buf(),
        None => PathBuf::from(
            package
                .rsplit_once('/')
                .map_or(package, |(_, basename)| basename),
        ),
    };
    if directory.as_os_str().is_empty() {
        bail!("Project directory cannot be empty");
    }
    Ok(if directory.is_absolute() {
        directory
    } else {
        current_dir.join(directory)
    })
}

fn validate_target(target: &Path) -> Result<()> {
    if !target.exists() {
        return Ok(());
    }
    if !target.is_dir() {
        bail!("Project path {} is not a directory", target.display());
    }
    if std::fs::read_dir(target)?.next().is_some() {
        bail!("Project directory {} is not empty", target.display());
    }
    Ok(())
}

fn install_preference(
    prefer_source: bool,
    prefer_dist: bool,
    prefer_install: Option<&str>,
) -> Result<(bool, bool)> {
    if prefer_source && prefer_dist {
        bail!("--prefer-source and --prefer-dist cannot be used together");
    }
    if (prefer_source || prefer_dist) && prefer_install.is_some() {
        bail!("--prefer-install cannot be combined with --prefer-source or --prefer-dist");
    }
    match prefer_install {
        None => Ok((prefer_source, prefer_dist)),
        Some("source") => Ok((true, false)),
        Some("dist") => Ok((false, true)),
        Some("auto") => Ok((false, false)),
        Some(value) => bail!("Unsupported installation preference '{value}'"),
    }
}

fn add_repository(
    manager: &mut RepositoryManager,
    repository: &str,
    current_dir: &Path,
) -> Result<()> {
    let repository = repository.trim();
    if repository.starts_with('{') {
        let config: JsonRepository = serde_json::from_str(repository)
            .context("Failed to parse inline repository configuration")?;
        manager.add_from_json_repository_at(&config, current_dir);
        return Ok(());
    }

    let path = Path::new(repository);
    let url = if !repository.contains("://") && current_dir.join(path).exists() {
        let path = current_dir
            .join(path)
            .canonicalize()
            .with_context(|| format!("Failed to resolve repository {repository}"))?;
        format!("file://{}", path.to_string_lossy())
    } else {
        repository.to_string()
    };
    manager.add_repository(Arc::new(ComposerRepository::new(repository, url)));
    Ok(())
}

async fn resolve_project_package(
    repositories: &RepositoryManager,
    name: &str,
    constraint: Option<&str>,
    platform_packages: &[Package],
) -> Result<Arc<Package>> {
    let candidates = repositories.find_packages(name).await;
    let candidates: Vec<_> = match constraint {
        None => candidates,
        Some(constraint) => {
            let normalized = VersionParser::new().normalize(constraint).ok();
            candidates
                .into_iter()
                .filter(|package| {
                    package.version == constraint
                        || package.pretty_version() == constraint
                        || normalized.as_deref() == Some(package.version.as_str())
                        || Semver::satisfies(&package.version, constraint)
                })
                .collect()
        }
    };
    let package = select_recommended_package(&candidates, platform_packages).with_context(
        || match constraint {
            Some(constraint) => format!("Could not find package {name} matching {constraint}"),
            None => format!("Could not find package {name}"),
        },
    )?;
    candidates
        .iter()
        .find(|candidate| candidate.name == package.name && candidate.version == package.version)
        .cloned()
        .context("Selected project package disappeared")
}

fn should_use_source(package: &Package, prefer_source: bool, prefer_dist: bool) -> bool {
    if prefer_source {
        return package.source.is_some();
    }
    if prefer_dist {
        return false;
    }
    package.is_dev() && package.source.is_some()
}

fn package_reference(package: &Package, verbose: u8) -> Option<String> {
    let reference = package
        .source
        .as_ref()
        .map(|source| source.reference.as_str())
        .or_else(|| {
            package
                .dist
                .as_ref()
                .and_then(|dist| dist.reference.as_deref())
        })?;
    let length = if package.is_dev() && verbose > 0 {
        usize::MAX
    } else {
        10
    };
    Some(reference.chars().take(length).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use riff_core::package::Source;

    #[test]
    fn default_directory_uses_package_basename() {
        assert_eq!(
            project_target(Path::new("/work"), "vendor/project", None).unwrap(),
            Path::new("/work/project")
        );
    }

    #[test]
    fn composer_create_project_shows_full_dev_reference_when_verbose() {
        let mut package = Package::new("vendor/project", "dev-main");
        package.pretty_version = Some("dev-main".into());
        package.source = Some(Source::new(
            "git",
            "https://example.com/project.git",
            "4451f2066efdc53f3fa954c44a47ead73f6838d2",
        ));

        assert_eq!(
            package_reference(&package, 1).as_deref(),
            Some("4451f2066efdc53f3fa954c44a47ead73f6838d2")
        );
        assert_eq!(
            package_reference(&package, 0).as_deref(),
            Some("4451f2066e")
        );
    }
}
