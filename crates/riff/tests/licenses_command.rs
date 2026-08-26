use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;

fn write_json(path: &Path, value: serde_json::Value) {
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn package(name: &str, version: &str, licenses: &[&str]) -> serde_json::Value {
    json!({
        "name": name,
        "version": version,
        "version_normalized": version,
        "type": "library",
        "license": licenses
    })
}

fn project() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({
            "name": "test/pkg",
            "version": "1.2.3",
            "license": "MIT",
            "require": {
                "first/pkg": "^2.0",
                "second/pkg": "3.*",
                "third/pkg": "^1.3"
            },
            "require-dev": {"dev/pkg": "~2.0"}
        }),
    );
    let production = vec![
        package("first/pkg", "2.3.4", &["MIT"]),
        package("second/pkg", "3.4.0", &["LGPL-2.0-only"]),
        package("third/pkg", "1.5.4", &[]),
    ];
    let development = package("dev/pkg", "2.3.4.5", &["MIT"]);
    fs::create_dir_all(project.path().join("vendor/composer")).unwrap();
    let mut installed = production.clone();
    installed.push(development.clone());
    write_json(
        &project.path().join("vendor/composer/installed.json"),
        json!({
            "packages": installed,
            "dev": true,
            "dev-package-names": ["dev/pkg"]
        }),
    );
    write_json(
        &project.path().join("composer.lock"),
        json!({
            "content-hash": "fixture",
            "packages": production,
            "packages-dev": [development]
        }),
    );
    project
}

fn licenses(project: &Path) -> Command {
    let mut command = Command::cargo_bin("riff").unwrap();
    command
        .env("COMPOSER_HOME", project.join("composer-home"))
        .args(["licenses", "-d"])
        .arg(project);
    command
}

// Ported from Composer\Test\Command\LicensesCommandTest::testBasicRun and
// testNoDev.
#[test]
fn composer_licenses_prints_text_and_filters_dev_dependencies() {
    let project = project();
    licenses(project.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Name: test/pkg"))
        .stdout(predicate::str::contains("Version: 1.2.3"))
        .stdout(predicate::str::contains("Licenses: MIT"))
        .stdout(predicate::str::contains("dev/pkg\t2.3.4.5\tMIT"))
        .stdout(predicate::str::contains("third/pkg\t1.5.4\tnone"));

    licenses(project.path())
        .arg("--no-dev")
        .assert()
        .success()
        .stdout(predicate::str::contains("first/pkg"))
        .stdout(predicate::str::contains("dev/pkg").not());

    let mut alias = Command::cargo_bin("riff").unwrap();
    alias
        .args(["license", "--no-dev", "-d"])
        .arg(project.path())
        .assert()
        .success();
}

// Ported from Composer\Test\Command\LicensesCommandTest::testFormatJson.
#[test]
fn composer_licenses_prints_machine_readable_json() {
    let project = project();
    let output = licenses(project.path())
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["name"], "test/pkg");
    assert_eq!(report["version"], "1.2.3");
    assert_eq!(report["license"], json!(["MIT"]));
    assert_eq!(report["dependencies"]["dev/pkg"]["version"], "2.3.4.5");
    assert_eq!(
        report["dependencies"]["second/pkg"]["license"],
        json!(["LGPL-2.0-only"])
    );
    assert_eq!(report["dependencies"]["third/pkg"]["license"], json!([]));
}

// Ported from Composer\Test\Command\LicensesCommandTest::testFormatSummary.
#[test]
fn composer_licenses_summarizes_license_usage() {
    let project = project();
    licenses(project.path())
        .args(["--format", "summary"])
        .assert()
        .success()
        .stdout(predicate::str::contains("License\tNumber of dependencies"))
        .stdout(predicate::str::contains("MIT\t2"))
        .stdout(predicate::str::contains("LGPL-2.0-only\t1"))
        .stdout(predicate::str::contains("none\t1"));
}

// Ported from Composer\Test\Command\LicensesCommandTest::testFormatUnknown.
#[test]
fn composer_licenses_rejects_unknown_formats() {
    let project = project();
    licenses(project.path())
        .args(["--format", "unknown"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Unsupported format \"unknown\""));
}

// Ported from Composer\Test\Command\LicensesCommandTest::testLocked and
// testLockedNoDev.
#[test]
fn composer_licenses_reads_complete_or_production_only_lock_data() {
    let project = project();
    fs::remove_file(project.path().join("vendor/composer/installed.json")).unwrap();

    licenses(project.path())
        .arg("--locked")
        .assert()
        .success()
        .stdout(predicate::str::contains("dev/pkg"));
    licenses(project.path())
        .args(["--locked", "--no-dev"])
        .assert()
        .success()
        .stdout(predicate::str::contains("first/pkg"))
        .stdout(predicate::str::contains("dev/pkg").not());
}

// Ported from Composer\Test\Command\LicensesCommandTest::
// testLockedWithoutLockFile.
#[test]
fn composer_licenses_requires_a_lock_for_locked_mode() {
    let project = project();
    fs::remove_file(project.path().join("composer.lock")).unwrap();
    licenses(project.path())
        .arg("--locked")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Valid composer.json and composer.lock files are required",
        ));
}
