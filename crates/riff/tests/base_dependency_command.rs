use std::fs;
use std::path::Path;
use std::process::Output;

use assert_cmd::Command;
use serde_json::{json, Value};

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn write_manifest(project: &Path, manifest: Value) {
    write_json(&project.join("composer.json"), &manifest);
}

fn write_lock(project: &Path, packages: Value, packages_dev: Value) {
    write_json(
        &project.join("composer.lock"),
        &json!({
            "content-hash": "",
            "packages": packages,
            "packages-dev": packages_dev,
        }),
    );
}

fn write_installed(project: &Path, packages: Value) {
    let composer = project.join("vendor/composer");
    fs::create_dir_all(&composer).unwrap();
    write_json(
        &composer.join("installed.json"),
        &json!({"packages": packages, "dev": true}),
    );
}

fn package(name: &str, version: &str, require: Value) -> Value {
    json!({
        "name": name,
        "version": version,
        "version_normalized": format!("{version}.0"),
        "type": "library",
        "require": require,
    })
}

fn run(project: &Path, arguments: &[&str]) -> Output {
    let mut command = Command::cargo_bin("riff").unwrap();
    command.args(arguments).arg("-d").arg(project);
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
}

#[test]
fn composer_base_dependency_command_requires_arguments() {
    for arguments in [
        vec!["why"],
        vec!["why-not"],
        vec!["why-not", "vendor/package"],
    ] {
        let output = Command::cargo_bin("riff")
            .unwrap()
            .args(arguments)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let display = format!("{}{}", stdout(&output), stderr(&output));
        assert!(display.contains("required arguments"));
    }
}

#[test]
fn composer_base_dependency_command_requires_lock_for_locked_queries() {
    let project = tempfile::tempdir().unwrap();
    write_manifest(project.path(), json!({}));

    for arguments in [
        vec!["why", "vendor/package", "--locked"],
        vec!["why-not", "vendor/package", "1.*", "--locked"],
    ] {
        let output = run(project.path(), &arguments);
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output)
            .contains("A valid composer.lock file is required to run this command with --locked"));
    }
}

#[test]
fn composer_base_dependency_command_rejects_unknown_package_without_dependencies() {
    let project = tempfile::tempdir().unwrap();
    write_manifest(project.path(), json!({}));

    for arguments in [
        vec!["why", "missing/package"],
        vec!["why-not", "missing/package", "1.*"],
    ] {
        let output = run(project.path(), &arguments);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            stderr(&output),
            "Error: Could not find package \"missing/package\" in your project"
        );
    }
}

#[test]
fn composer_base_dependency_command_rejects_unknown_package_in_installed_project() {
    let project = tempfile::tempdir().unwrap();
    write_manifest(
        project.path(),
        json!({"require": {"vendor/package1": "1.*"}}),
    );
    write_installed(
        project.path(),
        json!([package("vendor/package1", "1.0.0", json!({}))]),
    );

    for arguments in [
        vec!["why", "missing/package"],
        vec!["why-not", "missing/package", "1.*"],
    ] {
        let output = run(project.path(), &arguments);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            stderr(&output),
            "Error: Could not find package \"missing/package\" in your project"
        );
    }
}

#[test]
fn composer_base_dependency_command_warns_when_dependencies_are_not_installed() {
    let project = tempfile::tempdir().unwrap();
    write_manifest(
        project.path(),
        json!({
            "require": {"vendor/package1": "1.*"},
            "require-dev": {"vendor/package2": "2.*"},
        }),
    );
    write_lock(
        project.path(),
        json!([package("vendor/package1", "1.0.0", json!({}))]),
        json!([package("vendor/package2", "2.0.0", json!({}))]),
    );

    for arguments in [
        vec!["why", "vendor/package1"],
        vec!["why-not", "vendor/package1", "1.*"],
    ] {
        let output = run(project.path(), &arguments);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            stderr(&output),
            "Warning: No dependencies installed. Try running install or update, or use --locked."
        );
    }
}

#[test]
fn composer_base_dependency_command_why_outputs() {
    let project = tempfile::tempdir().unwrap();
    write_manifest(
        project.path(),
        json!({
            "require": {
                "vendor1/package2": "1.3.0",
                "vendor1/package3": "2.3.0",
            },
            "require-dev": {"vendor2/package1": "2.*"},
        }),
    );
    let packages = json!([
        package(
            "vendor1/package1",
            "1.3.0",
            json!({"vendor1/package2": "^2"})
        ),
        package(
            "vendor1/package2",
            "2.3.0",
            json!({"vendor1/package3": "^1"})
        ),
        package("vendor1/package3", "2.1.0", json!({})),
    ]);
    let packages_dev = json!([package("vendor2/package1", "2.0.0", json!({}))]);
    write_lock(project.path(), packages, packages_dev);

    let output = run(project.path(), &["why", "vendor1/package1", "--locked"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout(&output),
        "There is no installed package depending on \"vendor1/package1\""
    );

    let output = run(project.path(), &["why", "vendor1/package3", "--locked"]);
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        "__root__         -     requires vendor1/package3 (2.3.0)\n\
vendor1/package2 2.3.0 requires vendor1/package3 (^1)"
    );

    let output = run(
        project.path(),
        &["why", "vendor1/package3", "--tree", "--locked"],
    );
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        concat!(
            "vendor1/package3 2.1.0\n",
            "|--__root__ (requires vendor1/package3 2.3.0)\n",
            "`--vendor1/package2 2.3.0 (requires vendor1/package3 ^1)\n",
            "   |--__root__ (requires vendor1/package2 1.3.0)\n",
            "   `--vendor1/package1 1.3.0 (requires vendor1/package2 ^2)"
        )
    );

    let output = run(
        project.path(),
        &["why", "vendor1/package3", "--recursive", "--locked"],
    );
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        "__root__         -     requires vendor1/package2 (1.3.0)\n\
vendor1/package1 1.3.0 requires vendor1/package2 (^2)\n\
__root__         -     requires vendor1/package3 (2.3.0)\n\
vendor1/package2 2.3.0 requires vendor1/package3 (^1)"
    );

    let output = run(project.path(), &["why", "vendor2/package1", "--locked"]);
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        "__root__ - requires (for development) vendor2/package1 (2.*)"
    );
}

#[test]
fn composer_base_dependency_command_why_not_outputs() {
    let project = tempfile::tempdir().unwrap();
    write_manifest(
        project.path(),
        json!({
            "repositories": [{
                "type": "package",
                "package": [
                    package("vendor1/package1", "1.3.0", json!({})),
                    package("vendor2/package1", "2.0.0", json!({})),
                    package("vendor2/package2", "1.0.0", json!({"vendor2/package3": "1.4.*", "php": "^8.2"})),
                    package("vendor2/package3", "1.4.0", json!({})),
                    package("vendor2/package3", "1.5.0", json!({})),
                ],
            }],
            "require": {"vendor1/package1": "1.*", "php": "^8"},
            "require-dev": {"vendor2/package1": "2.*", "vendor2/package2": "^1"},
            "config": {"platform": {"php": "8.3.2"}},
        }),
    );
    write_installed(
        project.path(),
        json!([
            package("vendor1/package1", "1.3.0", json!({})),
            package("vendor2/package1", "2.0.0", json!({})),
            package(
                "vendor2/package2",
                "1.0.0",
                json!({"vendor2/package3": "1.4.*", "php": "^8.2"})
            ),
            package("vendor2/package3", "1.4.0", json!({})),
        ]),
    );

    let output = run(project.path(), &["why-not", "vendor1/package1", "3.*"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout(&output),
        "__root__ - requires vendor1/package1 (1.*)"
    );
    assert!(stderr(&output)
        .contains("Package \"vendor1/package1\" could not be found with constraint \"3.*\""));
    assert!(stderr(&output).contains("riff require \"vendor1/package1:3.*\" --dry-run"));

    let output = run(project.path(), &["why-not", "vendor1/package1", "^1.4"]);
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        "There is no installed package depending on \"vendor1/package1\" in versions not matching ^1.4"
    );
    assert!(stderr(&output).contains("could not be found with constraint \"^1.4\""));

    let output = run(project.path(), &["why-not", "vendor1/package1", "^1.3"]);
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        "Package \"vendor1/package1\" 1.3.0 is already installed! To find out why, run `riff why vendor1/package1`"
    );

    let output = run(project.path(), &["why-not", "vendor2/package3", "1.5.0"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout(&output),
        "vendor2/package2 1.0.0 requires vendor2/package3 (1.4.*)"
    );
    assert!(!stderr(&output).contains("could not be found"));
    assert!(stderr(&output).contains("riff update \"vendor2/package3:1.5.0\" --dry-run"));

    let output = run(project.path(), &["why-not", "php", "^8"]);
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        "Package \"php ^8\" found in version \"8.3.2\" (version provided by config.platform).\n\
There is no installed package depending on \"php\" in versions not matching ^8"
    );

    let output = run(project.path(), &["why-not", "php", "9.1.0"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout(&output),
        "__root__         -     requires php (^8)\n\
vendor2/package2 1.0.0 requires php (^8.2)"
    );
}
