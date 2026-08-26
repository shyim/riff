use std::collections::HashSet;
use std::sync::OnceLock;

use jsonschema::error::{ValidationError, ValidationErrorKind};
use jsonschema::Validator;
use regex::Regex;
use riff_semver::VersionParser;
use riff_spdx::SpdxLicenses;
use serde_json::{Map, Value};

use crate::package::branch_alias_is_valid;
use crate::{is_platform_package, url_utils::is_allowed_redirect};

const COMPOSER_SCHEMA: &str = include_str!("../../res/composer-schema.json");

static LAX_VALIDATOR: OnceLock<Validator> = OnceLock::new();
static STRICT_VALIDATOR: OnceLock<Validator> = OnceLock::new();
static SCHEMA: OnceLock<Value> = OnceLock::new();
static SPDX_LICENSES: OnceLock<SpdxLicenses> = OnceLock::new();
static PACKAGE_NAME_REGEX: OnceLock<Regex> = OnceLock::new();

#[derive(Clone, Copy, Debug)]
pub struct ManifestValidationOptions {
    pub check_constraints: bool,
    pub check_version: bool,
    pub check_publish: bool,
}

impl Default for ManifestValidationOptions {
    fn default() -> Self {
        Self {
            check_constraints: true,
            check_version: true,
            check_publish: true,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ManifestValidation {
    pub errors: Vec<String>,
    pub publish_errors: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn validate_composer_manifest(
    content: &str,
    source: &str,
    options: ManifestValidationOptions,
) -> ManifestValidation {
    let manifest: Value = match serde_json::from_str(content) {
        Ok(value) => value,
        Err(error) => {
            let mut result = ManifestValidation::default();
            result
                .errors
                .push(format!("{source} does not contain valid JSON: {error}"));
            return result;
        }
    };

    validate_parsed_composer_manifest(content, source, options, &manifest)
}

pub fn validate_parsed_composer_manifest(
    content: &str,
    source: &str,
    options: ManifestValidationOptions,
    manifest: &Value,
) -> ManifestValidation {
    let mut result = ManifestValidation::default();

    let lax_errors = schema_errors(lax_validator(), manifest);
    if lax_errors.is_empty() {
        if options.check_publish {
            result.publish_errors = schema_errors(strict_validator(), manifest);
        }
    } else {
        result.errors = lax_errors;
    }

    let Some(root) = manifest.as_object() else {
        return result;
    };

    for (key, line) in duplicate_keys(content) {
        result.warnings.push(format!(
            "Key {key} is a duplicate in {source} at line {line}"
        ));
    }
    add_manifest_warnings(root, options, &mut result);
    add_package_validation(root, options, &mut result);
    result
}

/// Apply the normalization performed by Composer's validating package loader
/// before handing metadata to the typed package loader.
pub fn sanitize_package_manifest(manifest: &mut Value) {
    let Some(root) = manifest.as_object_mut() else {
        return;
    };
    if let Some(license) = root.get_mut("license") {
        if license.is_string() {
            *license = Value::Array(vec![license.take()]);
        } else if let Some(licenses) = license.as_array_mut() {
            licenses.retain(Value::is_string);
        }
    }
    for source in ["source", "dist"] {
        if let Some(reference) = root
            .get_mut(source)
            .and_then(Value::as_object_mut)
            .and_then(|source| source.get_mut("reference"))
            .filter(|reference| reference.is_number())
        {
            *reference = Value::String(reference.to_string());
        }
    }
}

/// Validate package metadata (as opposed to a root project manifest), applying
/// the validating loader's normalizations and requiring a package name.
pub fn validate_package_manifest(manifest: &mut Value, source: &str) -> ManifestValidation {
    sanitize_package_manifest(manifest);
    let content = manifest.to_string();
    let mut validation = validate_parsed_composer_manifest(
        &content,
        source,
        ManifestValidationOptions {
            check_publish: false,
            ..ManifestValidationOptions::default()
        },
        manifest,
    );
    if manifest.get("name").is_none() {
        validation.errors.push("name : must be present".to_string());
    }
    validation
}

#[derive(Debug)]
enum JsonToken {
    ObjectStart,
    ObjectEnd,
    ArrayStart,
    ArrayEnd,
    Colon,
    Comma,
    String(String, usize),
    Primitive,
}

fn duplicate_keys(content: &str) -> Vec<(String, usize)> {
    let tokens = tokenize_json(content);
    let mut position = 0;
    let mut duplicates = Vec::new();
    parse_json_value(&tokens, &mut position, &mut duplicates);
    duplicates
}

fn tokenize_json(content: &str) -> Vec<JsonToken> {
    let bytes = content.as_bytes();
    let mut tokens = Vec::new();
    let mut position = 0;
    let mut line = 1;

    while position < bytes.len() {
        match bytes[position] {
            b' ' | b'\t' | b'\r' => position += 1,
            b'\n' => {
                line += 1;
                position += 1;
            }
            b'{' => {
                tokens.push(JsonToken::ObjectStart);
                position += 1;
            }
            b'}' => {
                tokens.push(JsonToken::ObjectEnd);
                position += 1;
            }
            b'[' => {
                tokens.push(JsonToken::ArrayStart);
                position += 1;
            }
            b']' => {
                tokens.push(JsonToken::ArrayEnd);
                position += 1;
            }
            b':' => {
                tokens.push(JsonToken::Colon);
                position += 1;
            }
            b',' => {
                tokens.push(JsonToken::Comma);
                position += 1;
            }
            b'"' => {
                let start = position;
                let string_line = line;
                position += 1;
                while position < bytes.len() {
                    match bytes[position] {
                        b'\\' => position = (position + 2).min(bytes.len()),
                        b'"' => {
                            position += 1;
                            break;
                        }
                        b'\n' => {
                            line += 1;
                            position += 1;
                        }
                        _ => position += 1,
                    }
                }
                if let Ok(value) = serde_json::from_slice::<String>(&bytes[start..position]) {
                    tokens.push(JsonToken::String(value, string_line));
                }
            }
            _ => {
                while position < bytes.len()
                    && !matches!(
                        bytes[position],
                        b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}'
                    )
                {
                    position += 1;
                }
                tokens.push(JsonToken::Primitive);
            }
        }
    }
    tokens
}

fn parse_json_value(
    tokens: &[JsonToken],
    position: &mut usize,
    duplicates: &mut Vec<(String, usize)>,
) {
    match tokens.get(*position) {
        Some(JsonToken::ObjectStart) => {
            *position += 1;
            let mut keys = HashSet::new();
            let mut reported_keys = HashSet::new();
            while !matches!(tokens.get(*position), None | Some(JsonToken::ObjectEnd)) {
                let Some(JsonToken::String(key, line)) = tokens.get(*position) else {
                    break;
                };
                if !keys.insert(key.clone()) && reported_keys.insert(key.clone()) {
                    duplicates.push((key.clone(), *line));
                }
                *position += 1;
                if matches!(tokens.get(*position), Some(JsonToken::Colon)) {
                    *position += 1;
                }
                parse_json_value(tokens, position, duplicates);
                if matches!(tokens.get(*position), Some(JsonToken::Comma)) {
                    *position += 1;
                }
            }
            if matches!(tokens.get(*position), Some(JsonToken::ObjectEnd)) {
                *position += 1;
            }
        }
        Some(JsonToken::ArrayStart) => {
            *position += 1;
            while !matches!(tokens.get(*position), None | Some(JsonToken::ArrayEnd)) {
                parse_json_value(tokens, position, duplicates);
                if matches!(tokens.get(*position), Some(JsonToken::Comma)) {
                    *position += 1;
                }
            }
            if matches!(tokens.get(*position), Some(JsonToken::ArrayEnd)) {
                *position += 1;
            }
        }
        Some(_) => *position += 1,
        None => {}
    }
}

fn lax_validator() -> &'static Validator {
    LAX_VALIDATOR.get_or_init(|| {
        jsonschema::draft4::new(schema()).expect("embedded Composer schema must compile")
    })
}

fn strict_validator() -> &'static Validator {
    STRICT_VALIDATOR.get_or_init(|| {
        let mut strict_schema = schema().clone();
        let root = strict_schema
            .as_object_mut()
            .expect("embedded Composer schema must be an object");
        root.insert("additionalProperties".to_string(), Value::Bool(false));
        root.insert(
            "required".to_string(),
            Value::Array(vec![Value::from("name"), Value::from("description")]),
        );
        jsonschema::draft4::new(&strict_schema).expect("strict Composer schema must compile")
    })
}

fn schema() -> &'static Value {
    SCHEMA.get_or_init(|| {
        serde_json::from_str(COMPOSER_SCHEMA)
            .expect("embedded Composer schema must contain valid JSON")
    })
}

fn schema_errors(validator: &Validator, manifest: &Value) -> Vec<String> {
    validator
        .iter_errors(manifest)
        .flat_map(format_schema_error)
        .collect()
}

fn format_schema_error(error: ValidationError<'_>) -> Vec<String> {
    let path = error
        .instance_path()
        .as_str()
        .trim_start_matches('/')
        .replace("/", ".")
        .replace("~1", "/")
        .replace("~0", "~");
    let prefix = (!path.is_empty()).then(|| format!("{path} : "));

    match error.kind() {
        ValidationErrorKind::Required { property } => {
            let property = property.as_str().unwrap_or("unknown");
            vec![format!("{property} : The property {property} is required")]
        }
        ValidationErrorKind::AdditionalProperties { unexpected } => unexpected
            .iter()
            .map(|property| {
                format!(
                    "{property} : The property {property} is not defined and the definition does not allow additional properties"
                )
            })
            .collect(),
        _ => vec![format!("{}{}", prefix.unwrap_or_default(), error)],
    }
}

fn add_manifest_warnings(
    root: &Map<String, Value>,
    options: ManifestValidationOptions,
    result: &mut ManifestValidation,
) {
    let licenses = root.get("license").and_then(license_strings);
    if licenses.as_ref().is_none_or(Vec::is_empty) {
        result.warnings.push(
            "No license specified, it is recommended to do so. For closed-source software you may use \"proprietary\" as license."
                .to_string(),
        );
    } else if let Some(licenses) = licenses {
        let spdx = SPDX_LICENSES.get_or_init(SpdxLicenses::new);
        for license in licenses {
            if license == "proprietary" {
                continue;
            }
            if !spdx.validate(license) {
                if spdx.validate(license.trim()) {
                    result.warnings.push(format!(
                        "License {} must not contain extra spaces, make sure to trim it.",
                        json_string(license)
                    ));
                } else {
                    result.warnings.push(format!(
                        "License {} is not a valid SPDX license identifier, see https://spdx.org/licenses/ if you use an open license.\nIf the software is closed-source, you may use \"proprietary\" as license.",
                        json_string(license)
                    ));
                }
            } else if spdx.is_deprecated_by_identifier(license) {
                result.warnings.push(deprecated_license_warning(license));
            }
        }
    }

    if options.check_version && root.contains_key("version") {
        result.warnings.push(
            "The version field is present, it is recommended to leave it out if the package is published on Packagist."
                .to_string(),
        );
    }

    if root.get("type").and_then(Value::as_str) == Some("composer-installer") {
        result.warnings.push(
            "The package type 'composer-installer' is deprecated. Please distribute your custom installers as plugins from now on. See https://getcomposer.org/doc/articles/plugins.md for plugin documentation."
                .to_string(),
        );
    }

    add_overlap_warnings(root, "require", "require-dev", result);
    for provided in ["provide", "replace"] {
        for required in ["require", "require-dev"] {
            let Some(provided_packages) = root.get(provided).and_then(Value::as_object) else {
                continue;
            };
            let Some(required_packages) = root.get(required).and_then(Value::as_object) else {
                continue;
            };
            for package in provided_packages.keys() {
                if required_packages.contains_key(package) {
                    result.warnings.push(format!(
                        "The package {package} in {required} is also listed in {provided} which satisfies the requirement. Remove it from {provided} if you wish to install it."
                    ));
                }
            }
        }
    }

    for link_type in ["require", "require-dev"] {
        if let Some(packages) = root.get(link_type).and_then(Value::as_object) {
            for (package, constraint) in packages {
                if constraint.as_str().is_some_and(|value| value.contains('#')) {
                    result.warnings.push(format!(
                        "The package \"{package}\" is pointing to a commit-ref, this is bad practice and can cause unforeseen issues."
                    ));
                }
            }
        }
    }

    add_missing_script_warnings(root, "scripts-descriptions", "Description", result);
    add_missing_script_warnings(root, "scripts-aliases", "Aliases", result);

    for standard in ["psr-0", "psr-4"] {
        if root
            .get("autoload")
            .and_then(Value::as_object)
            .and_then(|autoload| autoload.get(standard))
            .and_then(Value::as_object)
            .is_some_and(|rules| rules.contains_key(""))
        {
            result.warnings.push(format!(
                "Defining autoload.{standard} with an empty namespace prefix is a bad idea for performance"
            ));
        }
    }
}

fn add_package_validation(
    root: &Map<String, Value>,
    options: ManifestValidationOptions,
    result: &mut ManifestValidation,
) {
    let parser = VersionParser::new();
    let root_name = root.get("name").and_then(Value::as_str);

    if let Some(homepage) = root.get("homepage").and_then(Value::as_str) {
        add_http_url_warning("homepage", homepage, result);
    }
    if let Some(support) = root.get("support").and_then(Value::as_object) {
        for field in ["source", "forum", "issues", "wiki", "chat", "security"] {
            if let Some(value) = support.get(field).and_then(Value::as_str) {
                add_http_url_warning(&format!("support.{field}"), value, result);
            }
        }
    }

    if let Some(name) = root_name {
        if let Some(error) = package_naming_error(name, false) {
            result.errors.push(format!("name : {error}"));
        }
    }

    if let Some(autoload) = root.get("autoload").and_then(Value::as_object) {
        let allowed = [
            "psr-0",
            "psr-4",
            "classmap",
            "files",
            "exclude-from-classmap",
        ];
        for field in autoload
            .keys()
            .filter(|field| !allowed.contains(&field.as_str()))
        {
            result.errors.push(format!(
                "autoload : invalid value ({field}), must be one of {}",
                allowed.join(", ")
            ));
        }
    }

    if let Some(version) = root.get("version").and_then(Value::as_str) {
        if let Err(error) = parser.normalize(version) {
            result
                .errors
                .push(format!("version : invalid value ({version}): {error}"));
        }
    }

    for link_type in ["require", "require-dev", "conflict", "replace", "provide"] {
        let Some(packages) = root.get(link_type).and_then(Value::as_object) else {
            continue;
        };
        for (package, constraint) in packages {
            if root_name.is_some_and(|name| name.eq_ignore_ascii_case(package)) {
                result.errors.push(format!(
                    "{link_type}.{package} : a package cannot set a {link_type} on itself"
                ));
                continue;
            }

            if let Some(error) = package_naming_error(package, true) {
                result.warnings.push(format!("{link_type}.{error}"));
            }

            let Some(constraint) = constraint.as_str() else {
                continue;
            };
            if constraint == "self.version" {
                continue;
            }
            let parsed = match parser.parse_constraints_cached(constraint) {
                Ok(parsed) => parsed,
                Err(error) => {
                    result.errors.push(format!(
                        "{link_type}.{package} : invalid version constraint ({error})"
                    ));
                    continue;
                }
            };

            if options.check_constraints && parsed.is_match_none() {
                result.warnings.push(format!(
                    "{link_type}.{package} : this version constraint cannot possibly match anything ({constraint})"
                ));
            }

            if options.check_constraints && link_type == "require" {
                if !is_platform_package(package) && parsed.satisfies("10000000-dev") {
                    result.warnings.push(format!(
                        "{link_type}.{package} : unbound version constraints ({constraint}) should be avoided"
                    ));
                } else if looks_like_exact_constraint(constraint, &parser) {
                    result.warnings.push(format!(
                        "{link_type}.{package} : exact version constraints ({constraint}) should be avoided if the package follows semantic versioning"
                    ));
                }
            }
        }
    }

    if let Some(branch_aliases) = root
        .get("extra")
        .and_then(|extra| extra.get("branch-alias"))
        .and_then(Value::as_object)
    {
        for (source, target) in branch_aliases {
            let Some(target) = target.as_str() else {
                continue;
            };
            if !branch_alias_is_valid(source, target) {
                result.warnings.push(format!(
                    "extra.branch-alias.{source} : the target branch ({target}) is not a valid numeric alias for this version"
                ));
            }
        }
    }

    if let (Some(conflicts), Some(replaces)) = (
        root.get("conflict").and_then(Value::as_object),
        root.get("replace").and_then(Value::as_object),
    ) {
        for package in conflicts.keys().filter(|name| replaces.contains_key(*name)) {
            result.errors.push(format!(
                "conflict.{package} : you cannot conflict with a package that is also replaced, as replace already creates an implicit conflict rule"
            ));
        }
    }

    if let Some(psr4) = root
        .get("autoload")
        .and_then(Value::as_object)
        .and_then(|autoload| autoload.get("psr-4"))
        .and_then(Value::as_object)
    {
        for namespace in psr4
            .keys()
            .filter(|name| !name.is_empty() && !name.ends_with('\\'))
        {
            result.errors.push(format!(
                "autoload.psr-4 : invalid value ({namespace}), namespaces must end with a namespace separator, should be {namespace}\\"
            ));
        }
    }

    if root.contains_key("target-dir")
        && root
            .get("autoload")
            .and_then(Value::as_object)
            .is_some_and(|autoload| autoload.contains_key("psr-4"))
    {
        result.errors.push(
            "target-dir : this can not be used together with the autoload.psr-4 setting, remove target-dir to upgrade to psr-4"
                .to_string(),
        );
    }

    if let Some(bins) = root.get("bin") {
        for (index, bin) in string_or_array(bins).into_iter().enumerate() {
            if bin.split(['/', '\\']).any(|component| component == "..") {
                let field = if bins.is_string() {
                    "bin".to_string()
                } else {
                    format!("bin.{index}")
                };
                result.errors.push(format!(
                    "{field} : invalid value ({bin}), must not contain a \"..\" path component"
                ));
            }
        }
    }
}

fn add_http_url_warning(field: &str, value: &str, result: &mut ManifestValidation) {
    if !is_allowed_redirect(value) {
        result.warnings.push(format!(
            "{field} : invalid value ({value}), must be an http/https URL"
        ));
    }
}

fn add_overlap_warnings(
    root: &Map<String, Value>,
    first: &str,
    second: &str,
    result: &mut ManifestValidation,
) {
    let Some(first_packages) = root.get(first).and_then(Value::as_object) else {
        return;
    };
    let Some(second_packages) = root.get(second).and_then(Value::as_object) else {
        return;
    };
    let overlap: Vec<_> = first_packages
        .keys()
        .filter(|package| second_packages.contains_key(*package))
        .cloned()
        .collect();
    if !overlap.is_empty() {
        result.warnings.push(format!(
            "{} {} required both in {first} and {second}, this can lead to unexpected behavior",
            overlap.join(", "),
            if overlap.len() > 1 { "are" } else { "is" }
        ));
    }
}

fn add_missing_script_warnings(
    root: &Map<String, Value>,
    metadata_key: &str,
    label: &str,
    result: &mut ManifestValidation,
) {
    let scripts = root.get("scripts").and_then(Value::as_object);
    let Some(metadata) = root.get(metadata_key).and_then(Value::as_object) else {
        return;
    };
    for script in metadata.keys() {
        if scripts.is_none_or(|scripts| !scripts.contains_key(script)) {
            result.warnings.push(format!(
                "{label} for non-existent script \"{script}\" found in \"{metadata_key}\""
            ));
        }
    }
}

fn license_strings(value: &Value) -> Option<Vec<&str>> {
    if let Some(license) = value.as_str() {
        return Some(vec![license]);
    }
    value
        .as_array()
        .map(|licenses| licenses.iter().filter_map(Value::as_str).collect())
}

fn string_or_array(value: &Value) -> Vec<&str> {
    if let Some(value) = value.as_str() {
        return vec![value];
    }
    value
        .as_array()
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
}

fn deprecated_license_warning(license: &str) -> String {
    let legacy_gpl =
        Regex::new(r"(?i)^[al]?gpl-[123](?:\.[01])?$").expect("deprecated GPL regex must compile");
    let legacy_gpl_or_later = Regex::new(r"(?i)^[al]?gpl-[123](?:\.[01])?\+$")
        .expect("deprecated GPL-or-later regex must compile");

    if legacy_gpl_or_later.is_match(license) {
        format!(
            "License \"{license}\" is a deprecated SPDX license identifier, use \"{}-or-later\" instead",
            license.trim_end_matches('+')
        )
    } else if legacy_gpl.is_match(license) {
        format!(
            "License \"{license}\" is a deprecated SPDX license identifier, use \"{license}-only\" or \"{license}-or-later\" instead"
        )
    } else {
        format!(
            "License \"{license}\" is a deprecated SPDX license identifier, see https://spdx.org/licenses/"
        )
    }
}

fn package_naming_error(name: &str, is_link: bool) -> Option<String> {
    if is_platform_package(name) {
        return None;
    }
    let pattern = PACKAGE_NAME_REGEX.get_or_init(|| {
        Regex::new(r"(?i)^[a-z0-9](?:[_.-]?[a-z0-9]+)*/[a-z0-9](?:(?:[_.]|-{1,2})?[a-z0-9]+)*$")
            .expect("package name regex must compile")
    });
    if !pattern.is_match(name) {
        return Some(format!(
            "{name} is invalid, it should have a vendor name, a forward slash, and a package name. The vendor and package name can be words separated by -, . or _."
        ));
    }

    let reserved = [
        "nul", "con", "prn", "aux", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    if name
        .split('/')
        .any(|part| reserved.contains(&part.to_ascii_lowercase().as_str()))
    {
        return Some(format!(
            "{name} is reserved, package and vendor names can not use reserved device names."
        ));
    }
    if name.to_ascii_lowercase().ends_with(".json") {
        return Some(format!(
            "{name} is invalid, package names can not end in .json, consider renaming it or perhaps using a -json suffix instead."
        ));
    }
    if name.chars().any(|character| character.is_ascii_uppercase()) {
        let lowercase = name.to_ascii_lowercase();
        return Some(if is_link {
            format!(
                "{name} is invalid, it should not contain uppercase characters. Please use {lowercase} instead."
            )
        } else {
            format!(
                "{name} is invalid, it should not contain uppercase characters. We suggest using {lowercase} instead."
            )
        });
    }
    None
}

fn looks_like_exact_constraint(constraint: &str, parser: &VersionParser) -> bool {
    let trimmed = constraint.trim();
    if trimmed.contains(['*', 'x', 'X', '^', '~', '<', '>', '|', ',', ' ']) {
        return false;
    }
    let version = trimmed
        .strip_prefix("==")
        .or_else(|| trimmed.strip_prefix('='))
        .unwrap_or(trimmed);
    version
        .trim_start_matches(['v', 'V'])
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
        && parser.normalize(version).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_validation(json: &str) -> ManifestValidation {
        validate_composer_manifest(
            json,
            "./composer.json",
            ManifestValidationOptions {
                check_publish: false,
                ..ManifestValidationOptions::default()
            },
        )
    }

    fn validating_loader_validation(mut manifest: Value) -> ManifestValidation {
        validate_package_manifest(&mut manifest, "package metadata")
    }

    // Ported from Composer\Test\Util\ConfigValidatorTest::
    // testConfigValidatorCommitRefWarning.
    #[test]
    fn composer_config_validator_warns_about_commit_references() {
        let validation = schema_validation(r#"{"require":{"some/package":"dev-main#f00ba4"}}"#);

        assert!(validation.warnings.contains(&
            "The package \"some/package\" is pointing to a commit-ref, this is bad practice and can cause unforeseen issues."
                .to_string()
        ));
    }

    // Ported from Composer\Test\Util\ConfigValidatorTest::
    // testConfigValidatorWarnsOnScriptDescriptionForNonexistentScript.
    #[test]
    fn composer_config_validator_warns_about_description_for_missing_script() {
        let validation = schema_validation(
            r#"{
                "scripts":{"phpcs":"phpcs"},
                "scripts-descriptions":{"phpcsxxx":"Run PHPCS"}
            }"#,
        );

        assert!(validation.warnings.contains(
            &"Description for non-existent script \"phpcsxxx\" found in \"scripts-descriptions\""
                .to_string()
        ));
    }

    // Ported from Composer\Test\Util\ConfigValidatorTest::
    // testConfigValidatorWarnsOnScriptAliasForNonexistentScript.
    #[test]
    fn composer_config_validator_warns_about_alias_for_missing_script() {
        let validation = schema_validation(
            r#"{
                "scripts":{"phpcs":"phpcs"},
                "scripts-aliases":{"phpcsxxx":["lint"]}
            }"#,
        );

        assert!(validation.warnings.contains(
            &"Aliases for non-existent script \"phpcsxxx\" found in \"scripts-aliases\""
                .to_string()
        ));
    }

    // Ported from Composer\Test\Util\ConfigValidatorTest::
    // testConfigValidatorWarnsOnUnnecessaryProvideReplace.
    #[test]
    fn composer_config_validator_warns_about_require_provide_replace_overlap() {
        let validation = schema_validation(
            r#"{
                "require":{"a/a":"*","b/b":"*"},
                "require-dev":{"c/c":"*"},
                "provide":{"a/a":"*","c/c":"*"},
                "replace":{"b/b":"*"}
            }"#,
        );

        for warning in [
            "The package a/a in require is also listed in provide which satisfies the requirement. Remove it from provide if you wish to install it.",
            "The package b/b in require is also listed in replace which satisfies the requirement. Remove it from replace if you wish to install it.",
            "The package c/c in require-dev is also listed in provide which satisfies the requirement. Remove it from provide if you wish to install it.",
        ] {
            assert!(validation.warnings.contains(&warning.to_string()));
        }
    }

    #[test]
    fn separates_publish_errors_from_general_warnings() {
        let validation = validate_composer_manifest(
            r#"{"require":{"vendor/package":"^1.0"}}"#,
            "./composer.json",
            ManifestValidationOptions::default(),
        );

        assert!(validation.errors.is_empty());
        assert_eq!(validation.publish_errors.len(), 2);
        assert!(validation
            .warnings
            .iter()
            .any(|warning| warning.starts_with("No license specified")));
    }

    #[test]
    fn reports_semantic_errors_and_warnings() {
        let validation = validate_composer_manifest(
            r#"{
                "name":"vendor/package",
                "description":"test",
                "license":"MIT",
                "require":{"vendor/package":"*","other/package":"not a constraint"},
                "autoload":{"psr-4":{"Vendor":"src"}}
            }"#,
            "./composer.json",
            ManifestValidationOptions::default(),
        );

        assert!(validation
            .errors
            .iter()
            .any(|error| error.contains("cannot set a require on itself")));
        assert!(validation
            .errors
            .iter()
            .any(|error| error.contains("invalid version constraint")));
        assert!(validation
            .errors
            .iter()
            .any(|error| error.contains("namespaces must end")));
    }

    #[test]
    fn no_check_flags_suppress_constraint_and_version_warnings() {
        let validation = validate_composer_manifest(
            r#"{
                "name":"vendor/package",
                "description":"test",
                "license":"MIT",
                "version":"1.0.0",
                "require":{"other/package":"1.0.0"}
            }"#,
            "./composer.json",
            ManifestValidationOptions {
                check_constraints: false,
                check_version: false,
                check_publish: true,
            },
        );

        assert!(validation.warnings.is_empty());
    }

    #[test]
    fn no_publish_check_skips_strict_schema_validation() {
        let validation = validate_composer_manifest(
            r#"{"require":{"vendor/package":"^1.0"}}"#,
            "./composer.json",
            ManifestValidationOptions {
                check_publish: false,
                ..ManifestValidationOptions::default()
            },
        );

        assert!(validation.errors.is_empty());
        assert!(validation.publish_errors.is_empty());
    }

    #[test]
    fn reports_nested_duplicate_keys_with_their_line() {
        let validation = validate_composer_manifest(
            "{\n  \"name\": \"vendor/package\",\n  \"extra\": {\"key\": 1,\n    \"key\": 2}\n}",
            "./composer.json",
            ManifestValidationOptions::default(),
        );

        assert!(validation
            .warnings
            .iter()
            .any(|warning| { warning == "Key key is a duplicate in ./composer.json at line 4" }));
    }

    // Ported from Composer\Test\Json\ComposerSchemaTest::testNamePattern.
    #[test]
    fn composer_schema_rejects_invalid_package_name_patterns() {
        for name in ["vendor/-pack__age", "Vendor/Package"] {
            let validation = schema_validation(&format!(
                r#"{{"name":"{name}","description":"description"}}"#
            ));
            assert!(
                validation.errors.iter().any(|error| error.contains("name")),
                "expected invalid name {name:?}, got {validation:?}"
            );
        }
    }

    // Ported from Composer\Test\Json\ComposerSchemaTest::testVersionPattern.
    #[test]
    fn composer_schema_version_pattern_data_provider() {
        let valid = [
            "1.0.0",
            "1.0.2",
            "1.1.0",
            "1.0.0-dev",
            "1.0.0-Alpha",
            "1.0.0-ALPHA",
            "1.0.0-alphA",
            "1.0.0-alpha3",
            "1.0.0-Alpha3",
            "1.0.0-ALPHA3",
            "1.0.0-Beta",
            "1.0.0-BETA",
            "1.0.0-betA",
            "1.0.0-beta232",
            "1.0.0-Beta232",
            "1.0.0-BETA232",
            "10.4.13beta.2",
            "1.0.0.RC.15-dev",
            "1.0.0-RC",
            "v2.0.4-p",
            "dev-master",
            "0.2.5.4",
            "12345678-123456",
            "20100102-203040-p1",
            "2010-01-02.5",
            "0.2.5.4-rc.2",
            "dev-feature+issue-1",
            "1.0.0-alpha.3.1+foo/-bar",
            "00.01.03.04",
            "041.x-dev",
            "dev-foo bar",
        ];
        let invalid = ["invalid", "1.0be", "1.0.0-meh", "feature-foo", "1.0 .2"];

        for version in valid {
            let validation = schema_validation(&format!(
                r#"{{"name":"vendor/package","description":"description","version":"{version}"}}"#
            ));
            assert!(
                validation.errors.is_empty(),
                "expected valid version {version:?}, got {validation:?}"
            );
        }
        for version in invalid {
            let validation = schema_validation(&format!(
                r#"{{"name":"vendor/package","description":"description","version":"{version}"}}"#
            ));
            assert!(
                validation
                    .errors
                    .iter()
                    .any(|error| error.contains("version")),
                "expected invalid version {version:?}, got {validation:?}"
            );
        }
    }

    // Ported from Composer\Test\Json\ComposerSchemaTest::testOptionalAbandonedProperty.
    #[test]
    fn composer_schema_accepts_boolean_abandoned_property() {
        let validation = schema_validation(
            r#"{"name":"vendor/package","description":"description","abandoned":true}"#,
        );
        assert!(validation.errors.is_empty(), "{validation:?}");
    }

    // Ported from Composer\Test\Json\ComposerSchemaTest::testRequireTypes.
    #[test]
    fn composer_schema_requires_string_dependency_constraints() {
        let validation = schema_validation(
            r#"{"name":"vendor/package","description":"description","require":{"a":["b"]}}"#,
        );
        assert!(validation
            .errors
            .iter()
            .any(|error| error.contains("require.a") && error.contains("string")));
    }

    // Ported from Composer\Test\Json\ComposerSchemaTest::testMinimumStabilityValues.
    #[test]
    fn composer_schema_minimum_stability_data_provider() {
        for stability in ["dev", "alpha", "beta", "rc", "RC", "stable"] {
            let validation = schema_validation(&format!(
                r#"{{"name":"vendor/package","description":"description","minimum-stability":"{stability}"}}"#
            ));
            assert!(
                validation.errors.is_empty(),
                "expected valid stability {stability:?}, got {validation:?}"
            );
        }
        for stability in ["", "dummy", "devz"] {
            let validation = schema_validation(&format!(
                r#"{{"name":"vendor/package","description":"description","minimum-stability":"{stability}"}}"#
            ));
            assert!(
                validation
                    .errors
                    .iter()
                    .any(|error| error.contains("minimum-stability")),
                "expected invalid stability {stability:?}, got {validation:?}"
            );
        }
    }

    // Ported from Composer\Test\Json\ComposerSchemaTest::testReservedPolicyCustomListNamesAreRejected.
    #[test]
    fn composer_schema_rejects_reserved_custom_policy_names() {
        for list_name in [
            "ignore-foo",
            "ignoremalware",
            "package",
            "packages",
            "license",
            "licence",
            "licenses",
            "licences",
            "support",
            "maintenance",
            "security",
            "minimum-release-age",
        ] {
            let validation = schema_validation(&format!(
                r#"{{"name":"vendor/package","description":"description","config":{{"policy":{{"{list_name}":{{"block":true}}}}}}}}"#
            ));
            assert!(
                !validation.errors.is_empty(),
                "expected reserved policy {list_name:?} to fail"
            );
        }
    }

    // Ported from Composer\Test\Json\ComposerSchemaTest::testRegularPolicyCustomListNameIsAccepted.
    #[test]
    fn composer_schema_accepts_regular_custom_policy_names() {
        let validation = schema_validation(
            r#"{"name":"vendor/package","description":"description","config":{"policy":{"company-policy":{"block":true}}}}"#,
        );
        assert!(validation.errors.is_empty(), "{validation:?}");
    }

    // Ported from Composer\Test\Json\ComposerSchemaTest::testIgnoreUnreachablePolicyKeyIsAccepted.
    #[test]
    fn composer_schema_accepts_ignore_unreachable_policy_key() {
        let validation = schema_validation(
            r#"{"name":"vendor/package","description":"description","config":{"policy":{"ignore-unreachable":true}}}"#,
        );
        assert!(validation.errors.is_empty(), "{validation:?}");
    }

    #[test]
    fn composer_policy_ignore_rejects_unsupported_rule_shapes() {
        for ignore in [
            serde_json::json!([{"package": "vendor/foo", "constraint": "^1.0"}]),
            serde_json::json!([true]),
            serde_json::json!({"vendor/foo": true}),
            serde_json::json!([42]),
        ] {
            let validation = schema_validation(
                &serde_json::json!({
                    "name": "vendor/package",
                    "description": "description",
                    "config": {"policy": {"company-policy": {"ignore": ignore}}}
                })
                .to_string(),
            );
            assert!(
                !validation.errors.is_empty(),
                "expected invalid ignore config to fail: {ignore}"
            );
        }
    }

    #[test]
    fn composer_array_loader_rejects_invalid_link_constraints() {
        let validation = schema_validation(
            r#"{"name":"plugin/package","require":{"composer-plugin-api":"^^^"}}"#,
        );

        assert!(validation.errors.iter().any(|error| {
            error.contains("require.composer-plugin-api")
                && error.contains("invalid version constraint")
        }));
    }

    // Ported from Composer\Test\Json\JsonFileTest::testSchemaValidation.
    #[test]
    fn composer_schema_strict_and_lax_validation_accept_valid_manifest() {
        let validation = validate_composer_manifest(
            r#"{"name":"vendor/package","description":"description"}"#,
            "./composer.json",
            ManifestValidationOptions::default(),
        );
        assert!(validation.errors.is_empty(), "{validation:?}");
        assert!(validation.publish_errors.is_empty(), "{validation:?}");
    }

    // Ported from Composer\Test\Json\JsonFileTest::testSchemaValidationError.
    #[test]
    fn composer_schema_lax_validation_rejects_null_name() {
        let validation = schema_validation(r#"{"name":null}"#);
        assert!(validation
            .errors
            .iter()
            .any(|error| error.contains("name") && error.contains("null")));
    }

    // Ported from Composer\Test\Json\JsonFileTest::testSchemaValidationLaxAdditionalProperties.
    #[test]
    fn composer_schema_lax_allows_unknown_properties_while_strict_rejects_them() {
        let validation = validate_composer_manifest(
            r#"{"name":"vendor/package","description":"description","foo":"bar"}"#,
            "./composer.json",
            ManifestValidationOptions::default(),
        );
        assert!(validation.errors.is_empty(), "{validation:?}");
        assert!(validation
            .publish_errors
            .iter()
            .any(|error| error.contains("foo") && error.contains("not defined")));
    }

    // Ported from Composer\Test\Json\JsonFileTest::testSchemaValidationLaxRequired.
    #[test]
    fn composer_schema_lax_allows_missing_publish_fields_while_strict_requires_them() {
        for (json, required) in [
            ("{}", vec!["name", "description"]),
            (r#"{"name":"vendor/package"}"#, vec!["description"]),
            (r#"{"description":"description"}"#, vec!["name"]),
        ] {
            let validation = validate_composer_manifest(
                json,
                "./composer.json",
                ManifestValidationOptions::default(),
            );
            assert!(validation.errors.is_empty(), "{validation:?}");
            for field in required {
                assert!(validation
                    .publish_errors
                    .iter()
                    .any(|error| error.contains(field) && error.contains("required")));
            }
        }
    }

    // Ported from Composer\Test\Package\Loader\ValidatingArrayLoaderTest::testLoadSuccess.
    #[test]
    fn composer_validating_array_loader_accepts_success_provider() {
        let manifests = [
            serde_json::json!({"name": "foo/bar"}),
            serde_json::json!({"name": "foo/bar--baz", "bin": "bin1"}),
            serde_json::json!({
                "name": "foo/bar",
                "version": "1.0.0",
                "type": "library",
                "keywords": ["a", "b_c", "D E", "微信"],
                "homepage": "https://foo.com",
                "license": ["MIT", "WTFPL"],
                "authors": [{"name": "Alice", "email": "alice@example.org"}],
                "require": {"a/b": "1.*", "composer-runtime-api": "*"},
                "autoload": {"psr-0": {"Foo\\Bar": "src/"}, "files": ["functions.php"]},
                "extra": {"branch-alias": {"dev-master": "2.0-dev"}},
                "bin": ["bin/foo", "bin/bar"]
            }),
            serde_json::json!({
                "name": "foo/bar",
                "source": {"url": "https://example.org", "reference": 1234, "type": "git"},
                "dist": {"url": "https://example.org", "reference": "foobar", "type": "zip"}
            }),
            serde_json::json!({
                "name": "foo/bar",
                "type": "php-ext",
                "php-ext": {
                    "extension-name": "ext-xdebug",
                    "priority": 80,
                    "support-zts": true,
                    "download-url-method": ["pre-packaged-binary", "composer-default"],
                    "os-families": ["linux", "darwin"]
                }
            }),
        ];

        for manifest in manifests {
            let validation = validating_loader_validation(manifest.clone());
            assert!(
                validation.errors.is_empty(),
                "expected manifest to load: {manifest}; errors={:?}",
                validation.errors
            );
        }
    }

    // Ported from Composer\Test\Package\Loader\ValidatingArrayLoaderTest::
    // testLoadFailureThrowsException.
    #[test]
    fn composer_validating_array_loader_rejects_error_provider() {
        let manifests = [
            serde_json::json!({"name": "foo"}),
            serde_json::json!({"name": "foo/bar.json"}),
            serde_json::json!({"name": "com1/foo"}),
            serde_json::json!({"name": "Foo/Bar"}),
            serde_json::json!({"name": "foo/bar", "homepage": 43}),
            serde_json::json!({"name": "foo/bar", "autoload": "strings"}),
            serde_json::json!({"name": "foo/bar", "autoload": {"psr0": {"foo": "src"}}}),
            serde_json::json!({"name": "foo/bar", "require": {"foo/Bar": "1.*"}}),
            serde_json::json!({"name": "foo/bar", "bin": ["bin/foo", "../../../../etc/evil"]}),
            serde_json::json!({"name": "foo/bar", "replace": ["acme/bar"]}),
            serde_json::json!({"require": {"acme/bar": "^1.0"}}),
            serde_json::json!({"name": "foo/bar", "type": "php-ext", "php-ext": {"priority": "invalid"}}),
        ];

        for manifest in manifests {
            let validation = validating_loader_validation(manifest.clone());
            assert!(
                !validation.errors.is_empty(),
                "expected manifest to fail: {manifest}"
            );
        }
    }

    // Ported from Composer\Test\Package\Loader\ValidatingArrayLoaderTest::testLoadWarnings.
    #[test]
    fn composer_validating_array_loader_reports_warning_provider() {
        let cases = [
            (
                serde_json::json!({"name": "foo/bar", "homepage": "foo:bar"}),
                "homepage : invalid value (foo:bar), must be an http/https URL",
            ),
            (
                serde_json::json!({"name": "foo/bar", "support": {"source": "foo:bar"}}),
                "support.source : invalid value (foo:bar), must be an http/https URL",
            ),
            (
                serde_json::json!({"name": "foo/bar", "require": {"foo/baz": "*"}}),
                "require.foo/baz : unbound version constraints (*) should be avoided",
            ),
            (
                serde_json::json!({"name": "foo/bar", "require": {"bar/woo": "1.0.0"}}),
                "require.bar/woo : exact version constraints (1.0.0) should be avoided if the package follows semantic versioning",
            ),
            (
                serde_json::json!({"name": "foo/bar", "require": {"foo/baz": ">1, <0.5"}}),
                "require.foo/baz : this version constraint cannot possibly match anything (>1, <0.5)",
            ),
            (
                serde_json::json!({"name": "foo/bar", "extra": {"branch-alias": {"5.x-dev": "3.1.x-dev"}}}),
                "extra.branch-alias.5.x-dev : the target branch (3.1.x-dev) is not a valid numeric alias for this version",
            ),
            (
                serde_json::json!({"name": "foo/bar", "require": {"Foo/Baz": "^1.0"}}),
                "require.Foo/Baz is invalid, it should not contain uppercase characters. Please use foo/baz instead.",
            ),
            (
                serde_json::json!({"name": "a/b", "license": "XXXXX"}),
                "License \"XXXXX\" is not a valid SPDX license identifier, see https://spdx.org/licenses/ if you use an open license.\nIf the software is closed-source, you may use \"proprietary\" as license.",
            ),
        ];

        for (manifest, expected) in cases {
            let validation = validating_loader_validation(manifest.clone());
            assert!(
                validation
                    .warnings
                    .iter()
                    .any(|warning| warning == expected),
                "missing warning {expected:?} for {manifest}; got {:?}",
                validation.warnings
            );
        }
    }

    // Ported from Composer\Test\Package\Loader\ValidatingArrayLoaderTest::
    // testLoadSkipsWarningDataWhenIgnoringErrors.
    #[test]
    fn composer_validating_array_loader_sanitizes_warning_data_before_loading() {
        let mut scalar = serde_json::json!({"name": "a/b", "license": "XXXXX"});
        sanitize_package_manifest(&mut scalar);
        assert_eq!(scalar["license"], serde_json::json!(["XXXXX"]));

        let mut mixed = serde_json::json!({"name": "a/b", "license": [{"author": "bar"}, "MIT"]});
        sanitize_package_manifest(&mut mixed);
        assert_eq!(mixed["license"], serde_json::json!(["MIT"]));
    }
}
