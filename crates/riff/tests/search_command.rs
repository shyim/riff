use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;

fn search_project() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("composer.json"),
        serde_json::to_vec(&json!({
            "name": "fixture/project",
            "repositories": [
                {"packagist.org": false},
                {
                    "type": "package",
                    "package": [
                        {"name": "vendor-1/package-1", "description": "generic description", "version": "1.0.0"},
                        {"name": "foo/bar", "description": "generic description", "version": "1.0.0"},
                        {"name": "bar/baz", "description": "fancy baz", "version": "1.0.0", "abandoned": true},
                        {"name": "vendor-2/fancy-package", "version": "1.0.0", "type": "foo"}
                    ]
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    project
}

fn run(project: &Path, arguments: &[&str]) -> std::process::Output {
    let cache = project.join("riff-cache");
    Command::cargo_bin("riff")
        .unwrap()
        .env("RIFF_CACHE_DIR", cache)
        .arg("search")
        .args(arguments)
        .args(["-d"])
        .arg(project)
        .output()
        .unwrap()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

// Ported from Composer\Test\Command\SearchCommandTest::testSearch.
#[test]
fn composer_search_command_supports_modes_types_and_json() {
    let project = search_project();
    let cases: &[(&[&str], &[&str])] = &[
        (&["fancy"], &["bar/baz", "vendor-2/fancy-package"]),
        (
            &["fancy", "vendor"],
            &["vendor-1/package-1", "bar/baz", "vendor-2/fancy-package"],
        ),
        (&["fancy", "--only-name"], &["vendor-2/fancy-package"]),
        (&["bar", "--only-vendor"], &["bar"]),
        (&["vendor", "--type", "foo"], &["vendor-2/fancy-package"]),
    ];
    for (arguments, expected) in cases {
        let output = run(project.path(), arguments);
        assert!(
            output.status.success(),
            "search {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
        let actual = stdout(&output);
        for name in *expected {
            assert!(actual.contains(name), "missing {name} in {actual}");
        }
        if arguments.contains(&"--only-name") {
            assert!(!actual.contains("bar/baz"));
        }
    }

    let output = run(project.path(), &["vendor-2/fancy", "--format", "json"]);
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value,
        json!([{"name": "vendor-2/fancy-package", "description": null}])
    );

    let output = run(project.path(), &["invalid-package-name"]);
    assert!(output.status.success());
    assert!(stdout(&output).is_empty());
}

// Ported from Composer\Test\Command\SearchCommandTest::testInvalidFormat.
#[test]
fn composer_search_command_rejects_invalid_format() {
    let project = search_project();
    let output = run(project.path(), &["test", "--format", "test-format"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("Unsupported format \"test-format\". See help for supported formats."));
}

// Ported from Composer\Test\Command\SearchCommandTest::testInvalidFlags.
#[test]
fn composer_search_command_rejects_mutually_exclusive_modes() {
    let project = search_project();
    let output = run(project.path(), &["test", "--only-vendor", "--only-name"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("--only-name and --only-vendor cannot be used together"));
}

// Ported from Composer\Test\Command\SearchCommandTest::testVerboseOutput.
#[test]
fn composer_search_command_reports_per_repository_diagnostics() {
    let project = search_project();
    let output = run(project.path(), &["vendor-1", "-vvv"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = stdout(&output);
    assert!(predicate::str::contains(
        "Searched installed array repo (defining 0 packages), found 0 result(s)"
    )
    .eval(&output));
    assert!(predicate::str::contains("Searched platform repo, found 0 result(s)").eval(&output));
    assert!(predicate::str::contains(
        "Searched package repo (defining 4 packages), found 1 result(s)"
    )
    .eval(&output));
    assert!(output.contains("vendor-1/package-1"));
}
