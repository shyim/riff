use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use riff_core::json::RiffLockfile;
use riff_core::repository::InstalledRepository;

#[derive(Debug, usage_rs::Args)]
pub struct BumpArgs {
    /// Restrict bumping to matching package names
    #[usage(arg)]
    pub packages: Vec<String>,

    /// Only bump requirements in require-dev
    #[usage(short = 'D', long)]
    pub dev_only: bool,

    /// Only bump requirements in require
    #[usage(short = 'R', long)]
    pub no_dev_only: bool,

    /// Show changes without writing composer.json
    #[usage(long)]
    pub dry_run: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub fn execute(args: BumpArgs) -> Result<i32> {
    if args.dev_only && args.no_dev_only {
        bail!("--dev-only and --no-dev-only cannot be combined");
    }
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let manifest_path = working_dir.join("composer.json");
    if !manifest_path.is_file() || fs::File::open(&manifest_path).is_err() {
        riff_core::errln!("./composer.json is not readable.");
        return Ok(1);
    }
    if !is_writable(&manifest_path) {
        riff_core::errln!("./composer.json is not writable.");
        return Ok(1);
    }

    let mut versions = installed_versions(&working_dir)?;
    if !args.packages.is_empty() {
        let patterns: Vec<_> = args
            .packages
            .iter()
            .map(|package| {
                package
                    .split([':', '=', ' '])
                    .next()
                    .unwrap_or(package)
                    .to_lowercase()
            })
            .collect();
        versions.retain(|package, _| {
            patterns
                .iter()
                .any(|pattern| package_name_matches(pattern, package))
        });
    }
    let mode = if args.dev_only {
        "dev"
    } else if args.no_dev_only {
        "no-dev"
    } else {
        "all"
    };
    crate::update::bump_after_update(&working_dir, mode, &versions, &HashMap::new(), args.dry_run)
}

fn installed_versions(working_dir: &Path) -> Result<HashMap<String, String>> {
    let lock_path = working_dir.join("composer.lock");
    if lock_path.is_file() {
        let lock: RiffLockfile = serde_json::from_slice(&fs::read(&lock_path)?)
            .with_context(|| format!("Failed to parse {}", lock_path.display()))?;
        return Ok(lock
            .all_packages()
            .map(|package| (package.name.to_lowercase(), package.version.clone()))
            .collect());
    }
    let repository = InstalledRepository::new(working_dir.join("vendor"));
    Ok(repository
        .load_transaction_packages()
        .map_err(anyhow::Error::msg)?
        .into_iter()
        .map(|package| {
            (
                package.name.to_lowercase(),
                package.pretty_version().to_owned(),
            )
        })
        .collect())
}

fn package_name_matches(pattern: &str, package: &str) -> bool {
    let mut pattern = pattern.bytes().peekable();
    let mut package = package.bytes().peekable();
    let mut wildcard = None;
    let mut retry = None;
    loop {
        match (pattern.peek().copied(), package.peek().copied()) {
            (Some(b'*'), _) => {
                pattern.next();
                wildcard = Some(pattern.clone());
                retry = Some(package.clone());
            }
            (Some(left), Some(right)) if left.eq_ignore_ascii_case(&right) => {
                pattern.next();
                package.next();
            }
            (None, None) => return true,
            _ if wildcard.is_some() => {
                pattern = wildcard.clone().expect("wildcard exists");
                let mut next = retry.clone().expect("retry exists");
                if next.next().is_none() {
                    return pattern.all(|byte| byte == b'*');
                }
                retry = Some(next.clone());
                package = next;
            }
            _ => return false,
        }
    }
}

fn is_writable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = path.metadata() {
            if metadata.permissions().mode() & 0o222 == 0 {
                return false;
            }
        }
    }
    fs::OpenOptions::new().write(true).open(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from Composer\Test\Command\BumpCommandTest::
    // testBumpFailsOnWriteErrorToComposerFile. The permission-bit check keeps
    // this deterministic under the root user, where Composer skips the test.
    #[cfg(unix)]
    #[test]
    fn composer_bump_rejects_an_unwritable_manifest() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let manifest = directory.path().join("composer.json");
        fs::write(&manifest, "{}").unwrap();
        let mut permissions = fs::metadata(&manifest).unwrap().permissions();
        permissions.set_mode(0o444);
        fs::set_permissions(&manifest, permissions).unwrap();
        assert!(!is_writable(&manifest));
    }

    #[test]
    fn package_filters_support_wildcards_and_inline_constraints() {
        assert!(package_name_matches("dev/*", "dev/pkg"));
        assert!(!package_name_matches("dev/*", "first/pkg"));
        assert!(package_name_matches("first/pkg", "first/pkg"));
    }
}
