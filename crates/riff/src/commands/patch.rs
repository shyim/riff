use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use riff_core::config::Config;
use riff_core::json::{RiffLockfile, RiffManifest};
use riff_core::output::style;
use riff_core::patch::{
    begin_patch_edit, cleanup_patch_edit, commit_patch_edit, desired_patch_fingerprints,
    ensure_applied_patch_state_current, invalidate_applied_patch_state, native_declarations,
    read_applied_patch_state, read_native_lock, read_patch_edit, relock_compatibility,
    relock_native, remove_native_patches, NATIVE_PATCH_LOCK_FILE,
};
use riff_core::repository::InstalledRepository;
use riff_core::{Package, Riff, RiffBuilder};

use crate::CommandContext;

#[derive(Debug, usage_rs::Args)]
pub struct PatchArgs {
    /// Show the edit workspace that would be created without creating it
    #[usage(long)]
    pub dry_run: bool,

    /// Installed package name, optionally followed by an exact version
    #[usage(
        arg,
        name = "PACKAGE",
        complete = crate::commands::completion::complete_patch_package
    )]
    pub package: String,

    /// Parent directory for the immutable source and writable user trees
    #[usage(long, value_name = "DIR")]
    pub edit_dir: Option<PathBuf>,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug, usage_rs::Args)]
pub struct PatchCommitArgs {
    /// Show the patch commit and reinstall that would run without changing files
    #[usage(long)]
    pub dry_run: bool,

    /// Writable edit directory printed by `riff patch`
    #[usage(arg, name = "DIR")]
    pub edit_dir: PathBuf,

    /// Project-relative directory for newly generated patch files
    #[usage(long, value_name = "DIR", default = "patches")]
    pub patches_dir: PathBuf,
}

#[derive(Debug, usage_rs::Args)]
pub struct PatchRemoveArgs {
    /// Show the patch declarations and packages that would be restored
    #[usage(long)]
    pub dry_run: bool,

    /// Native patch selectors (`vendor/package@version`); omit to remove all
    #[usage(
        arg,
        name = "PATCH",
        complete = crate::commands::completion::complete_patch_selector
    )]
    pub patches: Vec<String>,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug, usage_rs::Args)]
pub struct PatchesRelockArgs {
    /// Show patch locks that would be regenerated without writing them
    #[usage(long)]
    pub dry_run: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug, usage_rs::Args)]
pub struct PatchesRepatchArgs {
    /// Show packages whose patch state would be invalidated and reinstalled
    #[usage(long)]
    pub dry_run: bool,

    /// Patched package names; omit to reinstall every patched package
    #[usage(
        arg,
        name = "PACKAGE",
        complete = crate::commands::completion::complete_patched_package
    )]
    pub packages: Vec<String>,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug, usage_rs::Args)]
pub struct PatchesDoctorArgs {
    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

struct PatchProject {
    root: PathBuf,
    vendor_dir: PathBuf,
    riff: Riff,
    installed: Vec<Package>,
    locked: Vec<Package>,
}

pub async fn execute_patch(args: PatchArgs, context: &CommandContext) -> Result<i32> {
    let project = load_project(&args.working_dir, context)?;
    let effective_installed = effective_installed_packages(&project.installed, &project.locked);
    let desired = desired_patch_fingerprints(&project.riff, &effective_installed).await?;
    ensure_applied_patch_state_current(&project.vendor_dir, &desired)?;
    if args.dry_run {
        let parent = args.edit_dir.as_deref().map_or_else(
            || riff_core::cache::runtime_cache_dir().join("patch-edits"),
            |path| {
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    project.root.join(path)
                }
            },
        );
        riff_core::outln!(
            context.output(),
            "{} Running in dry-run mode",
            style("Info:").cyan()
        );
        riff_core::outln!(
            context.output(),
            "{} Would create an editable snapshot for {} in {}",
            style("Info:").cyan(),
            args.package,
            parent.display()
        );
        return Ok(0);
    }
    let edit = begin_patch_edit(
        &project.root,
        &project.vendor_dir,
        &effective_installed,
        &project.riff.manifest.extra,
        &args.package,
        args.edit_dir.as_deref(),
    )?;

    riff_core::outln!(
        context.output(),
        "{} Edit {}",
        style("Patch:").green().bold(),
        edit.user_dir.display()
    );
    riff_core::outln!(
        context.output(),
        "When finished, run `riff patch-commit {}`.",
        shell_quote_path(&edit.user_dir)
    );
    Ok(0)
}

pub async fn execute_patch_commit(args: PatchCommitArgs, context: &CommandContext) -> Result<i32> {
    let edit = read_patch_edit(&args.edit_dir)?;
    if args.dry_run {
        riff_core::outln!(
            context.output(),
            "{} Running in dry-run mode",
            style("Info:").cyan()
        );
        riff_core::outln!(context.output(), "{} Would generate a patch for {} and update composer.json, {} and installed package contents", style("Info:").cyan(), edit.selector, NATIVE_PATCH_LOCK_FILE);
        return Ok(0);
    }
    let project = load_project(&edit.project, context)?;
    let packages = lock_or_installed(&project);
    let result = commit_patch_edit(&args.edit_dir, &args.patches_dir, packages)?;
    riff_core::outln!(
        context.output(),
        "{} {} {} at {}",
        style("Patch:").green().bold(),
        if result.appended {
            "Updated"
        } else {
            "Created"
        },
        result.selector,
        result.patch_path.display()
    );

    let code = crate::install::reconcile_after_patch(project.root.clone(), context).await?;
    if code == 0 {
        if let Err(error) = cleanup_patch_edit(&args.edit_dir) {
            riff_core::errln!(context.output(),
                "Warning: patch was installed, but the edit snapshot could not be removed: {error:#}"
            );
        }
    } else {
        riff_core::errln!(
            context.output(),
            "Patch files were committed, but installation failed; the edit snapshot remains at {}",
            args.edit_dir.display()
        );
    }
    Ok(code)
}

pub async fn execute_patch_remove(args: PatchRemoveArgs, context: &CommandContext) -> Result<i32> {
    let project = load_project(&args.working_dir, context)?;
    if args.dry_run {
        let selectors = if args.patches.is_empty() {
            native_declarations(&project.riff.manifest.extra)?
                .into_iter()
                .map(|declaration| declaration.selector)
                .collect::<Vec<_>>()
        } else {
            args.patches.clone()
        };
        riff_core::outln!(
            context.output(),
            "{} Running in dry-run mode",
            style("Info:").cyan()
        );
        for selector in selectors {
            riff_core::outln!(
                context.output(),
                "  {} Would remove {selector}",
                style("-").red()
            );
        }
        riff_core::outln!(
            context.output(),
            "{} composer.json, patch locks, and affected vendor packages would be updated",
            style("Info:").cyan()
        );
        return Ok(0);
    }
    let result = remove_native_patches(&project.root, &args.patches, lock_or_installed(&project))?;
    for selector in &result.selectors {
        riff_core::outln!(
            context.output(),
            "{} Removed {selector}",
            style("Patch:").green().bold()
        );
    }
    for path in &result.deleted_files {
        riff_core::outln!(context.output(), "  - Deleted {}", path.display());
    }
    for path in &result.preserved_files {
        riff_core::outln!(
            context.output(),
            "  - Preserved shared file {}",
            path.display()
        );
    }
    for warning in &result.warnings {
        riff_core::warnln!(context.output(), "Warning: {warning}");
    }
    crate::install::reconcile_after_patch(project.root, context).await
}

pub async fn execute_patches_relock(
    args: PatchesRelockArgs,
    context: &CommandContext,
) -> Result<i32> {
    let project = load_project(&args.working_dir, context)?;
    let packages = lock_or_installed(&project);
    let native_count = native_declarations(&project.riff.manifest.extra)?.len();
    if args.dry_run {
        riff_core::outln!(
            context.output(),
            "{} Running in dry-run mode",
            style("Info:").cyan()
        );
        if native_count > 0 || project.root.join(NATIVE_PATCH_LOCK_FILE).exists() {
            riff_core::outln!(
                context.output(),
                "{} {} would be regenerated",
                style("Info:").cyan(),
                NATIVE_PATCH_LOCK_FILE
            );
        }
        riff_core::outln!(
            context.output(),
            "{} Composer-compatible patch locks would be regenerated without writing them",
            style("Info:").cyan()
        );
        return Ok(0);
    }
    if native_count > 0 || project.root.join(NATIVE_PATCH_LOCK_FILE).exists() {
        let lock = relock_native(&project.root, &project.riff.manifest.extra, packages)?;
        riff_core::outln!(
            context.output(),
            "{} Locked {} native patch{} in {}",
            style("Patch:").green().bold(),
            lock.patches.len(),
            plural(lock.patches.len()),
            NATIVE_PATCH_LOCK_FILE
        );
    }
    match relock_compatibility(&project.riff, packages).await? {
        Some(result) if result.legacy => riff_core::outln!(
            context.output(),
            "{} Validated {} legacy Composer patch{} (legacy mode has no lock file)",
            style("Patch:").green().bold(),
            result.patch_count,
            plural(result.patch_count)
        ),
        Some(result) => riff_core::outln!(
            context.output(),
            "{} Locked {} Composer-compatible patch{} in patches.lock.json",
            style("Patch:").green().bold(),
            result.patch_count,
            plural(result.patch_count)
        ),
        None if native_count == 0 => riff_core::outln!(
            context.output(),
            "{} No native or Composer-compatible patches are configured.",
            style("Info:").cyan()
        ),
        None => {}
    }
    riff_core::outln!(
        context.output(),
        "Run `riff install` to reconcile changed patch fingerprints."
    );
    Ok(0)
}

pub async fn execute_patches_repatch(
    args: PatchesRepatchArgs,
    context: &CommandContext,
) -> Result<i32> {
    let project = load_project(&args.working_dir, context)?;
    let effective_installed = effective_installed_packages(&project.installed, &project.locked);
    let desired = desired_patch_fingerprints(&project.riff, &effective_installed).await?;
    if desired.is_empty() {
        bail!("no installed packages have patches to reapply");
    }
    let packages = if args.packages.is_empty() {
        desired.keys().cloned().collect::<Vec<_>>()
    } else {
        let mut packages = Vec::with_capacity(args.packages.len());
        for input in &args.packages {
            let name = package_name(input)?;
            if !desired.contains_key(&name) {
                bail!("installed package {name} has no configured patches");
            }
            packages.push(name);
        }
        packages.sort();
        packages.dedup();
        packages
    };
    if args.dry_run {
        riff_core::outln!(
            context.output(),
            "{} Running in dry-run mode",
            style("Info:").cyan()
        );
        for package in &packages {
            riff_core::outln!(
                context.output(),
                "  {} Would reinstall {package}",
                style("~").yellow()
            );
        }
        riff_core::outln!(
            context.output(),
            "{} Patch state and vendor contents would be updated",
            style("Info:").cyan()
        );
        return Ok(0);
    }
    invalidate_applied_patch_state(&project.vendor_dir, &packages)?;
    riff_core::outln!(
        context.output(),
        "{} Reinstalling {} patched package{}",
        style("Patch:").green().bold(),
        packages.len(),
        plural(packages.len())
    );
    crate::install::reconcile_after_patch(project.root, context).await
}

pub async fn execute_patches_doctor(
    args: PatchesDoctorArgs,
    context: &CommandContext,
) -> Result<i32> {
    let project = load_project(&args.working_dir, context)?;
    let declarations = native_declarations(&project.riff.manifest.extra)?;
    let native_lock = read_native_lock(&project.root)?;
    if declarations.is_empty() {
        riff_core::outln!(
            context.output(),
            "{} No native patch declarations",
            style("OK").green().bold()
        );
    } else {
        let lock = native_lock.as_ref().context(format!(
            "{} native patch declaration{} exist but {} is missing",
            declarations.len(),
            plural_s(declarations.len()),
            NATIVE_PATCH_LOCK_FILE
        ))?;
        riff_core::outln!(
            context.output(),
            "{} {} native declaration{} and {} locked entr{}",
            style("OK").green().bold(),
            declarations.len(),
            plural_s(declarations.len()),
            lock.patches.len(),
            if lock.patches.len() == 1 { "y" } else { "ies" }
        );
    }

    let effective_installed = effective_installed_packages(&project.installed, &project.locked);
    let desired = desired_patch_fingerprints(&project.riff, &effective_installed).await?;
    let applied = read_applied_patch_state(&project.vendor_dir);
    let changed = riff_core::patch::changed_patch_packages(&applied, &desired);
    if !changed.is_empty() {
        riff_core::errln!(
            context.output(),
            "{} Installed patch state is stale for: {}",
            style("FAIL").red().bold(),
            changed.into_iter().collect::<Vec<_>>().join(", ")
        );
        return Ok(1);
    }

    for package in desired.keys() {
        let path = project.vendor_dir.join(package);
        if !path.is_dir() {
            riff_core::errln!(
                context.output(),
                "{} Patched package {} is missing at {}",
                style("FAIL").red().bold(),
                package,
                path.display()
            );
            return Ok(1);
        }
    }
    riff_core::outln!(
        context.output(),
        "{} {} installed patched package{} match the lock fingerprints",
        style("OK").green().bold(),
        desired.len(),
        plural(desired.len())
    );
    riff_core::outln!(
        context.output(),
        "{} Pure-Rust patch engine is active",
        style("OK").green().bold()
    );
    Ok(0)
}

fn load_project(working_dir: &Path, context: &CommandContext) -> Result<PatchProject> {
    let root = working_dir.canonicalize().with_context(|| {
        format!(
            "Failed to resolve working directory {}",
            working_dir.display()
        )
    })?;
    let manifest_path = root.join("composer.json");
    let manifest = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("No readable composer.json found in {}", root.display()))?;
    let manifest: RiffManifest = serde_json::from_str(&manifest)
        .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;
    let lock_path = root.join("composer.lock");
    let lock = if lock_path.is_file() {
        Some(
            serde_json::from_slice::<RiffLockfile>(&std::fs::read(&lock_path)?)
                .with_context(|| format!("Failed to parse {}", lock_path.display()))?,
        )
    } else {
        None
    };
    let locked = lock
        .iter()
        .flat_map(|lock| lock.packages.iter().chain(&lock.packages_dev))
        .map(Package::from)
        .collect::<Vec<_>>();
    let config = Config::build(Some(&root), true)?;
    let vendor_dir = root.join(&config.vendor_dir);
    let installed = InstalledRepository::new(&vendor_dir)
        .load_transaction_packages()
        .map_err(anyhow::Error::msg)?
        .into_iter()
        .map(|package| package.as_ref().clone())
        .collect();
    let riff = RiffBuilder::new(root.clone())
        .with_config(config)
        .with_manifest(manifest)
        .with_lockfile(lock)
        .with_platform(context.platform().clone())
        .with_runtime(context.runtime().clone())
        .with_output(context.output().clone())
        .build()?;
    Ok(PatchProject {
        root,
        vendor_dir,
        riff,
        installed,
        locked,
    })
}

fn effective_installed_packages(installed: &[Package], locked: &[Package]) -> Vec<Package> {
    installed
        .iter()
        .map(|installed| {
            locked
                .iter()
                .find(|locked| {
                    locked.name.eq_ignore_ascii_case(&installed.name)
                        && locked.version == installed.version
                })
                .cloned()
                .unwrap_or_else(|| installed.clone())
        })
        .collect()
}

fn lock_or_installed(project: &PatchProject) -> &[Package] {
    if project.locked.is_empty() {
        &project.installed
    } else {
        &project.locked
    }
}

fn package_name(input: &str) -> Result<String> {
    let input = input.trim();
    let name = input
        .rsplit_once('@')
        .filter(|(name, version)| name.contains('/') && !version.is_empty())
        .map_or(input, |(name, _)| name);
    if !name.contains('/') {
        bail!("package {input:?} must use vendor/package or vendor/package@version");
    }
    Ok(name.to_lowercase())
}

fn shell_quote_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "es"
    }
}

fn plural_s(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_optional_version_from_package_name() {
        assert_eq!(
            package_name("Vendor/Package@1.2.3").unwrap(),
            "vendor/package"
        );
        assert_eq!(package_name("vendor/package").unwrap(), "vendor/package");
    }

    #[test]
    fn quotes_edit_paths_for_shell_copying() {
        assert_eq!(shell_quote_path(Path::new("a b/c")), "'a b/c'");
        assert_eq!(shell_quote_path(Path::new("a'b")), "'a'\\''b'");
    }
}
