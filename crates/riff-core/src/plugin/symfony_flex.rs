//! Native Symfony Flex integration.
//!
//! The recipe lifecycle lives here as well; auto-script expansion is kept
//! separate because Composer script references dispatch it directly.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;

use crate::json::RiffManifest;
use crate::process::escape_argument;
use crate::runtime::RuntimeContext;

use super::manager::{
    ObjectScriptAction, ObjectScriptHook, OperationHook, PackageArgumentHook, PackageLayoutHook,
    PackageOperation, PluginDescriptor, PluginRegistrar, PreparedPluginOperation, RootManifestHook,
    ScriptPluginContext, SolverConstraintHook, SolverConstraintRule,
};

mod configurator;
mod lock;
mod recipe;
mod synchronizer;

const PACKAGE_NAME: &str = "symfony/flex";

struct SymfonyFlexPlugin;

pub(super) fn register(registrar: &mut PluginRegistrar) {
    let plugin = Arc::new(SymfonyFlexPlugin);
    registrar.descriptor(PluginDescriptor::with_version_validator(
        PACKAGE_NAME,
        validate_version,
    ));
    registrar.package_arguments(PACKAGE_NAME, plugin.clone());
    registrar.root_manifest(PACKAGE_NAME, plugin.clone());
    registrar.solver_constraints(PACKAGE_NAME, plugin.clone());
    registrar.operations(PACKAGE_NAME, plugin.clone());
    registrar.package_layout(PACKAGE_NAME, plugin.clone());
    registrar.object_script(PACKAGE_NAME, "auto-scripts", plugin);
}

fn validate_version(package: &crate::package::Package) -> Result<()> {
    if !package.version.trim_start_matches('v').starts_with("2.") {
        bail!(
            "Composer plugin symfony/flex {} is not supported by riff; only Flex 2.x is supported",
            package.pretty_version()
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct FlexOptions {
    root_dir: PathBuf,
    bin_dir: PathBuf,
    directories: std::collections::HashMap<String, String>,
}

impl FlexOptions {
    pub(super) fn from_manifest(manifest: &RiffManifest) -> Self {
        let mut directories = std::collections::HashMap::from([
            ("BIN_DIR".to_owned(), "bin".to_owned()),
            ("CONF_DIR".to_owned(), "conf".to_owned()),
            ("CONFIG_DIR".to_owned(), "config".to_owned()),
            ("SRC_DIR".to_owned(), "src".to_owned()),
            ("VAR_DIR".to_owned(), "var".to_owned()),
            ("PUBLIC_DIR".to_owned(), "public".to_owned()),
            ("ROOT_DIR".to_owned(), ".".to_owned()),
        ]);

        if let Some(extra) = manifest.extra.as_object() {
            for (key, value) in extra {
                let Some(value) = value.as_str() else {
                    continue;
                };
                let option = key.replace('-', "_").to_ascii_uppercase();
                if directories.contains_key(&option) {
                    directories.insert(option, value.to_owned());
                }
            }
            if let Some(root_dir) = extra
                .get("symfony")
                .and_then(serde_json::Value::as_object)
                .and_then(|symfony| symfony.get("root-dir"))
                .and_then(serde_json::Value::as_str)
            {
                directories.insert("ROOT_DIR".to_owned(), root_dir.to_owned());
            }
        }

        let root_dir = PathBuf::from(&directories["ROOT_DIR"]);
        let bin_dir = PathBuf::from(&directories["BIN_DIR"]);
        Self {
            root_dir,
            bin_dir,
            directories,
        }
    }

    fn expand(&self, value: &str) -> String {
        let mut expanded = value.to_owned();
        for (name, replacement) in &self.directories {
            expanded = expanded.replace(&format!("%{name}%"), replacement.trim_end_matches('/'));
        }
        expanded
    }
}

struct FlexPlan {
    recipes: Vec<recipe::Recipe>,
    lock: lock::FlexLock,
    installed_packages: Vec<String>,
    force: bool,
}

struct FlexSolverRule {
    requirement: String,
    splits: std::collections::HashSet<String>,
}

impl SolverConstraintRule for FlexSolverRule {
    fn rewrite(&self, package: &str, constraint: &str) -> Option<String> {
        (package == "symfony/symfony" || self.splits.contains(package))
            .then(|| format!("({constraint}) {}", self.requirement))
    }
}

struct PreparedFlexOperation(FlexPlan);

impl PreparedPluginOperation for PreparedFlexOperation {
    fn apply(self: Box<Self>, riff: &crate::riff::Riff) -> Result<()> {
        apply(riff, self.0)
    }
}

#[async_trait]
impl PackageArgumentHook for SymfonyFlexPlugin {
    async fn transform_arguments(
        &self,
        riff: &crate::riff::Riff,
        operation: PackageOperation,
        arguments: &[String],
    ) -> Result<Vec<String>> {
        resolve_package_arguments(
            riff,
            arguments,
            matches!(operation, PackageOperation::Require),
        )
        .await
    }
}

#[async_trait]
impl RootManifestHook for SymfonyFlexPlugin {
    async fn transform_manifest(
        &self,
        riff: &mut crate::riff::Riff,
        _operation: PackageOperation,
    ) -> Result<Vec<String>> {
        Ok(unpack_symfony_packs(riff)
            .await?
            .into_iter()
            .map(|package| format!("Unpacked {package}"))
            .collect())
    }
}

#[async_trait]
impl SolverConstraintHook for SymfonyFlexPlugin {
    async fn prepare(
        &self,
        riff: &crate::riff::Riff,
    ) -> Result<Option<Box<dyn SolverConstraintRule>>> {
        Ok(package_filter(riff).await?.map(|(requirement, splits)| {
            Box::new(FlexSolverRule {
                requirement,
                splits,
            }) as Box<dyn SolverConstraintRule>
        }))
    }
}

#[async_trait]
impl OperationHook for SymfonyFlexPlugin {
    async fn prepare(
        &self,
        riff: &crate::riff::Riff,
        transaction: &crate::solver::Transaction,
        desired_packages: &[Arc<crate::package::Package>],
    ) -> Result<Option<Box<dyn PreparedPluginOperation>>> {
        Ok(prepare(riff, transaction, desired_packages)
            .await?
            .map(|plan| Box::new(PreparedFlexOperation(plan)) as Box<dyn PreparedPluginOperation>))
    }
}

impl ObjectScriptHook for SymfonyFlexPlugin {
    fn expand(
        &self,
        configuration: &indexmap::IndexMap<String, serde_json::Value>,
        arguments: &[String],
        context: &ScriptPluginContext<'_>,
    ) -> Result<Option<Vec<ObjectScriptAction>>> {
        if !is_required(context.manifest) {
            return Ok(None);
        }
        let mut actions = Vec::new();
        for (command, command_type) in configuration {
            let Some(command_type) = command_type.as_str() else {
                bail!(
                    "Invalid symfony/flex auto-script in composer.json: command type for '{}' must be a string",
                    command
                );
            };
            match expand_auto_script(
                command_type,
                command,
                context.manifest,
                context.working_dir,
                context.runtime,
                arguments,
            )? {
                Some((display, command)) => {
                    actions.push(ObjectScriptAction::Execute { display, command });
                }
                None => actions.push(ObjectScriptAction::Warning(format!(
                    "Skipping \"{command}\" (needs symfony/console to run)."
                ))),
            }
        }
        Ok(Some(actions))
    }
}

impl PackageLayoutHook for SymfonyFlexPlugin {
    fn is_fileless(&self, package: &crate::package::Package) -> bool {
        package.package_type() == "symfony-pack"
    }
}

/// Download all recipe metadata before package operations start. Applying the
/// prepared plan is deliberately separate so a failed package transaction can
/// never leave project configuration half-written.
async fn prepare(
    riff: &crate::riff::Riff,
    transaction: &crate::solver::Transaction,
    desired_packages: &[std::sync::Arc<crate::package::Package>],
) -> Result<Option<FlexPlan>> {
    if !is_required(&riff.manifest) || !riff.plugins().is_enabled(PACKAGE_NAME) {
        return Ok(None);
    }
    let lock = lock::FlexLock::load(&riff.working_dir)?;
    let mut recipe_transaction = transaction.clone();
    let operation_names = recipe_transaction
        .operations
        .iter()
        .filter_map(|operation| match operation {
            crate::solver::Operation::Install(package)
            | crate::solver::Operation::Reinstall(package)
            | crate::solver::Operation::Uninstall(package) => Some(package.name.clone()),
            crate::solver::Operation::Update { to, .. } => Some(to.name.clone()),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    for package in desired_packages {
        if !lock.has(&package.name) && !operation_names.contains(&package.name) {
            recipe_transaction
                .operations
                .push(crate::solver::Operation::Install(package.clone()));
        }
    }
    let has_recipe_operations =
        recipe_transaction
            .operations
            .iter()
            .any(|operation| match operation {
                crate::solver::Operation::Install(package)
                | crate::solver::Operation::Reinstall(package) => !lock.has(&package.name),
                crate::solver::Operation::Uninstall(package) => lock.has(&package.name),
                _ => false,
            });
    if !has_recipe_operations {
        return Ok(None);
    }

    let downloader = recipe::RecipeDownloader::new(riff).await?;
    let recipes = downloader
        .recipes_for_transaction(&recipe_transaction, &lock, desired_packages)
        .await?;
    let mut installed_packages = riff
        .lockfile
        .iter()
        .flat_map(|lock| lock.packages.iter().chain(&lock.packages_dev))
        .map(|package| package.name.clone())
        .collect::<Vec<_>>();
    installed_packages.extend(desired_packages.iter().map(|package| package.name.clone()));

    Ok(Some(FlexPlan {
        recipes,
        lock,
        installed_packages,
        force: false,
    }))
}

fn apply(riff: &crate::riff::Riff, mut plan: FlexPlan) -> Result<()> {
    let configurator = configurator::Configurator::new(
        &riff.working_dir,
        &riff.manifest,
        riff.vendor_dir(),
        plan.installed_packages,
        plan.force,
    );
    configurator.apply(&plan.recipes, &mut plan.lock)?;
    plan.lock.write()?;
    synchronizer::synchronize(riff)?;
    refresh_composer_lock_hash(&riff.working_dir)?;
    Ok(())
}

async fn package_filter(
    riff: &crate::riff::Riff,
) -> Result<Option<(String, std::collections::HashSet<String>)>> {
    if !is_required(&riff.manifest) || !riff.plugins().is_enabled(PACKAGE_NAME) {
        return Ok(None);
    }
    let requirement = std::env::var("SYMFONY_REQUIRE")
        .ok()
        .filter(|requirement| !requirement.is_empty())
        .or_else(|| {
            riff.manifest
                .extra
                .pointer("/symfony/require")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    let Some(mut requirement) = requirement else {
        return Ok(None);
    };
    if requirement.ends_with(".x") {
        requirement.push_str("-dev");
    }
    let downloader = recipe::RecipeDownloader::new(riff).await?;
    Ok(Some((requirement, downloader.symfony_splits()?)))
}

/// Install recipes for packages that are already present in composer.lock.
/// This backs Flex's `recipes:install`/`sync-recipes` command aliases.
pub async fn install_recipes(
    riff: &crate::riff::Riff,
    package_names: &[String],
    force: bool,
) -> Result<usize> {
    if !is_required(&riff.manifest) || !riff.plugins().is_enabled(PACKAGE_NAME) {
        bail!(
            "Symfony recipes are disabled: symfony/flex is not enabled in the root composer.json"
        );
    }
    let composer_lock = riff
        .lockfile
        .as_ref()
        .context("No composer.lock file found")?;
    let packages = composer_lock
        .packages
        .iter()
        .chain(&composer_lock.packages_dev)
        .map(crate::package::Package::from)
        .map(std::sync::Arc::new)
        .collect::<Vec<_>>();
    let available = packages
        .iter()
        .map(|package| package.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    for name in package_names {
        if !available.contains(name.as_str()) {
            bail!("Package {name} is not installed");
        }
    }

    let selected = packages
        .iter()
        .filter(|package| package_names.is_empty() || package_names.contains(&package.name))
        .cloned()
        .collect::<Vec<_>>();
    let mut lock = lock::FlexLock::load(&riff.working_dir)?;
    let mut transaction = crate::solver::Transaction::new();
    for package in &selected {
        if force {
            lock.remove(&package.name);
        }
        if force || !lock.has(&package.name) {
            transaction
                .operations
                .push(crate::solver::Operation::Install(package.clone()));
        }
    }
    if transaction.operations.is_empty() {
        return Ok(0);
    }

    let downloader = recipe::RecipeDownloader::new(riff).await?;
    let recipes = downloader
        .recipes_for_transaction(&transaction, &lock, &packages)
        .await?;
    let count = recipes.len();
    let configurator = configurator::Configurator::new(
        &riff.working_dir,
        &riff.manifest,
        riff.vendor_dir(),
        packages.iter().map(|package| package.name.clone()),
        force,
    );
    configurator.apply(&recipes, &mut lock)?;
    lock.write()?;
    synchronizer::synchronize(riff)?;
    refresh_composer_lock_hash(&riff.working_dir)?;
    Ok(count)
}

#[derive(Debug, Clone, Default)]
pub struct RecipeUpdateResult {
    pub up_to_date: bool,
    pub changed_files: Vec<String>,
    pub conflicted_files: Vec<String>,
    pub skipped_deleted_files: Vec<String>,
    pub copies_from_package: bool,
}

#[derive(Debug, Clone)]
pub struct RecipeInspection {
    pub name: String,
    pub package_version: String,
    pub installed_recipe_ref: Option<String>,
    pub latest_recipe_ref: Option<String>,
    pub installed_recipe_url: Option<String>,
    pub latest_recipe_url: Option<String>,
    pub files: Vec<String>,
    pub auto_generated: bool,
}

impl RecipeInspection {
    pub fn is_outdated(&self) -> bool {
        self.latest_recipe_ref.is_some() && self.latest_recipe_ref != self.installed_recipe_ref
    }
}

pub async fn inspect_recipes(
    riff: &crate::riff::Riff,
    package_name: Option<&str>,
) -> Result<Vec<RecipeInspection>> {
    if !is_required(&riff.manifest) || !riff.plugins().is_enabled(PACKAGE_NAME) {
        bail!("Symfony recipes are disabled: symfony/flex is not enabled in composer.json");
    }
    let composer_lock = riff
        .lockfile
        .as_ref()
        .context("No composer.lock file found")?;
    let lock = lock::FlexLock::load(&riff.working_dir)?;
    let mut packages = composer_lock
        .packages
        .iter()
        .chain(&composer_lock.packages_dev)
        .map(crate::package::Package::from)
        .map(std::sync::Arc::new)
        .collect::<Vec<_>>();
    let known = packages
        .iter()
        .map(|package| package.name.clone())
        .collect::<std::collections::HashSet<_>>();
    for (name, entry) in lock.all() {
        if known.contains(name) {
            continue;
        }
        let version = entry
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("9999999.9999999");
        packages.push(std::sync::Arc::new(crate::package::Package::new(
            name, version,
        )));
    }
    if let Some(name) = package_name {
        if !packages.iter().any(|package| package.name == name) {
            bail!("Package {name} is not installed");
        }
        packages.retain(|package| package.name == name);
    }
    let mut transaction = crate::solver::Transaction::new();
    transaction.operations.extend(
        packages
            .iter()
            .cloned()
            .map(crate::solver::Operation::Install),
    );
    let mut lookup_lock = lock.clone();
    lookup_lock.clear_for_lookup();
    let downloader = recipe::RecipeDownloader::new(riff).await?;
    let latest = downloader
        .recipes_for_transaction(&transaction, &lookup_lock, &packages)
        .await?
        .into_iter()
        .map(|recipe| (recipe.name.clone(), recipe))
        .collect::<std::collections::HashMap<_, _>>();
    let mut inspections = packages
        .into_iter()
        .filter_map(|package| {
            let installed = lock.get(&package.name);
            let current_recipe = installed.and_then(|entry| entry.get("recipe"));
            let latest_recipe = latest.get(&package.name);
            if installed.is_none() && latest_recipe.is_none() {
                return None;
            }
            Some(RecipeInspection {
                name: package.name.clone(),
                package_version: installed
                    .and_then(|entry| entry.get("version"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_else(|| package.pretty_version())
                    .to_owned(),
                installed_recipe_ref: current_recipe
                    .and_then(|recipe| recipe.get("ref"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                latest_recipe_ref: latest_recipe
                    .and_then(|recipe| recipe.lock.pointer("/recipe/ref"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                installed_recipe_url: current_recipe
                    .and_then(|recipe| recipe_url(&package.name, recipe)),
                latest_recipe_url: latest_recipe
                    .and_then(|recipe| recipe.lock.get("recipe"))
                    .and_then(|recipe| recipe_url(&package.name, recipe)),
                files: installed
                    .and_then(|entry| entry.get("files"))
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                auto_generated: installed.is_some() && current_recipe.is_none(),
            })
        })
        .collect::<Vec<_>>();
    inspections.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(inspections)
}

fn recipe_url(name: &str, recipe: &serde_json::Value) -> Option<String> {
    Some(format!(
        "https://{}/tree/{}/{}/{}",
        recipe.get("repo")?.as_str()?,
        recipe.get("branch")?.as_str()?,
        name,
        recipe.get("version")?.as_str()?
    ))
}

/// Update one installed recipe with a three-way merge. The installed recipe is
/// the merge ancestor, the project file is "ours", and the newest compatible
/// recipe is "theirs". Conflicts are written with standard conflict markers.
pub async fn update_recipe(
    riff: &crate::riff::Riff,
    package_name: &str,
) -> Result<RecipeUpdateResult> {
    if !is_required(&riff.manifest) || !riff.plugins().is_enabled(PACKAGE_NAME) {
        bail!("Symfony recipes are disabled: symfony/flex is not enabled in composer.json");
    }
    let composer_lock = riff
        .lockfile
        .as_ref()
        .context("No composer.lock file found")?;
    let packages = composer_lock
        .packages
        .iter()
        .chain(&composer_lock.packages_dev)
        .map(crate::package::Package::from)
        .map(std::sync::Arc::new)
        .collect::<Vec<_>>();
    let package = packages
        .iter()
        .find(|package| package.name == package_name)
        .cloned()
        .with_context(|| format!("Package {package_name} is not installed"))?;
    let mut lock = lock::FlexLock::load(&riff.working_dir)?;
    let lock_entry = lock
        .get(package_name)
        .with_context(|| format!("Package {package_name} was not found in symfony.lock"))?;
    if lock_entry.get("recipe").is_none() {
        bail!(
            "Package {package_name} has an auto-generated recipe; use recipes:install --force to replace it"
        );
    }

    let downloader = recipe::RecipeDownloader::new(riff).await?;
    let (original, latest) = downloader
        .recipes_for_update(package, &lock, &packages)
        .await?
        .with_context(|| format!("No updatable recipe found for {package_name}"))?;
    let original_ref = original
        .lock
        .pointer("/recipe/ref")
        .and_then(serde_json::Value::as_str);
    let latest_ref = latest
        .lock
        .pointer("/recipe/ref")
        .and_then(serde_json::Value::as_str);
    if original_ref == latest_ref {
        return Ok(RecipeUpdateResult {
            up_to_date: true,
            ..Default::default()
        });
    }

    let installed_names = packages.iter().map(|package| package.name.clone());
    let original_render = configurator::render_recipe(
        &riff.working_dir,
        &riff.manifest,
        riff.vendor_dir(),
        installed_names.clone(),
        &original,
    )?;
    let latest_render = configurator::render_recipe(
        &riff.working_dir,
        &riff.manifest,
        riff.vendor_dir(),
        installed_names,
        &latest,
    )?;
    let shared_files = lock
        .all()
        .iter()
        .filter(|(name, _)| name.as_str() != package_name)
        .filter_map(|(_, entry)| entry.get("files").and_then(serde_json::Value::as_array))
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect::<std::collections::HashSet<_>>();
    let paths = original_render
        .files
        .keys()
        .chain(latest_render.files.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut result = RecipeUpdateResult {
        copies_from_package: original_render.copies_from_package
            || latest_render.copies_from_package,
        ..Default::default()
    };
    let mut changes = Vec::new();
    for relative in paths {
        let ancestor = original_render.files.get(&relative).cloned().flatten();
        let theirs = latest_render.files.get(&relative).cloned().flatten();
        if ancestor == theirs {
            continue;
        }
        let target = safe_project_path(&riff.working_dir, &relative)?;
        let ours = std::fs::read(&target).ok();
        if ours == theirs {
            continue;
        }
        if ours.is_none() && ancestor.is_some() {
            result.skipped_deleted_files.push(relative);
            continue;
        }
        let merged = if ours == ancestor {
            theirs
        } else {
            match diffy::merge_bytes(
                ancestor.as_deref().unwrap_or_default(),
                ours.as_deref().unwrap_or_default(),
                theirs.as_deref().unwrap_or_default(),
            ) {
                Ok(merged) => Some(merged),
                Err(conflicted) => {
                    result.conflicted_files.push(relative.clone());
                    Some(conflicted)
                }
            }
        };
        if merged.is_none() && shared_files.contains(relative.as_str()) {
            result.skipped_deleted_files.push(relative);
            continue;
        }
        result.changed_files.push(relative.clone());
        changes.push((target, merged));
    }

    for (path, contents) in changes {
        if let Some(contents) = contents {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, contents)
                .with_context(|| format!("Failed to update {}", path.display()))?;
        } else if path.is_file() || path.is_symlink() {
            std::fs::remove_file(&path)
                .with_context(|| format!("Failed to remove {}", path.display()))?;
        }
    }
    lock.set(package_name.to_owned(), latest_render.lock_entry);
    lock.write()?;
    synchronizer::synchronize(riff)?;
    refresh_composer_lock_hash(&riff.working_dir)?;
    Ok(result)
}

fn safe_project_path(working_dir: &Path, relative: &str) -> Result<PathBuf> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        bail!("Recipe update path escapes the project: {relative:?}");
    }
    Ok(working_dir.join(relative))
}

pub async fn resolve_package_arguments(
    riff: &crate::riff::Riff,
    arguments: &[String],
    is_require: bool,
) -> Result<Vec<String>> {
    if !is_required(&riff.manifest) || !riff.plugins().is_enabled(PACKAGE_NAME) {
        return Ok(arguments.to_vec());
    }
    let downloader = recipe::RecipeDownloader::new(riff).await?;
    Ok(downloader.resolve_arguments(&riff.manifest, arguments, is_require))
}

/// Replace root `symfony-pack` requirements by the pack's concrete links.
/// Flex calls this "unpacking" and intentionally keeps the resulting links
/// first-class in composer.json.
pub async fn unpack_symfony_packs(riff: &mut crate::riff::Riff) -> Result<Vec<String>> {
    if !is_required(&riff.manifest) || !riff.plugins().is_enabled(PACKAGE_NAME) {
        return Ok(Vec::new());
    }
    let mut unpacked = Vec::new();
    let mut visited = std::collections::HashSet::new();
    loop {
        let requirements = riff
            .manifest
            .require
            .iter()
            .map(|(name, constraint)| (name.clone(), constraint.clone(), false))
            .chain(
                riff.manifest
                    .require_dev
                    .iter()
                    .map(|(name, constraint)| (name.clone(), constraint.clone(), true)),
            )
            .filter(|(name, _, _)| !visited.contains(name))
            .collect::<Vec<_>>();
        if requirements.is_empty() {
            break;
        }
        let mut changed = false;
        for (name, constraint, dev) in requirements {
            visited.insert(name.clone());
            if crate::util::is_platform_package(&name) {
                continue;
            }
            let package = riff
                .repository_manager
                .find_packages(&name)
                .await
                .into_iter()
                .filter(|package| riff_semver::Semver::satisfies(&package.version, &constraint))
                .max_by(|left, right| {
                    if riff_semver::Comparator::greater_than(&left.version, &right.version) {
                        std::cmp::Ordering::Greater
                    } else if riff_semver::Comparator::less_than(&left.version, &right.version) {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Equal
                    }
                });
            let Some(package) = package.filter(|package| {
                package.package_type.as_str() == "symfony-pack"
                    && (!package.require.is_empty() || !package.require_dev.is_empty())
            }) else {
                continue;
            };
            riff.manifest.require.shift_remove(&name);
            riff.manifest.require_dev.shift_remove(&name);
            for (dependency, dependency_constraint) in &package.require {
                if dependency.as_str() == "php" {
                    continue;
                }
                if riff.manifest.require.contains_key(dependency.as_str())
                    || riff.manifest.require_dev.contains_key(dependency.as_str())
                {
                    continue;
                }
                let target = if dev {
                    &mut riff.manifest.require_dev
                } else {
                    &mut riff.manifest.require
                };
                target.insert(dependency.to_string(), dependency_constraint.to_string());
            }
            unpacked.push(name);
            changed = true;
        }
        if !changed {
            break;
        }
    }
    Ok(unpacked)
}

pub fn read_recipe_lock(working_dir: &Path) -> Result<serde_json::Map<String, serde_json::Value>> {
    Ok(lock::FlexLock::load(working_dir)?.all().clone())
}

fn refresh_composer_lock_hash(working_dir: &Path) -> Result<()> {
    let composer_path = working_dir.join("composer.json");
    let lock_path = working_dir.join("composer.lock");
    if !lock_path.is_file() {
        return Ok(());
    }
    let manifest: RiffManifest = serde_json::from_slice(&std::fs::read(&composer_path)?)?;
    let hash = crate::util::compute_content_hash(&serde_json::to_string(&manifest)?);
    let mut lock: serde_json::Value = serde_json::from_slice(&std::fs::read(&lock_path)?)?;
    let current = lock.get("content-hash").and_then(serde_json::Value::as_str);
    if current == Some(&hash) {
        return Ok(());
    }
    lock.as_object_mut()
        .context("composer.lock must contain an object")?
        .insert("content-hash".to_owned(), serde_json::Value::String(hash));
    crate::json::write_json_value(&lock_path, &lock, true)?;
    Ok(())
}

fn is_required(manifest: &RiffManifest) -> bool {
    manifest.require.contains_key(PACKAGE_NAME) || manifest.require_dev.contains_key(PACKAGE_NAME)
}

/// Expand one Flex auto-script into the shell command Riff should execute.
/// A missing Symfony console intentionally returns `None`, matching Flex's
/// skip behavior for `symfony-cmd` entries.
fn expand_auto_script(
    command_type: &str,
    command: &str,
    manifest: &RiffManifest,
    working_dir: &Path,
    runtime: &RuntimeContext,
    arguments: &[String],
) -> Result<Option<(String, String)>> {
    let options = FlexOptions::from_manifest(manifest);
    let display_command = options.expand(command);
    let php = escape_argument(Some(runtime.php_binary.to_string_lossy().as_ref()));
    let script_arguments = arguments
        .iter()
        .map(|argument| escape_argument(Some(argument)))
        .collect::<Vec<_>>()
        .join(" ");
    let append_arguments = |mut command: String| {
        if !script_arguments.is_empty() {
            command.push(' ');
            command.push_str(&script_arguments);
        }
        command
    };

    let expanded = match command_type {
        "symfony-cmd" => {
            let root = if options.root_dir.is_absolute() {
                options.root_dir.clone()
            } else {
                working_dir.join(&options.root_dir)
            };
            let console = root.join(&options.bin_dir).join("console");
            if !console.is_file() {
                return Ok(None);
            }
            let console = escape_argument(Some(console.to_string_lossy().as_ref()));
            append_arguments(format!("{php} {console} {display_command}"))
        }
        "php-script" => append_arguments(format!("{php} {display_command}")),
        "script" => append_arguments(display_command.clone()),
        other => bail!(
            "Invalid symfony/flex auto-script in composer.json: \"{other}\" is not a valid type of command."
        ),
    };

    Ok(Some((display_command, expanded)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn expands_flex_directories_and_php_scripts() {
        let manifest: RiffManifest = serde_json::from_value(serde_json::json!({
            "extra": {"public-dir": "web"}
        }))
        .unwrap();
        let runtime = RuntimeContext::new(PathBuf::from("/opt/php"), PathBuf::from("riff"));
        let expanded = expand_auto_script(
            "php-script",
            "tool.php %PUBLIC_DIR%",
            &manifest,
            Path::new("/project"),
            &runtime,
            &[],
        )
        .unwrap()
        .unwrap();

        assert_eq!(expanded.0, "tool.php web");
        #[cfg(unix)]
        assert_eq!(expanded.1, "'/opt/php' tool.php web");
        #[cfg(windows)]
        assert_eq!(expanded.1, "/opt/php tool.php web");
    }

    #[tokio::test]
    async fn installs_a_recipe_and_records_its_lock() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let index = serde_json::json!({
            "recipes": {"vendor/package": ["1.0"]},
            "branch": "main",
            "_links": {
                "repository": "example.test/recipes",
                "origin_template": "{package}:{version}@example.test/recipes:main",
                "recipe_template": format!("{base}/{{package_dotted}}.{{version}}.json")
            }
        })
        .to_string();
        let recipe = serde_json::json!({
            "manifests": {
                "vendor/package": {
                    "manifest": {
                        "copy-from-recipe": {"config/": "config/"},
                        "composer-scripts": {"cache:warm": "symfony-cmd"}
                    },
                    "files": {
                        "config/app.yaml": {"contents": ["app:", "  enabled: true", ""], "executable": false}
                    },
                    "ref": "abc123"
                }
            }
        })
        .to_string();
        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut request = [0_u8; 2048];
                let size = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..size]);
                let body = if request.starts_with("GET /index.json ") {
                    &index
                } else {
                    &recipe
                };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });

        let project = tempfile::tempdir().unwrap();
        let manifest: RiffManifest = serde_json::from_value(serde_json::json!({
            "require": {"symfony/flex": "^2", "vendor/package": "^1"},
            "extra": {"symfony": {"endpoint": [format!("{base}/index.json")]}}
        }))
        .unwrap();
        std::fs::write(
            project.path().join("composer.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(
            project.path().join("composer.lock"),
            serde_json::to_vec_pretty(&crate::json::RiffLockfile::default()).unwrap(),
        )
        .unwrap();
        let mut config = crate::config::Config::with_base_dir(project.path());
        config.cache_read_only = true;
        let riff = crate::riff::RiffBuilder::new(project.path().to_owned())
            .with_config(config)
            .with_manifest(manifest)
            .with_lockfile(Some(crate::json::RiffLockfile::default()))
            .with_platform(crate::Platform::empty())
            .build()
            .unwrap();
        let package = Arc::new(crate::package::Package::new("vendor/package", "1.2.0.0"));
        let mut transaction = crate::solver::Transaction::new();
        transaction
            .operations
            .push(crate::solver::Operation::Install(package.clone()));

        let plan = prepare(&riff, &transaction, std::slice::from_ref(&package))
            .await
            .unwrap()
            .unwrap();
        apply(&riff, plan).unwrap();

        assert_eq!(
            std::fs::read_to_string(project.path().join("config/app.yaml")).unwrap(),
            "app:\n  enabled: true\n"
        );
        let lock: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project.path().join("symfony.lock")).unwrap())
                .unwrap();
        assert_eq!(
            lock.pointer("/vendor~1package/recipe/ref")
                .and_then(serde_json::Value::as_str),
            Some("abc123")
        );
        let composer: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project.path().join("composer.json")).unwrap())
                .unwrap();
        assert_eq!(
            composer.pointer("/scripts/auto-scripts/cache:warm"),
            Some(&serde_json::Value::String("symfony-cmd".to_owned()))
        );
    }

    #[tokio::test]
    async fn updates_recipe_files_with_a_three_way_merge() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let index = serde_json::json!({
            "aliases": {},
            "versions": {},
            "recipes": {"vendor/package": ["1.0"]},
            "branch": "main",
            "_links": {
                "repository": "example.test/recipes",
                "origin_template": "{package}:{version}@example.test/recipes:main",
                "recipe_template": format!("{base}/latest.json"),
                "archived_recipes_template": format!("{base}/archive/{{ref}}.json")
            }
        })
        .to_string();
        let response = |reference: &str, contents: &[&str]| {
            serde_json::json!({
                "manifests": {
                    "vendor/package": {
                        "manifest": {"copy-from-recipe": {"app.txt": "app.txt"}},
                        "files": {
                            "app.txt": {"contents": contents, "executable": false}
                        },
                        "ref": reference
                    }
                }
            })
            .to_string()
        };
        let original = response("old-ref", &["first", "middle", "last", ""]);
        let latest = response("new-ref", &["FIRST", "middle", "last", ""]);
        tokio::spawn(async move {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut request = [0_u8; 2048];
                let size = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..size]);
                let body = if request.starts_with("GET /index.json ") {
                    &index
                } else if request.starts_with("GET /archive/old-ref.json ") {
                    &original
                } else {
                    &latest
                };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(), body
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });

        let project = tempfile::tempdir().unwrap();
        let manifest: RiffManifest = serde_json::from_value(serde_json::json!({
            "require": {"symfony/flex": "^2", "vendor/package": "^1"},
            "extra": {"symfony": {"endpoint": [format!("{base}/index.json")]}}
        }))
        .unwrap();
        std::fs::write(
            project.path().join("composer.json"),
            crate::json::encode_pretty_json(&manifest, b"    ").unwrap(),
        )
        .unwrap();
        let package = crate::package::Package::new("vendor/package", "1.2.0.0");
        let mut composer_lock = crate::json::RiffLockfile::default();
        composer_lock.packages.push((&package).into());
        std::fs::write(
            project.path().join("composer.lock"),
            crate::json::encode_pretty_json(&composer_lock, b"    ").unwrap(),
        )
        .unwrap();
        std::fs::write(
            project.path().join("symfony.lock"),
            crate::json::encode_pretty_json(
                &serde_json::json!({
                    "vendor/package": {
                        "version": "1.2",
                        "recipe": {
                            "repo": "example.test/recipes",
                            "branch": "main",
                            "version": "1.0",
                            "ref": "old-ref"
                        },
                        "files": ["app.txt"]
                    }
                }),
                b"    ",
            )
            .unwrap(),
        )
        .unwrap();
        std::fs::write(project.path().join("app.txt"), "first\nmiddle\nLAST\n").unwrap();
        let mut config = crate::config::Config::with_base_dir(project.path());
        config.cache_read_only = true;
        let riff = crate::riff::RiffBuilder::new(project.path().to_owned())
            .with_config(config)
            .with_manifest(manifest)
            .with_lockfile(Some(composer_lock))
            .with_platform(crate::Platform::empty())
            .build()
            .unwrap();

        let result = update_recipe(&riff, "vendor/package").await.unwrap();

        assert_eq!(result.changed_files, ["app.txt"]);
        assert!(result.conflicted_files.is_empty());
        assert_eq!(
            std::fs::read_to_string(project.path().join("app.txt")).unwrap(),
            "FIRST\nmiddle\nLAST\n"
        );
        let lock: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project.path().join("symfony.lock")).unwrap())
                .unwrap();
        assert_eq!(
            lock.pointer("/vendor~1package/recipe/ref")
                .and_then(serde_json::Value::as_str),
            Some("new-ref")
        );
    }
}
