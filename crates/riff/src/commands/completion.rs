//! Dynamic shell completion helpers.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use riff_core::{is_platform_package, json::RiffLockfile};

use crate::{
    commands::{
        config::{self, ConfigArgs},
        outdated::OutdatedArgs,
        patch::{PatchArgs, PatchRemoveArgs, PatchesRepatchArgs},
        run::RunArgs,
        show::ShowArgs,
    },
    remove::RemoveArgs,
};

/// Fill dynamic candidates for optional positional arguments.
///
/// usage-rs deliberately falls back to file completion when an optional
/// positional is absent. Composer uses that position for several domain values
/// (packages, scripts, and binaries), so Riff supplements only that fallback;
/// normal flag and required-argument completion remains entirely generated.
pub fn supplement_completion(argv: &[OsString], generated: String) -> String {
    if !generated.lines().any(|line| line == "\u{1}files") {
        return generated;
    }
    let Some(request) = usage_rs::complete::CompletionRequest::parse(argv) else {
        return generated;
    };
    let words = &request.split.words;
    let Some((command_at, command)) = words.iter().enumerate().find_map(|(index, word)| {
        matches!(
            word.as_str(),
            "update"
                | "remove"
                | "exec"
                | "run"
                | "run-script"
                | "archive"
                | "require"
                | "add"
                | "create-project"
                | "show"
                | "info"
        )
        .then_some((index, word.as_str()))
    }) else {
        return generated;
    };
    if request.split.cword <= command_at || request.split.prefix.starts_with('-') {
        return generated;
    }
    let previous = request
        .split
        .cword
        .checked_sub(1)
        .and_then(|index| words.get(index))
        .map(String::as_str);
    if matches!(
        previous,
        Some(
            "--prefer-install"
                | "--with"
                | "--bump-after-update"
                | "--apcu-autoloader-prefix"
                | "--ignore-platform-req"
                | "-d"
                | "--working-dir"
                | "--audit-format"
                | "--format"
                | "-f"
                | "--repository"
        )
    ) {
        return generated;
    }

    let working_dir = working_dir_words(&words[command_at + 1..request.split.cword]);
    let prefix = request.split.prefix.as_str();
    let values = match command {
        "update" => installed_packages(&working_dir),
        "remove" => direct_dependencies(&working_dir, true),
        "exec" => available_binaries(&working_dir),
        "run" | "run-script" => project_scripts(&working_dir),
        "archive" | "require" | "add" | "create-project" => available_packages(&working_dir),
        "show" | "info" => read_lockfile(&working_dir)
            .map(|lock| {
                lock.all_packages()
                    .map(|package| package.name.clone())
                    .collect()
            })
            .unwrap_or_else(|| direct_dependencies(&working_dir, false)),
        _ => return generated,
    };
    let candidates = candidates(values)
        .into_iter()
        .filter(|candidate| candidate.value.starts_with(prefix))
        .collect();
    usage_rs::complete::render(
        &usage_rs::complete::Completions {
            candidates,
            files: None,
        },
        request.shell,
    )
}

/// Complete package names exposed by inline `package` repositories.
///
/// Network-backed repository completion would make every tab press slow and
/// flaky. Riff instead completes names that are already available in the
/// project manifest; cached remote indexes can be added here later without
/// changing the command definitions.
pub fn complete_available_package<T>(
    _: &T,
    ctx: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    candidates(available_packages(&working_dir(ctx)))
}

/// Complete Composer's supported install preference values.
pub fn complete_prefer_install<T>(
    _: &T,
    _: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    candidates(vec!["dist".into(), "source".into(), "auto".into()])
}

/// Complete text output formats shared by `search` and `show`.
pub fn complete_output_format<T>(
    _: &T,
    _: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    candidates(vec!["text".into(), "json".into()])
}

/// Complete archive formats supported by Riff.
pub fn complete_archive_format<T>(
    _: &T,
    _: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    candidates(vec!["tar".into(), "zip".into()])
}

/// Complete executable names from the configured Composer bin directory.
pub fn complete_binary<T>(
    _: &T,
    ctx: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    candidates(available_binaries(&working_dir(ctx)))
}

/// Complete package names accepted by `riff show`.
pub fn complete_show_package(
    _: &<ShowArgs as usage_rs::spec::CommandArgs>::Partial,
    ctx: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    let working_dir = working_dir(ctx);
    let package_names = read_lockfile(&working_dir)
        .map(|lock| {
            lock.all_packages()
                .map(|package| package.name.clone())
                .collect()
        })
        .unwrap_or_else(|| direct_dependencies(&working_dir, false));
    candidates(package_names)
}

/// Complete direct, removable package names accepted by `riff remove`.
pub fn complete_remove_package(
    _: &<RemoveArgs as usage_rs::spec::CommandArgs>::Partial,
    ctx: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    candidates(direct_dependencies(&working_dir(ctx), true))
}

/// Complete installed package names for dependency-inspection commands.
pub fn complete_installed_package<T>(
    _: &T,
    ctx: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    candidates(installed_packages(&working_dir(ctx)))
}

/// Complete installed package names for `riff outdated`.
pub fn complete_outdated_package(
    _: &<OutdatedArgs as usage_rs::spec::CommandArgs>::Partial,
    ctx: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    candidates(installed_packages(&working_dir(ctx)))
}

/// Complete installed package names for `riff patch`.
pub fn complete_patch_package(
    _: &<PatchArgs as usage_rs::spec::CommandArgs>::Partial,
    ctx: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    candidates(installed_packages(&working_dir(ctx)))
}

/// Complete native patch selectors for `riff patch-remove`.
pub fn complete_patch_selector(
    _: &<PatchRemoveArgs as usage_rs::spec::CommandArgs>::Partial,
    ctx: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    candidates(native_patch_selectors(&working_dir(ctx)))
}

/// Complete patched package names for `riff patches-repatch`.
pub fn complete_patched_package(
    _: &<PatchesRepatchArgs as usage_rs::spec::CommandArgs>::Partial,
    ctx: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    candidates(
        native_patch_selectors(&working_dir(ctx))
            .into_iter()
            .filter_map(|selector| {
                selector
                    .rsplit_once('@')
                    .map(|(package, _)| package.to_owned())
            })
            .collect(),
    )
}

/// Complete script names declared in composer.json.
pub fn complete_script(
    _: &<RunArgs as usage_rs::spec::CommandArgs>::Partial,
    ctx: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    candidates(project_scripts(&working_dir(ctx)))
}

fn project_scripts(working_dir: &Path) -> Vec<String> {
    std::fs::read_to_string(working_dir.join("composer.json"))
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|manifest| {
            manifest
                .get("scripts")
                .and_then(serde_json::Value::as_object)
                .cloned()
        })
        .map(|scripts| scripts.into_iter().map(|(name, _)| name).collect())
        .unwrap_or_default()
}

/// Complete built-in and project-defined configuration keys.
pub fn complete_config_key(
    _: &<ConfigArgs as usage_rs::spec::CommandArgs>::Partial,
    ctx: &usage_rs::complete::CompleteCtx<'_>,
) -> Vec<usage_rs::complete::Candidate<'static>> {
    let working_dir = working_dir(ctx);
    let mut keys = config::completion_keys()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    keys.extend(manifest_keys(&working_dir));
    candidates(keys)
}

fn working_dir(ctx: &usage_rs::complete::CompleteCtx<'_>) -> PathBuf {
    working_dir_words(ctx.command_words)
}

fn working_dir_words(words: &[String]) -> PathBuf {
    let mut args = words.iter();
    while let Some(argument) = args.next() {
        if argument == "-d" || argument == "--working-dir" {
            if let Some(path) = args.next() {
                return PathBuf::from(path);
            }
        }
        if let Some(path) = argument.strip_prefix("--working-dir=") {
            return PathBuf::from(path);
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn read_lockfile(working_dir: &Path) -> Option<RiffLockfile> {
    std::fs::read_to_string(working_dir.join("composer.lock"))
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
}

fn installed_packages(working_dir: &Path) -> Vec<String> {
    read_lockfile(working_dir)
        .map(|lock| {
            lock.all_packages()
                .map(|package| package.name.clone())
                .collect()
        })
        .unwrap_or_else(|| direct_dependencies(working_dir, false))
}

fn available_packages(working_dir: &Path) -> Vec<String> {
    let Some(repositories) = std::fs::read_to_string(working_dir.join("composer.json"))
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|manifest| manifest.get("repositories").cloned())
    else {
        return Vec::new();
    };

    fn collect(value: &serde_json::Value, names: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(name) = object.get("name").and_then(serde_json::Value::as_str) {
                    if name.contains('/') {
                        names.push(name.to_owned());
                    }
                }
                for value in object.values() {
                    collect(value, names);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    collect(value, names);
                }
            }
            _ => {}
        }
    }

    let mut names = Vec::new();
    collect(&repositories, &mut names);
    names
}

fn available_binaries(working_dir: &Path) -> Vec<String> {
    let manifest = std::fs::read_to_string(working_dir.join("composer.json"))
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok());
    let bin_dir = manifest
        .as_ref()
        .and_then(|value| value.pointer("/config/bin-dir"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("vendor/bin");
    let mut binaries = std::fs::read_dir(working_dir.join(bin_dir))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    if let Some(root_binaries) = manifest
        .as_ref()
        .and_then(|value| value.get("bin"))
        .and_then(serde_json::Value::as_array)
    {
        binaries.extend(root_binaries.iter().filter_map(|binary| {
            Path::new(binary.as_str()?)
                .file_name()?
                .to_str()
                .map(str::to_owned)
        }));
    }
    binaries
}

fn native_patch_selectors(working_dir: &Path) -> Vec<String> {
    std::fs::read_to_string(working_dir.join(riff_core::patch::NATIVE_PATCH_LOCK_FILE))
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|lock| {
            lock.get("patches")
                .and_then(serde_json::Value::as_object)
                .cloned()
        })
        .map(|patches| patches.into_iter().map(|(selector, _)| selector).collect())
        .unwrap_or_default()
}

fn manifest_keys(working_dir: &Path) -> Vec<String> {
    let Some(manifest) = std::fs::read_to_string(working_dir.join("composer.json"))
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
    else {
        return Vec::new();
    };
    fn collect(prefix: &str, value: &serde_json::Value, keys: &mut Vec<String>) {
        let Some(object) = value.as_object() else {
            return;
        };
        for (key, value) in object {
            let key = format!("{prefix}.{key}");
            keys.push(key.clone());
            collect(&key, value, keys);
        }
    }

    let mut keys = Vec::new();
    for root in ["extra", "suggest"] {
        if let Some(value) = manifest.get(root) {
            collect(root, value, &mut keys);
        }
    }
    if let Some(repositories) = manifest
        .get("repositories")
        .and_then(serde_json::Value::as_object)
    {
        keys.extend(
            repositories
                .keys()
                .map(|name| format!("repositories.{name}")),
        );
    }
    keys
}

fn direct_dependencies(working_dir: &Path, exclude_platform: bool) -> Vec<String> {
    let Some(manifest) = std::fs::read_to_string(working_dir.join("composer.json"))
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
    else {
        return Vec::new();
    };
    ["require", "require-dev"]
        .into_iter()
        .filter_map(|section| manifest.get(section).and_then(serde_json::Value::as_object))
        .flat_map(|requirements| requirements.keys())
        .filter(|name| !exclude_platform || !is_platform_package(name))
        .cloned()
        .collect()
}

fn candidates(package_names: Vec<String>) -> Vec<usage_rs::complete::Candidate<'static>> {
    package_names
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(usage_rs::complete::Candidate::new)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn show_candidates_include_all_locked_packages() {
        let project = TempDir::new().unwrap();
        std::fs::write(
            project.path().join("composer.lock"),
            r#"{"packages":[{"name":"acme/library","version":"1.0.0"}],"packages-dev":[{"name":"acme/tool","version":"1.0.0"}]}"#,
        )
        .unwrap();

        let names = read_lockfile(project.path())
            .unwrap()
            .all_packages()
            .map(|package| package.name.clone())
            .collect();

        assert_eq!(
            candidates(names)
                .into_iter()
                .map(|candidate| candidate.value)
                .collect::<Vec<_>>(),
            ["acme/library", "acme/tool"]
        );
    }

    #[test]
    fn remove_candidates_include_only_direct_non_platform_dependencies() {
        let project = TempDir::new().unwrap();
        std::fs::write(
            project.path().join("composer.json"),
            r#"{"require":{"acme/library":"^1.0","php":"^8.2","ext-json":"*"},"require-dev":{"acme/tool":"^1.0"}}"#,
        )
        .unwrap();

        assert_eq!(
            candidates(direct_dependencies(project.path(), true))
                .into_iter()
                .map(|candidate| candidate.value)
                .collect::<Vec<_>>(),
            ["acme/library", "acme/tool"]
        );
    }

    #[test]
    fn discovers_native_patch_selectors_and_project_repository_keys() {
        let project = TempDir::new().unwrap();
        std::fs::write(
            project.path().join("riff-patches.lock.json"),
            r#"{"lock-version":1,"_hash":"","patches":{"acme/library@1.2.3":{"path":"patches/library.patch","version-normalized":"1.2.3.0","sha256":"abc"}}}"#,
        )
        .unwrap();
        std::fs::write(
            project.path().join("composer.json"),
            r#"{"scripts":{"test":"phpunit","lint":"php-cs-fixer"},"repositories":{"private":{"type":"composer","url":"https://example.test"}}}"#,
        )
        .unwrap();

        assert_eq!(
            native_patch_selectors(project.path()),
            ["acme/library@1.2.3"]
        );
        assert_eq!(manifest_keys(project.path()), ["repositories.private"]);
    }

    #[test]
    fn config_candidates_include_supported_keys() {
        let keys = config::completion_keys();
        assert!(keys.contains(&"vendor-dir"));
        assert!(keys.contains(&"policy.advisories.block"));
        assert!(keys.contains(&"repositories"));
    }
}
