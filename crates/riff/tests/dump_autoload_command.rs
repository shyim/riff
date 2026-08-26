use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::{json, Value};

struct DumpAutoloadFixture {
    root: tempfile::TempDir,
    home: PathBuf,
}

impl DumpAutoloadFixture {
    fn new(manifest: Value) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            root.path().join("composer.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        Self { root, home }
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::cargo_bin("riff").unwrap();
        command
            .env("COMPOSER_HOME", &self.home)
            .env("NO_COLOR", "1")
            .arg("dump-autoload")
            .args(arguments)
            .args(["-d"])
            .arg(self.root.path());
        command
    }

    fn write_lock(&self, content_hash: &str) {
        fs::write(
            self.root.path().join("composer.lock"),
            serde_json::to_vec_pretty(&json!({
                "content-hash": content_hash,
                "packages": [],
                "packages-dev": []
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn autoload_php(&self) -> String {
        fs::read_to_string(self.root.path().join("vendor/autoload.php")).unwrap()
    }
}

// Ported from Composer\Test\Command\DumpAutoloadCommandTest::testUsingOptimizeAndStrictPsr.
#[test]
fn composer_dump_autoload_command_runs_strict_psr_with_optimization() {
    let fixture = DumpAutoloadFixture::new(json!({"name": "fixture/project"}));

    fixture
        .command(&["--optimize", "--strict-psr"])
        .assert()
        .success()
        .stdout(contains("Generating optimized autoload files"))
        .stdout(contains(
            "Generated optimized autoload files containing 1 classes",
        ));
}

// Ported from Composer\Test\Command\DumpAutoloadCommandTest::
// testFailsUsingStrictPsrIfClassMapViolationsAreFound.
#[test]
fn composer_dump_autoload_command_fails_for_strict_psr_violations() {
    let fixture = DumpAutoloadFixture::new(json!({
        "name": "fixture/project",
        "autoload": {"psr-4": {"Application\\": "src"}}
    }));
    fs::create_dir_all(fixture.root.path().join("src")).unwrap();
    fs::write(
        fixture.root.path().join("src/Foo.php"),
        "<?php namespace Application\\Src; class Foo {}",
    )
    .unwrap();

    fixture
        .command(&["--optimize", "--strict-psr"])
        .assert()
        .failure()
        .code(1)
        .stderr(contains("Class Application\\Src\\Foo located in"))
        .stderr(contains(
            "does not comply with psr-4 autoloading standard (rule: Application\\ => ./src). Skipping.",
        ));
}

// Ported from Composer\Test\Command\DumpAutoloadCommandTest::
// testUsingClassmapAuthoritativeAndStrictPsr.
#[test]
fn composer_dump_autoload_command_runs_strict_psr_with_an_authoritative_classmap() {
    let fixture = DumpAutoloadFixture::new(json!({"name": "fixture/project"}));

    fixture
        .command(&["--classmap-authoritative", "--strict-psr"])
        .assert()
        .success()
        .stdout(contains("Generating optimized autoload files"))
        .stdout(contains(
            "Generated optimized autoload files (authoritative) containing 1 classes",
        ));
}

// Ported from Composer\Test\Command\DumpAutoloadCommandTest::
// testStrictPsrDoesNotWorkWithoutOptimizedAutoloader.
#[test]
fn composer_dump_autoload_command_rejects_strict_psr_without_optimization() {
    let fixture = DumpAutoloadFixture::new(json!({"name": "fixture/project"}));

    fixture
        .command(&["--strict-psr"])
        .assert()
        .failure()
        .stderr(contains(
            "--strict-psr mode only works with optimized autoloader, use --optimize or --classmap-authoritative if you want a strict return value.",
        ));
}

// Ported from Composer\Test\Command\DumpAutoloadCommandTest::testDevAndNoDevCannotBeCombined.
#[test]
fn composer_dump_autoload_command_rejects_dev_with_no_dev() {
    let fixture = DumpAutoloadFixture::new(json!({"name": "fixture/project"}));

    fixture
        .command(&["--dev", "--no-dev"])
        .assert()
        .failure()
        .stderr(contains(
            "You can not use both --no-dev and --dev as they conflict with each other.",
        ));
}

// Ported from Composer\Test\Command\DumpAutoloadCommandTest::testWithCustomAutoloaderSuffix.
#[test]
fn composer_dump_autoload_command_uses_a_configured_suffix() {
    let fixture = DumpAutoloadFixture::new(json!({
        "name": "fixture/project",
        "config": {"autoloader-suffix": "Foobar"}
    }));

    fixture.command(&[]).assert().success();
    assert!(fixture
        .autoload_php()
        .contains("ComposerAutoloaderInitFoobar"));
}

// Ported from Composer\Test\Command\DumpAutoloadCommandTest::
// testWithExistingComposerLockAndAutoloaderSuffix.
#[test]
fn composer_dump_autoload_command_prefers_a_configured_suffix_over_the_lock_hash() {
    let fixture = DumpAutoloadFixture::new(json!({
        "name": "fixture/project",
        "config": {"autoloader-suffix": "Foobar"}
    }));
    fixture.write_lock("d751713988987e9331980363e24189ce");

    fixture.command(&[]).assert().success();
    let autoload = fixture.autoload_php();
    assert!(autoload.contains("ComposerAutoloaderInitFoobar"));
    assert!(!autoload.contains("ComposerAutoloaderInitd751713988987e9331980363e24189ce"));
}
