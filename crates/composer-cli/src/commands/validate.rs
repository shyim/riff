use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use composer_rs_core::{
    compute_content_hash, is_platform_package,
    json::{
        validate_parsed_composer_manifest, ComposerJson, ComposerLock, LockedPackage,
        ManifestValidation, ManifestValidationOptions,
    },
};
use composer_rs_semver::VersionParser;
use serde_json::Value;

#[derive(usage_rs::Args, Debug)]
pub struct ValidateArgs {
    /// Do not validate requires for overly strict or loose constraints
    #[usage(long)]
    pub no_check_all: bool,

    /// Check if the lock file is up to date even when config.lock is false
    #[usage(long)]
    pub check_lock: bool,

    /// Do not treat lock file issues as errors
    #[usage(long)]
    pub no_check_lock: bool,

    /// Do not check for errors preventing package publication
    #[usage(long)]
    pub no_check_publish: bool,

    /// Do not warn when the version field is present
    #[usage(long)]
    pub no_check_version: bool,

    /// Also validate installed dependencies
    #[usage(short = 'A', long)]
    pub with_dependencies: bool,

    /// Return a non-zero exit code for warnings
    #[usage(long)]
    pub strict: bool,

    /// Path to composer.json
    pub file: Option<PathBuf>,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub async fn execute(args: ValidateArgs) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let (manifest_path, display_name) = resolve_manifest_path(&working_dir, args.file.as_deref());

    if !manifest_path.exists() {
        eprintln!("{display_name} not found.");
        return Ok(3);
    }
    if !is_readable(&manifest_path) {
        eprintln!("{display_name} is not readable.");
        return Ok(3);
    }

    let content = fs::read_to_string(&manifest_path)
        .with_context(|| format!("Failed to read {}", manifest_path.display()))?;
    let manifest = serde_json::from_str::<Value>(&content)
        .map_err(|error| anyhow::anyhow!("{display_name} does not contain valid JSON: {error}"))?;
    let options = ManifestValidationOptions {
        check_constraints: !args.no_check_all,
        check_version: !args.no_check_version,
        check_publish: !args.no_check_publish,
    };
    let validation = validate_parsed_composer_manifest(&content, &display_name, options, &manifest);
    let composer = serde_json::from_str::<ComposerJson>(&content).ok();
    let config_lock_enabled = composer
        .as_ref()
        .and_then(|composer| composer.config.lock)
        .unwrap_or(true);
    let lock_path = lock_path_for(&manifest_path);
    let lock_errors = if config_lock_enabled {
        composer
            .as_ref()
            .map(|composer| validate_lock(&manifest_path, &content, composer))
            .unwrap_or_default()
    } else {
        if lock_path.exists() {
            let display_lock = lock_path_for(Path::new(&display_name));
            for _ in 0..2 {
                eprintln!(
                    "{} is present but ignored as the \"lock\" config option is disabled.",
                    display_lock.display()
                );
            }
        }
        Vec::new()
    };
    let check_lock = args.check_lock || (!args.no_check_lock && config_lock_enabled);

    let mut exit_code = output_result(
        &display_name,
        validation,
        !args.no_check_publish,
        check_lock,
        lock_errors,
        args.strict,
        true,
    );

    if args.with_dependencies {
        let vendor_dir = composer
            .as_ref()
            .and_then(|composer| composer.config.vendor_dir.as_deref())
            .map(|vendor| working_dir.join(vendor))
            .unwrap_or_else(|| working_dir.join("vendor"));
        let dependency_manifests = dependency_manifests(&vendor_dir)?;
        for (dependency_name, validation) in
            validate_dependency_manifests(&dependency_manifests, options)?
        {
            exit_code = exit_code.max(output_result(
                &dependency_name,
                validation,
                !args.no_check_publish,
                false,
                Vec::new(),
                args.strict,
                false,
            ));
        }
    }

    Ok(exit_code)
}

fn validate_dependency_manifests(
    manifests: &[PathBuf],
    options: ManifestValidationOptions,
) -> Result<Vec<(String, ManifestValidation)>> {
    let workers = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(8)
        .min(manifests.len());
    if workers <= 1 || manifests.len() < 96 {
        return manifests
            .iter()
            .map(|manifest| validate_dependency_manifest(manifest, options))
            .collect();
    }

    let chunk_size = manifests.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let handles: Vec<_> = manifests
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|manifest| validate_dependency_manifest(manifest, options))
                        .collect::<Result<Vec<_>>>()
                })
            })
            .collect();
        let mut results = Vec::with_capacity(manifests.len());
        for handle in handles {
            let chunk = handle
                .join()
                .map_err(|_| anyhow::anyhow!("Dependency validation worker panicked"))??;
            results.extend(chunk);
        }
        Ok(results)
    })
}

fn validate_dependency_manifest(
    dependency_manifest: &Path,
    options: ManifestValidationOptions,
) -> Result<(String, ManifestValidation)> {
    let dependency_content = fs::read_to_string(dependency_manifest)
        .with_context(|| format!("Failed to read {}", dependency_manifest.display()))?;
    let dependency_manifest_value = serde_json::from_str::<Value>(&dependency_content);
    let dependency_name = dependency_manifest_value
        .as_ref()
        .ok()
        .and_then(|value| value.get("name")?.as_str().map(str::to_string))
        .unwrap_or_else(|| dependency_manifest.display().to_string());
    let dependency_source = dependency_manifest.display().to_string();
    let validation = match dependency_manifest_value {
        Ok(manifest) => validate_parsed_composer_manifest(
            &dependency_content,
            &dependency_source,
            options,
            &manifest,
        ),
        Err(error) => ManifestValidation {
            errors: vec![format!(
                "{dependency_source} does not contain valid JSON: {error}"
            )],
            ..ManifestValidation::default()
        },
    };
    Ok((dependency_name, validation))
}

fn resolve_manifest_path(working_dir: &Path, file: Option<&Path>) -> (PathBuf, String) {
    match file {
        Some(file) if file.is_absolute() => (file.to_path_buf(), file.display().to_string()),
        Some(file) => (working_dir.join(file), file.display().to_string()),
        None => (
            working_dir.join("composer.json"),
            "./composer.json".to_string(),
        ),
    }
}

fn is_readable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = path.metadata() {
            if metadata.permissions().mode() & 0o444 == 0 {
                return false;
            }
        }
    }
    fs::File::open(path).is_ok()
}

fn output_result(
    name: &str,
    validation: ManifestValidation,
    check_publish: bool,
    check_lock: bool,
    lock_errors: Vec<String>,
    strict: bool,
    print_schema_url: bool,
) -> i32 {
    let ManifestValidation {
        errors,
        publish_errors,
        warnings,
    } = validation;

    if !errors.is_empty() {
        eprintln!("{name} is invalid, the following errors/warnings were found:");
    } else if check_publish && !publish_errors.is_empty() {
        eprintln!("{name} is valid for simple usage with Composer but has");
        eprintln!("strict errors that make it unable to be published as a package");
        if print_schema_url {
            eprintln!("See https://getcomposer.org/doc/04-schema.md for details on the schema");
        }
    } else if !warnings.is_empty() {
        eprintln!("{name} is valid, but with a few warnings");
        if print_schema_url {
            eprintln!("See https://getcomposer.org/doc/04-schema.md for details on the schema");
        }
    } else if !lock_errors.is_empty() {
        println!(
            "{name} is valid but your composer.lock has some {}",
            if check_lock { "errors" } else { "warnings" }
        );
    } else {
        println!("{name} is valid");
    }

    if !errors.is_empty() {
        eprintln!("# General errors");
        for error in &errors {
            eprintln!("- {error}");
        }
    }
    if check_publish && !publish_errors.is_empty() {
        eprintln!("# Publish errors");
        for error in &publish_errors {
            eprintln!("- {error}");
        }
    }
    if !warnings.is_empty() {
        eprintln!("# General warnings");
        for warning in &warnings {
            eprintln!("- {warning}");
        }
    }
    if !lock_errors.is_empty() {
        eprintln!(
            "# Lock file {}",
            if check_lock { "errors" } else { "warnings" }
        );
        for error in &lock_errors {
            eprintln!("{error}");
        }
    }

    if !errors.is_empty()
        || (check_publish && !publish_errors.is_empty())
        || (check_lock && !lock_errors.is_empty())
    {
        2
    } else if strict && !warnings.is_empty() {
        1
    } else {
        0
    }
}

fn validate_lock(
    manifest_path: &Path,
    manifest_content: &str,
    composer: &ComposerJson,
) -> Vec<String> {
    let lock_path = lock_path_for(manifest_path);
    if !lock_path.exists() {
        return Vec::new();
    }

    let lock_content = match fs::read_to_string(&lock_path) {
        Ok(content) => content,
        Err(error) => {
            return vec![format!("- Failed to read {}: {error}", lock_path.display())];
        }
    };
    let lock: ComposerLock = match serde_json::from_str(&lock_content) {
        Ok(lock) => lock,
        Err(error) => {
            return vec![format!("- {} is invalid: {error}", lock_path.display())];
        }
    };

    let mut errors = Vec::new();
    if lock.content_hash.is_empty() || lock.content_hash != compute_content_hash(manifest_content) {
        errors.push(
            "- The lock file is not up to date with the latest changes in composer.json, it is recommended that you run `composer update` or `composer update <package name>`."
                .to_string(),
        );
    }
    errors.extend(missing_requirement_info(composer, &lock));
    errors
}

fn lock_path_for(manifest_path: &Path) -> PathBuf {
    if manifest_path
        .extension()
        .is_some_and(|extension| extension == "json")
    {
        manifest_path.with_extension("lock")
    } else {
        PathBuf::from(format!("{}.lock", manifest_path.display()))
    }
}

#[derive(Debug)]
struct LockCandidate {
    package_name: String,
    package_version: String,
    constraint: String,
    provider_kind: Option<&'static str>,
}

fn missing_requirement_info(composer: &ComposerJson, lock: &ComposerLock) -> Vec<String> {
    let mut errors = Vec::new();
    check_requirement_set(
        "Required",
        &composer.require,
        &lock.packages,
        composer,
        &mut errors,
    );

    let mut all_packages = lock.packages.clone();
    all_packages.extend(lock.packages_dev.clone());
    check_requirement_set(
        "Required (in require-dev)",
        &composer.require_dev,
        &all_packages,
        composer,
        &mut errors,
    );

    if !errors.is_empty() {
        errors.push(
            "This usually happens when composer files are incorrectly merged or the composer.json file is manually edited."
                .to_string(),
        );
        errors.push(
            "Read more about correctly resolving merge conflicts https://getcomposer.org/doc/articles/resolving-merge-conflicts.md"
                .to_string(),
        );
        errors.push(
            "and prefer using the \"require\" command over editing the composer.json file directly https://getcomposer.org/doc/03-cli.md#require-r"
                .to_string(),
        );
    }
    errors
}

fn check_requirement_set(
    description: &str,
    requirements: &indexmap::IndexMap<String, String>,
    packages: &[LockedPackage],
    composer: &ComposerJson,
    errors: &mut Vec<String>,
) {
    let parser = VersionParser::new();
    for (target, required_constraint) in requirements {
        if is_platform_package(target) || required_constraint == "self.version" {
            continue;
        }
        let Ok(required) = parser.parse_constraints_cached(required_constraint) else {
            continue;
        };
        let candidates = lock_candidates(target, packages, composer);
        let matching = candidates.iter().any(|candidate| {
            if candidate.provider_kind.is_none() {
                required.satisfies(&candidate.constraint)
            } else {
                required.intersects(&candidate.constraint).unwrap_or(false)
            }
        });
        if matching {
            continue;
        }

        if let Some(candidate) = candidates.first() {
            let installed = if let Some(kind) = candidate.provider_kind {
                format!(
                    "{kind} as {} by {} {}",
                    candidate.constraint, candidate.package_name, candidate.package_version
                )
            } else {
                candidate.package_version.clone()
            };
            errors.push(format!(
                "- {description} package \"{target}\" is in the lock file as \"{installed}\" but that does not satisfy your constraint \"{required_constraint}\"."
            ));
        } else {
            errors.push(format!(
                "- {description} package \"{target}\" is not present in the lock file."
            ));
        }
    }
}

fn lock_candidates(
    target: &str,
    packages: &[LockedPackage],
    composer: &ComposerJson,
) -> Vec<LockCandidate> {
    let mut candidates = Vec::new();
    for package in packages {
        if package.name.eq_ignore_ascii_case(target) {
            candidates.push(LockCandidate {
                package_name: package.name.clone(),
                package_version: package.version.clone(),
                constraint: package.version.clone(),
                provider_kind: None,
            });
        }
        add_link_candidates(
            &mut candidates,
            target,
            &package.name,
            &package.version,
            &package.replace,
            "replaced",
        );
        add_link_candidates(
            &mut candidates,
            target,
            &package.name,
            &package.version,
            &package.provide,
            "provided",
        );
    }

    let root_name = composer.name.as_deref().unwrap_or("__root__");
    let root_version = composer.version.as_deref().unwrap_or("1.0.0");
    add_link_candidates(
        &mut candidates,
        target,
        root_name,
        root_version,
        &composer.replace,
        "replaced",
    );
    add_link_candidates(
        &mut candidates,
        target,
        root_name,
        root_version,
        &composer.provide,
        "provided",
    );
    candidates
}

fn add_link_candidates(
    candidates: &mut Vec<LockCandidate>,
    target: &str,
    package_name: &str,
    package_version: &str,
    links: &indexmap::IndexMap<String, String>,
    provider_kind: &'static str,
) {
    if let Some(constraint) = links
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(target))
        .map(|(_, constraint)| constraint)
    {
        candidates.push(LockCandidate {
            package_name: package_name.to_string(),
            package_version: package_version.to_string(),
            constraint: if constraint == "self.version" {
                format!("={package_version}")
            } else {
                constraint.clone()
            },
            provider_kind: Some(provider_kind),
        });
    }
}

fn dependency_manifests(vendor_dir: &Path) -> Result<Vec<PathBuf>> {
    if !vendor_dir.is_dir() {
        return Ok(Vec::new());
    }
    let installed_path = vendor_dir.join("composer/installed.json");
    if installed_path.is_file() {
        let content = fs::read_to_string(&installed_path)?;
        let installed: Value = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", installed_path.display()))?;
        let mut manifests: Vec<_> = installed
            .get("packages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|package| package.get("install-path").and_then(Value::as_str))
            .map(|install_path| {
                lexical_join(&vendor_dir.join("composer"), Path::new(install_path))
                    .join("composer.json")
            })
            .filter(|manifest| manifest.is_file())
            .collect();
        manifests.sort();
        manifests.dedup();
        return Ok(manifests);
    }

    let mut manifests = Vec::new();
    for vendor in fs::read_dir(vendor_dir)? {
        let vendor = vendor?;
        if !vendor.path().is_dir() || vendor.file_name() == "bin" {
            continue;
        }
        for package in fs::read_dir(vendor.path())? {
            let package = package?;
            let manifest = package.path().join("composer.json");
            if manifest.is_file() {
                manifests.push(manifest);
            }
        }
    }
    manifests.sort();
    Ok(manifests)
}

fn lexical_join(base: &Path, relative: &Path) -> PathBuf {
    if relative.is_absolute() {
        return relative.to_path_buf();
    }
    let mut joined = base.to_path_buf();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                joined.pop();
            }
            Component::Normal(component) => joined.push(component),
            Component::RootDir | Component::Prefix(_) => unreachable!("relative path expected"),
        }
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_path_matches_composer_naming() {
        assert_eq!(
            lock_path_for(Path::new("custom.json")),
            PathBuf::from("custom.lock")
        );
        assert_eq!(
            lock_path_for(Path::new("custom")),
            PathBuf::from("custom.lock")
        );
    }

    #[test]
    fn joins_installed_paths_without_filesystem_canonicalization() {
        assert_eq!(
            lexical_join(
                Path::new("/project/vendor/composer"),
                Path::new("../vendor/package")
            ),
            PathBuf::from("/project/vendor/vendor/package")
        );
    }

    #[test]
    fn reports_missing_and_mismatched_locked_requirements() {
        let composer: ComposerJson =
            serde_json::from_str(r#"{"require":{"vendor/missing":"^1","vendor/wrong":"^2"}}"#)
                .unwrap();
        let lock: ComposerLock =
            serde_json::from_str(r#"{"packages":[{"name":"vendor/wrong","version":"1.0.0"}]}"#)
                .unwrap();

        let errors = missing_requirement_info(&composer, &lock);
        assert!(errors
            .iter()
            .any(|error| error.contains("vendor/missing") && error.contains("not present")));
        assert!(errors
            .iter()
            .any(|error| error.contains("vendor/wrong") && error.contains("does not satisfy")));
    }

    #[test]
    fn accepts_locked_provider() {
        let composer: ComposerJson =
            serde_json::from_str(r#"{"require":{"virtual/package":"^2"}}"#).unwrap();
        let lock: ComposerLock = serde_json::from_str(
            r#"{
                "packages":[{
                    "name":"vendor/provider",
                    "version":"1.0.0",
                    "provide":{"virtual/package":"^2.1"}
                }]
            }"#,
        )
        .unwrap();

        assert!(missing_requirement_info(&composer, &lock).is_empty());
    }
}
