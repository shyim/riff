use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use riff_core::config::ConfigLoader;
use riff_core::policy_config::{
    add_custom_policy_source, AddPolicySourceOutcome, PolicySourceError,
};
use serde_json::Value;

#[derive(Debug, usage_rs::Args)]
pub struct PolicyArgs {
    /// Apply the command to Composer's global config file
    #[usage(short = 'g', long)]
    pub global: bool,

    /// Read or modify a custom composer.json or config.json
    #[usage(short = 'f', long)]
    pub file: Option<PathBuf>,

    /// Action to perform: add-source
    #[usage(arg, name = "ACTION")]
    pub action: String,

    /// Custom dependency policy name
    #[usage(arg, name = "NAME")]
    pub name: Option<String>,

    /// Source type (`url`) or a JSON source object
    #[usage(arg, name = "TYPE-OR-JSON")]
    pub arg1: Option<String>,

    /// Source URL when TYPE-OR-JSON is `url`
    #[usage(arg, name = "URL")]
    pub arg2: Option<String>,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PolicyCommandOutcome {
    Added,
    AlreadyPresent { name: String, url: String },
}

pub async fn execute(args: PolicyArgs) -> Result<i32> {
    let global_home = ConfigLoader::new(true).get_composer_home();
    match run(args, &global_home)? {
        PolicyCommandOutcome::Added => {}
        PolicyCommandOutcome::AlreadyPresent { name, url } => {
            riff_core::outln!("Source {url} already present in policy {name}");
        }
    }
    Ok(0)
}

fn run(args: PolicyArgs, global_home: &Path) -> Result<PolicyCommandOutcome> {
    if args.global && args.file.is_some() {
        bail!("--file and --global can not be combined");
    }
    let action = args.action.to_ascii_lowercase();
    if action != "add-source" {
        bail!("Unknown action \"{action}\". Use add-source.");
    }
    let name = args.name.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "You must pass a dependency policy name. Example: riff policy add-source my-policy url https://example.org"
        )
    })?;
    let arg1 = args.arg1.as_deref().ok_or_else(|| {
        anyhow::anyhow!("You must pass the source type and a url, or a JSON string.")
    })?;
    let source = parse_source(arg1, args.arg2.as_deref())?;
    let source_url = source
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let target = if args.global {
        global_home.join("config.json")
    } else {
        let requested = args
            .file
            .as_deref()
            .unwrap_or_else(|| Path::new("composer.json"));
        if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            working_dir.join(requested)
        }
    };
    let mut document = read_target(&target, args.global)?;
    let outcome =
        add_custom_policy_source(&mut document, name, source).map_err(anyhow::Error::new)?;

    match outcome {
        AddPolicySourceOutcome::Added => {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create {}", parent.display()))?;
            }
            riff_core::json::write_json_value(&target, &document, true)
                .with_context(|| format!("Failed to write {}", target.display()))?;
            Ok(PolicyCommandOutcome::Added)
        }
        AddPolicySourceOutcome::AlreadyPresent => Ok(PolicyCommandOutcome::AlreadyPresent {
            name: name.to_string(),
            url: source_url,
        }),
    }
}

fn parse_source(arg1: &str, arg2: Option<&str>) -> Result<Value> {
    if arg1.trim_start().starts_with('{') {
        let source: Value = serde_json::from_str(arg1).context("Source JSON is invalid")?;
        if !source.is_object() {
            return Err(PolicySourceError::SourceMustBeObject.into());
        }
        return Ok(source);
    }
    let url = arg2.ok_or_else(|| {
        anyhow::anyhow!(
            "You must pass the source type and a url. Example: riff policy add-source my-policy url https://example.org"
        )
    })?;
    Ok(serde_json::json!({"type": arg1, "url": url}))
}

fn read_target(path: &Path, allow_missing: bool) -> Result<Value> {
    if !path.exists() {
        if allow_missing {
            return Ok(Value::Object(serde_json::Map::new()));
        }
        bail!("File \"{}\" cannot be found", path.display());
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("{} does not contain valid JSON", path.display()))?;
    if !value.is_object() {
        bail!("{} must contain a JSON object", path.display());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(root: &Path) -> PolicyArgs {
        PolicyArgs {
            global: false,
            file: None,
            action: "add-source".to_string(),
            name: Some("my-list".to_string()),
            arg1: Some("url".to_string()),
            arg2: Some("https://example.org/list.json".to_string()),
            working_dir: root.to_path_buf(),
        }
    }

    fn project() -> tempfile::TempDir {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("composer.json"), "{}\n").unwrap();
        project
    }

    fn read(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    // Ported from Composer\Test\Command\PolicyCommandTest::testAddSourceCreatesNewList.
    #[test]
    fn composer_policy_command_add_source_creates_a_new_list() {
        let project = project();
        let home = project.path().join("home");

        assert_eq!(
            run(args(project.path()), &home).unwrap(),
            PolicyCommandOutcome::Added
        );
        assert_eq!(
            read(&project.path().join("composer.json")),
            serde_json::json!({"config": {"policy": {"my-list": {"sources": [
                {"type": "url", "url": "https://example.org/list.json"}
            ]}}}})
        );
    }

    // Ported from Composer\Test\Command\PolicyCommandTest::testAddSourceAppendsToExistingList.
    #[test]
    fn composer_policy_command_add_source_appends_to_an_existing_list() {
        let project = project();
        let path = project.path().join("composer.json");
        riff_core::json::write_json_value(
            &path,
            &serde_json::json!({"config": {"policy": {"my-list": {
                "block": true,
                "sources": [{"type": "url", "url": "https://first.example.org/list.json"}]
            }}}}),
            true,
        )
        .unwrap();
        let mut command = args(project.path());
        command.arg2 = Some("https://second.example.org/list.json".to_string());

        assert_eq!(
            run(command, &project.path().join("home")).unwrap(),
            PolicyCommandOutcome::Added
        );
        let document = read(&path);
        assert_eq!(document["config"]["policy"]["my-list"]["block"], true);
        assert_eq!(
            document["config"]["policy"]["my-list"]["sources"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    // Ported from Composer\Test\Command\PolicyCommandTest::testAddSourceIsNoopWhenUrlAlreadyPresent.
    #[test]
    fn composer_policy_command_add_source_is_idempotent() {
        let project = project();
        let home = project.path().join("home");
        assert_eq!(
            run(args(project.path()), &home).unwrap(),
            PolicyCommandOutcome::Added
        );

        assert_eq!(
            run(args(project.path()), &home).unwrap(),
            PolicyCommandOutcome::AlreadyPresent {
                name: "my-list".to_string(),
                url: "https://example.org/list.json".to_string(),
            }
        );
        assert_eq!(
            read(&project.path().join("composer.json"))["config"]["policy"]["my-list"]["sources"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    // Ported from Composer\Test\Command\PolicyCommandTest::testAddSourceWithJson.
    #[test]
    fn composer_policy_command_add_source_accepts_json() {
        let project = project();
        let mut command = args(project.path());
        command.arg1 = Some(r#"{"type":"url","url":"https://example.org/list.json"}"#.to_string());
        command.arg2 = None;

        assert_eq!(
            run(command, &project.path().join("home")).unwrap(),
            PolicyCommandOutcome::Added
        );
        assert_eq!(
            read(&project.path().join("composer.json"))["config"]["policy"]["my-list"]["sources"]
                [0]["url"],
            "https://example.org/list.json"
        );
    }

    // Ported from Composer\Test\Command\PolicyCommandTest::
    // testAddSourceWithGlobalFlagWritesToHomeConfigJson.
    #[test]
    fn composer_policy_command_add_source_writes_only_the_global_config() {
        let project = project();
        let home = project.path().join("home");
        let mut command = args(project.path());
        command.global = true;

        assert_eq!(run(command, &home).unwrap(), PolicyCommandOutcome::Added);
        assert_eq!(
            read(&home.join("config.json"))["config"]["policy"]["my-list"]["sources"][0]["url"],
            "https://example.org/list.json"
        );
        assert_eq!(
            read(&project.path().join("composer.json")),
            serde_json::json!({})
        );
    }

    // Ported from Composer\Test\Command\PolicyCommandTest::
    // testAddSourceWithFileFlagWritesToCustomFile.
    #[test]
    fn composer_policy_command_add_source_writes_only_the_custom_file() {
        let project = project();
        fs::write(project.path().join("alt.composer.json"), "{}\n").unwrap();
        let mut command = args(project.path());
        command.file = Some(PathBuf::from("alt.composer.json"));

        assert_eq!(
            run(command, &project.path().join("home")).unwrap(),
            PolicyCommandOutcome::Added
        );
        assert_eq!(
            read(&project.path().join("alt.composer.json"))["config"]["policy"]["my-list"]
                ["sources"][0]["url"],
            "https://example.org/list.json"
        );
        assert_eq!(
            read(&project.path().join("composer.json")),
            serde_json::json!({})
        );
    }

    // Ported from Composer\Test\Command\PolicyCommandTest::testAddSourceRejectsBuiltInListName.
    #[test]
    fn composer_policy_command_rejects_a_built_in_list_name() {
        let project = project();
        let mut command = args(project.path());
        command.name = Some("advisories".to_string());

        assert!(run(command, &project.path().join("home"))
            .unwrap_err()
            .to_string()
            .contains("Built-in dependency policy \"advisories\" does not support sources"));
    }

    // Ported from Composer\Test\Command\PolicyCommandTest::testAddSourceRejectsIgnoreUnreachableName.
    #[test]
    fn composer_policy_command_rejects_a_reserved_prefix() {
        let project = project();
        let mut command = args(project.path());
        command.name = Some("ignore-unreachable".to_string());

        assert!(run(command, &project.path().join("home"))
            .unwrap_err()
            .to_string()
            .contains("reserved prefix \"ignore\""));
    }

    // Ported from Composer\Test\Command\PolicyCommandTest::testAddSourceRejectsNonHttpsUrl.
    #[test]
    fn composer_policy_command_rejects_a_non_https_url() {
        let project = project();
        let mut command = args(project.path());
        command.arg2 = Some("http://insecure.example.org/list.json".to_string());

        assert!(run(command, &project.path().join("home"))
            .unwrap_err()
            .to_string()
            .contains("must start with \"https://\""));
    }

    // Ported from Composer\Test\Command\PolicyCommandTest::testAddSourceRejectsUnsupportedType.
    #[test]
    fn composer_policy_command_rejects_an_unsupported_source_type() {
        let project = project();
        let mut command = args(project.path());
        command.arg1 = Some("file".to_string());

        assert!(run(command, &project.path().join("home"))
            .unwrap_err()
            .to_string()
            .contains("Unsupported source type"));
    }

    // Ported from Composer\Test\Command\PolicyCommandTest::testAddSourceRejectsNameContainingDot.
    #[test]
    fn composer_policy_command_rejects_a_name_containing_a_dot() {
        let project = project();
        let mut command = args(project.path());
        command.name = Some("bad.name".to_string());

        assert!(run(command, &project.path().join("home"))
            .unwrap_err()
            .to_string()
            .contains("Invalid dependency policy name \"bad.name\""));
    }

    // Ported from Composer\Test\Command\PolicyCommandTest::testAddSourceRejectsJsonMissingUrl.
    #[test]
    fn composer_policy_command_rejects_json_without_a_url() {
        let project = project();
        let mut command = args(project.path());
        command.arg1 = Some(r#"{"type":"url"}"#.to_string());
        command.arg2 = None;

        assert!(run(command, &project.path().join("home"))
            .unwrap_err()
            .to_string()
            .contains("missing a string \"url\""));
    }

    // Ported from Composer\Test\Command\PolicyCommandTest::testUnknownActionThrows.
    #[test]
    fn composer_policy_command_rejects_an_unknown_action() {
        let project = project();
        let mut command = args(project.path());
        command.action = "bogus".to_string();
        command.name = None;
        command.arg1 = None;
        command.arg2 = None;

        assert!(run(command, &project.path().join("home"))
            .unwrap_err()
            .to_string()
            .contains("Unknown action"));
    }
}
