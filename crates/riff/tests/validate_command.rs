use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;

fn write_json(path: &Path, value: serde_json::Value) {
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn validate(project: &Path) -> Command {
    let mut command = Command::cargo_bin("riff").unwrap();
    command.args(["validate", "-d"]).arg(project);
    command
}

// Ported from Composer\Test\Command\ValidateCommandTest::testValidate,
// including its valid, publish-error, and --no-check-publish cases.
#[test]
fn composer_validate_reports_valid_and_publish_only_issues() {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({
            "name": "test/suite",
            "type": "library",
            "description": "A generic test suite",
            "license": "MIT",
            "require": {}
        }),
    );
    validate(project.path())
        .arg("--no-check-version")
        .assert()
        .success()
        .stdout(predicate::str::contains("./composer.json is valid"));

    write_json(
        &project.path().join("composer.json"),
        json!({"require": {}}),
    );
    validate(project.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("# Publish errors"))
        .stderr(predicate::str::contains("No license specified"));
    validate(project.path())
        .arg("--no-check-publish")
        .assert()
        .success()
        .stderr(predicate::str::contains("valid, but with a few warnings"))
        .stderr(predicate::str::contains("# Publish errors").not());
}

// Ported from Composer\Test\Command\ValidateCommandTest::
// testValidateOnFileIssues.
#[test]
fn composer_validate_reports_a_missing_manifest() {
    let project = tempfile::tempdir().unwrap();
    validate(project.path())
        .assert()
        .code(3)
        .stderr(predicate::str::contains("./composer.json not found."));
}

// Ported from Composer\Test\Command\ValidateCommandTest::testWithComposerLock.
#[test]
fn composer_validate_reports_requirements_missing_from_the_lock() {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({
            "name": "test/suite",
            "type": "library",
            "description": "A generic test suite",
            "license": "MIT",
            "require": {"root/req": "1.*"}
        }),
    );
    write_json(
        &project.path().join("composer.lock"),
        json!({"content-hash": "stale", "packages": [], "packages-dev": []}),
    );

    validate(project.path())
        .arg("--no-check-version")
        .assert()
        .failure()
        .stdout(predicate::str::contains("composer.lock has some errors"))
        .stderr(predicate::str::contains("# Lock file errors"))
        .stderr(predicate::str::contains(
            "Required package \"root/req\" is not present in the lock file",
        ));
}
