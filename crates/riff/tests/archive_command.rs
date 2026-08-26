use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;

fn write_json(path: &Path, value: serde_json::Value) {
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn archive(project: &Path) -> Command {
    let mut command = Command::cargo_bin("riff").unwrap();
    command
        .env("COMPOSER_HOME", project.join("composer-home"))
        .args(["archive", "-d"])
        .arg(project);
    command
}

// Ported from Composer\Test\Command\ArchiveCommandTest::
// testUsesConfigFromComposerObject.
#[test]
fn composer_archive_command_uses_project_config() {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({
            "name": "test/pkg",
            "version": "1.2.3",
            "config": {"archive-format": "zip", "archive-dir": "artifacts"}
        }),
    );
    fs::write(project.path().join("source.php"), "<?php return 1;").unwrap();

    let run = archive(project.path()).output().unwrap();
    assert!(
        run.status.success(),
        "archive failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(predicate::str::contains("Created:").eval(&String::from_utf8_lossy(&run.stdout)));
    let output = project.path().join("artifacts/test-pkg-1.2.3.zip");
    assert!(
        output.is_file(),
        "missing archive at {}\nstdout:\n{}\nstderr:\n{}",
        output.display(),
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}

// Ported from Composer\Test\Command\ArchiveCommandTest::
// testUsesConfigFromComposerObjectWithPackageName.
#[test]
fn composer_archive_command_archives_an_installed_package_by_name() {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({
            "name": "test/pkg",
            "version": "1.2.3",
            "config": {"archive-format": "zip", "archive-dir": "artifacts"}
        }),
    );
    let package_dir = project.path().join("vendor/foo/bar");
    fs::create_dir_all(&package_dir).unwrap();
    write_json(
        &package_dir.join("composer.json"),
        json!({"name": "foo/bar", "version": "1.0.0"}),
    );
    fs::create_dir_all(project.path().join("vendor/composer")).unwrap();
    write_json(
        &project.path().join("vendor/composer/installed.json"),
        json!({
            "packages": [{
                "name": "foo/bar",
                "version": "1.0.0",
                "version_normalized": "1.0.0.0",
                "type": "library",
                "install-path": "../foo/bar"
            }]
        }),
    );

    let run = archive(project.path()).arg("foo/bar").output().unwrap();
    assert!(
        run.status.success(),
        "archive failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(predicate::str::contains("Created:").eval(&String::from_utf8_lossy(&run.stdout)));
    let output = project.path().join("artifacts/foo-bar-1.0.0.zip");
    assert!(
        output.is_file(),
        "missing archive at {}\nstdout:\n{}\nstderr:\n{}",
        output.display(),
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}
