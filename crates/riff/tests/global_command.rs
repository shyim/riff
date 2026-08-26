use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::json;

fn write_json(path: &Path, value: serde_json::Value) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn global(home: &Path) -> Command {
    let mut command = Command::cargo_bin("riff").unwrap();
    command
        .env("COMPOSER_HOME", home)
        .env("RIFF_CACHE_DIR", home.join("riff-cache"))
        .arg("global");
    command
}

// Ported from Composer\Test\Command\GlobalCommandTest::testGlobal.
#[test]
fn composer_global_runs_scripts_in_home_and_clears_composer_override() {
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join("probe.php"),
        "<?php echo 'COMPOSER SCRIPT OUTPUT: ', getenv('COMPOSER'), PHP_EOL;",
    )
    .unwrap();
    write_json(
        &home.path().join("composer.json"),
        json!({
            "name": "fixture/global",
            "scripts": {"test-script": "@php probe.php"}
        }),
    );
    let run = global(home.path())
        .env("COMPOSER", "TMP_COMPOSER.JSON")
        .args(["test-script", "--no-interaction"])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains(&format!(
        "Changed current directory to {}",
        home.path().display()
    )));
    assert!(
        stdout
            .lines()
            .any(|line| line == "COMPOSER SCRIPT OUTPUT: "),
        "{stdout}"
    );
    assert!(!stdout.contains("TMP_COMPOSER.JSON"));
}

// Ported from Composer\Test\Command\GlobalCommandTest::testGlobalShow.
#[test]
fn composer_global_forwards_show() {
    let home = tempfile::tempdir().unwrap();
    write_json(
        &home.path().join("composer.json"),
        json!({"name": "fixture/global", "require": {"vendor/global-tool": "1.0.0"}}),
    );
    write_json(
        &home.path().join("vendor/composer/installed.json"),
        json!({"packages": [{
            "name": "vendor/global-tool",
            "version": "1.0.0",
            "version_normalized": "1.0.0.0",
            "description": "A globally installed tool",
            "type": "library",
            "install-path": "../vendor/global-tool"
        }]}),
    );
    let run = global(home.path()).arg("show").output().unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("vendor/global-tool"));
    assert!(stdout.contains("1.0.0"));
}

// Ported from Composer\Test\Command\GlobalCommandTest::testGlobalShowWithoutPackages.
#[test]
fn composer_global_show_succeeds_without_packages() {
    let home = tempfile::tempdir().unwrap();
    write_json(
        &home.path().join("composer.json"),
        json!({"name": "fixture/global"}),
    );
    write_json(
        &home.path().join("vendor/composer/installed.json"),
        json!({"packages": []}),
    );
    assert!(global(home.path())
        .arg("show")
        .output()
        .unwrap()
        .status
        .success());
}

// Ported from Composer\Test\Command\GlobalCommandTest::testGlobalRequire.
#[test]
fn composer_global_forwards_require_arguments() {
    let home = tempfile::tempdir().unwrap();
    write_json(
        &home.path().join("composer.json"),
        json!({"name": "fixture/global"}),
    );
    let run = global(home.path())
        .args([
            "require",
            "vendor/required-pkg:2.0.0",
            "--no-update",
            "--no-interaction",
        ])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(home.path().join("composer.json")).unwrap()).unwrap();
    assert_eq!(manifest["require"]["vendor/required-pkg"], "2.0.0");
}

// Ported from Composer\Test\Command\GlobalCommandTest::testGlobalUpdate.
#[test]
fn composer_global_forwards_update() {
    let home = tempfile::tempdir().unwrap();
    write_json(
        &home.path().join("composer.json"),
        json!({"name": "fixture/global"}),
    );
    write_json(
        &home.path().join("composer.lock"),
        json!({"content-hash": "", "packages": [], "packages-dev": []}),
    );
    let run = global(home.path())
        .args(["update", "--dry-run", "--no-audit", "--no-interaction"])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
}

// Ported from Composer\Test\Command\GlobalCommandTest::testGlobalChangesDirectory.
#[test]
fn composer_global_changes_directory_before_forwarding() {
    let home = tempfile::tempdir().unwrap();
    write_json(
        &home.path().join("composer.json"),
        json!({"name": "test/global"}),
    );
    let run = global(home.path())
        .args(["config", "name"])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains(&format!(
        "Changed current directory to {}",
        home.path().display()
    )));
    assert!(stdout.contains("test/global"));
}

// Ported from Composer\Test\Command\GlobalCommandTest::testGlobalMissingCommandName.
#[test]
fn composer_global_requires_a_command_name() {
    let home = tempfile::tempdir().unwrap();
    global(home.path()).assert().failure();
}
