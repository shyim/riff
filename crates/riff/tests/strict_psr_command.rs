use assert_cmd::Command;
use predicates::str::contains;
use serde_json::json;
use std::fs;

#[test]
fn strict_psr_preserves_artifacts_and_post_dump_scripts() {
    for command in ["update", "install", "dump-autoload"] {
        for configured in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            fs::create_dir(root.join("src")).unwrap();
            fs::write(
                root.join("src/Wrong.php"),
                "<?php namespace App; class Correct {}",
            )
            .unwrap();
            fs::write(root.join("composer.json"), serde_json::to_vec(&json!({
                "name": "fixture/root", "repositories": {"packagist.org": false},
                "autoload": {"psr-4": {"App\\": "src/"}},
                "config": {"optimize-autoloader": true, "strict-psr-autoloader": configured},
                "scripts": {
                    "post-autoload-dump": "@php -r \"file_put_contents('post-dump', 'yes');\"",
                    "post-install-cmd": "@php -r \"file_put_contents('post-operation', 'yes');\"",
                    "post-update-cmd": "@php -r \"file_put_contents('post-operation', 'yes');\""
                }
            })).unwrap()).unwrap();
            if command == "install" {
                // Exercise the lockfile installation path independently of install's update fallback.
                Command::cargo_bin("riff")
                    .unwrap()
                    .current_dir(root)
                    .env("COMPOSER_HOME", root.join("home"))
                    .args(["update", "--no-install", "--no-scripts"])
                    .assert()
                    .success();
            }
            let mut cmd = Command::cargo_bin("riff").unwrap();
            cmd.current_dir(root)
                .env("COMPOSER_HOME", root.join("home"))
                .arg(command);
            if !configured {
                cmd.arg(if command == "dump-autoload" {
                    "--strict-psr"
                } else {
                    "--strict-psr-autoloader"
                });
            }
            cmd.assert()
                .code(if command == "dump-autoload" { 1 } else { 6 })
                .stderr(contains("does not comply with psr-4"));
            assert!(root.join("vendor/autoload.php").exists());
            assert!(root.join("post-dump").exists());
            assert!(!root.join("post-operation").exists());
        }
    }
}

#[test]
fn strict_psr_requires_optimization_before_writing() {
    for command in ["update", "install", "dump-autoload"] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("composer.json"),
            r#"{"config":{"strict-psr-autoloader":true},"repositories":{"packagist.org":false}}"#,
        )
        .unwrap();
        Command::cargo_bin("riff")
            .unwrap()
            .current_dir(dir.path())
            .env("COMPOSER_HOME", dir.path().join("home"))
            .arg(command)
            .assert()
            .failure();
        assert!(!dir.path().join("vendor/autoload.php").exists());
        assert!(!dir.path().join("composer.lock").exists());
    }
}

#[test]
fn strict_psr_excludes_vendor_violations() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("vendor/fixture/pkg")).unwrap();
    fs::write(
        root.join("vendor/fixture/pkg/Wrong.php"),
        "<?php namespace Dependency; class Correct {}",
    )
    .unwrap();
    fs::write(
        root.join("composer.json"),
        serde_json::to_vec(&json!({
            "autoload": {"psr-4": {"Dependency\\": "vendor/fixture/pkg/"}},
            "config": {"strict-psr-autoloader": true, "optimize-autoloader": true}
        }))
        .unwrap(),
    )
    .unwrap();
    Command::cargo_bin("riff")
        .unwrap()
        .current_dir(root)
        .env("COMPOSER_HOME", root.join("home"))
        .arg("dump-autoload")
        .assert()
        .success();
    assert!(
        !fs::read_to_string(root.join("vendor/composer/autoload_classmap.php"))
            .unwrap()
            .contains("Dependency")
    );
}
