mod support;

use std::path::Path;

use assert_cmd::Command;
use serde_json::{json, Value};

fn write_json(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn composer(project: &Path) -> Command {
    let mut command = Command::cargo_bin("composer").unwrap();
    command
        .env("COMPOSER_HOME", project.join("composer-home"))
        .env("RIFF_CACHE_DIR", project.join("cache"));
    command
}

// Ported from Composer\Test\Command\InstallCommandTest::testInstallCommandErrors.
#[test]
fn composer_install_command_compatibility_errors_and_deprecations() {
    let project = tempfile::tempdir().unwrap();
    let manifest = json!({"repositories": []});
    let manifest_content = format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap());
    std::fs::write(project.path().join("composer.json"), &manifest_content).unwrap();
    let package = json!({
        "name": "vendor/package",
        "version": "1.2.3",
        "version_normalized": "1.2.3.0",
        "type": "metapackage"
    });
    let dev_package = json!({
        "name": "vendor/devpackage",
        "version": "2.3.4",
        "version_normalized": "2.3.4.0",
        "type": "metapackage"
    });
    write_json(
        &project.path().join("composer.lock"),
        &json!({
            "content-hash": riff_core::compute_content_hash(&manifest_content),
            "packages": [package.clone()],
            "packages-dev": [dev_package.clone()]
        }),
    );
    write_json(
        &project.path().join("vendor/composer/installed.json"),
        &json!({
            "packages": [package, dev_package],
            "dev": true,
            "dev-package-names": ["vendor/devpackage"]
        }),
    );

    for (option, diagnostic) in [
        (
            "--dev",
            "You are using the deprecated option \"--dev\". It has no effect",
        ),
        (
            "--no-suggest",
            "You are using the deprecated option \"--no-suggest\". It has no effect",
        ),
    ] {
        let output = composer(project.path())
            .args([
                "install",
                option,
                "--dry-run",
                "--no-autoloader",
                "--no-audit",
                "--no-scripts",
                "--no-plugins",
                "-d",
            ])
            .arg(project.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains(diagnostic));
    }

    let output = composer(project.path())
        .args(["install", "vendor/package", "-d"])
        .arg(project.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        "Invalid argument vendor/package. Use \"riff require vendor/package\" instead to add packages to your composer.json."
    );

    let output = composer(project.path())
        .args(["install", "--no-install", "-d"])
        .arg(project.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        "Invalid option \"--no-install\". Use \"riff update --no-install\" instead if you are trying to update composer.lock."
    );
}

// Ported from Composer\Test\Command\InstallCommandTest::testInstallFromEmptyVendor.
#[test]
fn composer_install_from_empty_vendor_installs_locked_prod_and_dev_packages() {
    support::composer_fixture::run(
        r#"
--TEST--
Install prod and dev metapackages from a lock into an empty vendor directory
--COMPOSER--
{"require":{"root/req":"1.*"},"require-dev":{"root/another":"1.*"}}
--LOCK--
{"packages":[{"name":"root/req","version":"1.0.0","type":"metapackage"}],"packages-dev":[{"name":"root/another","version":"1.0.0","type":"metapackage"}]}
--RUN--
install
--EXPECT--
Installing root/another (1.0.0)
Installing root/req (1.0.0)
"#,
    );
}

// Ported from Composer\Test\Command\InstallCommandTest::testInstallFromEmptyVendorNoDev.
#[test]
fn composer_install_from_empty_vendor_no_dev_installs_only_locked_prod_packages() {
    support::composer_fixture::run(
        r#"
--TEST--
Install only production metapackages from a lock into an empty vendor directory
--COMPOSER--
{"require":{"root/req":"1.*"},"require-dev":{"root/another":"1.*"}}
--LOCK--
{"packages":[{"name":"root/req","version":"1.0.0","type":"metapackage"}],"packages-dev":[{"name":"root/another","version":"1.0.0","type":"metapackage"}]}
--RUN--
install --no-dev
--EXPECT--
Installing root/req (1.0.0)
"#,
    );
}

#[test]
fn composer_install_prefetches_and_reports_audit_with_a_custom_vendor_directory() {
    let project = tempfile::tempdir().unwrap();
    let manifest = json!({
        "config": {"vendor-dir": "dependencies"},
        "repositories": [{"packagist.org": false}],
        "require": {"vendor/package": "1.0.0"}
    });
    let manifest_content = format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap());
    std::fs::write(project.path().join("composer.json"), &manifest_content).unwrap();
    write_json(
        &project.path().join("composer.lock"),
        &json!({
            "content-hash": riff_core::compute_content_hash(&manifest_content),
            "packages": [{
                "name": "vendor/package",
                "version": "1.0.0",
                "version_normalized": "1.0.0.0",
                "type": "metapackage"
            }],
            "packages-dev": []
        }),
    );

    let output = composer(project.path())
        .args([
            "install",
            "--no-plugins",
            "--no-scripts",
            "--audit-format",
            "summary",
            "-d",
        ])
        .arg(project.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert!(project
        .path()
        .join("dependencies/composer/installed.json")
        .exists());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("No security vulnerability advisories found."));
}

#[test]
fn quiet_install_suppresses_suggestion_and_funding_notices() {
    let project = tempfile::tempdir().unwrap();
    let manifest = json!({
        "repositories": [{"packagist.org": false}],
        "require": {"vendor/package": "1.0.0"}
    });
    let manifest_content = format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap());
    std::fs::write(project.path().join("composer.json"), &manifest_content).unwrap();
    write_json(
        &project.path().join("composer.lock"),
        &json!({
            "content-hash": riff_core::compute_content_hash(&manifest_content),
            "packages": [{
                "name": "vendor/package",
                "version": "1.0.0",
                "version_normalized": "1.0.0.0",
                "type": "metapackage",
                "suggest": {"vendor/optional": "Optional integration"},
                "funding": [{"type": "custom", "url": "https://example.com/fund"}]
            }],
            "packages-dev": []
        }),
    );

    composer(project.path())
        .args([
            "--quiet",
            "install",
            "--no-autoloader",
            "--no-audit",
            "--no-scripts",
            "--no-plugins",
            "-d",
        ])
        .arg(project.path())
        .assert()
        .success()
        .stdout("")
        .stderr("");
}

// Ported from Composer\Test\Command\InstallCommandTest::testInstallNewPackagesWithExistingPartialVendor.
#[test]
fn composer_install_reconciles_a_partial_vendor_directory() {
    support::composer_fixture::run(
        r#"
--TEST--
Install the locked package missing from an otherwise partial vendor directory
--COMPOSER--
{"require":{"root/req":"1.*","root/another":"1.*"}}
--INSTALLED--
[{"name":"root/req","version":"1.0.0","type":"metapackage"}]
--LOCK--
{"packages":[{"name":"root/req","version":"1.0.0","type":"metapackage"},{"name":"root/another","version":"1.0.0","type":"metapackage"}],"packages-dev":[]}
--RUN--
install
--EXPECT--
Installing root/another (1.0.0)
"#,
    );
}

fn reinstall_project() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        &json!({
            "repositories": [],
            "require": {"root/req": "1.*"},
            "require-dev": {
                "root/anotherreq": "2.*",
                "root/anotherreq2": "2.*",
                "root/lala": "2.*"
            }
        }),
    );
    let package = |name: &str| {
        json!({
            "name": name,
            "version": "1.0.0",
            "version_normalized": "1.0.0.0",
            "type": "metapackage"
        })
    };
    write_json(
        &project.path().join("vendor/composer/installed.json"),
        &json!({
            "packages": [
                package("root/req"),
                package("root/anotherreq"),
                package("root/anotherreq2"),
                package("root/lala")
            ],
            "dev": true,
            "dev-package-names": ["root/anotherreq", "root/anotherreq2", "root/lala"]
        }),
    );
    project
}

fn run_reinstall(project: &Path, args: &[&str]) -> std::process::Output {
    composer(project)
        .arg("reinstall")
        .arg("--no-plugins")
        .args(args)
        .arg("-d")
        .arg(project)
        .output()
        .unwrap()
}

// Ported from Composer\Test\Command\ReinstallCommandTest::testReinstallCommand.
#[test]
fn composer_reinstall_command_covers_name_type_and_unmatched_data_provider_cases() {
    let project = reinstall_project();
    let output = run_reinstall(project.path(), &["root/req", "root/anotherreq*"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "- Removing root/req (1.0.0)\n  - Removing root/anotherreq2 (1.0.0)\n  - Removing root/anotherreq (1.0.0)\n  - Installing root/anotherreq (1.0.0)\n  - Installing root/anotherreq2 (1.0.0)\n  - Installing root/req (1.0.0)"
    );
    assert!(output.stderr.is_empty());

    let output = run_reinstall(project.path(), &["--type", "metapackage"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "- Removing root/req (1.0.0)\n  - Removing root/lala (1.0.0)\n  - Removing root/anotherreq2 (1.0.0)\n  - Removing root/anotherreq (1.0.0)\n  - Installing root/anotherreq (1.0.0)\n  - Installing root/anotherreq2 (1.0.0)\n  - Installing root/lala (1.0.0)\n  - Installing root/req (1.0.0)"
    );
    assert!(output.stderr.is_empty());

    let output = run_reinstall(project.path(), &["root/unknownreq"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        "Pattern \"root/unknownreq\" does not match any currently installed packages.\nFound no packages to reinstall, aborting."
    );
}
