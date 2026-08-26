use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;

fn write_json(path: &Path, value: serde_json::Value) {
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn package(name: &str, version: &str) -> serde_json::Value {
    json!({
        "name": name,
        "version": version,
        "version_normalized": version,
        "type": "library"
    })
}

fn project(lock: bool, requirements: serde_json::Value) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    write_json(&project.path().join("composer.json"), requirements);
    let production = vec![
        package("first/pkg", "2.3.4"),
        package("second/pkg", "3.4.0"),
    ];
    let development = package("dev/pkg", "2.3.4.5");
    fs::create_dir_all(project.path().join("vendor/composer")).unwrap();
    let mut installed = production.clone();
    installed.push(development.clone());
    write_json(
        &project.path().join("vendor/composer/installed.json"),
        json!({"packages": installed}),
    );
    if lock {
        write_json(
            &project.path().join("composer.lock"),
            json!({"content-hash": "fixture", "packages": production, "packages-dev": [development]}),
        );
    }
    project
}

fn bump(project: &Path) -> Command {
    let mut command = Command::cargo_bin("riff").unwrap();
    command.args(["bump", "-d"]).arg(project);
    command
}

fn manifest(project: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(project.join("composer.json")).unwrap()).unwrap()
}

fn requirements() -> serde_json::Value {
    json!({
        "type": "project",
        "require": {"first/pkg": "^v2.0", "second/pkg": "3.*"},
        "require-dev": {"dev/pkg": "~2.0"}
    })
}

// Ported from all Composer\Test\Command\BumpCommandTest::testBump data cases.
#[test]
fn composer_bump_updates_selected_installed_constraints_and_supports_dry_run() {
    let all = project(true, requirements());
    bump(all.path()).assert().success();
    let updated = manifest(all.path());
    assert_eq!(updated["require"]["first/pkg"], "^2.3.4");
    assert_eq!(updated["require"]["second/pkg"], "^3.4");
    assert_eq!(updated["require-dev"]["dev/pkg"], "^2.3.4.5");

    let dev = project(true, requirements());
    bump(dev.path()).arg("--dev-only").assert().success();
    let updated = manifest(dev.path());
    assert_eq!(updated["require"]["first/pkg"], "^v2.0");
    assert_eq!(updated["require-dev"]["dev/pkg"], "^2.3.4.5");

    let production = project(true, requirements());
    bump(production.path())
        .arg("--no-dev-only")
        .assert()
        .success();
    let updated = manifest(production.path());
    assert_eq!(updated["require"]["first/pkg"], "^2.3.4");
    assert_eq!(updated["require-dev"]["dev/pkg"], "~2.0");

    let selected = project(true, requirements());
    bump(selected.path())
        .args(["first/pkg:3.0.1", "dev/*"])
        .assert()
        .success();
    let updated = manifest(selected.path());
    assert_eq!(updated["require"]["first/pkg"], "^2.3.4");
    assert_eq!(updated["require"]["second/pkg"], "3.*");
    assert_eq!(updated["require-dev"]["dev/pkg"], "^2.3.4.5");

    let installed_only = project(false, requirements());
    bump(installed_only.path()).assert().success();
    assert_eq!(
        manifest(installed_only.path())["require"]["second/pkg"],
        "^3.4"
    );

    let dry_run = project(true, requirements());
    let before = fs::read(dry_run.path().join("composer.json")).unwrap();
    bump(dry_run.path())
        .arg("--dry-run")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("would be updated with"));
    assert_eq!(
        fs::read(dry_run.path().join("composer.json")).unwrap(),
        before
    );

    let already_current = project(
        true,
        json!({
            "type": "project",
            "require": {"php": ">=5.3", "first/pkg": "^2.3.4", "second/pkg": "^3.4", "third/pkg": "^1.2"},
            "require-dev": {"dev/pkg": "^2.3.4.5"}
        }),
    );
    bump(already_current.path())
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicate::str::contains("No requirements to bump"));

    let alias = project(
        true,
        json!({
            "type": "project",
            "require": {"first/pkg": "^2.3.4", "second/pkg": "dev-bugfix as 3.4.x-dev"}
        }),
    );
    bump(alias.path()).assert().success();
    assert_eq!(
        manifest(alias.path())["require"]["second/pkg"],
        "dev-bugfix as 3.4.x-dev"
    );
}

// Ported from Composer\Test\Command\BumpCommandTest::
// testBumpFailsOnNonExistingComposerFile.
#[test]
fn composer_bump_reports_a_missing_manifest() {
    let project = tempfile::tempdir().unwrap();
    bump(project.path())
        .assert()
        .code(1)
        .stderr(predicate::str::contains("./composer.json is not readable."));
}
