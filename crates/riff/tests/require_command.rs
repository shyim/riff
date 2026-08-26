use std::fs;
use std::path::Path;
use std::process::Output;

use assert_cmd::Command;
use serde_json::{json, Value};

fn write_manifest(project: &Path, value: Value) {
    fs::write(
        project.join("composer.json"),
        serde_json::to_vec_pretty(&value).unwrap(),
    )
    .unwrap();
}

fn package_repository(packages: Value) -> Value {
    json!({
        "repositories": {
            "packages": {
                "type": "package",
                "package": packages,
            }
        }
    })
}

fn command(project: &Path) -> Command {
    let mut command = Command::cargo_bin("riff").unwrap();
    command
        .arg("require")
        .env("COMPOSER_HOME", project.join("composer-home"))
        .env("RIFF_CACHE_DIR", project.join("cache"))
        .arg("-d")
        .arg(project);
    command
}

fn run(project: &Path, arguments: &[&str]) -> Output {
    command(project).args(arguments).output().unwrap()
}

fn display(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn read_manifest(project: &Path) -> Value {
    serde_json::from_slice(&fs::read(project.join("composer.json")).unwrap()).unwrap()
}

#[test]
fn composer_require_reports_when_no_package_version_matches_the_platform() {
    let project = tempfile::tempdir().unwrap();
    write_manifest(
        project.path(),
        package_repository(json!([
            {"name": "required/pkg", "version": "1.0.0", "require": {"ext-foobar": "^1"}}
        ])),
    );

    let output = run(
        project.path(),
        &[
            "--dry-run",
            "--no-audit",
            "--no-interaction",
            "required/pkg",
        ],
    );
    assert!(!output.status.success());
    assert!(display(&output).contains(concat!(
        "Package required/pkg has requirements incompatible with your PHP version, PHP extensions and Composer version:\n",
        "  - required/pkg 1.0.0 requires ext-foobar ^1 but it is not present."
    )));
}

#[test]
fn composer_require_rejects_an_unquoted_inline_alias() {
    let project = tempfile::tempdir().unwrap();
    write_manifest(project.path(), json!({}));

    let output = run(
        project.path(),
        &[
            "--dry-run",
            "--no-audit",
            "--no-interaction",
            "required/pkg",
            "dev-main",
            "as",
            "1.2.x-dev",
        ],
    );
    assert!(!output.status.success());
    assert!(display(&output).contains(
        "Cannot use \"as\" as a separate argument. Quote the inline alias as one argument, e.g. \"vendor/package:dev-main as 1.2.x-dev\"."
    ));
}

#[test]
fn composer_require_warns_before_using_a_feature_branch() {
    let project = tempfile::tempdir().unwrap();
    let mut manifest = package_repository(json!([
        {"name": "required/pkg", "version": "2.0.0", "require": {"common/dep": "^1"}},
        {"name": "required/pkg", "version": "dev-foo-bar", "require": {"common/dep": "^2"}},
        {"name": "common/dep", "version": "2.0.0"}
    ]));
    manifest["require"] = json!({"common/dep": "^2.0"});
    manifest["minimum-stability"] = json!("dev");
    manifest["prefer-stable"] = json!(true);
    write_manifest(project.path(), manifest.clone());

    let assert = command(project.path())
        .args(["--dry-run", "--no-audit", "required/pkg"])
        .write_stdin("n\n")
        .assert()
        .failure();
    let output = assert.get_output();
    let output = display(output);
    assert!(output.contains("Using version dev-foo-bar for required/pkg"));
    assert!(output.contains("Version dev-foo-bar looks like it may be a feature branch"));
    assert!(output.contains("Installation failed, reverting composer.json"));
    assert_eq!(read_manifest(project.path()), manifest);
}

#[test]
fn composer_require_selects_and_records_compatible_constraints() {
    let project = tempfile::tempdir().unwrap();
    write_manifest(
        project.path(),
        package_repository(json!([
            {"name": "required/pkg", "version": "1.2.0", "require": {"ext-foobar": "^1"}},
            {"name": "required/pkg", "version": "1.1.0", "require": {"ext-foobar": "^1"}},
            {"name": "required/pkg", "version": "1.0.0"}
        ])),
    );
    let output = run(
        project.path(),
        &[
            "--dry-run",
            "--no-audit",
            "--no-interaction",
            "required/pkg",
        ],
    );
    assert!(output.status.success(), "{}", display(&output));
    let output = display(&output);
    assert!(output.contains("Cannot use required/pkg's latest version 1.2.0"));
    assert!(output.contains("Using version ^1.0 for required/pkg"));

    let output = run(
        project.path(),
        &[
            "--dry-run",
            "--no-audit",
            "--no-install",
            "--no-interaction",
            "-v",
            "required/pkg",
        ],
    );
    assert!(output.status.success(), "{}", display(&output));
    let output = display(&output);
    assert!(output.contains("latest version 1.2.0"));
    assert!(output.contains("required/pkg 1.1.0"));

    write_manifest(
        project.path(),
        package_repository(json!([
            {"name": "required/pkg", "version": "1.1.0", "require": {"php": "^20"}},
            {"name": "required/pkg", "version": "1.0.0", "require": {"php": ">=7"}}
        ])),
    );
    let output = run(
        project.path(),
        &[
            "--dry-run",
            "--no-audit",
            "--no-install",
            "--no-interaction",
            "required/pkg",
        ],
    );
    assert!(output.status.success(), "{}", display(&output));
    let output = display(&output);
    assert!(output.contains("requires php ^20 which is not satisfied by your platform"));
    assert!(output.contains("Using version ^1.0 for required/pkg"));

    let output = run(
        project.path(),
        &[
            "--dry-run",
            "--no-audit",
            "--no-update",
            "--no-interaction",
            "required/pkg",
        ],
    );
    assert!(output.status.success(), "{}", display(&output));
    assert!(display(&output).contains("Using version ^1.0 for required/pkg"));

    let mut manifest = package_repository(json!([
        {"name": "existing/dep", "version": "1.1.0", "require": {"required/pkg": "^1"}},
        {"name": "required/pkg", "version": "2.0.0"},
        {"name": "required/pkg", "version": "1.1.0"},
        {"name": "required/pkg", "version": "1.0.0"}
    ]));
    manifest["require"] = json!({"existing/dep": "^1"});
    write_manifest(project.path(), manifest);
    let output = run(
        project.path(),
        &[
            "--dry-run",
            "--no-audit",
            "--no-install",
            "--no-interaction",
            "required/pkg",
        ],
    );
    assert!(output.status.success(), "{}", display(&output));
    assert!(display(&output).contains("Using version ^1.1 for required/pkg"));

    let mut manifest = package_repository(json!([
        {"name": "required/pkg", "version": "1.1.0"}
    ]));
    manifest["type"] = json!("project");
    write_manifest(project.path(), manifest);
    let output = run(
        project.path(),
        &[
            "--dry-run",
            "--no-audit",
            "--no-install",
            "--fixed",
            "--no-interaction",
            "required/pkg",
        ],
    );
    assert!(output.status.success(), "{}", display(&output));
    assert!(display(&output).contains("Using version 1.1.0 for required/pkg"));
}

#[test]
fn composer_require_moves_dependencies_between_root_sections() {
    for (dev, interactive) in [(true, false), (false, false), (true, true), (false, true)] {
        let project = tempfile::tempdir().unwrap();
        let current = if dev { "require" } else { "require-dev" };
        let target = if dev { "require-dev" } else { "require" };
        let mut manifest = package_repository(json!([
            {"name": "required/pkg", "version": "1.0.0"}
        ]));
        manifest[current] = json!({"required/pkg": "^1.0"});
        write_manifest(project.path(), manifest);

        let mut command = command(project.path());
        command.args(["--no-audit", "--no-update"]);
        if dev {
            command.arg("--dev");
        }
        if interactive {
            command.write_stdin("yes\n");
        } else {
            command.arg("--no-interaction");
        }
        let output = command.arg("required/pkg").output().unwrap();
        assert!(output.status.success(), "{}", display(&output));
        let output_text = display(&output);
        assert!(output_text.contains("which will move it to the"));
        let manifest = read_manifest(project.path());
        assert!(manifest.get(current).is_none());
        assert_eq!(manifest[target]["required/pkg"], "^1.0");
    }
}
