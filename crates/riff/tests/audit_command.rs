use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::{json, Value};

struct AuditFixture {
    root: tempfile::TempDir,
    home: PathBuf,
}

impl AuditFixture {
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
            .arg("audit")
            .args(arguments)
            .args(["-d"])
            .arg(self.root.path());
        command
    }

    fn write_installed(&self, packages: &[Value], dev_packages: &[&str]) {
        let directory = self.root.path().join("vendor/composer");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("installed.json"),
            serde_json::to_vec_pretty(&json!({
                "packages": packages,
                "dev": true,
                "dev-package-names": dev_packages
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn write_lock(&self, packages: &[Value], packages_dev: &[Value]) {
        fs::write(
            self.root.path().join("composer.lock"),
            serde_json::to_vec_pretty(&json!({
                "packages": packages,
                "packages-dev": packages_dev,
                "plugin-api-version": "2.6.0"
            }))
            .unwrap(),
        )
        .unwrap();
    }
}

fn package(name: &str) -> Value {
    json!({
        "name": name,
        "version": "1.0.0",
        "version_normalized": "1.0.0.0",
        "type": "library"
    })
}

// Ported from Composer\Test\Command\AuditCommandTest::
// testSuccessfulResponseCodeWhenNoPackagesAreRequired.
#[test]
fn composer_audit_command_skips_a_project_without_packages() {
    let fixture = AuditFixture::new(json!({}));

    fixture
        .command(&[])
        .assert()
        .success()
        .stdout(contains("No packages - skipping audit."));
}

// Ported from Composer\Test\Command\AuditCommandTest::
// testErrorAuditingLockFileWhenItIsMissing.
#[test]
fn composer_audit_command_rejects_locked_mode_without_a_lock_file() {
    let fixture = AuditFixture::new(json!({}));
    fixture.write_installed(&[package("dummy/pkg")], &[]);

    fixture.command(&["--locked"]).assert().failure().stderr(contains(
        "Valid composer.json and composer.lock files are required to run this command with --locked",
    ));
}

// Ported from Composer\Test\Command\AuditCommandTest::testErrorAuditWithNoInstalledPackages.
#[test]
fn composer_audit_command_fails_when_required_packages_are_not_installed() {
    let fixture = AuditFixture::new(json!({"require": {"dummy/pkg": "1.0.0"}}));

    fixture
        .command(&[])
        .assert()
        .failure()
        .code(1)
        .stdout(contains(
        "No installed packages found. Please run \"riff install\" before running \"riff audit\"",
    ));
}

// Ported from Composer\Test\Command\AuditCommandTest::testAuditPackageWithNoDevOptionPassed.
#[test]
fn composer_audit_command_no_dev_skips_a_dev_only_installation() {
    let fixture = AuditFixture::new(json!({}));
    let dev_package = package("dummy/dev-package");
    fixture.write_installed(std::slice::from_ref(&dev_package), &["dummy/dev-package"]);
    fixture.write_lock(&[], &[dev_package]);

    fixture
        .command(&["--no-dev"])
        .assert()
        .success()
        .stdout(contains("No packages - skipping audit."));
}

fn advisory_fixture(audit_behavior: Option<&str>) -> AuditFixture {
    let policy = audit_behavior.map_or_else(
        || json!({}),
        |behavior| json!({"advisories": {"audit": behavior}}),
    );
    let fixture = AuditFixture::new(json!({
        "repositories": [
            {
                "type": "package",
                "package": package("vulnerable/pkg"),
                "security-advisories": {"vulnerable/pkg": [{
                    "advisoryId": "PKSA-test-audit",
                    "packageName": "vulnerable/pkg",
                    "affectedVersions": "*",
                    "title": "Test vulnerability",
                    "severity": "high",
                    "reportedAt": "2026-01-01T00:00:00+00:00",
                    "sources": [{"name": "test", "remoteId": "TEST-1"}]
                }]}
            },
            {"packagist.org": false}
        ],
        "require": {"vulnerable/pkg": "1.0.0"},
        "config": {"policy": policy}
    }));
    fixture.write_lock(&[package("vulnerable/pkg")], &[]);
    fixture
}

#[test]
fn composer_audit_policy_controls_advisory_exit_status() {
    advisory_fixture(None)
        .command(&["--locked", "--format=summary"])
        .assert()
        .failure()
        .code(1)
        .stderr(contains("Found 1 security vulnerability advisory"));

    advisory_fixture(Some("report"))
        .command(&["--locked", "--format=summary"])
        .assert()
        .success()
        .stderr(contains("Found 1 security vulnerability advisory"));
}

#[test]
fn composer_audit_cli_severity_override_retains_ignored_advisory_in_json() {
    advisory_fixture(None)
        .command(&["--locked", "--format=json", "--ignore-severity=high"])
        .assert()
        .success()
        .stdout(contains("ignored-advisories"))
        .stdout(contains("PKSA-test-audit"));
}

#[test]
fn composer_audit_policy_controls_abandoned_exit_status() {
    for (behavior, expected) in [("report", 0), ("fail", 1)] {
        let fixture = AuditFixture::new(json!({
            "repositories": [{"packagist.org": false}],
            "require": {"abandoned/pkg": "1.0.0"},
            "config": {"policy": {"abandoned": {"audit": behavior}}}
        }));
        let mut abandoned = package("abandoned/pkg");
        abandoned["abandoned"] = json!(true);
        fixture.write_lock(&[abandoned], &[]);

        fixture
            .command(&["--locked", "--format=plain"])
            .assert()
            .code(expected)
            .stderr(contains("abandoned/pkg is abandoned"));
    }
}

#[test]
fn composer_audit_policy_controls_repository_filter_exit_status() {
    for (behavior, expected) in [("report", 0), ("fail", 1)] {
        let fixture = AuditFixture::new(json!({
            "repositories": [
                {
                    "type": "package",
                    "package": package("blocked/pkg"),
                    "filter": {"company-policy": [{
                        "package": "blocked/pkg",
                        "constraint": "*",
                        "reason": "company policy"
                    }]}
                },
                {"packagist.org": false}
            ],
            "require": {"blocked/pkg": "1.0.0"},
            "config": {"policy": {"company-policy": {"audit": behavior}}}
        }));
        fixture.write_lock(&[package("blocked/pkg")], &[]);

        fixture
            .command(&["--locked", "--format=plain"])
            .assert()
            .code(expected)
            .stderr(contains("blocked/pkg matched dependency policy"));
    }
}
