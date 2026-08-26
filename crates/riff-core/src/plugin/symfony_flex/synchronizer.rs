use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::json::{LockedPackage, RiffLockfile};
use crate::riff::Riff;

use super::FlexOptions;

pub(crate) fn synchronize(riff: &Riff) -> Result<()> {
    if riff
        .manifest
        .extra
        .pointer("/symfony~1flex/synchronize_package_json")
        .and_then(Value::as_bool)
        == Some(false)
    {
        return Ok(());
    }
    let lock_path = riff.working_dir.join("composer.lock");
    if !lock_path.is_file() {
        return Ok(());
    }
    let lock: RiffLockfile = serde_json::from_slice(&std::fs::read(lock_path)?)?;
    let packages = lock
        .packages
        .iter()
        .chain(&lock.packages_dev)
        .collect::<Vec<_>>();
    let options = FlexOptions::from_manifest(&riff.manifest);
    let root = if options.root_dir.is_absolute() {
        options.root_dir.clone()
    } else {
        riff.working_dir.join(&options.root_dir)
    };
    let assets = asset_packages(&root, &riff.vendor_dir(), &packages)?;

    if root.join("importmap.php").is_file() {
        synchronize_importmap(riff, &root, &options, &assets)?;
    } else if root.join("package.json").is_file() {
        synchronize_package_json(&root, &riff.vendor_dir(), &assets)?;
    }
    synchronize_controllers(&root, &assets)?;
    Ok(())
}

struct AssetPackage<'a> {
    package: &'a LockedPackage,
    path: PathBuf,
    json: Value,
}

fn asset_packages<'a>(
    root: &Path,
    vendor_dir: &Path,
    packages: &'a [&LockedPackage],
) -> Result<Vec<AssetPackage<'a>>> {
    let mut result = Vec::new();
    for package in packages {
        if !package
            .keywords
            .iter()
            .any(|keyword| keyword == "symfony-ux")
        {
            continue;
        }
        for subdir in ["assets", "Resources/assets", "src/Resources/assets"] {
            let path = vendor_dir
                .join(&package.name)
                .join(subdir)
                .join("package.json");
            if !path.is_file() {
                continue;
            }
            let json = serde_json::from_slice(&std::fs::read(&path)?)
                .with_context(|| format!("Invalid {}", path.display()))?;
            let path = path.strip_prefix(root).unwrap_or(&path).to_owned();
            result.push(AssetPackage {
                package,
                path,
                json,
            });
            break;
        }
    }
    Ok(result)
}

fn synchronize_package_json(
    root: &Path,
    vendor_dir: &Path,
    assets: &[AssetPackage<'_>],
) -> Result<()> {
    let path = root.join("package.json");
    let mut package_json: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let object = package_json
        .as_object_mut()
        .context("package.json must be an object")?;

    for section in ["dependencies", "devDependencies"] {
        if let Some(dependencies) = object.get_mut(section).and_then(Value::as_object_mut) {
            dependencies.retain(|name, constraint| {
                if !name.starts_with('@') {
                    return true;
                }
                let Some(relative) = constraint
                    .as_str()
                    .and_then(|constraint| constraint.strip_prefix("file:"))
                else {
                    return true;
                };
                !relative.contains("/assets") || root.join(relative).join("package.json").is_file()
            });
        }
    }

    let mut requested: HashMap<String, Vec<String>> = HashMap::new();
    for asset in assets {
        if asset
            .json
            .pointer("/symfony/needsPackageAsADependency")
            .and_then(Value::as_bool)
            != Some(false)
        {
            let directory = asset.path.parent().unwrap_or(&asset.path);
            requested
                .entry(format!("@{}", asset.package.name))
                .or_default()
                .push(format!(
                    "file:{}",
                    directory.to_string_lossy().replace('\\', "/")
                ));
        }
        if let Some(peers) = asset
            .json
            .get("peerDependencies")
            .and_then(Value::as_object)
        {
            for (name, constraint) in peers {
                if let Some(constraint) = constraint.as_str() {
                    requested
                        .entry(name.clone())
                        .or_default()
                        .push(constraint.to_owned());
                }
            }
        }
    }
    let prod = object
        .get("dependencies")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let dependencies = object
        .entry("devDependencies")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("package.json devDependencies must be an object")?;
    for (name, constraints) in requested {
        if prod.contains_key(&name)
            || constraints
                .iter()
                .any(|constraint| constraint != &constraints[0])
        {
            continue;
        }
        dependencies.insert(name, Value::String(constraints[0].clone()));
    }
    let mut sorted = std::mem::take(dependencies).into_iter().collect::<Vec<_>>();
    sorted.sort_by(|left, right| left.0.cmp(&right.0));
    dependencies.extend(sorted);
    write_json(&path, &package_json)?;
    let _ = vendor_dir;
    Ok(())
}

fn synchronize_controllers(root: &Path, assets: &[AssetPackage<'_>]) -> Result<()> {
    let path = root.join("assets/controllers.json");
    if !path.is_file() {
        return Ok(());
    }
    let previous: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let mut controllers = Map::new();
    let mut entrypoints = previous
        .get("entrypoints")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    for asset in assets {
        let package_name = format!("@{}", asset.package.name);
        if let Some(package_controllers) = asset
            .json
            .pointer("/symfony/controllers")
            .and_then(Value::as_object)
        {
            let mut merged = Map::new();
            for (name, defaults) in package_controllers {
                let previous_config = previous
                    .pointer(&format!(
                        "/controllers/{}/{}",
                        escape_pointer(&package_name),
                        escape_pointer(name)
                    ))
                    .and_then(Value::as_object);
                let mut config = Map::new();
                config.insert(
                    "enabled".to_owned(),
                    previous_config
                        .and_then(|config| config.get("enabled"))
                        .cloned()
                        .or_else(|| defaults.get("enabled").cloned())
                        .unwrap_or(Value::Bool(false)),
                );
                config.insert(
                    "fetch".to_owned(),
                    previous_config
                        .and_then(|config| config.get("fetch"))
                        .cloned()
                        .or_else(|| defaults.get("fetch").cloned())
                        .unwrap_or_else(|| Value::String("eager".to_owned())),
                );
                if let Some(autoimport) = defaults.get("autoimport").and_then(Value::as_object) {
                    let previous_autoimport =
                        previous_config.and_then(|config| config.get("autoimport"));
                    let merged = autoimport
                        .iter()
                        .map(|(name, enabled)| {
                            let enabled = previous_autoimport
                                .and_then(|values| values.get(name))
                                .cloned()
                                .unwrap_or_else(|| enabled.clone());
                            (name.clone(), enabled)
                        })
                        .collect();
                    config.insert("autoimport".to_owned(), Value::Object(merged));
                }
                merged.insert(name.clone(), Value::Object(config));
            }
            controllers.insert(package_name, Value::Object(merged));
        }
        if let Some(package_entrypoints) = asset
            .json
            .pointer("/symfony/entrypoints")
            .and_then(Value::as_object)
        {
            for (name, filename) in package_entrypoints {
                if entrypoints.is_array() {
                    let existing = entrypoints
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(|name| (name.to_owned(), Value::String(name.to_owned())))
                        .collect();
                    entrypoints = Value::Object(existing);
                }
                entrypoints
                    .as_object_mut()
                    .expect("entrypoints was normalized to an object")
                    .entry(name.clone())
                    .or_insert_with(|| filename.clone());
            }
        }
    }
    write_json(
        &path,
        &serde_json::json!({"controllers": controllers, "entrypoints": entrypoints}),
    )
}

fn synchronize_importmap(
    riff: &Riff,
    root: &Path,
    options: &FlexOptions,
    assets: &[AssetPackage<'_>],
) -> Result<()> {
    let importmap_contents = std::fs::read_to_string(root.join("importmap.php"))?;
    let console = root.join(&options.bin_dir).join("console");
    if !console.is_file() {
        return Ok(());
    }
    let mut entries = BTreeMap::new();
    for asset in assets {
        if let Some(importmap) = asset
            .json
            .pointer("/symfony/importmap")
            .and_then(Value::as_object)
        {
            for (name, config) in importmap {
                entries
                    .entry(name.clone())
                    .or_insert_with(|| (asset, config));
            }
        }
    }
    for (name, (asset, config)) in entries {
        if importmap_contents.contains(&format!("'{name}'"))
            || importmap_contents.contains(&format!("\"{name}\""))
        {
            continue;
        }
        let (mut version, package, mut entrypoint) = match config {
            Value::String(version) => (version.clone(), name.clone(), false),
            Value::Object(config) => (
                config
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                config
                    .get("package")
                    .and_then(Value::as_str)
                    .unwrap_or(&name)
                    .to_owned(),
                config
                    .get("entrypoint")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
            _ => continue,
        };
        if let Some(path) = version.strip_prefix("entrypoint:") {
            version = format!("path:{path}");
            entrypoint = true;
        }
        let mut command = Command::new(&riff.runtime.php_binary);
        command.arg(&console).arg("importmap:require");
        if let Some(path) = version.strip_prefix("path:") {
            let package_dir = asset.path.parent().unwrap_or(&asset.path).to_string_lossy();
            command.arg(&name).arg(format!(
                "--path={}",
                path.replace("%PACKAGE%", &package_dir)
            ));
            if entrypoint {
                command.arg("--entrypoint");
            }
        } else {
            let mut requirement = format!("{package}@{version}");
            if package != name {
                requirement.push('=');
                requirement.push_str(&name);
            }
            command.arg(requirement);
        }
        let status = command.current_dir(root).status()?;
        if !status.success() {
            anyhow::bail!("importmap:require failed while synchronizing {name}");
        }
    }
    Ok(())
}

fn escape_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if std::fs::read(path)
        .ok()
        .and_then(|contents| serde_json::from_slice::<Value>(&contents).ok())
        .as_ref()
        == Some(value)
    {
        return Ok(());
    }
    crate::json::write_json_value(path, value, true)?;
    Ok(())
}
