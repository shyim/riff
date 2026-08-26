use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use super::schema::RiffManifest;

/// Errors that can occur when loading composer.json
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("Failed to read file: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to parse JSON: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("Validation error: {0}")]
    Validation(String),
}

/// Load and parse a composer.json file
pub fn load_manifest(path: &Path) -> Result<RiffManifest, LoadError> {
    let content = fs::read_to_string(path)?;
    parse_manifest(&content)
}

/// Parse composer.json from a string
pub fn parse_manifest(content: &str) -> Result<RiffManifest, LoadError> {
    let json: RiffManifest = serde_json::from_str(content)?;
    Ok(json)
}

/// Validate a composer.json structure
pub fn validate_manifest(json: &RiffManifest) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();

    // Name validation
    if let Some(ref name) = json.name {
        if !is_valid_package_name(name) {
            errors.push(format!(
                "Invalid package name '{}'. Must be lowercase and match vendor/package format",
                name
            ));
        }
    }

    // Version validation (if specified)
    if let Some(ref version) = json.version {
        if version.is_empty() {
            errors.push("Version cannot be empty if specified".to_string());
        }
    }

    // Minimum stability validation
    let valid_stabilities = ["dev", "alpha", "beta", "rc", "stable"];
    if let Some(ref min_stability) = json.minimum_stability {
        if !valid_stabilities.contains(&min_stability.to_lowercase().as_str()) {
            errors.push(format!(
                "Invalid minimum-stability '{}'. Must be one of: {:?}",
                min_stability, valid_stabilities
            ));
        }
    }

    // Type validation
    let valid_types = ["library", "project", "metapackage", "composer-plugin"];
    if !valid_types.contains(&json.package_type.to_lowercase().as_str())
        && !json.package_type.starts_with("library")
    {
        // Allow custom types but warn (not an error)
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Check if a package name is valid
fn is_valid_package_name(name: &str) -> bool {
    static PACKAGE_NAME: OnceLock<Regex> = OnceLock::new();
    PACKAGE_NAME
        .get_or_init(|| {
            Regex::new(r"^[a-z0-9]([_.-]?[a-z0-9]+)*/[a-z0-9](([_.]|-{1,2})?[a-z0-9]+)*$")
                .expect("Composer package-name regex must compile")
        })
        .is_match(name)
}

/// Write composer.json to a file
pub fn write_manifest(path: &Path, json: &RiffManifest) -> Result<(), LoadError> {
    write_json_value(path, json, false)
}

/// Encode arbitrary JSON with Composer-style pretty formatting and a final newline.
pub fn encode_pretty_json<T: serde::Serialize + ?Sized>(
    value: &T,
    indent: &[u8],
) -> Result<String, serde_json::Error> {
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent);
    let mut serializer = serde_json::Serializer::with_formatter(Vec::new(), formatter);
    value.serialize(&mut serializer)?;
    let mut content = String::from_utf8(serializer.into_inner())
        .expect("serializing JSON into a byte buffer always produces UTF-8");
    content.push('\n');
    Ok(content)
}

/// Write arbitrary JSON, optionally preserving indentation from an existing file.
pub fn write_json_value<T: serde::Serialize + ?Sized>(
    path: &Path,
    value: &T,
    preserve_existing_indent: bool,
) -> Result<(), LoadError> {
    let existing = preserve_existing_indent
        .then(|| fs::read_to_string(path).ok())
        .flatten();
    let indent = existing
        .as_deref()
        .and_then(detect_json_indent)
        .unwrap_or(b"    ");
    fs::write(path, encode_pretty_json(value, indent)?)?;
    Ok(())
}

fn detect_json_indent(content: &str) -> Option<&[u8]> {
    content.lines().find_map(|line| {
        let indent_length = line
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        (indent_length > 0 && line.as_bytes().get(indent_length) == Some(&b'"'))
            .then(|| &line.as_bytes()[..indent_length])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::ScriptValue;

    #[test]
    fn test_parse_minimal() {
        let json = r#"{
            "name": "vendor/package",
            "require": {
                "php": ">=8.0"
            }
        }"#;

        let result = parse_manifest(json).unwrap();
        assert_eq!(result.name, Some("vendor/package".to_string()));
        assert_eq!(result.require.get("php"), Some(&">=8.0".to_string()));
    }

    #[test]
    fn test_parse_full() {
        let json = r#"{
            "name": "vendor/package",
            "description": "A test package",
            "type": "library",
            "license": "MIT",
            "authors": [
                {
                    "name": "John Doe",
                    "email": "john@example.com"
                }
            ],
            "require": {
                "php": ">=8.0",
                "vendor/other": "^1.0"
            },
            "require-dev": {
                "phpunit/phpunit": "^10.0"
            },
            "autoload": {
                "psr-4": {
                    "Vendor\\Package\\": "src/"
                }
            }
        }"#;

        let result = parse_manifest(json).unwrap();
        assert_eq!(result.name, Some("vendor/package".to_string()));
        assert_eq!(result.description, Some("A test package".to_string()));
        assert_eq!(result.package_type, "library");
        assert_eq!(result.authors.len(), 1);
        assert_eq!(result.require.len(), 2);
        assert_eq!(result.require_dev.len(), 1);
    }

    #[test]
    fn parses_and_preserves_plugin_owned_object_scripts() {
        let json = r#"{
            "scripts": {
                "auto-scripts": {
                    "cache:clear": "symfony-cmd",
                    "-r \"@rename('.env.local.demo', '.env.local');\"": "php-script"
                },
                "post-install-cmd": ["@auto-scripts"]
            }
        }"#;

        let manifest = parse_manifest(json).unwrap();
        let ScriptValue::Object(auto_scripts) = &manifest.scripts.custom["auto-scripts"] else {
            panic!("auto-scripts should remain plugin-owned object configuration");
        };
        assert_eq!(auto_scripts["cache:clear"], "symfony-cmd");
        assert_eq!(
            auto_scripts["-r \"@rename('.env.local.demo', '.env.local');\""],
            "php-script"
        );
        assert!(manifest.scripts.custom["auto-scripts"].as_vec().is_empty());

        let encoded = serde_json::to_value(&manifest).unwrap();
        assert_eq!(
            encoded["scripts"]["auto-scripts"]["cache:clear"],
            "symfony-cmd"
        );
    }

    #[test]
    fn test_valid_package_name() {
        assert!(is_valid_package_name("vendor/package"));
        assert!(is_valid_package_name("my-vendor/my-package"));
        assert!(is_valid_package_name("vendor123/package456"));
        assert!(is_valid_package_name("vendor/package--name"));
        assert!(!is_valid_package_name("invalid"));
        assert!(!is_valid_package_name("Invalid/Package"));
        assert!(!is_valid_package_name("/package"));
        assert!(!is_valid_package_name("vendor/"));
        assert!(!is_valid_package_name("vendor/-pack__age"));
    }

    // Ported from Composer\Test\Json\JsonFileTest parse-error contracts.
    #[test]
    fn composer_json_parser_rejects_malformed_syntax() {
        let malformed = [
            r#"{"foo":"bar",}"#,
            r#"{"foo":["bar",]}"#,
            "{\"fo\\o\":\"bar\"}",
            "{\"fo\\\\\\\\o\":\"bar\" \"a\":\"b\"}",
            r#"{'foo':"bar"}"#,
            r#"{foo:"bar"}"#,
            r#"{"foo":["bar":"baz"]}"#,
            r#"{"foo":"bar" "bar":"foo"}"#,
            "{\n\"foo\": \"barbar\"\n\n\"bar\": \"foo\"\n}",
            r#"{"foo":"bar","bar" "foo"}"#,
        ];

        for json in malformed {
            let error = parse_manifest(json).unwrap_err();
            assert!(
                matches!(error, LoadError::Parse(_)),
                "unexpected error: {error}"
            );
        }
    }

    // Ported from Composer\Test\Json\JsonFileTest escaping and Unicode contracts.
    #[test]
    fn composer_json_round_trip_preserves_backslashes_quotes_and_unicode() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("composer.json");
        let mut manifest = RiffManifest {
            name: Some("vendor/package".to_string()),
            description: Some("Žluťoučký \" kůň úpěl ďábelské ódy za €".to_string()),
            ..Default::default()
        };
        manifest
            .require
            .insert("Metadata\\".to_string(), "src/".to_string());
        manifest
            .require
            .insert("Metadata\\\"".to_string(), "src\\path/".to_string());
        manifest.require.insert(
            "Žluťoučký \" kůň".to_string(),
            "úpěl ďábelské ódy za €".to_string(),
        );

        write_manifest(&path, &manifest).unwrap();
        let encoded = std::fs::read_to_string(&path).unwrap();
        assert!(encoded.contains(r#""Metadata\\": "src/""#));
        assert!(encoded.contains(r#""Metadata\\\"": "src\\path/""#));
        assert!(encoded.contains("Žluťoučký \\\" kůň úpěl ďábelské ódy za €"));
        assert!(encoded.contains(r#""Žluťoučký \" kůň": "úpěl ďábelské ódy za €""#));

        let decoded = load_manifest(&path).unwrap();
        assert_eq!(decoded.description, manifest.description);
        assert_eq!(decoded.require, manifest.require);
    }

    #[test]
    fn composer_json_formatter_unescapes_unicode_after_a_literal_backslash() {
        let value: serde_json::Value = serde_json::from_str(r#""\\\\\u0119""#).unwrap();

        assert_eq!(serde_json::to_string(&value).unwrap(), r#""\\\\ę""#);
    }

    // Ported from Composer\Test\Json\JsonFileTest::testDoubleEscapedUnicode.
    #[test]
    fn composer_json_round_trip_does_not_interpret_literal_unicode_escapes_twice() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("composer.json");
        let manifest = RiffManifest {
            name: Some("vendor/package".to_string()),
            description: Some(r"Zdjęcia hjkjhl\u0119kkjk".to_string()),
            ..Default::default()
        };

        write_manifest(&path, &manifest).unwrap();
        assert_eq!(
            load_manifest(&path).unwrap().description,
            manifest.description
        );
    }

    // Ported from Composer\Test\Json\JsonFileTest::testSimpleJsonString and
    // testOverwritesIndentationByDefault.
    #[test]
    fn composer_json_writer_uses_four_space_indentation_and_a_final_newline() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("composer.json");
        let manifest = RiffManifest {
            name: Some("composer/composer".to_string()),
            ..Default::default()
        };

        write_manifest(&path, &manifest).unwrap();

        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "{\n    \"name\": \"composer/composer\"\n}\n"
        );
    }

    #[test]
    fn composer_json_writer_distinguishes_empty_arrays_and_objects() {
        let value = serde_json::json!({"test": [], "test2": {}});
        assert_eq!(
            encode_pretty_json(&value, b"    ").unwrap(),
            "{\n    \"test\": [],\n    \"test2\": {}\n}\n"
        );
    }

    #[test]
    fn composer_json_writer_preserves_detected_indentation_after_read() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("composer.json");
        fs::write(&path, "{\n\t\"foo\": \"bar\"\n}\n").unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let mut value = value.as_object().unwrap().clone();
        value.insert("foo".to_string(), serde_json::json!("baz"));

        write_json_value(&path, &value, true).unwrap();

        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "{\n\t\"foo\": \"baz\"\n}\n"
        );
    }

    #[test]
    fn test_validate_manifest() {
        let mut json = RiffManifest {
            name: Some("vendor/package".to_string()),
            minimum_stability: Some("stable".to_string()),
            ..RiffManifest::default()
        };

        assert!(validate_manifest(&json).is_ok());

        json.name = Some("InvalidName".to_string());
        assert!(validate_manifest(&json).is_err());
    }

    #[test]
    fn test_branch_aliases() {
        let json = r#"{
            "name": "vendor/package",
            "extra": {
                "branch-alias": {
                    "dev-main": "1.0.x-dev",
                    "dev-2.x": "2.0.x-dev"
                }
            }
        }"#;

        let result = parse_manifest(json).unwrap();
        let aliases = result.get_branch_aliases();

        // Should have parsed branch aliases
        assert!(!aliases.is_empty());
    }

    #[test]
    fn test_inline_alias() {
        // Test inline alias parsing from RiffManifest helper
        let result = RiffManifest::get_inline_alias("dev-main as 1.0.0");
        assert!(result.is_some());
        let (actual, alias) = result.unwrap();
        assert_eq!(actual, "dev-main");
        assert_eq!(alias, "1.0.0");

        // Test regular constraint (no alias)
        let result = RiffManifest::get_inline_alias("^1.0");
        assert!(result.is_none());
    }
}
