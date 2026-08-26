use std::fs;
use std::path::Path;
use std::process::Output;

use assert_cmd::Command;
use serde_json::{json, Value};

fn command(project: &Path) -> Command {
    let mut command = Command::cargo_bin("riff").unwrap();
    command
        .arg("init")
        .env("COMPOSER_DEFAULT_AUTHOR", "John Smith")
        .env("COMPOSER_DEFAULT_EMAIL", "john@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("-d")
        .arg(project);
    command
}

fn run_non_interactive(project: &Path, arguments: &[&str]) -> Output {
    command(project)
        .arg("--no-interaction")
        .args(arguments)
        .output()
        .unwrap()
}

fn manifest(project: &Path) -> Value {
    serde_json::from_slice(&fs::read(project.join("composer.json")).unwrap()).unwrap()
}

fn assert_manifest(arguments: &[&str], expected: Value) {
    let project = tempfile::tempdir().unwrap();
    let output = run_non_interactive(project.path(), arguments);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(manifest(project.path()), expected);
}

fn default_author() -> Value {
    json!([{"name": "John Smith", "email": "john@example.com"}])
}

#[test]
fn composer_init_command_builds_non_interactive_manifest_variants() {
    assert_manifest(
        &["--name", "test/pkg"],
        json!({"name": "test/pkg", "authors": default_author(), "require": {}}),
    );
    assert_manifest(
        &[
            "--name",
            "test/pkg",
            "--author",
            "Mr. Test <test@example.org>",
        ],
        json!({
            "name": "test/pkg",
            "authors": [{"name": "Mr. Test", "email": "test@example.org"}],
            "require": {},
        }),
    );
    assert_manifest(
        &["--name", "test/pkg", "--author", "Mr. Test"],
        json!({
            "name": "test/pkg",
            "authors": [{"name": "Mr. Test"}],
            "require": {},
        }),
    );
    assert_manifest(
        &[
            "--name",
            "test/pkg",
            "--repository",
            r#"{"type":"vcs","url":"http://packages.example.com"}"#,
        ],
        json!({
            "name": "test/pkg",
            "authors": default_author(),
            "require": {},
            "repositories": [{"type": "vcs", "url": "http://packages.example.com"}],
        }),
    );
    assert_manifest(
        &[
            "--name",
            "test/pkg",
            "--repository",
            r#"{"type":"vcs","url":"http://vcs.example.com"}"#,
            "--repository",
            r#"{"type":"composer","url":"http://composer.example.com"}"#,
            "--repository",
            r#"{"type":"composer","url":"http://composer2.example.com","options":{"ssl":{"verify_peer":"true"}}}"#,
        ],
        json!({
            "name": "test/pkg",
            "authors": default_author(),
            "require": {},
            "repositories": [
                {"type": "vcs", "url": "http://vcs.example.com"},
                {"type": "composer", "url": "http://composer.example.com"},
                {"type": "composer", "url": "http://composer2.example.com", "options": {"ssl": {"verify_peer": "true"}}},
            ],
        }),
    );
    assert_manifest(
        &["--name", "test/pkg", "--stability", "dev"],
        json!({
            "name": "test/pkg",
            "authors": default_author(),
            "require": {},
            "minimum-stability": "dev",
        }),
    );
    assert_manifest(
        &["--name", "test/pkg", "--require", "first/pkg:1.0.0"],
        json!({
            "name": "test/pkg",
            "authors": default_author(),
            "require": {"first/pkg": "1.0.0"},
        }),
    );
    assert_manifest(
        &[
            "--name",
            "test/pkg",
            "--require",
            "first/pkg:1.0.0",
            "--require",
            "second/pkg:^3.4",
        ],
        json!({
            "name": "test/pkg",
            "authors": default_author(),
            "require": {"first/pkg": "1.0.0", "second/pkg": "^3.4"},
        }),
    );
    assert_manifest(
        &["--name", "test/pkg", "--require-dev", "first/pkg:1.0.0"],
        json!({
            "name": "test/pkg",
            "authors": default_author(),
            "require": {},
            "require-dev": {"first/pkg": "1.0.0"},
        }),
    );
    assert_manifest(
        &[
            "--name",
            "test/pkg",
            "--require-dev",
            "first/pkg:1.0.0",
            "--require-dev",
            "second/pkg:^3.4",
        ],
        json!({
            "name": "test/pkg",
            "authors": default_author(),
            "require": {},
            "require-dev": {"first/pkg": "1.0.0", "second/pkg": "^3.4"},
        }),
    );
    assert_manifest(
        &["--name", "test/pkg", "--autoload", "testMapping/"],
        json!({
            "name": "test/pkg",
            "authors": default_author(),
            "require": {},
            "autoload": {"psr-4": {"Test\\Pkg\\": "testMapping/"}},
        }),
    );
    assert_manifest(
        &["--name", "test/pkg", "--homepage", "https://example.org/"],
        json!({
            "name": "test/pkg",
            "homepage": "https://example.org/",
            "authors": default_author(),
            "require": {},
        }),
    );
    assert_manifest(
        &[
            "--name",
            "test/pkg",
            "--description",
            "My first example package",
        ],
        json!({
            "name": "test/pkg",
            "description": "My first example package",
            "authors": default_author(),
            "require": {},
        }),
    );
    assert_manifest(
        &["--name", "test/pkg", "--type", "project"],
        json!({
            "name": "test/pkg",
            "type": "project",
            "authors": default_author(),
            "require": {},
        }),
    );
    assert_manifest(
        &["--name", "test/pkg", "--license", "MIT"],
        json!({
            "name": "test/pkg",
            "license": "MIT",
            "authors": default_author(),
            "require": {},
        }),
    );
}

#[test]
fn composer_init_command_rejects_invalid_options() {
    let cases = [
        (vec!["--name", "test"], "package name test is invalid"),
        (
            vec!["--name", "test/pkg", "--author", "Mr. Test <test>"],
            "Invalid email \"test\"",
        ),
        (
            vec!["--name", "test/pkg", "--stability", "bogus"],
            "minimum-stability: Does not have a value in the enumeration",
        ),
        (
            vec!["--name", "test/pkg", "--require", "first"],
            "Option first is missing a version constraint, use e.g. first:^1.0",
        ),
        (
            vec!["--name", "test/pkg", "--require-dev", "first"],
            "Option first is missing a version constraint, use e.g. first:^1.0",
        ),
        (
            vec!["--name", "test/pkg", "--homepage", "not-a-url"],
            "homepage: Invalid URL format",
        ),
    ];

    for (arguments, expected) in cases {
        let project = tempfile::tempdir().unwrap();
        let output = run_non_interactive(project.path(), &arguments);
        assert!(!output.status.success(), "{arguments:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!project.path().join("composer.json").exists());
    }
}

#[test]
fn composer_init_command_sanitizes_the_directory_name() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("_foo_--bar__baz.--..qux__");
    fs::create_dir(&project).unwrap();

    let output = command(&project)
        .arg("--no-interaction")
        .env("COMPOSER_DEFAULT_VENDOR", ".vendorName")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        manifest(&project),
        json!({
            "name": "vendor-name/foo-bar_baz.qux",
            "authors": default_author(),
            "require": {},
        })
    );
}

#[test]
fn composer_init_command_supports_interactive_generation() {
    let project = tempfile::tempdir().unwrap();
    let mut command = command(project.path());
    let assert = command
        .write_stdin(concat!(
            "vendor/pkg\n",
            "my description\n",
            "Mr. Test <test@example.org>\n",
            "stable\n",
            "library\n",
            "AGPL-3.0-only\n",
            "no\n",
            "no\n",
            "n\n",
            "\n",
        ))
        .assert();
    assert.success();
    assert_eq!(
        manifest(project.path()),
        json!({
            "name": "vendor/pkg",
            "description": "my description",
            "type": "library",
            "license": "AGPL-3.0-only",
            "authors": [{"name": "Mr. Test", "email": "test@example.org"}],
            "minimum-stability": "stable",
            "require": {},
        })
    );
}
