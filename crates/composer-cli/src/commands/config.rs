use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::{Map, Number, Value};
use sonata_core::config::ConfigLoader;

const ROOT_STRING_PROPERTIES: &[&str] = &["name", "type", "description", "homepage", "version"];
const ROOT_LIST_PROPERTIES: &[&str] = &["keywords", "license"];
const BOOL_CONFIG_KEYS: &[&str] = &[
    "use-include-path",
    "use-github-api",
    "notify-on-install",
    "sort-packages",
    "optimize-autoloader",
    "classmap-authoritative",
    "apcu-autoloader",
    "prepend-autoloader",
    "update-with-minimal-changes",
    "disable-tls",
    "secure-http",
    "source-fallback",
    "github-expose-hostname",
    "htaccess-protect",
    "lock",
    "allow-plugins",
    "audit.ignore-unreachable",
    "audit.block-insecure",
    "audit.block-abandoned",
    "policy.advisories.block",
    "policy.malware.block",
    "policy.abandoned.block",
    "policy.ignore-unreachable",
];
const INTEGER_CONFIG_KEYS: &[&str] = &["process-timeout", "cache-ttl", "cache-files-ttl"];
const STRING_CONFIG_KEYS: &[&str] = &[
    "vendor-dir",
    "bin-dir",
    "archive-dir",
    "archive-format",
    "data-dir",
    "cache-dir",
    "cache-files-dir",
    "cache-repo-dir",
    "cache-vcs-dir",
    "cache-files-maxsize",
    "autoloader-suffix",
    "cafile",
    "capath",
];
const MULTI_CONFIG_KEYS: &[&str] = &[
    "github-protocols",
    "github-domains",
    "gitlab-domains",
    "audit.ignore-severity",
    "policy.advisories.ignore-severity",
    "policy.malware.ignore-source",
];
const JSON_MERGE_KEYS: &[&str] = &[
    "audit.ignore",
    "audit.ignore-abandoned",
    "policy.advisories.ignore",
    "policy.advisories.ignore-id",
    "policy.malware.ignore",
    "policy.abandoned.ignore",
];
const AUTH_KEYS: &[&str] = &[
    "bitbucket-oauth",
    "github-oauth",
    "gitlab-oauth",
    "gitlab-token",
    "http-basic",
    "custom-headers",
    "bearer",
    "forgejo-token",
];

#[derive(usage_rs::Args, Debug)]
pub struct ConfigArgs {
    /// Apply the command to Composer's global config file
    #[usage(short = 'g', long)]
    pub global: bool,

    /// Open the selected config file in an editor
    #[usage(short = 'e', long)]
    pub editor: bool,

    /// Edit auth.json instead of the regular config file with --editor
    #[usage(short = 'a', long)]
    pub auth: bool,

    /// Remove the selected setting
    #[usage(long)]
    pub unset: bool,

    /// List configuration settings
    #[usage(short = 'l', long)]
    pub list: bool,

    /// Read or modify a custom composer.json or config.json
    #[usage(short = 'f', long)]
    pub file: Option<PathBuf>,

    /// Return absolute values for directory settings
    #[usage(long)]
    pub absolute: bool,

    /// Decode the setting value as JSON
    #[usage(short = 'j', long)]
    pub json: bool,

    /// Merge a decoded JSON value with the current value
    #[usage(short = 'm', long)]
    pub merge: bool,

    /// Append a repository instead of prepending it
    #[usage(long)]
    pub append: bool,

    /// Show where a value was loaded from
    #[usage(long)]
    pub source: bool,

    /// Setting to read, write, or remove
    #[usage(value_name = "SETTING-KEY")]
    pub setting_key: Option<String>,

    /// Value assigned to the setting
    #[usage(value_name = "SETTING-VALUE")]
    pub setting_value: Vec<String>,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug)]
struct ConfigFiles {
    config_path: PathBuf,
    auth_path: PathBuf,
    source_name: String,
    global_home: PathBuf,
}

pub async fn execute(args: ConfigArgs) -> Result<i32> {
    if args.global && args.file.is_some() {
        bail!("--file and --global can not be combined");
    }
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let files = resolve_files(&args, &working_dir);

    if args.global {
        initialize_global_files(&files)?;
    } else if !files.config_path.exists() {
        bail!(
            "File \"{}\" cannot be found in the current directory",
            args.file
                .as_deref()
                .unwrap_or_else(|| Path::new("./composer.json"))
                .display()
        );
    }

    if args.editor {
        let path = if args.auth {
            &files.auth_path
        } else {
            &files.config_path
        };
        open_editor(path)?;
        return Ok(0);
    }

    let mut document = read_json_object(&files.config_path)?;
    let auth_document = read_optional_json_object(&files.auth_path)?;
    let merged = load_merged_config(&args, &working_dir, &files, &document, &auth_document)?;
    let disable_tls = nested(&merged.value, &["config", "disable-tls"])
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if disable_tls && args.setting_key.as_deref() != Some("disable-tls") {
        eprintln!("You are running Composer with SSL/TLS protection disabled.");
    }

    if args.list {
        list_configuration(
            &merged.value,
            &merged.sources,
            args.source,
            &working_dir,
            &files,
        )?;
        return Ok(0);
    }

    let Some(setting_key) = args.setting_key.as_deref() else {
        return Ok(0);
    };

    if args.unset && !args.setting_value.is_empty() {
        bail!("You can not combine a setting value with --unset");
    }

    if args.setting_value.is_empty() && !args.unset {
        let (value, source) = read_setting(
            setting_key,
            &merged,
            &document,
            &files,
            &working_dir,
            args.absolute,
        )?;
        let mut output = display_value(value);
        if args.source {
            output.push_str(" (");
            output.push_str(&source);
            output.push(')');
        }
        println!("{output}");
        return Ok(0);
    }

    if args.unset {
        if setting_key == "disable-tls" && disable_tls {
            eprintln!("You are now running Composer with SSL/TLS protection enabled.");
        }
        unset_setting(setting_key, &mut document, &files)?;
    } else {
        if setting_key == "disable-tls" {
            if let Some(value) = args.setting_value.first() {
                let next = parse_bool(value)?;
                if next && !disable_tls {
                    eprintln!("You are now running Composer with SSL/TLS protection disabled.");
                } else if !next && disable_tls {
                    eprintln!("You are now running Composer with SSL/TLS protection enabled.");
                }
            }
        }
        set_setting(setting_key, &args, &mut document, &files)?;
    }
    write_json(&files.config_path, &document)?;

    Ok(0)
}

fn resolve_files(args: &ConfigArgs, working_dir: &Path) -> ConfigFiles {
    let loader = ConfigLoader::new(true);
    let global_home = loader.get_composer_home();
    if args.global {
        return ConfigFiles {
            config_path: global_home.join("config.json"),
            auth_path: global_home.join("auth.json"),
            source_name: global_home.join("config.json").display().to_string(),
            global_home,
        };
    }

    let requested = args
        .file
        .as_deref()
        .unwrap_or_else(|| Path::new("composer.json"));
    let config_path = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        working_dir.join(requested)
    };
    let source_name = if args.file.is_none() {
        "./composer.json".to_string()
    } else {
        config_path.display().to_string()
    };
    let auth_path = config_path
        .parent()
        .unwrap_or(working_dir)
        .join("auth.json");

    ConfigFiles {
        config_path,
        auth_path,
        source_name,
        global_home,
    }
}

fn initialize_global_files(files: &ConfigFiles) -> Result<()> {
    fs::create_dir_all(&files.global_home)
        .with_context(|| format!("Failed to create {}", files.global_home.display()))?;
    if !files.config_path.exists() {
        write_json(&files.config_path, &serde_json::json!({"config": {}}))?;
    }
    if !files.auth_path.exists() {
        write_json(
            &files.auth_path,
            &serde_json::json!({
                "bitbucket-oauth": {},
                "github-oauth": {},
                "gitlab-oauth": {},
                "gitlab-token": {},
                "http-basic": {},
                "bearer": {},
                "forgejo-token": {}
            }),
        )?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&files.config_path, fs::Permissions::from_mode(0o600))?;
        fs::set_permissions(&files.auth_path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn read_json_object(path: &Path) -> Result<Value> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("{} does not contain valid JSON", path.display()))?;
    if !value.is_object() {
        bail!("{} must contain a JSON object", path.display());
    }
    Ok(value)
}

fn read_optional_json_object(path: &Path) -> Result<Value> {
    if path.exists() {
        read_json_object(path)
    } else {
        Ok(Value::Object(Map::new()))
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut content = serde_json::to_string_pretty(value)?;
    content.push('\n');
    fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))
}

fn open_editor(path: &Path) -> Result<()> {
    let editor = env::var("EDITOR")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(find_editor)
        .ok_or_else(|| anyhow::anyhow!("No text editor could be found"))?;

    #[cfg(unix)]
    let status = Command::new("sh")
        .args(["-c", "$EDITOR \"$1\"", "sonata-editor"])
        .arg(path)
        .env("EDITOR", editor)
        .status()?;
    #[cfg(not(unix))]
    let status = Command::new(editor).arg(path).status()?;

    if !status.success() {
        bail!("Editor exited with status {status}");
    }
    Ok(())
}

fn find_editor() -> Option<String> {
    ["editor", "vim", "vi", "nano", "pico", "ed"]
        .into_iter()
        .find(|candidate| {
            Command::new("sh")
                .args([
                    "-c",
                    "command -v \"$1\" >/dev/null 2>&1",
                    "sonata-editor",
                    candidate,
                ])
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        })
        .map(str::to_string)
}

#[derive(Debug)]
struct MergedConfig {
    value: Value,
    sources: Map<String, Value>,
}

fn load_merged_config(
    args: &ConfigArgs,
    working_dir: &Path,
    files: &ConfigFiles,
    selected: &Value,
    selected_auth: &Value,
) -> Result<MergedConfig> {
    let mut value = default_configuration(&files.global_home);
    let mut sources = source_tree(&value, "default");

    if args.global {
        merge_document(&mut value, &mut sources, selected, &files.source_name);
        merge_auth(
            &mut value,
            &mut sources,
            selected_auth,
            &files.auth_path.display().to_string(),
        );
    } else {
        let global_path = files.global_home.join("config.json");
        let global_auth_path = files.global_home.join("auth.json");
        if global_path.exists() {
            let global = read_json_object(&global_path)?;
            merge_document(
                &mut value,
                &mut sources,
                &global,
                &global_path.display().to_string(),
            );
        }
        if global_auth_path.exists() {
            let global_auth = read_json_object(&global_auth_path)?;
            merge_auth(
                &mut value,
                &mut sources,
                &global_auth,
                &global_auth_path.display().to_string(),
            );
        }
        merge_document(&mut value, &mut sources, selected, &files.source_name);
        merge_auth(
            &mut value,
            &mut sources,
            selected_auth,
            &files.auth_path.display().to_string(),
        );
    }

    apply_environment(&mut value, &mut sources);
    let disable_tls = nested(&value, &["config", "disable-tls"])
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if disable_tls {
        set_nested(&mut value, &["config", "secure-http"], Value::Bool(false));
    } else if nested(&value, &["config", "secure-http"]).and_then(Value::as_bool) == Some(false) {
        if let Some(protocols) = value
            .get_mut("config")
            .and_then(|config| config.get_mut("github-protocols"))
            .and_then(Value::as_array_mut)
        {
            if !protocols.iter().any(|protocol| protocol == "git") {
                protocols.push(Value::String("git".to_string()));
            }
        }
    }
    ensure_packagist(&mut value, &mut sources);
    resolve_computed_paths(&mut value, working_dir, &files.global_home, args.global);

    Ok(MergedConfig { value, sources })
}

fn default_configuration(home: &Path) -> Value {
    let cache = home.join("cache");
    let mut value: Value = serde_json::from_str(DEFAULT_CONFIG_JSON)
        .expect("embedded Composer defaults must be valid JSON");
    set_nested(
        &mut value,
        &["config", "cache-dir"],
        Value::String(cache.display().to_string()),
    );
    set_nested(
        &mut value,
        &["config", "data-dir"],
        Value::String(home.display().to_string()),
    );
    set_nested(
        &mut value,
        &["config", "home"],
        Value::String(home.display().to_string()),
    );
    value
}

const DEFAULT_CONFIG_JSON: &str = r#"{
  "repositories": {
    "packagist.org": {"type": "composer", "url": "https://repo.packagist.org"}
  },
  "config": {
    "process-timeout": 300,
    "use-include-path": false,
    "use-parent-dir": "prompt",
    "preferred-install": "dist",
    "audit": {"abandoned": "fail"},
    "policy": true,
    "notify-on-install": true,
    "github-protocols": ["https", "ssh"],
    "gitlab-protocol": null,
    "vendor-dir": "vendor",
    "bin-dir": "{$vendor-dir}/bin",
    "cache-dir": "",
    "data-dir": "",
    "cache-files-dir": "{$cache-dir}/files",
    "cache-repo-dir": "{$cache-dir}/repo",
    "cache-vcs-dir": "{$cache-dir}/vcs",
    "cache-ttl": 15552000,
    "cache-files-ttl": 15552000,
    "cache-files-maxsize": "300MiB",
    "cache-read-only": false,
    "bin-compat": "auto",
    "discard-changes": false,
    "autoloader-suffix": null,
    "sort-packages": false,
    "optimize-autoloader": false,
    "classmap-authoritative": false,
    "apcu-autoloader": false,
    "prepend-autoloader": true,
    "update-with-minimal-changes": false,
    "github-domains": ["github.com"],
    "bitbucket-expose-hostname": true,
    "disable-tls": false,
    "secure-http": true,
    "cafile": null,
    "capath": null,
    "github-expose-hostname": true,
    "gitlab-domains": ["gitlab.com"],
    "store-auths": "prompt",
    "archive-format": "tar",
    "archive-dir": ".",
    "htaccess-protect": true,
    "use-github-api": true,
    "lock": true,
    "platform-check": "php-only",
    "bump-after-update": false,
    "allow-missing-requirements": false,
    "forgejo-domains": ["codeberg.org"],
    "source-fallback": false,
    "home": ""
  }
}"#;

fn source_tree(value: &Value, source: &str) -> Map<String, Value> {
    let mut sources = Map::new();
    if let Value::Object(object) = value {
        for (key, child) in object {
            sources.insert(key.clone(), source_value_tree(child, source));
        }
    }
    sources
}

fn source_value_tree(value: &Value, source: &str) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), source_value_tree(value, source)))
                .collect(),
        ),
        _ => Value::String(source.to_string()),
    }
}

fn merge_document(
    target: &mut Value,
    sources: &mut Map<String, Value>,
    document: &Value,
    source: &str,
) {
    if let Some(config) = document.get("config") {
        merge_at_root(target, sources, "config", config, source);
    }
    if let Some(repositories) = document.get("repositories") {
        merge_at_root(target, sources, "repositories", repositories, source);
    }
    if let (Some(target_object), Some(document_object)) =
        (target.as_object_mut(), document.as_object())
    {
        for (key, value) in document_object {
            if key != "config" && key != "repositories" {
                target_object.insert(key.clone(), value.clone());
                sources.insert(key.clone(), source_value_tree(value, source));
            }
        }
    }
}

fn merge_auth(target: &mut Value, sources: &mut Map<String, Value>, auth: &Value, source: &str) {
    let Some(auth_object) = auth.as_object() else {
        return;
    };
    let config = target
        .as_object_mut()
        .expect("default config is an object")
        .entry("config")
        .or_insert_with(|| Value::Object(Map::new()));
    let config_sources = sources
        .entry("config".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    for (key, value) in auth_object {
        merge_value(config, value, key, source);
        merge_source_value(config_sources, value, key, source);
    }
}

fn merge_at_root(
    target: &mut Value,
    sources: &mut Map<String, Value>,
    key: &str,
    value: &Value,
    source: &str,
) {
    let target_child = target
        .as_object_mut()
        .expect("merged config must be an object")
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    merge_objects(target_child, value);
    let source_child = sources
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    merge_source_tree(source_child, value, source);
}

fn merge_objects(target: &mut Value, incoming: &Value) {
    match (target, incoming) {
        (Value::Object(target), Value::Object(incoming)) => {
            for (key, value) in incoming {
                match target.get_mut(key) {
                    Some(existing) if existing.is_object() && value.is_object() => {
                        merge_objects(existing, value)
                    }
                    _ => {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (target, incoming) => *target = incoming.clone(),
    }
}

fn merge_source_tree(target: &mut Value, incoming: &Value, source: &str) {
    match incoming {
        Value::Object(incoming) => {
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            let target = target.as_object_mut().unwrap();
            for (key, value) in incoming {
                let source_value = target
                    .entry(key.clone())
                    .or_insert_with(|| Value::String(source.to_string()));
                if value.is_object() {
                    merge_source_tree(source_value, value, source);
                } else {
                    *source_value = Value::String(source.to_string());
                }
            }
        }
        _ => *target = Value::String(source.to_string()),
    }
}

fn merge_value(target: &mut Value, value: &Value, key: &str, _source: &str) {
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let child = target
        .as_object_mut()
        .unwrap()
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    merge_objects(child, value);
}

fn merge_source_value(target: &mut Value, value: &Value, key: &str, source: &str) {
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let child = target
        .as_object_mut()
        .unwrap()
        .entry(key.to_string())
        .or_insert_with(|| Value::String(source.to_string()));
    merge_source_tree(child, value, source);
}

fn ensure_packagist(value: &mut Value, sources: &mut Map<String, Value>) {
    let repositories = value
        .as_object_mut()
        .unwrap()
        .entry("repositories")
        .or_insert_with(|| Value::Object(Map::new()));
    match repositories {
        Value::Object(object) => {
            if matches!(object.get("packagist.org"), Some(Value::Bool(false))) {
                object.shift_remove("packagist.org");
                if let Some(source) = sources
                    .get_mut("repositories")
                    .and_then(Value::as_object_mut)
                {
                    source.shift_remove("packagist.org");
                }
            }
        }
        Value::Array(list) => {
            let repository_source = sources
                .get("repositories")
                .and_then(|source| source_leaf(Some(source)))
                .unwrap_or("unknown")
                .to_string();
            let disabled = list.iter().any(|repository| {
                matches!(repository.get("packagist.org"), Some(Value::Bool(false)))
            });
            let named_packagist = list.iter().any(|repository| {
                repository.get("name").and_then(Value::as_str) == Some("packagist.org")
            });
            let mut object = Map::new();
            let mut source_object = Map::new();
            for repository in std::mem::take(list) {
                if matches!(repository.get("packagist.org"), Some(Value::Bool(false))) {
                    continue;
                }
                let key = object.len().to_string();
                source_object.insert(
                    key.clone(),
                    source_value_tree(&repository, &repository_source),
                );
                object.insert(key, repository);
            }
            if !disabled && !named_packagist {
                let packagist = serde_json::json!({
                    "type": "composer",
                    "url": "https://repo.packagist.org"
                });
                object.insert("packagist.org".to_string(), packagist.clone());
                source_object.insert(
                    "packagist.org".to_string(),
                    source_value_tree(&packagist, "default"),
                );
            }
            *repositories = Value::Object(object);
            sources.insert("repositories".to_string(), Value::Object(source_object));
        }
        _ => {}
    }
}

fn apply_environment(value: &mut Value, sources: &mut Map<String, Value>) {
    let mappings = [
        (
            "COMPOSER_PROCESS_TIMEOUT",
            "process-timeout",
            EnvironmentType::Integer,
        ),
        ("COMPOSER_VENDOR_DIR", "vendor-dir", EnvironmentType::String),
        ("COMPOSER_BIN_DIR", "bin-dir", EnvironmentType::String),
        ("COMPOSER_CACHE_DIR", "cache-dir", EnvironmentType::String),
        (
            "COMPOSER_DISCARD_CHANGES",
            "discard-changes",
            EnvironmentType::String,
        ),
        (
            "COMPOSER_CACHE_READ_ONLY",
            "cache-read-only",
            EnvironmentType::Boolean,
        ),
    ];
    for (variable, key, value_type) in mappings {
        let Ok(raw) = env::var(variable) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let parsed = match value_type {
            EnvironmentType::String => Value::String(raw),
            EnvironmentType::Integer => raw
                .parse::<i64>()
                .ok()
                .map(Number::from)
                .map(Value::Number)
                .unwrap_or(Value::String(raw)),
            EnvironmentType::Boolean => Value::Bool(!matches!(raw.as_str(), "0" | "false" | "")),
        };
        set_nested(value, &["config", key], parsed);
        set_source(sources, &["config", key], variable);
    }
}

enum EnvironmentType {
    String,
    Integer,
    Boolean,
}

fn set_source(sources: &mut Map<String, Value>, path: &[&str], source: &str) {
    let mut root = Value::Object(std::mem::take(sources));
    set_nested(&mut root, path, Value::String(source.to_string()));
    *sources = root.as_object_mut().map(std::mem::take).unwrap_or_default();
}

fn resolve_computed_paths(value: &mut Value, working_dir: &Path, home: &Path, global: bool) {
    let base = if global { home } else { working_dir };
    let raw_vendor = nested(value, &["config", "vendor-dir"])
        .and_then(Value::as_str)
        .unwrap_or("vendor")
        .to_string();
    let raw_cache = nested(value, &["config", "cache-dir"])
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| home.join("cache").display().to_string());
    let vendor = absolute_path(&raw_vendor, base);
    let cache = absolute_path(&raw_cache, base);

    let replacements = [
        ("vendor-dir", vendor.display().to_string()),
        (
            "bin-dir",
            resolve_path_setting(value, "bin-dir", base, &vendor, &cache),
        ),
        ("cache-dir", cache.display().to_string()),
        (
            "cache-files-dir",
            resolve_path_setting(value, "cache-files-dir", base, &vendor, &cache),
        ),
        (
            "cache-repo-dir",
            resolve_path_setting(value, "cache-repo-dir", base, &vendor, &cache),
        ),
        (
            "cache-vcs-dir",
            resolve_path_setting(value, "cache-vcs-dir", base, &vendor, &cache),
        ),
        (
            "data-dir",
            resolve_path_setting(value, "data-dir", base, &vendor, &cache),
        ),
    ];

    let config = value
        .as_object_mut()
        .unwrap()
        .entry("_resolved-paths")
        .or_insert_with(|| Value::Object(Map::new()));
    for (key, resolved) in replacements {
        set_nested(config, &[key], Value::String(resolved));
    }
}

fn resolve_path_setting(
    value: &Value,
    key: &str,
    base: &Path,
    vendor: &Path,
    cache: &Path,
) -> String {
    let raw = nested(value, &["config", key])
        .and_then(Value::as_str)
        .unwrap_or("");
    let expanded = raw
        .replace("{$vendor-dir}", &vendor.display().to_string())
        .replace("{$cache-dir}", &cache.display().to_string());
    absolute_path(&expanded, base).display().to_string()
}

fn absolute_path(raw: &str, base: &Path) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn read_setting(
    setting_key: &str,
    merged: &MergedConfig,
    selected: &Value,
    files: &ConfigFiles,
    _working_dir: &Path,
    absolute: bool,
) -> Result<(Value, String)> {
    if repository_key(setting_key).is_some() {
        let repository = repository_key(setting_key).unwrap();
        let value = if repository.is_empty() {
            nested(&merged.value, &["repositories"])
        } else {
            nested(&merged.value, &["repositories", repository])
        }
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("There is no {repository} repository defined"))?;
        let path = if repository.is_empty() {
            return Ok((value, "unknown".to_string()));
        } else {
            vec!["repositories", repository]
        };
        return Ok((
            value,
            source_at(&merged.sources, &path)
                .unwrap_or("unknown")
                .to_string(),
        ));
    }

    if let Some((auth_type, host)) = auth_key(setting_key) {
        let value = nested(&merged.value, &["config", auth_type, host])
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{setting_key} is not defined."))?;
        let source = source_at(&merged.sources, &["config", auth_type, host])
            .unwrap_or("unknown")
            .to_string();
        return Ok((value, source));
    }

    if setting_key.contains('.') {
        let mut bits: Vec<&str> = setting_key.split('.').collect();
        let root = bits[0];
        let (data, source_path) = if root == "extra" || root == "suggest" {
            (selected, bits.clone())
        } else {
            bits.insert(0, "config");
            (&merged.value, bits.clone())
        };
        let value = nested(data, &bits)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{setting_key} is not defined."))?;
        let source = if root == "extra" || root == "suggest" {
            files.source_name.clone()
        } else {
            source_at(&merged.sources, &source_path)
                .unwrap_or("default")
                .to_string()
        };
        return Ok((value, source));
    }

    if let Some(value) = nested(&merged.value, &["config", setting_key]).cloned() {
        if absolute && is_path_setting(setting_key) {
            if let Some(resolved) = nested(&merged.value, &["_resolved-paths", setting_key]) {
                return Ok((
                    resolved.clone(),
                    source_at(&merged.sources, &["config", setting_key])
                        .unwrap_or("default")
                        .to_string(),
                ));
            }
        }
        return Ok((
            value,
            source_at(&merged.sources, &["config", setting_key])
                .unwrap_or("default")
                .to_string(),
        ));
    }

    if let Some(value) = selected.get(setting_key) {
        return Ok((value.clone(), files.source_name.clone()));
    }

    let default = match setting_key {
        "type" => Some(Value::String("library".to_string())),
        "description" | "homepage" => Some(Value::String(String::new())),
        "minimum-stability" => Some(Value::String("stable".to_string())),
        "prefer-stable" => Some(Value::Bool(false)),
        "keywords" | "license" => Some(Value::Array(Vec::new())),
        "suggest" | "extra" => Some(Value::Object(Map::new())),
        _ => None,
    };
    default
        .map(|value| (value, "defaults".to_string()))
        .ok_or_else(|| anyhow::anyhow!("{setting_key} is not defined"))
}

fn repository_key(key: &str) -> Option<&str> {
    for prefix in ["repositories", "repository", "repos", "repo"] {
        if key == prefix {
            return Some("");
        }
        if let Some(rest) = key
            .strip_prefix(prefix)
            .and_then(|value| value.strip_prefix('.'))
        {
            return Some(rest);
        }
    }
    None
}

fn is_path_setting(key: &str) -> bool {
    matches!(
        key,
        "vendor-dir"
            | "bin-dir"
            | "cache-dir"
            | "data-dir"
            | "cache-files-dir"
            | "cache-repo-dir"
            | "cache-vcs-dir"
    )
}

fn display_value(value: Value) -> String {
    match value {
        Value::String(value) => value,
        Value::Null => String::new(),
        other => serde_json::to_string(&other).unwrap_or_default(),
    }
}

fn list_configuration(
    merged: &Value,
    sources: &Map<String, Value>,
    show_source: bool,
    _working_dir: &Path,
    _files: &ConfigFiles,
) -> Result<()> {
    let mut output = String::new();
    if let Some(repositories) = merged.get("repositories") {
        list_value(
            "repositories",
            repositories,
            sources.get("repositories"),
            show_source,
            None,
            &mut output,
        );
    }
    if let Some(config) = merged.get("config") {
        list_value(
            "",
            config,
            sources.get("config"),
            show_source,
            merged.get("_resolved-paths"),
            &mut output,
        );
    }
    match io::stdout().write_all(output.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn list_value(
    prefix: &str,
    value: &Value,
    source: Option<&Value>,
    show_source: bool,
    resolved_paths: Option<&Value>,
    output: &mut String,
) {
    if let Value::Object(object) = value {
        for (key, child) in object {
            let next = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            let child_source = source.and_then(|source| source.get(key));
            if child.is_object() {
                list_value(
                    &next,
                    child,
                    child_source,
                    show_source,
                    resolved_paths,
                    output,
                );
            } else {
                let resolved = if prefix.is_empty() {
                    resolved_paths.and_then(|paths| paths.get(key))
                } else {
                    None
                };
                let mut line = format!("[{next}] {}", list_display_value(&next, child, resolved));
                if show_source {
                    line.push_str(" (");
                    line.push_str(source_leaf(child_source).unwrap_or("default"));
                    line.push(')');
                }
                output.push_str(&line);
                output.push('\n');
            }
        }
    }
}

fn list_display_value(key: &str, value: &Value, resolved: Option<&Value>) -> String {
    let raw = match value {
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(|value| match value {
                    Value::String(value) => value.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Bool(value) => value.to_string(),
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        other => other.to_string(),
    };
    if key == "cache-files-maxsize" {
        if let Some(bytes) = parse_size(&raw) {
            return format!("{raw} ({bytes})");
        }
    }
    if let Some(resolved) = resolved.and_then(Value::as_str) {
        if resolved != raw && !resolved.is_empty() {
            return format!("{raw} ({resolved})");
        }
    }
    raw
}

fn parse_size(value: &str) -> Option<u64> {
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(value.len());
    let number = value[..split].parse::<f64>().ok()?;
    let suffix = value[split..].trim().to_ascii_lowercase();
    let multiplier = match suffix.as_str() {
        "" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        _ => return None,
    };
    Some((number * multiplier as f64) as u64)
}

fn source_leaf(source: Option<&Value>) -> Option<&str> {
    match source {
        Some(Value::String(source)) => Some(source),
        Some(Value::Object(object)) => object.values().find_map(|value| source_leaf(Some(value))),
        _ => None,
    }
}

fn source_at<'a>(sources: &'a Map<String, Value>, path: &[&str]) -> Option<&'a str> {
    let mut current = sources.get(*path.first()?)?;
    for key in &path[1..] {
        current = current.get(*key)?;
    }
    source_leaf(Some(current))
}

fn unset_setting(key: &str, document: &mut Value, files: &ConfigFiles) -> Result<()> {
    if let Some(repository) = repository_key(key) {
        if repository.is_empty() {
            remove_nested(document, &["repositories"], false);
        } else {
            remove_repository(document, repository);
        }
        return Ok(());
    }

    if let Some((auth_type, host)) = auth_key(key) {
        let mut auth = read_optional_json_object(&files.auth_path)?;
        remove_nested(&mut auth, &[auth_type, host], false);
        write_json(&files.auth_path, &auth)?;
        remove_nested(document, &["config", auth_type, host], true);
        return Ok(());
    }

    if let Some(policy_key) = key.strip_prefix("policy.") {
        if let Some(policy) = document
            .get_mut("config")
            .and_then(|config| config.get_mut("policy"))
        {
            let path: Vec<&str> = policy_key.split('.').collect();
            remove_nested(policy, &path, true);
        }
    } else if is_config_key(key)
        || key.starts_with("platform.")
        || key.starts_with("preferred-install.")
        || key.starts_with("allow-plugins.")
        || key == "platform"
        || key == "audit"
        || key == "policy"
        || key.starts_with("audit.")
    {
        let mut path = vec!["config"];
        path.extend(key.split('.'));
        remove_nested(document, &path, false);
    } else {
        let path: Vec<&str> = key.split('.').collect();
        remove_nested(document, &path, false);
    }
    Ok(())
}

fn set_setting(
    key: &str,
    args: &ConfigArgs,
    document: &mut Value,
    files: &ConfigFiles,
) -> Result<()> {
    let values = &args.setting_value;

    if args.global
        && (ROOT_STRING_PROPERTIES.contains(&key)
            || ROOT_LIST_PROPERTIES.contains(&key)
            || key.starts_with("extra."))
    {
        bail!("The {key} property can not be set in the global config.json file. Use `composer global config` to apply changes to the global composer.json");
    }

    if let Some(repository) = repository_key(key) {
        if repository.is_empty() {
            bail!("Setting {key} does not exist or is not supported by this command");
        }
        let value = repository_value(values)?;
        set_repository(document, repository, value, args.append);
        return Ok(());
    }

    if let Some((auth_type, host)) = auth_key(key) {
        let value = normalize_auth(auth_type, values)?;
        let mut auth = read_optional_json_object(&files.auth_path)?;
        set_nested(&mut auth, &[auth_type, host], value);
        write_json(&files.auth_path, &auth)?;
        remove_nested(document, &["config", auth_type, host], true);
        return Ok(());
    }

    if ROOT_STRING_PROPERTIES.contains(&key) {
        require_one(values)?;
        set_nested(document, &[key], Value::String(values[0].clone()));
        return Ok(());
    }
    if ROOT_LIST_PROPERTIES.contains(&key) {
        set_nested(
            document,
            &[key],
            Value::Array(values.iter().cloned().map(Value::String).collect()),
        );
        return Ok(());
    }
    if key == "minimum-stability" {
        require_one(values)?;
        let stability = values[0].to_ascii_lowercase();
        if !matches!(
            stability.as_str(),
            "dev" | "alpha" | "beta" | "rc" | "stable"
        ) {
            bail!("\"{}\" is an invalid value", values[0]);
        }
        set_nested(document, &[key], Value::String(stability));
        return Ok(());
    }
    if key == "prefer-stable" {
        require_one(values)?;
        set_nested(document, &[key], Value::Bool(parse_bool(&values[0])?));
        return Ok(());
    }

    if let Some(path) = key.strip_prefix("scripts.") {
        let value = if values.len() == 1 {
            Value::String(values[0].clone())
        } else {
            Value::Array(values.iter().cloned().map(Value::String).collect())
        };
        set_nested(document, &["scripts", path], value);
        return Ok(());
    }
    if let Some(path) = key.strip_prefix("suggest.") {
        set_nested(
            document,
            &["suggest", path],
            Value::String(values.join(" ")),
        );
        return Ok(());
    }
    if let Some(path) = key.strip_prefix("extra.") {
        let mut value = if args.json {
            parse_json_argument(values)?
        } else {
            Value::String(values.first().cloned().unwrap_or_default())
        };
        let mut full_path = vec!["extra"];
        full_path.extend(path.split('.'));
        if args.merge {
            if let Some(current) = nested(document, &full_path) {
                value = merge_json_values(current, &value, key)?;
            }
        }
        set_nested(document, &full_path, value);
        return Ok(());
    }

    if let Some(platform) = key.strip_prefix("platform.") {
        require_one(values)?;
        if platform.eq_ignore_ascii_case("php") && values[0] == "false" {
            bail!("config.platform.php cannot be disabled");
        }
        let value = if values[0] == "false" {
            Value::Bool(false)
        } else {
            Value::String(values[0].clone())
        };
        set_nested(document, &["config", "platform", platform], value);
        return Ok(());
    }
    if let Some(pattern) = key.strip_prefix("preferred-install.") {
        require_one(values)?;
        validate_enum(key, &values[0], &["auto", "source", "dist"])?;
        set_nested(
            document,
            &["config", "preferred-install", pattern],
            Value::String(values[0].clone()),
        );
        return Ok(());
    }
    if let Some(plugin) = key.strip_prefix("allow-plugins.") {
        require_one(values)?;
        set_nested(
            document,
            &["config", "allow-plugins", plugin],
            Value::Bool(parse_bool(&values[0])?),
        );
        return Ok(());
    }

    if JSON_MERGE_KEYS.contains(&key) || is_policy_ignore_key(key) {
        if is_policy_ignore_key(key) {
            validate_policy_name(key)?;
        }
        let mut value = if args.json {
            parse_json_argument(values)?
        } else {
            Value::Array(values.iter().cloned().map(Value::String).collect())
        };
        if !value.is_array() && !value.is_object() {
            bail!("Expected an array or object for {key}");
        }
        let mut path = vec!["config"];
        path.extend(key.split('.'));
        if args.merge {
            if let Some(current) = nested(document, &path) {
                value = merge_json_values(current, &value, key)?;
            }
        }
        set_nested(document, &path, value);
        return Ok(());
    }

    if key == "policy.ignore-unreachable"
        && (args.json
            || values
                .first()
                .is_some_and(|value| matches!(value.as_str(), "install" | "update")))
    {
        let value = if args.json {
            parse_json_argument(values)?
        } else {
            Value::Array(values.iter().cloned().map(Value::String).collect())
        };
        let valid = value.as_array().is_some_and(|values| {
            values
                .iter()
                .all(|value| matches!(value.as_str(), Some("install" | "update")))
        });
        if !valid {
            bail!("valid values for {key} include: install, update");
        }
        set_nested(document, &["config", "policy", "ignore-unreachable"], value);
        return Ok(());
    }

    if key.starts_with("policy.") && key.ends_with(".sources") {
        let list = key.split('.').nth(1).unwrap_or_default();
        bail!("Setting dependency policy sources is not supported by `composer config`. Use `composer policy add-source {list} url <https-url>` instead.");
    }

    if is_policy_list_toggle(key) {
        require_one(values)?;
        validate_policy_name(key)?;
        let mut path = vec!["config"];
        path.extend(key.split('.'));
        set_nested(document, &path, Value::Bool(parse_bool(&values[0])?));
        return Ok(());
    }

    if BOOL_CONFIG_KEYS.contains(&key) {
        require_one(values)?;
        let mut path = vec!["config"];
        path.extend(key.split('.'));
        set_nested(document, &path, Value::Bool(parse_bool(&values[0])?));
        return Ok(());
    }
    if INTEGER_CONFIG_KEYS.contains(&key) {
        require_one(values)?;
        let number = values[0]
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .ok_or_else(|| anyhow::anyhow!("\"{}\" is an invalid value", values[0]))?
            as i64;
        set_nested(
            document,
            &["config", key],
            Value::Number(Number::from(number)),
        );
        return Ok(());
    }
    if STRING_CONFIG_KEYS.contains(&key) {
        require_one(values)?;
        let value =
            if matches!(key, "autoloader-suffix" | "cafile" | "capath") && values[0] == "null" {
                Value::Null
            } else {
                Value::String(values[0].clone())
            };
        set_nested(document, &["config", key], value);
        return Ok(());
    }
    if MULTI_CONFIG_KEYS.contains(&key) {
        validate_multi(key, values)?;
        let mut path = vec!["config"];
        path.extend(key.split('.'));
        set_nested(
            document,
            &path,
            Value::Array(values.iter().cloned().map(Value::String).collect()),
        );
        return Ok(());
    }

    if matches!(
        key,
        "preferred-install"
            | "gitlab-protocol"
            | "store-auths"
            | "discard-changes"
            | "bin-compat"
            | "platform-check"
            | "use-parent-dir"
            | "bump-after-update"
            | "audit.abandoned"
            | "policy.advisories.audit"
            | "policy.malware.block-scope"
            | "policy.malware.audit"
            | "policy.abandoned.audit"
    ) {
        require_one(values)?;
        let value = normalize_enum_or_bool(key, &values[0])?;
        let mut path = vec!["config"];
        path.extend(key.split('.'));
        set_nested(document, &path, value);
        return Ok(());
    }

    if key.starts_with("policy.") && (key.ends_with(".block") || key.ends_with(".audit")) {
        require_one(values)?;
        validate_policy_name(key)?;
        let value = if key.ends_with(".block") {
            Value::Bool(parse_bool(&values[0])?)
        } else {
            validate_enum(key, &values[0], &["ignore", "report", "fail"])?;
            Value::String(values[0].clone())
        };
        let mut path = vec!["config"];
        path.extend(key.split('.'));
        set_nested(document, &path, value);
        return Ok(());
    }

    bail!("Setting {key} does not exist or is not supported by this command")
}

fn is_config_key(key: &str) -> bool {
    BOOL_CONFIG_KEYS.contains(&key)
        || INTEGER_CONFIG_KEYS.contains(&key)
        || STRING_CONFIG_KEYS.contains(&key)
        || MULTI_CONFIG_KEYS.contains(&key)
        || matches!(
            key,
            "preferred-install"
                | "gitlab-protocol"
                | "store-auths"
                | "discard-changes"
                | "bin-compat"
                | "platform-check"
                | "use-parent-dir"
                | "bump-after-update"
        )
}

fn require_one(values: &[String]) -> Result<()> {
    if values.len() != 1 {
        bail!("You can only pass one value. Example: php composer.phar config process-timeout 300");
    }
    Ok(())
}

fn parse_bool(value: &str) -> Result<bool> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => bail!("\"{value}\" is an invalid value, expected a boolean"),
    }
}

fn normalize_enum_or_bool(key: &str, value: &str) -> Result<Value> {
    let choices: &[&str] = match key {
        "preferred-install" => &["auto", "source", "dist"],
        "gitlab-protocol" => &["git", "http", "https"],
        "store-auths" | "use-parent-dir" => &["true", "false", "prompt"],
        "discard-changes" => &["stash", "true", "false", "1", "0"],
        "bin-compat" => &["auto", "full", "proxy", "symlink"],
        "platform-check" => &["php-only", "true", "false", "1", "0"],
        "bump-after-update" => &["dev", "no-dev", "true", "false", "1", "0"],
        "audit.abandoned"
        | "policy.advisories.audit"
        | "policy.malware.audit"
        | "policy.abandoned.audit" => &["ignore", "report", "fail"],
        "policy.malware.block-scope" => &["all", "update", "install"],
        _ => &[],
    };
    validate_enum(key, value, choices)?;
    if matches!(value, "true" | "false" | "1" | "0") {
        Ok(Value::Bool(parse_bool(value)?))
    } else {
        Ok(Value::String(value.to_string()))
    }
}

fn validate_enum(key: &str, value: &str, choices: &[&str]) -> Result<()> {
    if !choices.contains(&value) {
        bail!("\"{value}\" is an invalid value for {key}");
    }
    Ok(())
}

fn validate_multi(key: &str, values: &[String]) -> Result<()> {
    if key == "github-protocols"
        && values
            .iter()
            .any(|value| !matches!(value.as_str(), "git" | "https" | "ssh"))
    {
        bail!(
            "{} is an invalid value (valid protocols include: git, https, ssh)",
            serde_json::to_string(values)?
        );
    }
    if key.ends_with("ignore-severity")
        && values
            .iter()
            .any(|value| !matches!(value.as_str(), "low" | "medium" | "high" | "critical"))
    {
        bail!(
            "{} is an invalid value (valid severities include: low, medium, high, critical)",
            serde_json::to_string(values)?
        );
    }
    Ok(())
}

fn parse_json_argument(values: &[String]) -> Result<Value> {
    require_one(values)?;
    serde_json::from_str(&values[0]).with_context(|| "Setting value is not valid JSON")
}

fn merge_json_values(current: &Value, incoming: &Value, key: &str) -> Result<Value> {
    match (current, incoming) {
        (Value::Array(current), Value::Array(incoming)) => {
            let mut merged = current.clone();
            merged.extend(incoming.clone());
            Ok(Value::Array(merged))
        }
        (Value::Object(current), Value::Object(incoming)) => {
            let mut merged = incoming.clone();
            for (key, value) in current {
                merged.entry(key.clone()).or_insert_with(|| value.clone());
            }
            Ok(Value::Object(merged))
        }
        (Value::Array(_), Value::Object(_)) | (Value::Object(_), Value::Array(_)) => {
            bail!("Cannot merge array and object for {key}")
        }
        _ => Ok(incoming.clone()),
    }
}

fn repository_value(values: &[String]) -> Result<Value> {
    match values {
        [value] if value == "false" || value == "0" => Ok(Value::Bool(false)),
        [value] if value == "true" || value == "1" => bail!("You must pass the type and a url. Example: php composer.phar config repositories.foo vcs https://bar.com"),
        [value] => serde_json::from_str(value).with_context(|| "Repository value is not valid JSON"),
        [repository_type, url] => Ok(serde_json::json!({"type": repository_type, "url": url})),
        _ => bail!("You must pass the type and a url. Example: php composer.phar config repositories.foo vcs https://bar.com"),
    }
}

fn set_repository(document: &mut Value, name: &str, value: Value, append: bool) {
    let repositories = document
        .as_object_mut()
        .unwrap()
        .entry("repositories")
        .or_insert_with(|| Value::Array(Vec::new()));
    let mut list = repository_list(std::mem::take(repositories));
    list.retain(|repository| {
        repository.get("name").and_then(Value::as_str) != Some(name)
            && repository != &disabled_repository(name)
    });

    let value = if value == Value::Bool(false) {
        disabled_repository(name)
    } else if let Value::Object(mut object) = value {
        if !object.contains_key("name") {
            let existing = std::mem::take(&mut object);
            object.insert("name".to_string(), Value::String(name.to_string()));
            object.extend(existing);
        }
        Value::Object(object)
    } else {
        value
    };
    if append {
        list.push(value);
    } else {
        list.insert(0, value);
    }
    *repositories = Value::Array(list);
}

fn remove_repository(document: &mut Value, name: &str) {
    let Some(repositories) = document.get_mut("repositories") else {
        return;
    };
    match repositories {
        Value::Object(object) => {
            object.shift_remove(name);
        }
        Value::Array(list) => {
            list.retain(|repository| {
                repository.get("name").and_then(Value::as_str) != Some(name)
                    && repository != &disabled_repository(name)
            });
            if list.is_empty() {
                document
                    .as_object_mut()
                    .unwrap()
                    .shift_remove("repositories");
            }
        }
        _ => {}
    }
}

fn repository_list(repositories: Value) -> Vec<Value> {
    match repositories {
        Value::Array(list) => list,
        Value::Object(object) => object
            .into_iter()
            .map(|(name, value)| match value {
                Value::Object(mut repository) => {
                    if !repository.contains_key("name") {
                        let existing = std::mem::take(&mut repository);
                        repository.insert("name".to_string(), Value::String(name));
                        repository.extend(existing);
                    }
                    Value::Object(repository)
                }
                value => {
                    let mut repository = Map::new();
                    repository.insert(name, value);
                    Value::Object(repository)
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn disabled_repository(name: &str) -> Value {
    let mut repository = Map::new();
    repository.insert(name.to_string(), Value::Bool(false));
    Value::Object(repository)
}

fn auth_key(key: &str) -> Option<(&str, &str)> {
    let (auth_type, host) = key.split_once('.')?;
    AUTH_KEYS.contains(&auth_type).then_some((auth_type, host))
}

fn normalize_auth(auth_type: &str, values: &[String]) -> Result<Value> {
    match auth_type {
        "bitbucket-oauth" => {
            if values.len() != 2 {
                bail!(
                    "Expected two arguments (consumer-key, consumer-secret), got {}",
                    values.len()
                );
            }
            Ok(serde_json::json!({"consumer-key": values[0], "consumer-secret": values[1]}))
        }
        "http-basic" => {
            if values.len() != 2 {
                bail!(
                    "Expected two arguments (username, password), got {}",
                    values.len()
                );
            }
            Ok(serde_json::json!({"username": values[0], "password": values[1]}))
        }
        "forgejo-token" => {
            if values.len() != 2 {
                bail!(
                    "Expected two arguments (username, access token), got {}",
                    values.len()
                );
            }
            Ok(serde_json::json!({"username": values[0], "token": values[1]}))
        }
        "gitlab-token" if values.len() == 2 => {
            Ok(serde_json::json!({"username": values[0], "token": values[1]}))
        }
        "custom-headers" => {
            if values.is_empty() {
                bail!("Expected at least one argument (header), got none");
            }
            for header in values {
                let valid = header
                    .split_once(':')
                    .is_some_and(|(name, value)| !name.is_empty() && !value.trim().is_empty());
                if !valid {
                    bail!("Header \"{header}\" is not in \"Header-Name: Header-Value\" format");
                }
            }
            Ok(Value::Array(
                values.iter().cloned().map(Value::String).collect(),
            ))
        }
        _ => {
            if values.len() != 1 {
                bail!("Too many arguments, expected only one token");
            }
            Ok(Value::String(values[0].clone()))
        }
    }
}

fn is_policy_ignore_key(key: &str) -> bool {
    key.starts_with("policy.") && key.ends_with(".ignore")
}

fn is_policy_list_toggle(key: &str) -> bool {
    let parts: Vec<&str> = key.split('.').collect();
    parts.len() == 2 && parts[0] == "policy" && parts[1] != "ignore-unreachable"
}

fn validate_policy_name(key: &str) -> Result<()> {
    let name = key.split('.').nth(1).unwrap_or_default();
    if name.starts_with("ignore") {
        bail!("Invalid dependency policy name: reserved prefix \"ignore\"");
    }
    if matches!(
        name,
        "package"
            | "packages"
            | "license"
            | "licence"
            | "licenses"
            | "licences"
            | "support"
            | "maintenance"
            | "security"
            | "minimum-release-age"
    ) {
        bail!("Invalid dependency policy name: reserved for future use");
    }
    Ok(())
}

fn nested<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn set_nested(root: &mut Value, path: &[&str], value: Value) {
    if path.is_empty() {
        *root = value;
        return;
    }
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    let child = root
        .as_object_mut()
        .unwrap()
        .entry(path[0].to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    set_nested(child, &path[1..], value);
}

fn remove_nested(root: &mut Value, path: &[&str], prune_empty: bool) -> bool {
    let Some((first, rest)) = path.split_first() else {
        return false;
    };
    let Some(object) = root.as_object_mut() else {
        return false;
    };
    if rest.is_empty() {
        return object.shift_remove(*first).is_some();
    }
    let Some(child) = object.get_mut(*first) else {
        return false;
    };
    let removed = remove_nested(child, rest, prune_empty);
    if prune_empty && child.as_object().is_some_and(Map::is_empty) {
        object.shift_remove(*first);
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_values_round_trip() {
        let mut value = serde_json::json!({});
        set_nested(
            &mut value,
            &["config", "audit", "ignore"],
            serde_json::json!(["CVE"]),
        );
        assert_eq!(
            nested(&value, &["config", "audit", "ignore"]),
            Some(&serde_json::json!(["CVE"]))
        );
        assert!(remove_nested(
            &mut value,
            &["config", "audit", "ignore"],
            false
        ));
        assert_eq!(value, serde_json::json!({"config": {"audit": {}}}));
    }

    #[test]
    fn merge_arrays_and_objects_like_composer() {
        assert_eq!(
            merge_json_values(
                &serde_json::json!(["old"]),
                &serde_json::json!(["new"]),
                "extra.test"
            )
            .unwrap(),
            serde_json::json!(["old", "new"])
        );
        assert_eq!(
            merge_json_values(
                &serde_json::json!({"old": 1, "replace": 1}),
                &serde_json::json!({"new": 2, "replace": 2}),
                "extra.test"
            )
            .unwrap(),
            serde_json::json!({"new": 2, "replace": 2, "old": 1})
        );
    }

    #[test]
    fn repository_aliases_are_recognized() {
        assert_eq!(repository_key("repo.foo"), Some("foo"));
        assert_eq!(
            repository_key("repositories.packagist.org"),
            Some("packagist.org")
        );
        assert_eq!(repository_key("repos"), Some(""));
        assert_eq!(repository_key("report"), None);
    }

    #[test]
    fn auth_host_keeps_dots() {
        assert_eq!(
            auth_key("http-basic.repo.example.org"),
            Some(("http-basic", "repo.example.org"))
        );
    }
}
