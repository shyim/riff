use std::fs;
use std::path::Path;
use std::process::Command as ProcessCommand;

use assert_cmd::Command;
use predicates::prelude::*;
use riff_core::archive::create_package_archive;
use riff_core::Package;
use serde_json::json;

fn write_json(path: &Path, value: serde_json::Value) {
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn project() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({"name": "fixture/project", "require": {"root/req": "1.*"}}),
    );
    fs::create_dir_all(project.path().join("vendor/composer")).unwrap();
    project
}

fn riff_status(project: &Path) -> Command {
    let mut command = Command::cargo_bin("riff").unwrap();
    command
        .env("RIFF_CACHE_DIR", project.join("cache"))
        .env("COMPOSER_HOME", project.join("composer-home"))
        .args(["status", "--no-scripts", "-d"])
        .arg(project);
    command
}

fn write_installed(project: &Path, package: serde_json::Value) {
    write_json(
        &project.join("vendor/composer/installed.json"),
        json!({"packages": [package], "dev": true, "dev-package-names": []}),
    );
}

fn run_git(directory: &Path, arguments: &[&str]) -> String {
    let output = ProcessCommand::new("git")
        .args(arguments)
        .current_dir(directory)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

// Ported from Composer\Test\Command\StatusCommandTest::testNoLocalChanges.
#[test]
fn composer_status_reports_no_local_changes() {
    let project = project();
    write_installed(
        project.path(),
        json!({
            "name": "root/req",
            "version": "1.0.0",
            "type": "metapackage",
            "installation-source": "dist",
            "install-path": null
        }),
    );

    riff_status(project.path())
        .assert()
        .success()
        .stderr(predicate::str::contains("No local changes"));
}

// Ported from Composer\Test\Command\StatusCommandTest::
// testLocallyModifiedPackages, including both upstream data-provider cases.
#[test]
fn composer_status_reports_modified_source_and_dist_packages() {
    let source_project = project();
    let source_install = source_project.path().join("vendor/root/req");
    fs::create_dir_all(&source_install).unwrap();
    fs::write(source_install.join("tracked.php"), "<?php return 1;").unwrap();
    run_git(&source_install, &["init", "--quiet"]);
    run_git(&source_install, &["add", "tracked.php"]);
    run_git(
        &source_install,
        &[
            "-c",
            "user.name=Riff Tests",
            "-c",
            "user.email=tests@riff.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
    let source_reference = run_git(&source_install, &["rev-parse", "HEAD"]);
    write_installed(
        source_project.path(),
        json!({
            "name": "root/req",
            "version": "1.0.0",
            "installation-source": "source",
            "source": {"type": "git", "reference": source_reference},
            "install-path": "../root/req"
        }),
    );
    fs::write(source_install.join("tracked.php"), "<?php return 2;").unwrap();
    let install_path = Path::new("vendor").join("root").join("req");

    riff_status(source_project.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "You have changes in the following dependencies:",
        ))
        .stdout(predicate::str::contains(install_path.display().to_string()));

    let dist_project = project();
    let dist_source = tempfile::tempdir().unwrap();
    fs::write(
        dist_source.path().join("composer.json"),
        "{\"name\":\"root/req\"}",
    )
    .unwrap();
    fs::write(dist_source.path().join("tracked.php"), "<?php return 1;").unwrap();
    let archive = create_package_archive(
        &Package::new("root/req", "1.0.0"),
        dist_source.path(),
        dist_project.path(),
        "zip",
        Some("root-req"),
        true,
    )
    .unwrap();
    let dist_install = dist_project.path().join("vendor/root/req");
    fs::create_dir_all(&dist_install).unwrap();
    fs::copy(
        dist_source.path().join("composer.json"),
        dist_install.join("composer.json"),
    )
    .unwrap();
    fs::copy(
        dist_source.path().join("tracked.php"),
        dist_install.join("tracked.php"),
    )
    .unwrap();
    write_installed(
        dist_project.path(),
        json!({
            "name": "root/req",
            "version": "1.0.0",
            "installation-source": "dist",
            "dist": {
                "type": "zip",
                "url": reqwest::Url::from_file_path(&archive).unwrap().to_string(),
                "reference": "fixture-reference",
                "shasum": ""
            },
            "install-path": "../root/req"
        }),
    );
    fs::write(dist_install.join("tracked.php"), "<?php return 2;").unwrap();

    riff_status(dist_project.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "You have changes in the following dependencies:",
        ))
        .stdout(predicate::str::contains(install_path.display().to_string()));
}
