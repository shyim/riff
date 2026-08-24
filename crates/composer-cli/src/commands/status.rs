use std::collections::BTreeMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::platform::AppContext;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sonata_core::config::{AuthConfig, Config};
use sonata_core::downloader::{ArchiveExtractor, ArchiveType, FileDownloader};
use sonata_core::http::HttpClient;
use sonata_core::json::ComposerJson;
use sonata_core::scripts::run_event_script;

const EXIT_ERRORS: i32 = 1;
const EXIT_UNPUSHED: i32 = 2;
const EXIT_VERSION_CHANGES: i32 = 4;

#[derive(usage_rs::Args, Debug)]
pub struct StatusArgs {
    /// Show the modified files for each dependency
    #[usage(short = 'v', long, count)]
    pub verbose: u8,

    /// Do not run pre-status-cmd and post-status-cmd scripts
    #[usage(long)]
    pub no_scripts: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct InstalledRepository {
    #[serde(default)]
    packages: Vec<InstalledPackage>,
}

#[derive(Debug, Deserialize)]
struct InstalledPackage {
    name: String,
    version: String,
    #[serde(default)]
    version_normalized: String,
    #[serde(rename = "type", default = "default_package_type")]
    package_type: String,
    #[serde(rename = "installation-source")]
    installation_source: Option<String>,
    source: Option<PackageSource>,
    dist: Option<PackageDist>,
    #[serde(rename = "install-path")]
    install_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PackageSource {
    #[serde(rename = "type")]
    source_type: String,
    reference: String,
}

#[derive(Debug, Deserialize)]
struct PackageDist {
    #[serde(rename = "type")]
    dist_type: String,
    url: String,
    reference: Option<String>,
    #[serde(default)]
    shasum: String,
}

#[derive(Debug)]
struct VersionChange {
    previous_version: String,
    previous_ref: String,
    current_version: String,
    current_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SnapshotEntry {
    File(u64),
    Symlink(PathBuf),
}

pub async fn execute(args: StatusArgs, context: &AppContext) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let composer_path = working_dir.join("composer.json");
    if !composer_path.exists() {
        bail!(
            "Composer could not find a composer.json file in {}",
            working_dir.display()
        );
    }
    let composer_content = fs::read_to_string(&composer_path)?;
    let composer_json: ComposerJson =
        serde_json::from_str(&composer_content).context("Failed to parse composer.json")?;

    if !args.no_scripts {
        let code = run_event_script(
            "pre-status-cmd",
            &composer_json,
            &working_dir,
            false,
            context.runtime(),
        )?;
        if code != 0 {
            return Ok(code);
        }
    }

    let config = Config::build(Some(&working_dir), true)?;
    let vendor_dir = config.get_vendor_dir();
    let composer_dir = vendor_dir.join("composer");
    let installed_path = composer_dir.join("installed.json");
    let packages = if installed_path.exists() {
        load_installed_packages(&installed_path)?
    } else {
        Vec::new()
    };

    let auth = AuthConfig::build(Some(&working_dir))?;
    let http_client = Arc::new(HttpClient::new()?.with_auth(auth));
    let downloader = FileDownloader::new(http_client);
    let cache_dir = config
        .cache_dir
        .clone()
        .unwrap_or_else(|| sonata_core::config::ConfigLoader::new(true).get_cache_dir());

    let mut errors = BTreeMap::new();
    let mut unpushed_changes = BTreeMap::new();
    let mut version_changes = BTreeMap::new();

    for package in packages {
        if package.package_type == "metapackage" {
            continue;
        }
        let Some(target_dir) = install_path(&composer_dir, &package) else {
            continue;
        };
        let display_path = target_dir.display().to_string();

        if fs::symlink_metadata(&target_dir)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            errors.insert(
                display_path.clone(),
                format!("{display_path} is a symbolic link."),
            );
        }

        let installation_source = package
            .installation_source
            .as_deref()
            .or_else(|| package.dist.as_ref().map(|_| "dist"))
            .or_else(|| package.source.as_ref().map(|_| "source"));

        let local_changes = match installation_source {
            Some("source") => source_local_changes(&package, &target_dir),
            Some("dist") => {
                dist_local_changes(&package, &target_dir, &cache_dir, &downloader).await
            }
            _ => Ok(None),
        };
        match local_changes {
            Ok(Some(changes)) => {
                errors.insert(display_path.clone(), changes);
            }
            Ok(None) => {}
            Err(error) => {
                errors.insert(
                    display_path.clone(),
                    format!("Failed to detect changes: {error}"),
                );
            }
        }

        if target_dir.join(".git").exists() {
            if let Some(unpushed) = git_unpushed_changes(&target_dir)? {
                unpushed_changes.insert(display_path.clone(), unpushed);
            }
            if let Some(change) = git_version_change(&package, &target_dir)? {
                version_changes.insert(display_path, change);
            }
        }
    }

    let mut exit_code = 0;
    if errors.is_empty() && unpushed_changes.is_empty() && version_changes.is_empty() {
        eprintln!("No local changes");
    } else {
        if !errors.is_empty() {
            exit_code += EXIT_ERRORS;
            eprintln!("You have changes in the following dependencies:");
            print_changes(&errors, args.verbose > 0);
        }
        if !unpushed_changes.is_empty() {
            exit_code += EXIT_UNPUSHED;
            eprintln!(
                "You have unpushed changes on the current branch in the following dependencies:"
            );
            print_changes(&unpushed_changes, args.verbose > 0);
        }
        if !version_changes.is_empty() {
            exit_code += EXIT_VERSION_CHANGES;
            eprintln!("You have version variations in the following dependencies:");
            for (path, change) in &version_changes {
                if args.verbose > 0 {
                    let mut previous = if change.previous_version.is_empty() {
                        change.previous_ref.clone()
                    } else {
                        change.previous_version.clone()
                    };
                    let mut current = if change.current_version.is_empty() {
                        change.current_ref.clone()
                    } else {
                        change.current_version.clone()
                    };
                    if args.verbose > 1 {
                        previous.push_str(&format!(" ({})", change.previous_ref));
                        current.push_str(&format!(" ({})", change.current_ref));
                    }
                    println!("{path}:");
                    println!("    From {previous} to {current}");
                } else {
                    println!("{path}");
                }
            }
        }
        if args.verbose == 0 {
            eprintln!("Use --verbose (-v) to see a list of files");
        }
    }

    if !args.no_scripts {
        let code = run_event_script(
            "post-status-cmd",
            &composer_json,
            &working_dir,
            false,
            context.runtime(),
        )?;
        if code != 0 {
            return Ok(code);
        }
    }

    Ok(exit_code)
}

fn default_package_type() -> String {
    "library".to_string()
}

fn load_installed_packages(path: &Path) -> Result<Vec<InstalledPackage>> {
    let content = fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    if value.is_array() {
        return Ok(serde_json::from_value(value)?);
    }
    Ok(serde_json::from_value::<InstalledRepository>(value)?.packages)
}

fn install_path(composer_dir: &Path, package: &InstalledPackage) -> Option<PathBuf> {
    package
        .install_path
        .as_deref()
        .map(|path| normalize_path(&composer_dir.join(path)))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn source_local_changes(package: &InstalledPackage, path: &Path) -> Result<Option<String>> {
    let Some(source) = &package.source else {
        return Ok(None);
    };
    let metadata_path = match source.source_type.as_str() {
        "git" => ".git",
        "hg" => ".hg",
        "svn" => ".svn",
        "fossil" => ".fslckout",
        _ => return Ok(None),
    };
    if !path.join(metadata_path).exists() {
        return Ok(None);
    }
    let command = match source.source_type.as_str() {
        "git" => vec!["status", "--porcelain", "--untracked-files=no"],
        "hg" => vec!["st"],
        "svn" => vec!["status", "--ignore-externals"],
        "fossil" => vec!["changes"],
        _ => return Ok(None),
    };
    let output = run_vcs(&source.source_type, &command, path)?;
    let output = output.trim();
    Ok((!output.is_empty()).then(|| output.to_string()))
}

async fn dist_local_changes(
    package: &InstalledPackage,
    target_dir: &Path,
    cache_dir: &Path,
    downloader: &FileDownloader,
) -> Result<Option<String>> {
    let Some(dist) = &package.dist else {
        return Ok(None);
    };
    let archive_type = archive_type(&dist.dist_type)
        .ok_or_else(|| anyhow::anyhow!("Unsupported archive type {}", dist.dist_type))?;
    let temp = tempfile::tempdir()?;
    let compare_dir = temp.path().join("compare");
    let archive_path = temp
        .path()
        .join(format!("package.{}", archive_extension(archive_type)));
    let cached = cache_archive_path(cache_dir, package, &dist.dist_type);

    if cached.exists() {
        fs::copy(&cached, &archive_path)?;
    } else if let Ok(url) = reqwest::Url::parse(&dist.url) {
        if url.scheme() == "file" {
            let source = url
                .to_file_path()
                .map_err(|_| anyhow::anyhow!("Invalid file URL {}", dist.url))?;
            fs::copy(source, &archive_path)?;
        } else if dist.shasum.is_empty() {
            downloader
                .download(&dist.url, &archive_path, None::<fn(u64, u64)>)
                .await?;
        } else {
            downloader
                .download_verified(&dist.url, &archive_path, &dist.shasum, None::<fn(u64, u64)>)
                .await?;
        }
    } else {
        bail!("Invalid dist URL {}", dist.url);
    }

    ArchiveExtractor::extract_with_type(&archive_path, &compare_dir, archive_type)?;
    let baseline = snapshot_tree(&compare_dir)?;
    let installed = snapshot_tree(target_dir)?;
    let mut changes = Vec::new();

    for (path, baseline_entry) in &baseline {
        match installed.get(path) {
            Some(installed_entry) if installed_entry != baseline_entry => {
                changes.push(format!("./{}", path.display()));
            }
            None => changes.push(format!("./{}", path.display())),
            _ => {}
        }
    }
    for path in installed.keys() {
        if !baseline.contains_key(path) {
            changes.push(format!("./{}", path.display()));
        }
    }

    Ok((!changes.is_empty()).then(|| changes.join("\n")))
}

fn archive_type(value: &str) -> Option<ArchiveType> {
    match value.to_ascii_lowercase().as_str() {
        "zip" => Some(ArchiveType::Zip),
        "tar" => Some(ArchiveType::Tar),
        "tar.gz" | "tgz" | "gzip" => Some(ArchiveType::TarGz),
        "tar.bz2" | "tbz2" | "bzip2" => Some(ArchiveType::TarBz2),
        "tar.xz" | "txz" | "xz" => Some(ArchiveType::TarXz),
        _ => None,
    }
}

fn archive_extension(archive_type: ArchiveType) -> &'static str {
    match archive_type {
        ArchiveType::Zip => "zip",
        ArchiveType::Tar => "tar",
        ArchiveType::TarGz => "tar.gz",
        ArchiveType::TarBz2 => "tar.bz2",
        ArchiveType::TarXz => "tar.xz",
    }
}

fn cache_archive_path(cache_dir: &Path, package: &InstalledPackage, dist_type: &str) -> PathBuf {
    let version = if package.version_normalized.is_empty() {
        &package.version
    } else {
        &package.version_normalized
    };
    cache_dir.join("files").join(&package.name).join(format!(
        "{}-{}.{}",
        package.name.replace('/', "-"),
        version,
        dist_type
    ))
}

fn snapshot_tree(root: &Path) -> Result<BTreeMap<PathBuf, SnapshotEntry>> {
    let mut snapshot = BTreeMap::new();
    if root.exists() {
        snapshot_directory(root, root, &mut snapshot)?;
    }
    Ok(snapshot)
}

fn snapshot_directory(
    root: &Path,
    directory: &Path,
    snapshot: &mut BTreeMap<PathBuf, SnapshotEntry>,
) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        if metadata.file_type().is_symlink() {
            snapshot.insert(relative, SnapshotEntry::Symlink(fs::read_link(&path)?));
        } else if metadata.is_dir() {
            snapshot_directory(root, &path, snapshot)?;
        } else if metadata.is_file() && metadata.len() > 0 {
            let mut hasher = DefaultHasher::new();
            fs::read(&path)?.hash(&mut hasher);
            snapshot.insert(relative, SnapshotEntry::File(hasher.finish()));
        }
    }
    Ok(())
}

fn git_unpushed_changes(path: &Path) -> Result<Option<String>> {
    let branch = run_vcs("git", &["symbolic-ref", "--quiet", "--short", "HEAD"], path);
    let Ok(branch) = branch else {
        return Ok(None);
    };
    let branch = branch.trim();
    if branch.is_empty() {
        return Ok(None);
    }

    let refs = run_vcs(
        "git",
        &["for-each-ref", "--format=%(refname:short)", "refs/remotes"],
        path,
    )?;
    let remote_branches: Vec<&str> = refs
        .lines()
        .filter(|reference| reference.ends_with(&format!("/{branch}")))
        .collect();
    if remote_branches.is_empty() {
        return Ok(Some(format!(
            "Branch {branch} could not be found on any remote and appears to be unpushed"
        )));
    }

    let mut shortest: Option<String> = None;
    for remote in remote_branches {
        let range = format!("{remote}...{branch}");
        let changes = run_vcs("git", &["diff", "--name-status", &range, "--"], path)?;
        let changes = changes.trim().replace('\t', "");
        if !changes.is_empty()
            && shortest
                .as_ref()
                .is_none_or(|current| changes.len() < current.len())
        {
            shortest = Some(changes);
        }
    }
    Ok(shortest)
}

fn git_version_change(package: &InstalledPackage, path: &Path) -> Result<Option<VersionChange>> {
    let previous_ref = match package.installation_source.as_deref() {
        Some("source") => package
            .source
            .as_ref()
            .map(|source| source.reference.as_str()),
        Some("dist") => package
            .dist
            .as_ref()
            .and_then(|dist| dist.reference.as_deref()),
        _ => None,
    };
    let Some(previous_ref) = previous_ref else {
        return Ok(None);
    };
    let current_ref = run_vcs("git", &["rev-parse", "HEAD"], path)?
        .trim()
        .to_string();
    if current_ref == previous_ref {
        return Ok(None);
    }

    let current_version = run_vcs("git", &["describe", "--tags", "--exact-match"], path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            run_vcs("git", &["symbolic-ref", "--quiet", "--short", "HEAD"], path)
                .ok()
                .map(|branch| format!("dev-{}", branch.trim()))
        })
        .or_else(|| git_remote_branch_version(path))
        .unwrap_or_default();

    Ok(Some(VersionChange {
        previous_version: package.version.clone(),
        previous_ref: previous_ref.to_string(),
        current_version,
        current_ref,
    }))
}

fn git_remote_branch_version(path: &Path) -> Option<String> {
    let refs = run_vcs(
        "git",
        &[
            "for-each-ref",
            "--format=%(refname)",
            "--contains=HEAD",
            "refs/remotes",
        ],
        path,
    )
    .ok()?;
    refs.lines().find_map(|reference| {
        let branch = reference.strip_prefix("refs/remotes/")?.split_once('/')?.1;
        (branch != "HEAD").then(|| format!("dev-{branch}"))
    })
}

fn run_vcs(program: &str, arguments: &[&str], directory: &Path) -> Result<String> {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(directory)
        .output()
        .with_context(|| format!("Failed to execute {program}"))?;
    if !output.status.success() {
        bail!(
            "Failed to execute {} {}\n\n{}",
            program,
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn print_changes(changes: &BTreeMap<String, String>, verbose: bool) {
    for (path, detail) in changes {
        if verbose {
            println!("{path}:");
            for line in detail.lines() {
                println!("    {}", line.trim_start());
            }
        } else {
            println!("{path}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_composer_relative_install_paths() {
        assert_eq!(
            normalize_path(Path::new("/project/vendor/composer/../vendor/pkg")),
            PathBuf::from("/project/vendor/vendor/pkg")
        );
    }

    #[test]
    fn snapshots_ignore_empty_files_and_detect_content_changes() {
        let baseline = tempfile::tempdir().unwrap();
        let update = tempfile::tempdir().unwrap();
        fs::write(baseline.path().join("same"), "same").unwrap();
        fs::write(update.path().join("same"), "changed").unwrap();
        fs::write(baseline.path().join("empty"), "").unwrap();
        fs::write(update.path().join("empty"), "content").unwrap();

        let baseline = snapshot_tree(baseline.path()).unwrap();
        let update = snapshot_tree(update.path()).unwrap();
        assert_ne!(
            baseline.get(Path::new("same")),
            update.get(Path::new("same"))
        );
        assert!(!baseline.contains_key(Path::new("empty")));
        assert!(update.contains_key(Path::new("empty")));
    }

    #[test]
    fn parses_installed_repository_object_and_legacy_array() {
        let package = r#"{
            "name":"vendor/pkg",
            "version":"1.0.0",
            "type":"metapackage",
            "installation-source":"dist",
            "install-path":null
        }"#;
        let object: InstalledRepository =
            serde_json::from_str(&format!(r#"{{"packages":[{package}]}}"#)).unwrap();
        let array: Vec<InstalledPackage> = serde_json::from_str(&format!("[{package}]")).unwrap();
        assert_eq!(object.packages[0].name, "vendor/pkg");
        assert_eq!(array[0].package_type, "metapackage");
    }
}
