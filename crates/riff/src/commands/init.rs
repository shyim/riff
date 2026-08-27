use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value};

#[derive(Debug, usage_rs::Args)]
pub struct InitArgs {
    /// Name of the package
    #[usage(long)]
    pub name: Option<String>,

    /// Description of the package
    #[usage(long)]
    pub description: Option<String>,

    /// Author in "Name <email>" format
    #[usage(long)]
    pub author: Option<String>,

    /// Type of package
    #[usage(long = "type")]
    pub package_type: Option<String>,

    /// Homepage of the package
    #[usage(long)]
    pub homepage: Option<String>,

    /// Package and version constraint, for example vendor/package:^1.0
    #[usage(
        long,
        value_name = "PACKAGE:CONSTRAINT",
        complete = crate::commands::completion::complete_available_package
    )]
    pub require: Vec<String>,

    /// Development package and version constraint
    #[usage(
        long,
        value_name = "PACKAGE:CONSTRAINT",
        complete = crate::commands::completion::complete_available_package
    )]
    pub require_dev: Vec<String>,

    /// Minimum stability
    #[usage(short = 's', long)]
    pub stability: Option<String>,

    /// Package license
    #[usage(short = 'l', long)]
    pub license: Option<String>,

    /// Custom repository URL or JSON object
    #[usage(long, value_name = "REPOSITORY")]
    pub repository: Vec<String>,

    /// Add a PSR-4 mapping to this relative directory
    #[usage(short = 'a', long)]
    pub autoload: Option<String>,

    /// Do not ask interactive questions
    #[usage(long)]
    pub no_interaction: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InitAuthor {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

#[derive(Debug)]
struct ResolvedInit {
    name: String,
    description: Option<String>,
    author: Option<String>,
    package_type: Option<String>,
    homepage: Option<String>,
    require: Vec<String>,
    require_dev: Vec<String>,
    stability: Option<String>,
    license: Option<String>,
    repositories: Vec<String>,
    autoload: Option<String>,
}

pub fn execute(args: InitArgs, context: &crate::CommandContext) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let git_config = get_git_config(&working_dir);
    let defaults = InitDefaults::from_environment(&working_dir, &git_config)?;

    let resolved = if args.no_interaction {
        resolve_non_interactive(&args, &defaults)
    } else {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut output = RiffOutputWriter(context.output());
        let Some(resolved) = resolve_interactive(&args, &defaults, &mut input, &mut output)? else {
            riff_core::errln!(context.output(), "Command aborted");
            return Ok(1);
        };
        resolved
    };

    let manifest = build_manifest(&resolved)?;
    let path = working_dir.join("composer.json");
    let contents = riff_core::json::encode_pretty_json(&manifest, b"    ")?;
    fs::write(&path, contents).with_context(|| format!("Failed to write {}", path.display()))?;

    if let Some(autoload) = resolved.autoload.as_deref() {
        fs::create_dir_all(working_dir.join(autoload))
            .with_context(|| format!("Failed to create autoload directory {autoload}"))?;
    }

    if args.no_interaction {
        riff_core::errln!(context.output(), "Writing {}", path.display());
    }
    Ok(0)
}

struct RiffOutputWriter<'a>(&'a riff_core::Output);

impl Write for RiffOutputWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(
            riff_core::OutputLevel::Info,
            riff_core::OutputStream::Stderr,
            format_args!("{}", String::from_utf8_lossy(buffer)),
        );
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct InitDefaults {
    package_name: String,
    author: Option<String>,
}

impl InitDefaults {
    fn from_environment(working_dir: &Path, git: &BTreeMap<String, String>) -> Result<Self> {
        Ok(Self {
            package_name: default_package_name(working_dir, git)?,
            author: default_author(git),
        })
    }
}

fn resolve_non_interactive(args: &InitArgs, defaults: &InitDefaults) -> ResolvedInit {
    ResolvedInit {
        name: args
            .name
            .clone()
            .unwrap_or_else(|| defaults.package_name.clone()),
        description: args.description.clone(),
        author: args.author.clone().or_else(|| defaults.author.clone()),
        package_type: args.package_type.clone(),
        homepage: args.homepage.clone(),
        require: args.require.clone(),
        require_dev: args.require_dev.clone(),
        stability: args.stability.clone(),
        license: args.license.clone(),
        repositories: args.repository.clone(),
        autoload: args.autoload.clone(),
    }
}

fn resolve_interactive(
    args: &InitArgs,
    defaults: &InitDefaults,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Option<ResolvedInit>> {
    writeln!(output, "Welcome to the Riff config generator")?;
    let name = ask(
        input,
        output,
        "Package name (<vendor>/<name>)",
        args.name.as_deref().unwrap_or(&defaults.package_name),
    )?;
    validate_package_name(&name)?;
    let description = optional(ask(
        input,
        output,
        "Description",
        args.description.as_deref().unwrap_or(""),
    )?);
    let author_answer = ask(
        input,
        output,
        "Author (n to skip)",
        args.author
            .as_deref()
            .or(defaults.author.as_deref())
            .unwrap_or(""),
    )?;
    let author = if matches!(author_answer.as_str(), "n" | "no") {
        None
    } else {
        parse_author_string(&author_answer)?;
        optional(author_answer)
    };
    let stability = optional(ask(
        input,
        output,
        "Minimum Stability",
        args.stability.as_deref().unwrap_or(""),
    )?);
    if let Some(stability) = stability.as_deref() {
        validate_stability(stability)?;
    }
    let package_type = optional(ask(
        input,
        output,
        "Package Type",
        args.package_type.as_deref().unwrap_or(""),
    )?);
    let license = optional(ask(
        input,
        output,
        "License",
        args.license.as_deref().unwrap_or(""),
    )?);

    let require = if args.require.is_empty()
        && !ask_confirmation(input, output, "Define dependencies", true)?
    {
        Vec::new()
    } else if args.require.is_empty() {
        ask_requirements(input, output, "Dependency")?
    } else {
        args.require.clone()
    };
    let require_dev = if args.require_dev.is_empty()
        && !ask_confirmation(input, output, "Define dev dependencies", true)?
    {
        Vec::new()
    } else if args.require_dev.is_empty() {
        ask_requirements(input, output, "Dev dependency")?
    } else {
        args.require_dev.clone()
    };
    let autoload_default = args.autoload.as_deref().unwrap_or("src/");
    let autoload_answer = ask(
        input,
        output,
        "Add PSR-4 autoload mapping (n to skip)",
        autoload_default,
    )?;
    let autoload = if matches!(autoload_answer.as_str(), "n" | "no") {
        None
    } else {
        validate_autoload_path(&autoload_answer)?;
        Some(autoload_answer)
    };

    if !ask_confirmation(input, output, "Confirm generation", true)? {
        return Ok(None);
    }
    Ok(Some(ResolvedInit {
        name,
        description,
        author,
        package_type,
        homepage: args.homepage.clone(),
        require,
        require_dev,
        stability,
        license,
        repositories: args.repository.clone(),
        autoload,
    }))
}

fn ask(
    input: &mut impl BufRead,
    output: &mut impl Write,
    prompt: &str,
    default: &str,
) -> Result<String> {
    if default.is_empty() {
        write!(output, "{prompt}: ")?;
    } else {
        write!(output, "{prompt} [{default}]: ")?;
    }
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    let answer = answer.trim().to_owned();
    Ok(if answer.is_empty() {
        default.to_owned()
    } else {
        answer
    })
}

fn ask_confirmation(
    input: &mut impl BufRead,
    output: &mut impl Write,
    prompt: &str,
    default: bool,
) -> Result<bool> {
    let label = if default { "yes" } else { "no" };
    let answer = ask(input, output, prompt, label)?;
    Ok(matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn ask_requirements(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
) -> Result<Vec<String>> {
    let mut requirements = Vec::new();
    loop {
        let requirement = ask(input, output, &format!("{label} (blank to finish)"), "")?;
        if requirement.is_empty() {
            break;
        }
        parse_requirement(&requirement)?;
        requirements.push(requirement);
    }
    Ok(requirements)
}

fn build_manifest(options: &ResolvedInit) -> Result<Value> {
    validate_package_name(&options.name)?;
    if let Some(stability) = options.stability.as_deref() {
        validate_stability(stability)?;
    }
    if let Some(homepage) = options.homepage.as_deref() {
        validate_homepage(homepage)?;
    }

    let mut manifest = Map::new();
    manifest.insert("name".to_owned(), Value::String(options.name.clone()));
    if let Some(description) = options.description.as_ref() {
        manifest.insert("description".to_owned(), Value::String(description.clone()));
    }
    if let Some(package_type) = options.package_type.as_ref() {
        manifest.insert("type".to_owned(), Value::String(package_type.clone()));
    }
    if let Some(homepage) = options.homepage.as_ref() {
        manifest.insert("homepage".to_owned(), Value::String(homepage.clone()));
    }
    if let Some(license) = options.license.as_ref() {
        manifest.insert("license".to_owned(), Value::String(license.clone()));
    }
    if let Some(author) = options.author.as_deref() {
        manifest.insert(
            "authors".to_owned(),
            serde_json::to_value(format_authors(author)?)?,
        );
    }
    manifest.insert(
        "require".to_owned(),
        Value::Object(format_requirements(&options.require)?),
    );
    if !options.require_dev.is_empty() {
        manifest.insert(
            "require-dev".to_owned(),
            Value::Object(format_requirements(&options.require_dev)?),
        );
    }
    if !options.repositories.is_empty() {
        manifest.insert(
            "repositories".to_owned(),
            Value::Array(
                options
                    .repositories
                    .iter()
                    .map(|repository| parse_repository(repository))
                    .collect::<Result<Vec<_>>>()?,
            ),
        );
    }
    if let Some(stability) = options.stability.as_ref() {
        manifest.insert(
            "minimum-stability".to_owned(),
            Value::String(stability.clone()),
        );
    }
    if let Some(path) = options.autoload.as_ref() {
        validate_autoload_path(path)?;
        let namespace = namespace_from_package_name(&options.name)
            .context("Unable to derive an autoload namespace from the package name")?;
        manifest.insert(
            "autoload".to_owned(),
            serde_json::json!({"psr-4": {format!("{namespace}\\"): path}}),
        );
    }
    Ok(Value::Object(manifest))
}

pub fn parse_author_string(author: &str) -> Result<InitAuthor> {
    static AUTHOR_NAME: OnceLock<Regex> = OnceLock::new();
    let author = author.trim();
    if author.is_empty() {
        bail!("Invalid author string. Must be in the formats: Jane Doe or John Smith <john@example.com>");
    }
    let (name, email) = if author.ends_with('>') {
        let Some((name, email)) = author.rsplit_once(" <") else {
            bail!("Invalid author string. Must be in the formats: Jane Doe or John Smith <john@example.com>");
        };
        (name.trim(), Some(email.trim_end_matches('>').trim()))
    } else {
        (author, None)
    };
    if name.is_empty()
        || !AUTHOR_NAME
            .get_or_init(|| {
                Regex::new(r#"^[\p{L}\p{N}\p{Mn}\- .,\'’\"()]+$"#)
                    .expect("author name regex must compile")
            })
            .is_match(name)
    {
        bail!("Invalid author string. Must be in the formats: Jane Doe or John Smith <john@example.com>");
    }
    if let Some(email) = email {
        if !is_valid_email(email) {
            bail!("Invalid email \"{email}\"");
        }
    }
    Ok(InitAuthor {
        name: name.to_owned(),
        email: email.map(str::to_owned),
    })
}

pub fn format_authors(author: &str) -> Result<Vec<InitAuthor>> {
    Ok(vec![parse_author_string(author)?])
}

pub fn namespace_from_package_name(package_name: &str) -> Option<String> {
    let (vendor, package) = package_name.split_once('/')?;
    if vendor.is_empty() || package.is_empty() || package.contains('/') {
        return None;
    }
    Some(
        [vendor, package]
            .into_iter()
            .map(namespace_part)
            .collect::<Vec<_>>()
            .join("\\"),
    )
}

fn namespace_part(part: &str) -> String {
    let mut result = String::new();
    let mut uppercase_next = true;
    for character in part.chars() {
        if character.is_ascii_alphanumeric() {
            if uppercase_next {
                result.extend(character.to_uppercase());
                uppercase_next = false;
            } else {
                result.push(character);
            }
        } else {
            uppercase_next = true;
        }
    }
    result
}

pub fn parse_git_config(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

pub fn get_git_config(working_dir: &Path) -> BTreeMap<String, String> {
    Command::new("git")
        .args(["config", "-l"])
        .current_dir(working_dir)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| parse_git_config(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default()
}

pub fn has_vendor_ignore(ignore_file: &Path, vendor: &str) -> Result<bool> {
    let contents = match fs::read_to_string(ignore_file) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let vendor = vendor.trim_matches('/');
    Ok(contents.lines().any(|line| {
        let line = line.strip_prefix('/').unwrap_or(line);
        matches!(line.strip_prefix(vendor), Some("") | Some("/") | Some("/*"))
    }))
}

pub fn add_vendor_ignore(ignore_file: &Path, vendor: &str) -> Result<()> {
    let mut contents = match fs::read_to_string(ignore_file) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(vendor);
    contents.push('\n');
    if let Some(parent) = ignore_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(ignore_file, contents)?;
    Ok(())
}

pub fn sanitize_package_name_component(name: &str) -> String {
    let characters = name.chars().collect::<Vec<_>>();
    let mut separated = String::new();
    for (index, character) in characters.iter().copied().enumerate() {
        let previous = index.checked_sub(1).and_then(|index| characters.get(index));
        let next = characters.get(index + 1);
        let camel_boundary = character.is_ascii_uppercase()
            && previous.is_some_and(char::is_ascii_lowercase)
            || character.is_ascii_uppercase()
                && previous.is_some_and(char::is_ascii_uppercase)
                && next.is_some_and(char::is_ascii_lowercase);
        if camel_boundary {
            separated.push('-');
        }
        separated.extend(character.to_lowercase());
    }

    let cleaned = separated
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || "_.-".contains(*character))
        .collect::<String>();
    let cleaned = cleaned.trim_matches(|character| "_.-".contains(character));
    let mut result = String::new();
    let mut separator = None;
    for character in cleaned.chars() {
        if "_.-".contains(character) {
            separator = Some(character);
        } else {
            if let Some(separator) = separator.take() {
                result.push(separator);
            }
            result.push(character);
        }
    }
    result
}

fn default_package_name(working_dir: &Path, git: &BTreeMap<String, String>) -> Result<String> {
    let directory = working_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("Unable to derive a package name from the working directory")?;
    let package = sanitize_package_name_component(directory);
    let vendor = std::env::var("COMPOSER_DEFAULT_VENDOR")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| git.get("github.user").cloned())
        .or_else(|| std::env::var("USERNAME").ok())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| package.clone());
    let vendor = sanitize_package_name_component(&vendor);
    if vendor.is_empty() || package.is_empty() {
        bail!("Unable to derive a valid package name from the working directory");
    }
    Ok(format!("{vendor}/{package}"))
}

fn default_author(git: &BTreeMap<String, String>) -> Option<String> {
    let name = std::env::var("COMPOSER_DEFAULT_AUTHOR")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| git.get("user.name").cloned());
    let email = std::env::var("COMPOSER_DEFAULT_EMAIL")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| git.get("user.email").cloned());
    match (name, email) {
        (Some(name), Some(email)) => Some(format!("{name} <{email}>")),
        _ => None,
    }
}

fn format_requirements(requirements: &[String]) -> Result<Map<String, Value>> {
    requirements
        .iter()
        .map(|requirement| {
            let (package, constraint) = parse_requirement(requirement)?;
            Ok((package, Value::String(constraint)))
        })
        .collect()
}

fn parse_requirement(requirement: &str) -> Result<(String, String)> {
    let separator = requirement
        .char_indices()
        .find(|(index, character)| {
            *index > requirement.find('/').unwrap_or(usize::MAX)
                && matches!(*character, ':' | '=' | ' ')
        })
        .map(|(index, _)| index);
    let Some(separator) = separator else {
        bail!("Option {requirement} is missing a version constraint, use e.g. {requirement}:^1.0");
    };
    let package = requirement[..separator].trim();
    let constraint = requirement[separator + 1..].trim();
    if package.is_empty() || constraint.is_empty() {
        bail!("Option {package} is missing a version constraint, use e.g. {package}:^1.0");
    }
    Ok((package.to_owned(), constraint.to_owned()))
}

fn parse_repository(repository: &str) -> Result<Value> {
    let repository_start = repository.trim_start();
    if repository_start.starts_with('{') || repository_start.starts_with('[') {
        return serde_json::from_str(repository)
            .with_context(|| format!("Invalid repository JSON: {repository}"));
    }
    validate_homepage(repository)?;
    Ok(serde_json::json!({"type": "composer", "url": repository}))
}

fn validate_package_name(name: &str) -> Result<()> {
    static PACKAGE_NAME: OnceLock<Regex> = OnceLock::new();
    if PACKAGE_NAME
        .get_or_init(|| {
            Regex::new(r"^[a-z0-9]([_.-]?[a-z0-9]+)*/[a-z0-9](([_.]|-{1,2})?[a-z0-9]+)*$")
                .expect("package name regex must compile")
        })
        .is_match(name)
    {
        Ok(())
    } else {
        bail!("The package name {name} is invalid, it should be lowercase and have a vendor name, a forward slash, and a package name")
    }
}

fn validate_stability(stability: &str) -> Result<()> {
    if ["dev", "alpha", "beta", "rc", "stable"]
        .iter()
        .any(|valid| stability.eq_ignore_ascii_case(valid))
    {
        Ok(())
    } else {
        bail!("minimum-stability: Does not have a value in the enumeration")
    }
}

fn validate_homepage(homepage: &str) -> Result<()> {
    if reqwest::Url::parse(homepage)
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https"))
    {
        Ok(())
    } else {
        bail!("homepage: Invalid URL format")
    }
}

fn validate_autoload_path(path: &str) -> Result<()> {
    if !path.starts_with('/')
        && path.ends_with('/')
        && path.len() > 1
        && path
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_/".contains(character))
    {
        Ok(())
    } else {
        bail!("The src folder name \"{path}\" is invalid. Please use a relative path ending in a forward slash")
    }
}

fn is_valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain
            .split_once('.')
            .is_some_and(|(name, suffix)| !name.is_empty() && !suffix.is_empty())
        && !email.chars().any(char::is_whitespace)
}

fn optional(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_init_parses_valid_author_strings() {
        let cases = [
            (
                "John Smith",
                Some("john@example.com"),
                "John Smith <john@example.com>",
            ),
            ("John Smith", None, "John Smith"),
            (
                "Matti Meikäläinen",
                Some("matti@example.com"),
                "Matti Meikäläinen <matti@example.com>",
            ),
            (
                "Matti Meika\u{308}la\u{308}inen",
                Some("matti@example.com"),
                "Matti Meika\u{308}la\u{308}inen <matti@example.com>",
            ),
            ("h4x0r", Some("h4x@example.com"), "h4x0r <h4x@example.com>"),
            (
                "Johnathon \"Johnny\" Smith",
                Some("john@example.com"),
                "Johnathon \"Johnny\" Smith <john@example.com>",
            ),
            (
                "Johnathon (Johnny) Smith",
                Some("john@example.com"),
                "Johnathon (Johnny) Smith <john@example.com>",
            ),
        ];
        for (name, email, input) in cases {
            assert_eq!(
                parse_author_string(input).unwrap(),
                InitAuthor {
                    name: name.to_owned(),
                    email: email.map(str::to_owned),
                },
                "{input}"
            );
        }
    }

    #[test]
    fn composer_init_rejects_an_empty_author() {
        assert!(parse_author_string("").is_err());
    }

    #[test]
    fn composer_init_rejects_an_invalid_author_email() {
        assert_eq!(
            parse_author_string("John Smith <john>")
                .unwrap_err()
                .to_string(),
            "Invalid email \"john\""
        );
    }

    #[test]
    fn composer_init_derives_namespace_from_a_valid_package_name() {
        assert_eq!(
            namespace_from_package_name("new_projects.acme-extra/package-name"),
            Some("NewProjectsAcmeExtra\\PackageName".to_owned())
        );
    }

    #[test]
    fn composer_init_rejects_namespace_for_an_invalid_package_name() {
        assert_eq!(namespace_from_package_name("invalid-package-name"), None);
    }

    #[test]
    fn composer_init_rejects_namespace_for_a_missing_package_name() {
        assert_eq!(namespace_from_package_name(""), None);
    }

    #[test]
    fn composer_init_formats_authors_with_optional_email() {
        assert_eq!(
            format_authors("John Smith <john@example.com>").unwrap(),
            vec![InitAuthor {
                name: "John Smith".to_owned(),
                email: Some("john@example.com".to_owned()),
            }]
        );
        assert_eq!(
            format_authors("John Smith").unwrap(),
            vec![InitAuthor {
                name: "John Smith".to_owned(),
                email: None,
            }]
        );
    }

    #[test]
    fn composer_init_parses_git_configuration() {
        let config = parse_git_config("user.name=John Smith\nuser.email=john@example.com\n");
        assert_eq!(
            config.get("user.name").map(String::as_str),
            Some("John Smith")
        );
        assert_eq!(
            config.get("user.email").map(String::as_str),
            Some("john@example.com")
        );
    }

    #[test]
    fn composer_init_adds_vendor_ignore() {
        let directory = tempfile::tempdir().unwrap();
        let ignore = directory.path().join("ignore");
        add_vendor_ignore(&ignore, "/vendor/").unwrap();
        assert!(fs::read_to_string(ignore).unwrap().contains("/vendor/"));
    }

    #[test]
    fn composer_init_detects_vendor_ignore() {
        let directory = tempfile::tempdir().unwrap();
        let ignore = directory.path().join("ignore");
        assert!(!has_vendor_ignore(&ignore, "vendor").unwrap());
        add_vendor_ignore(&ignore, "/vendor/").unwrap();
        assert!(has_vendor_ignore(&ignore, "vendor").unwrap());
    }

    #[test]
    fn package_component_sanitization_matches_composer() {
        assert_eq!(
            sanitize_package_name_component("_foo_--bar__baz.--..qux__"),
            "foo-bar_baz.qux"
        );
        assert_eq!(
            sanitize_package_name_component(".vendorName"),
            "vendor-name"
        );
    }
}
