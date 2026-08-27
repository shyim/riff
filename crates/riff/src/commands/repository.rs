use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use riff_core::config::ConfigLoader;
use riff_core::json::JsonManipulator;
use serde_json::Value;

#[derive(Debug, usage_rs::Args)]
pub struct RepositoryArgs {
    /// Apply the command to Composer's global config file
    #[usage(short = 'g', long)]
    pub global: bool,

    /// Read or modify a custom composer.json or config.json
    #[usage(short = 'f', long)]
    pub file: Option<PathBuf>,

    /// Append a repository instead of prepending it
    #[usage(long)]
    pub append: bool,

    /// Insert a repository before this named repository
    #[usage(long, value_name = "NAME")]
    pub before: Option<String>,

    /// Insert a repository after this named repository
    #[usage(long, value_name = "NAME")]
    pub after: Option<String>,

    /// Action: list, add, remove, set-url, get-url, enable, or disable
    #[usage(arg, name = "ACTION")]
    pub action: Option<String>,

    /// Repository name
    #[usage(arg, name = "NAME")]
    pub name: Option<String>,

    /// Repository type, JSON configuration, or new URL
    #[usage(arg, name = "TYPE-OR-JSON-OR-URL")]
    pub arg1: Option<String>,

    /// Repository URL when adding by type
    #[usage(arg, name = "URL")]
    pub arg2: Option<String>,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub async fn execute(args: RepositoryArgs, context: &crate::CommandContext) -> Result<i32> {
    let global_home = ConfigLoader::new(true).get_composer_home();
    for line in run(args, &global_home)? {
        riff_core::outln!(context.output(), "{line}");
    }
    Ok(0)
}

fn run(args: RepositoryArgs, global_home: &Path) -> Result<Vec<String>> {
    if args.global && args.file.is_some() {
        bail!("--file and --global can not be combined");
    }
    if args.before.is_some() && args.after.is_some() {
        bail!("You can not combine --before and --after");
    }

    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let target = target_path(&args, global_home, &working_dir);
    let contents = read_target(&target, args.global)?;
    let parsed: Value = serde_json::from_str(&contents)
        .with_context(|| format!("{} does not contain valid JSON", target.display()))?;
    if !parsed.is_object() {
        bail!("{} must contain a JSON object", target.display());
    }

    let action = args
        .action
        .as_deref()
        .unwrap_or("list")
        .to_ascii_lowercase();
    if matches!(action.as_str(), "list" | "ls" | "show") {
        return list_repositories(&parsed);
    }
    if matches!(action.as_str(), "get-url" | "geturl") {
        let name = args
            .name
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Usage: riff repo get-url <name>"))?;
        return Ok(vec![repository_url(&parsed, name)?.to_string()]);
    }

    let mut document = JsonManipulator::new(&contents)?;
    match action.as_str() {
        "add" => {
            let name = args.name.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "You must pass a repository name. Example: riff repo add foo vcs https://example.org"
                )
            })?;
            let arg1 = args.arg1.as_deref().ok_or_else(|| {
                anyhow::anyhow!("You must pass the type and a url, or a JSON string.")
            })?;
            let config = repository_config(arg1, args.arg2.as_deref())?;
            if let Some(reference) = args.before.as_deref().or(args.after.as_deref()) {
                let offset = usize::from(args.after.is_some());
                if !document.insert_repository(name, config, reference, offset)? {
                    bail!("There is no {reference} repository defined");
                }
            } else {
                document.add_repository(name, config, args.append)?;
            }
        }
        "remove" | "rm" | "delete" => {
            let name = args
                .name
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("You must pass the repository name to remove."))?;
            document.remove_repository(normalize_packagist_name(name))?;
            if is_packagist_name(name) {
                document.add_repository("packagist.org", Value::Bool(false), false)?;
            }
        }
        "set-url" | "seturl" => {
            let (Some(name), Some(url)) = (args.name.as_deref(), args.arg1.as_deref()) else {
                bail!("Usage: riff repo set-url <name> <new-url>");
            };
            if !document.set_repository_url(name, url)? {
                bail!("There is no {name} repository defined");
            }
        }
        "disable" => {
            let name = args
                .name
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Usage: riff repo disable packagist.org"))?;
            if !is_packagist_name(name) {
                bail!("Only packagist.org can be enabled/disabled using this command. Use add/remove for other repositories.");
            }
            document.add_repository("packagist.org", Value::Bool(false), args.append)?;
        }
        "enable" => {
            let name = args
                .name
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Usage: riff repo enable packagist.org"))?;
            if !is_packagist_name(name) {
                bail!("Only packagist.org can be enabled/disabled using this command.");
            }
            document.remove_repository("packagist.org")?;
        }
        _ => bail!(
            "Unknown action \"{action}\". Use list, add, remove, set-url, get-url, enable, disable"
        ),
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    fs::write(&target, document.contents())
        .with_context(|| format!("Failed to write {}", target.display()))?;
    Ok(Vec::new())
}

fn target_path(args: &RepositoryArgs, global_home: &Path, working_dir: &Path) -> PathBuf {
    if args.global {
        return global_home.join("config.json");
    }
    let requested = args
        .file
        .as_deref()
        .unwrap_or_else(|| Path::new("composer.json"));
    if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        working_dir.join(requested)
    }
}

fn read_target(path: &Path, allow_missing: bool) -> Result<String> {
    if !path.exists() {
        if allow_missing {
            return Ok("{}\n".to_string());
        }
        bail!("File \"{}\" cannot be found", path.display());
    }
    fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))
}

fn repository_config(arg1: &str, arg2: Option<&str>) -> Result<Value> {
    if arg1.trim_start().starts_with('{') {
        let value: Value = serde_json::from_str(arg1).context("Repository JSON is invalid")?;
        if !value.is_object() {
            bail!("Repository JSON must be an object");
        }
        return Ok(value);
    }
    let url = arg2.ok_or_else(|| {
        anyhow::anyhow!(
            "You must pass the type and a url. Example: riff repo add foo vcs https://example.org"
        )
    })?;
    Ok(serde_json::json!({"type": arg1, "url": url}))
}

fn list_repositories(document: &Value) -> Result<Vec<String>> {
    let Some(repositories) = document.get("repositories") else {
        return Ok(vec![
            "[packagist.org] composer https://repo.packagist.org".to_string()
        ]);
    };
    let mut lines = Vec::new();
    let mut packagist_present = false;
    match repositories {
        Value::Array(entries) => {
            for (index, repository) in entries.iter().enumerate() {
                packagist_present |= is_packagist_repository(repository);
                if let Some(line) = repository_line(&index.to_string(), repository)? {
                    lines.push(line);
                }
            }
        }
        Value::Object(entries) => {
            for (name, repository) in entries {
                packagist_present |= is_packagist_repository(repository);
                if let Some(line) = repository_line(name, repository)? {
                    lines.push(line);
                }
            }
        }
        _ => bail!("repositories must be an object or array"),
    }
    if lines.is_empty() {
        return Ok(vec![
            "[packagist.org] composer https://repo.packagist.org".to_string()
        ]);
    }
    if !packagist_present
        && !lines
            .iter()
            .any(|line| line.starts_with("[packagist.org] disabled"))
    {
        lines.push("[packagist.org] disabled".to_string());
    }
    Ok(lines)
}

fn repository_line(default_name: &str, repository: &Value) -> Result<Option<String>> {
    if repository == &Value::Bool(false) {
        return Ok(Some(format!("[{default_name}] disabled")));
    }
    let Some(repository) = repository.as_object() else {
        return Ok(None);
    };
    if repository.len() == 1 {
        if let Some((name, Value::Bool(false))) = repository.iter().next() {
            return Ok(Some(format!("[{name}] disabled")));
        }
    }
    let name = repository
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(default_name);
    let repository_type = repository
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let url = match repository.get("url").and_then(Value::as_str) {
        Some(url) => url.to_string(),
        None => serde_json::to_string(repository)?,
    };
    Ok(Some(format!("[{name}] {repository_type} {url}")))
}

fn repository_url<'a>(document: &'a Value, name: &str) -> Result<&'a str> {
    let repositories = document
        .get("repositories")
        .ok_or_else(|| anyhow::anyhow!("There is no {name} repository defined"))?;
    let repository = match repositories {
        Value::Object(entries) => entries.get(name).or_else(|| {
            entries
                .values()
                .find(|repository| repository.get("name").and_then(Value::as_str) == Some(name))
        }),
        Value::Array(entries) => entries
            .iter()
            .find(|repository| repository.get("name").and_then(Value::as_str) == Some(name)),
        _ => None,
    }
    .ok_or_else(|| anyhow::anyhow!("There is no {name} repository defined"))?;
    repository
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("The {name} repository does not have a URL"))
}

fn is_packagist_repository(repository: &Value) -> bool {
    let Some(repository) = repository.as_object() else {
        return false;
    };
    repository.get("type").and_then(Value::as_str) == Some("composer")
        && repository
            .get("url")
            .and_then(Value::as_str)
            .and_then(|url| reqwest::Url::parse(url).ok())
            .and_then(|url| url.host_str().map(str::to_owned))
            .is_some_and(|host| host == "packagist.org" || host.ends_with(".packagist.org"))
}

fn is_packagist_name(name: &str) -> bool {
    matches!(name, "packagist" | "packagist.org")
}

fn normalize_packagist_name(name: &str) -> &str {
    if is_packagist_name(name) {
        "packagist.org"
    } else {
        name
    }
}
