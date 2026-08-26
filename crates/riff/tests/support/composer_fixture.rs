use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use regex::Regex;
use riff_core::json::{LockedPackage, RiffLockfile};
use riff_core::package::{parse_branch_aliases, AliasPackage, Stability, DEFAULT_BRANCH_ALIAS};
use riff_core::solver::Operation as RiffOperation;
use riff_core::{Package, Transaction};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Operation {
    Install {
        package: String,
        version: String,
    },
    Update {
        package: String,
        from: String,
        to: String,
    },
    Remove {
        package: String,
        version: String,
    },
    Alias {
        package: String,
        version: String,
        alias_of: String,
        installed: bool,
    },
}

pub fn run(source: &str) {
    if let Err(error) = run_inner(source) {
        panic!("Composer fixture failed: {error:#}");
    }
}

fn run_inner(source: &str) -> Result<()> {
    let fixture = parse_sections(source)?;
    let command_environment = parse_condition(fixture.get("CONDITION").map(String::as_str))?;
    let project = tempfile::tempdir()?;
    let mut manifest: Value =
        serde_json::from_str(required(&fixture, "COMPOSER")?).context("invalid COMPOSER JSON")?;
    prepare_manifest(&mut manifest, project.path())?;
    let manifest_content = format!("{}\n", serde_json::to_string_pretty(&manifest)?);
    fs::write(project.path().join("composer.json"), &manifest_content)?;

    let installed = fixture
        .get("INSTALLED")
        .filter(|value| !value.trim().is_empty())
        .map(|value| serde_json::from_str::<Vec<LockedPackage>>(value))
        .transpose()
        .context("invalid INSTALLED JSON")?;
    if let Some(installed) = &installed {
        write_installed(project.path(), installed)?;
    }

    let initial_lock = initial_lock(&fixture, &manifest_content)?;
    if let Some(lock) = &initial_lock {
        write_lock(project.path(), lock)?;
    }

    let run = required(&fixture, "RUN")?;
    let mut arguments = shlex::split(run).context("RUN contains invalid shell quoting")?;
    if arguments.is_empty() {
        bail!("RUN must contain install or update");
    }
    let fixture_command = arguments[0].clone();
    let fixture_skips_installation = arguments.iter().any(|argument| argument == "--no-install");
    let fixture_dry_run = arguments.iter().any(|argument| argument == "--dry-run");
    let install_dev = !arguments.iter().any(|argument| argument == "--no-dev");
    let install_from_lock = fixture_command == "install" && initial_lock.is_some();
    match fixture_command.as_str() {
        // A no-lock install resolves the same package set as update. Existing
        // locks use the real install planner so lock validation and installed
        // state reconciliation remain covered without downloading packages.
        "install" if !install_from_lock => arguments[0] = "update".to_string(),
        "install" => {}
        "update" => {}
        command => bail!("unsupported fixture command {command:?}"),
    }
    if install_from_lock {
        push_flag(&mut arguments, "--dry-run");
    } else {
        push_flag(&mut arguments, "--no-install");
    }
    push_flag(&mut arguments, "--no-audit");
    push_flag(&mut arguments, "--no-scripts");
    push_flag(&mut arguments, "--no-plugins");
    push_flag(&mut arguments, "--no-interaction");
    arguments.push("-d".to_string());
    arguments.push(project.path().display().to_string());

    let output = execute_riff(&arguments, project.path(), &command_environment)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected_exit = fixture
        .get("EXPECT-EXIT-CODE")
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().parse::<i32>())
        .transpose()
        .context("EXPECT-EXIT-CODE must be an integer")?
        .unwrap_or(0);
    let actual_exit = output.status.code().unwrap_or(1);
    let expects_exception = fixture
        .get("EXPECT-EXCEPTION")
        .is_some_and(|exception| !exception.trim().is_empty());
    if (expects_exception && actual_exit == 0)
        || (!expects_exception && actual_exit != expected_exit)
    {
        bail!(
            "expected exit code {expected_exit}, got {actual_exit}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
    let command_succeeded = actual_exit == 0;
    let expected_output = fixture
        .get("EXPECT-OUTPUT-OPTIMIZED")
        .filter(|value| !value.trim().is_empty())
        .or_else(|| fixture.get("EXPECT-OUTPUT"));
    if let Some(expected_output) = expected_output.filter(|value| !value.trim().is_empty()) {
        assert_output_semantics(expected_output, &stdout, &stderr, expected_exit)?;
    }

    let final_lock = read_lock(project.path())?;
    let expects_no_lock = fixture
        .get("EXPECT-LOCK")
        .is_some_and(|value| value.trim() == "false");
    if expects_no_lock && final_lock.is_some() {
        bail!("fixture expected no composer.lock, but Riff created one");
    }
    if command_succeeded && final_lock.is_none() && !expects_no_lock && !fixture_dry_run {
        bail!("successful fixture did not produce composer.lock");
    }

    // `config.lock=false` deliberately leaves no artifact to compare. Re-run
    // the same isolated resolution with lock writing enabled only to obtain the
    // projected transaction; the first run above remains the behavioral check.
    let transaction_lock = if expects_no_lock && command_succeeded {
        let config = manifest
            .as_object_mut()
            .context("COMPOSER section must contain a JSON object")?
            .entry("config")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .context("COMPOSER.config must contain a JSON object")?;
        config.insert("lock".to_string(), Value::Bool(true));
        fs::write(
            project.path().join("composer.json"),
            format!("{}\n", serde_json::to_string_pretty(&manifest)?),
        )?;
        let projection = execute_riff(&arguments, project.path(), &command_environment)?;
        if !projection.status.success() {
            bail!(
                "lock projection failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&projection.stdout),
                String::from_utf8_lossy(&projection.stderr)
            );
        }
        read_lock(project.path())?
    } else if fixture_dry_run && command_succeeded && final_lock.is_none() {
        let mut projection_arguments = arguments.clone();
        projection_arguments.retain(|argument| argument != "--dry-run");
        let projection = execute_riff(&projection_arguments, project.path(), &command_environment)?;
        if !projection.status.success() {
            bail!(
                "dry-run lock projection failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&projection.stdout),
                String::from_utf8_lossy(&projection.stderr)
            );
        }
        read_lock(project.path())?
    } else {
        final_lock.clone()
    };

    let expected_text = required(&fixture, "EXPECT")?;
    let expected_trace = if expects_exception {
        let output_text = format!("{stdout}\n{stderr}");
        for line in expected_text.lines().filter(|line| !line.trim().is_empty()) {
            if !output_text.contains(line) {
                bail!("exception output is missing {line:?}\nstdout:\n{stdout}\nstderr:\n{stderr}");
            }
        }
        Vec::new()
    } else {
        parse_expected_operations(expected_text)?
    };
    // A failed command has no materialized transaction, and an explicit
    // --no-install intentionally changes only the lock. Those fixtures assert
    // the exit code and/or lock/installed state instead of package operations.
    if command_succeeded && !fixture_skips_installation && !expects_exception {
        let actual = if fixture_dry_run {
            Vec::new()
        } else {
            transaction_operations(installed.as_deref(), transaction_lock.as_ref(), install_dev)
        };
        if actual != expected_trace {
            bail!(
                "operation mismatch\nexpected: {expected_trace:#?}\nactual: {actual:#?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
        }
    }

    if let Some(expected) = fixture
        .get("EXPECT-LOCK")
        .filter(|value| !value.trim().is_empty() && value.trim() != "false")
    {
        let expected: Value = serde_json::from_str(expected).context("invalid EXPECT-LOCK JSON")?;
        let actual = serde_json::to_value(
            final_lock
                .as_ref()
                .context("EXPECT-LOCK requires a generated lock file")?,
        )?;
        assert_lock_matches(&expected, &actual)?;
    }

    if command_succeeded && !fixture_skips_installation && !fixture_dry_run {
        if let Some(lock) = &transaction_lock {
            let projected: Vec<_> = if install_dev {
                lock.all_packages().cloned().collect()
            } else {
                lock.packages.clone()
            };
            write_installed(project.path(), &projected)?;
        }
    }

    if let Some(expected) = fixture
        .get("EXPECT-INSTALLED")
        .filter(|value| !value.trim().is_empty())
    {
        let expected: Vec<Value> =
            serde_json::from_str(expected).context("invalid EXPECT-INSTALLED JSON")?;
        let actual = read_installed(project.path())?;
        assert_package_list_matches("installed packages", &expected, &actual)?;
    }

    Ok(())
}

fn execute_riff(
    arguments: &[String],
    project: &Path,
    environment: &HashMap<String, String>,
) -> Result<std::process::Output> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_riff"));
    command
        .args(arguments)
        .env("RIFF_CACHE_DIR", project.join("cache"))
        .env("COMPOSER_HOME", project.join("composer-home"))
        .env_remove("COMPOSER_ROOT_VERSION");
    command.envs(environment);
    command.output().context("failed to execute Riff")
}

fn parse_condition(condition: Option<&str>) -> Result<HashMap<String, String>> {
    let mut environment = HashMap::new();
    let Some(condition) = condition.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(environment);
    };
    if condition == "!defined('HHVM_VERSION')" {
        return Ok(environment);
    }

    let putenv = Regex::new(r"^putenv\('([^'=]+)=([^']*)'\)$").expect("valid putenv regex");
    if let Some(captures) = putenv.captures(condition) {
        environment.insert(captures[1].to_string(), captures[2].to_string());
        return Ok(environment);
    }

    bail!("unsupported Composer fixture CONDITION {condition:?}")
}

fn assert_output_semantics(
    expected: &str,
    stdout: &str,
    stderr: &str,
    expected_exit: i32,
) -> Result<()> {
    let strip_tags = Regex::new(r"</?(?:warning|info|comment|error)>").expect("valid tag regex");
    let package_name =
        Regex::new(r"(?i)\b[a-z0-9_-]+/[a-z0-9_.-]+\b").expect("valid package-name regex");
    let platform_package = Regex::new(
        r"(?i)\b(?:php(?:-64bit|-ipv6)?|hhvm|composer-(?:plugin-api|runtime-api)|ext-[a-z0-9_.-]+|lib-[a-z0-9_.-]+)\b",
    )
    .expect("valid platform package regex");
    let actual = strip_tags
        .replace_all(&format!("{stdout}\n{stderr}"), "")
        .to_ascii_lowercase();
    let expected = strip_tags.replace_all(expected, "");

    let is_boilerplate = |line: &str| {
        let line = line.trim();
        line.is_empty()
            || line.starts_with("Loading composer repositories")
            || line == "Updating dependencies"
            || line.starts_with("Installing dependencies from lock file")
            || line == "Verifying lock file contents can be installed on current platform."
            || line == "Your requirements could not be resolved to an installable set of packages."
            || line.starts_with("Lock file operations:")
            || line.starts_with("Package operations:")
            || line == "Writing lock file"
            || line == "Generating autoload files"
            || line.starts_with("- Locking ")
            || line.starts_with("- Installing ")
            || line.starts_with("- Upgrading ")
            || line.starts_with("- Downgrading ")
            || line.starts_with("- Removing ")
            || line.starts_with("Problem ")
    };
    let semantic_lines: Vec<_> = expected
        .lines()
        .map(str::trim)
        .filter(|line| !is_boilerplate(line))
        .collect();

    let mut expected_packages: std::collections::BTreeSet<_> = semantic_lines
        .iter()
        .flat_map(|line| {
            let matches: Vec<_> = package_name.find_iter(line).collect();
            if line.starts_with("- Required package \"") {
                matches.into_iter().take(1).collect::<Vec<_>>()
            } else {
                matches
            }
        })
        .map(|found| found.as_str().to_ascii_lowercase())
        .filter(|name| !name.starts_with("org/") && !name.starts_with("articles/"))
        .collect();
    expected_packages.extend(
        semantic_lines
            .iter()
            .flat_map(|line| platform_package.find_iter(line))
            .map(|found| found.as_str().to_ascii_lowercase()),
    );
    let has_expected_package = expected_packages
        .iter()
        .any(|package| actual.contains(package.as_str()));
    if !expected_packages.is_empty() && !has_expected_package {
        bail!(
            "output does not identify any expected package {expected_packages:?}\nexpected Composer semantics:\n{expected}\nactual stdout:\n{stdout}\nactual stderr:\n{stderr}"
        );
    }

    let categories: &[(&str, &[&str])] = &[
        (
            "could not be resolved",
            &["could not resolve", "could not be resolved"],
        ),
        (
            "does not match the constraint",
            &[
                "does not match the constraint",
                "does not satisfy",
                "no version satisfying",
                "no matching package",
                "does not contain a matching package",
            ],
        ),
        (
            "not present in the lock file",
            &["not present in the lock file", "lock file does not contain"],
        ),
        (
            "listed for update is not locked",
            &["listed for update is not locked"],
        ),
        (
            "listed for update does not match any locked packages",
            &["does not match any locked packages"],
        ),
        ("higher repository priority", &["repository priority"]),
        (
            "root package and cannot be modified",
            &["root package", "cannot be modified"],
        ),
        (
            "lock file is not up to date",
            &["lock file is not up to date"],
        ),
        ("abandoned", &["abandoned"]),
        ("funding", &["fund"]),
        ("suggestions", &["suggest"]),
        ("security advisory", &["security", "advisory"]),
        ("malware", &["malware"]),
    ];
    let expected_lower = expected.to_ascii_lowercase();
    for (trigger, alternatives) in categories {
        if expected_lower.contains(trigger)
            && !alternatives
                .iter()
                .any(|alternative| actual.contains(alternative))
        {
            bail!(
                "output is missing semantic category {trigger:?}\nexpected Composer semantics:\n{expected}\nactual stdout:\n{stdout}\nactual stderr:\n{stderr}"
            );
        }
    }

    if expected_exit != 0 && actual.trim().is_empty() {
        bail!("failing fixture produced no diagnostic output");
    }
    Ok(())
}

fn parse_sections(source: &str) -> Result<HashMap<String, String>> {
    let marker = Regex::new(r"^--([A-Z-]+)--$").expect("valid fixture marker regex");
    let mut sections = HashMap::new();
    let mut current: Option<String> = None;
    let mut content = String::new();
    for line in source.lines() {
        if let Some(captures) = marker.captures(line) {
            if let Some(section) = current.replace(captures[1].to_string()) {
                sections.insert(section, content.trim().to_string());
                content.clear();
            }
        } else if current.is_some() {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(line);
        } else if !line.trim().is_empty() {
            bail!("content found before first fixture section");
        }
    }
    if let Some(section) = current {
        sections.insert(section, content.trim().to_string());
    }
    for required in ["TEST", "COMPOSER", "RUN", "EXPECT"] {
        if !sections.contains_key(required) {
            bail!("fixture is missing required {required} section");
        }
    }
    Ok(sections)
}

fn required<'a>(fixture: &'a HashMap<String, String>, section: &str) -> Result<&'a str> {
    fixture
        .get(section)
        .map(String::as_str)
        .with_context(|| format!("fixture is missing {section}"))
}

fn prepare_manifest(manifest: &mut Value, project: &Path) -> Result<()> {
    let object = manifest
        .as_object_mut()
        .context("COMPOSER section must contain a JSON object")?;
    object
        .entry("name")
        .or_insert_with(|| Value::String("fixture/root".to_string()));
    let repositories = object
        .entry("repositories")
        .or_insert_with(|| Value::Array(Vec::new()));
    match repositories {
        Value::Array(repositories) => {
            for repository in repositories.iter_mut() {
                prepare_repository(repository, project)?;
            }
            repositories.push(json!({"packagist.org": false}));
        }
        Value::Object(repositories) => {
            for repository in repositories.values_mut() {
                prepare_repository(repository, project)?;
            }
            repositories.insert("packagist.org".to_string(), Value::Bool(false));
        }
        _ => bail!("repositories must be an array or object"),
    }
    Ok(())
}

fn prepare_repository(repository: &mut Value, project: &Path) -> Result<()> {
    let Some(repository) = repository.as_object_mut() else {
        return Ok(());
    };
    if repository.get("type").and_then(Value::as_str) == Some("composer") {
        if let Some(relative) = repository
            .get("url")
            .and_then(Value::as_str)
            .and_then(|url| url.strip_prefix("file://"))
            .filter(|path| !path.starts_with('/'))
        {
            let absolute = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/composer/installer")
                .join(relative);
            repository.insert(
                "url".to_string(),
                Value::String(format!("file://{}", absolute.display())),
            );
        }
        return Ok(());
    }
    if repository.get("type").and_then(Value::as_str) == Some("path") {
        let Some(relative) = repository
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !Path::new(url).is_absolute())
        else {
            return Ok(());
        };
        let relative = relative.trim_start_matches("./");
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/composer/path-repositories")
            .join(relative);
        if source.is_dir() {
            copy_fixture_directory(&source, &project.join(relative))?;
        }
        return Ok(());
    }
    if repository.get("type").and_then(Value::as_str) != Some("package") {
        return Ok(());
    }
    let packages = repository
        .get_mut("package")
        .context("package repository is missing package data")?;
    match packages {
        Value::Array(packages) => {
            for package in packages {
                prepare_package(package, project)?;
            }
        }
        Value::Object(_) => prepare_package(packages, project)?,
        _ => bail!("package repository data must be an object or array"),
    }
    Ok(())
}

fn copy_fixture_directory(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_fixture_directory(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn prepare_package(package: &mut Value, project: &Path) -> Result<()> {
    let package = package
        .as_object_mut()
        .context("package definition must be an object")?;
    if !package.contains_key("dist") && !package.contains_key("source") {
        package.insert(
            "dist".to_string(),
            json!({
                "type": "path",
                "url": project.join("fixture-source")
            }),
        );
    }
    Ok(())
}

fn initial_lock(
    fixture: &HashMap<String, String>,
    manifest_content: &str,
) -> Result<Option<RiffLockfile>> {
    let mut lock: RiffLockfile =
        if let Some(raw) = fixture.get("LOCK").filter(|value| !value.trim().is_empty()) {
            serde_json::from_str(raw).context("invalid LOCK JSON")?
        } else {
            return Ok(None);
        };
    if lock.content_hash.is_empty() {
        lock.content_hash = riff_core::compute_content_hash(manifest_content);
    }
    Ok(Some(lock))
}

fn write_lock(project: &Path, lock: &RiffLockfile) -> Result<()> {
    fs::write(
        project.join("composer.lock"),
        format!("{}\n", serde_json::to_string_pretty(lock)?),
    )?;
    Ok(())
}

fn read_lock(project: &Path) -> Result<Option<RiffLockfile>> {
    let path = project.join("composer.lock");
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

fn write_installed(project: &Path, packages: &[LockedPackage]) -> Result<()> {
    let directory = project.join("vendor/composer");
    fs::create_dir_all(&directory)?;
    fs::write(
        directory.join("installed.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({"packages": packages}))?
        ),
    )?;
    Ok(())
}

fn read_installed(project: &Path) -> Result<Vec<Value>> {
    let value: Value =
        serde_json::from_slice(&fs::read(project.join("vendor/composer/installed.json"))?)?;
    value
        .get("packages")
        .and_then(Value::as_array)
        .cloned()
        .context("installed.json does not contain a packages array")
}

fn push_flag(arguments: &mut Vec<String>, flag: &str) {
    if !arguments.iter().any(|argument| argument == flag) {
        arguments.push(flag.to_string());
    }
}

fn transaction_operations(
    before: Option<&[LockedPackage]>,
    after: Option<&RiffLockfile>,
    install_dev: bool,
) -> Vec<Operation> {
    let before_packages: Vec<_> = before
        .unwrap_or_default()
        .iter()
        .map(Package::from)
        .collect();
    let after_packages: Vec<_> = after.map_or_else(Vec::new, |lock| {
        if install_dev {
            lock.all_packages().map(Package::from).collect()
        } else {
            lock.packages.iter().map(Package::from).collect()
        }
    });
    let before_packages: Vec<_> = before_packages.into_iter().map(Arc::new).collect();
    let after_packages: Vec<_> = after_packages.into_iter().map(Arc::new).collect();
    let mut transaction = Transaction::from_package_sets(
        before_packages.clone(),
        transaction_aliases(&before_packages, &[]),
        after_packages.clone(),
        transaction_aliases(
            &after_packages,
            after.map_or(&[], |lock| lock.aliases.as_slice()),
        ),
    );
    transaction.sort();
    transaction
        .operations
        .into_iter()
        .filter_map(|operation| match operation {
            RiffOperation::Install(package) => Some(Operation::Install {
                package: package.name.clone(),
                version: operation_version(&package),
            }),
            RiffOperation::Update { from, to } => {
                let (from_version, to_version) = update_operation_versions(&from, &to);
                Some(Operation::Update {
                    package: to.name.clone(),
                    from: from_version,
                    to: to_version,
                })
            }
            RiffOperation::Uninstall(package) => Some(Operation::Remove {
                package: package.name.clone(),
                version: operation_version(&package),
            }),
            RiffOperation::MarkAliasInstalled(alias) => Some(Operation::Alias {
                package: alias.name().to_string(),
                version: operation_alias_version(&alias),
                alias_of: operation_version(alias.alias_of()),
                installed: true,
            }),
            RiffOperation::MarkAliasUninstalled(alias) => Some(Operation::Alias {
                package: alias.name().to_string(),
                version: operation_alias_version(&alias),
                alias_of: operation_version(alias.alias_of()),
                installed: false,
            }),
            RiffOperation::Reinstall(_) | RiffOperation::MarkUnneeded(_) => None,
        })
        .collect()
}

fn transaction_aliases(
    packages: &[Arc<Package>],
    lock_aliases: &[riff_core::json::LockAlias],
) -> Vec<Arc<AliasPackage>> {
    let mut aliases = Vec::new();

    for package in packages {
        let branch_aliases = parse_branch_aliases(package.extra.as_ref());
        let mut has_explicit_branch_alias = false;
        for (source, (normalized, pretty)) in branch_aliases {
            if package.version == source || package.pretty_version() == source {
                has_explicit_branch_alias = true;
                aliases.push(Arc::new(AliasPackage::new(
                    package.clone(),
                    normalized,
                    pretty,
                )));
            }
        }
        if package.default_branch == Some(true)
            && package.pretty_version().starts_with("dev-")
            && !has_explicit_branch_alias
        {
            aliases.push(Arc::new(AliasPackage::new(
                package.clone(),
                DEFAULT_BRANCH_ALIAS.to_string(),
                DEFAULT_BRANCH_ALIAS.to_string(),
            )));
        }
    }

    for lock_alias in lock_aliases {
        let Some(package) = packages
            .iter()
            .find(|package| package.name.eq_ignore_ascii_case(&lock_alias.package))
        else {
            continue;
        };
        let mut alias = AliasPackage::new(
            package.clone(),
            lock_alias.alias_normalized.clone(),
            lock_alias.alias.clone(),
        );
        alias.set_root_package_alias(true);
        aliases.push(Arc::new(alias));
    }

    aliases
}

fn operation_version(package: &Package) -> String {
    let version = package.pretty_version().to_string();
    decorate_reference(
        &version,
        package_reference(package),
        package.stability() == Stability::Dev,
    )
}

fn operation_alias_version(alias: &AliasPackage) -> String {
    decorate_reference(
        alias.pretty_version(),
        package_reference(alias.alias_of()),
        Stability::from_version(alias.pretty_version()) == Stability::Dev,
    )
}

fn update_operation_versions(from: &Package, to: &Package) -> (String, String) {
    let mut from_version = operation_version(from);
    let mut to_version = operation_version(to);

    if from_version == to_version {
        let from_source = from.source.as_ref().map(|source| source.reference.as_str());
        let to_source = to.source.as_ref().map(|source| source.reference.as_str());
        if from_source != to_source {
            from_version = decorate_reference(from.pretty_version(), from_source, true);
            to_version = decorate_reference(to.pretty_version(), to_source, true);
        } else {
            let from_dist = from
                .dist
                .as_ref()
                .and_then(|dist| dist.reference.as_deref());
            let to_dist = to.dist.as_ref().and_then(|dist| dist.reference.as_deref());
            if from_dist != to_dist {
                from_version = decorate_reference(from.pretty_version(), from_dist, true);
                to_version = decorate_reference(to.pretty_version(), to_dist, true);
            }
        }
    }

    (from_version, to_version)
}

fn decorate_reference(version: &str, reference: Option<&str>, include: bool) -> String {
    if include {
        if let Some(reference) = reference {
            let display_reference = if reference.len() == 40
                && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                &reference[..7]
            } else {
                reference
            };
            return format!("{version} {display_reference}");
        }
    }
    version.to_string()
}

fn package_reference(package: &Package) -> Option<&str> {
    package
        .source
        .as_ref()
        .map(|source| source.reference.as_str())
        .or_else(|| {
            package
                .dist
                .as_ref()
                .and_then(|dist| dist.reference.as_deref())
        })
        .filter(|reference| !reference.is_empty())
}

fn parse_expected_operations(trace: &str) -> Result<Vec<Operation>> {
    let install = Regex::new(r"^Installing (\S+) \(([^)]+)\)$").unwrap();
    let update = Regex::new(r"^(?:Upgrading|Downgrading) (\S+) \((.+) => (.+)\)$").unwrap();
    let remove = Regex::new(r"^Removing (\S+) \(([^)]+)\)$").unwrap();
    let alias = Regex::new(
        r"^Marking (\S+) \(([^)]+)\) as (installed|uninstalled), alias of \S+ \(([^)]+)\)$",
    )
    .unwrap();
    trace
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            if let Some(captures) = install.captures(line) {
                Ok(Operation::Install {
                    package: captures[1].to_string(),
                    version: captures[2].to_string(),
                })
            } else if let Some(captures) = update.captures(line) {
                Ok(Operation::Update {
                    package: captures[1].to_string(),
                    from: captures[2].to_string(),
                    to: captures[3].to_string(),
                })
            } else if let Some(captures) = remove.captures(line) {
                Ok(Operation::Remove {
                    package: captures[1].to_string(),
                    version: captures[2].to_string(),
                })
            } else if let Some(captures) = alias.captures(line) {
                Ok(Operation::Alias {
                    package: captures[1].to_string(),
                    version: captures[2].to_string(),
                    alias_of: captures[4].to_string(),
                    installed: &captures[3] == "installed",
                })
            } else {
                bail!("unsupported EXPECT operation {line:?}")
            }
        })
        .collect()
}

fn assert_lock_matches(expected: &Value, actual: &Value) -> Result<()> {
    let expected = expected
        .as_object()
        .context("EXPECT-LOCK must contain a JSON object")?;
    for (key, expected_value) in expected {
        if matches!(key.as_str(), "packages" | "packages-dev") {
            let expected_packages = expected_value
                .as_array()
                .with_context(|| format!("EXPECT-LOCK.{key} must be an array"))?;
            let actual_packages = actual
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            assert_package_list_matches(key, expected_packages, &actual_packages)?;
        } else {
            let actual_value = actual.get(key).unwrap_or(&Value::Null);
            if !value_contains(actual_value, expected_value) {
                bail!(
                    "lock field {key:?} mismatch\nexpected: {expected_value}\nactual: {actual_value}"
                );
            }
        }
    }
    Ok(())
}

fn assert_package_list_matches(label: &str, expected: &[Value], actual: &[Value]) -> Result<()> {
    let by_name = |packages: &[Value]| -> Result<BTreeMap<String, Value>> {
        packages
            .iter()
            .map(|package| {
                let name = package
                    .get("name")
                    .and_then(Value::as_str)
                    .context("expected package name")?;
                Ok((name.to_string(), package.clone()))
            })
            .collect()
    };
    let expected = by_name(expected)?;
    let actual = by_name(actual)?;
    if expected.len() != actual.len() || expected.keys().ne(actual.keys()) {
        bail!(
            "{label} names mismatch\nexpected: {:?}\nactual: {:?}",
            expected.keys().collect::<Vec<_>>(),
            actual.keys().collect::<Vec<_>>()
        );
    }
    for (name, expected_package) in expected {
        let actual_package = &actual[&name];
        if !value_contains(actual_package, &expected_package) {
            bail!(
                "{label} package {name:?} mismatch\nexpected subset: {expected_package}\nactual: {actual_package}"
            );
        }
    }
    Ok(())
}

fn value_contains(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected.iter().all(|(key, value)| {
            actual
                .get(key)
                .map(|actual| value_contains(actual, value))
                .unwrap_or_else(|| value.as_object().is_some_and(Map::is_empty))
        }),
        (Value::Array(actual), Value::Array(expected)) => {
            actual.len() == expected.len()
                && actual
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| value_contains(actual, expected))
        }
        (Value::Null, Value::Object(expected)) if expected.is_empty() => true,
        _ => actual == expected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_composer_fixture_sections() {
        let fixture =
            parse_sections("--TEST--\nExample\n--COMPOSER--\n{}\n--RUN--\nupdate\n--EXPECT--\n")
                .unwrap();
        assert_eq!(fixture["TEST"], "Example");
        assert_eq!(fixture["EXPECT"], "");
    }

    #[test]
    fn package_comparison_is_recursive_but_allows_extra_metadata() {
        assert!(value_contains(
            &json!({"name": "a/a", "version": "1.0.0", "dist": {"type": "path"}}),
            &json!({"name": "a/a", "version": "1.0.0"})
        ));
    }

    #[test]
    fn parses_reference_bearing_operations() {
        assert_eq!(
            parse_expected_operations(
                "Upgrading a/a (dev-main old-ref => dev-main new-ref)\nRemoving b/b (1.0.0 old-ref)"
            )
            .unwrap(),
            vec![
                Operation::Update {
                    package: "a/a".to_string(),
                    from: "dev-main old-ref".to_string(),
                    to: "dev-main new-ref".to_string(),
                },
                Operation::Remove {
                    package: "b/b".to_string(),
                    version: "1.0.0 old-ref".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parses_alias_markers() {
        assert_eq!(
            parse_expected_operations(
                "Marking a/a (9999999-dev abc) as installed, alias of a/a (dev-main abc)"
            )
            .unwrap(),
            vec![Operation::Alias {
                package: "a/a".to_string(),
                version: "9999999-dev abc".to_string(),
                alias_of: "dev-main abc".to_string(),
                installed: true,
            }]
        );
    }

    #[test]
    fn composer_operation_references_shorten_commit_hashes() {
        assert_eq!(
            decorate_reference(
                "dev-main",
                Some("459720ff3b74ee0c0d159277c6f2f5df89d8a4f6"),
                true
            ),
            "dev-main 459720f"
        );
        assert_eq!(
            decorate_reference("dev-main", Some("named-reference"), true),
            "dev-main named-reference"
        );
    }

    #[test]
    fn composer_update_format_falls_back_to_dist_when_source_refs_match() {
        let mut from = Package::new("vendor/package", "dev-main");
        from.source = Some(riff_core::package::Source::git(
            "https://example.test/repo.git",
            "same-ref",
        ));
        from.dist = Some(
            riff_core::package::Dist::zip("https://example.test/archive.zip")
                .with_reference("installed-dist-ref"),
        );
        let mut to = from.clone();
        to.dist = None;

        assert_eq!(
            update_operation_versions(&from, &to),
            (
                "dev-main installed-dist-ref".to_string(),
                "dev-main".to_string()
            )
        );
    }

    #[test]
    fn composer_output_comparison_uses_semantics_not_composer_formatting() {
        assert_output_semantics(
            "Your requirements could not be resolved to an installable set of packages.\n\
             - Root composer.json requires a/aliased 3.0.2, but it does not match the constraint.",
            "",
            "Error: Could not resolve dependencies\n- Root composer.json requires a/aliased 3.0.2, but no matching package was found",
            2,
        )
        .unwrap();

        let error = assert_output_semantics(
            "- Root composer.json requires missing/package 1.0.0",
            "",
            "Error: Could not resolve dependencies",
            2,
        )
        .unwrap_err();
        assert!(error.to_string().contains("missing/package"));

        assert_output_semantics(
            "- a/first requires b/second 1.0.0\n- b/second conflicts with c/third",
            "",
            "Error: Could not resolve dependencies\n- a/first requires b/second 1.0.0",
            2,
        )
        .unwrap();
    }

    #[test]
    fn composer_conditions_map_supported_environment_checks() {
        assert!(parse_condition(Some("!defined('HHVM_VERSION')"))
            .unwrap()
            .is_empty());
        assert_eq!(
            parse_condition(Some("putenv('COMPOSER_FUND=0')"))
                .unwrap()
                .get("COMPOSER_FUND")
                .map(String::as_str),
            Some("0")
        );
        assert!(parse_condition(Some("PHP_INT_SIZE === 8")).is_err());
    }
}
