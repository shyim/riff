use std::path::Path;

use assert_cmd::Command;
use serde_json::{json, Value};

fn write_json(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn project(manifest: Value, homepages: &[(&str, &str)]) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    write_json(&project.path().join("composer.json"), &manifest);
    let package = |name: &str, version: &str| {
        let mut package = json!({
            "name": name,
            "version": version,
            "version_normalized": version,
            "type": "library"
        });
        if let Some((_, homepage)) = homepages.iter().find(|(package, _)| *package == name) {
            package["homepage"] = json!(homepage);
        }
        package
    };
    write_json(
        &project.path().join("vendor/composer/installed.json"),
        &json!({
            "packages": [
                package("vendor/package", "1.2.3.0"),
                package("vendor/devpackage", "2.3.4.0")
            ],
            "dev": true,
            "dev-package-names": ["vendor/devpackage"]
        }),
    );
    project
}

fn home(project: &Path, packages: &[&str]) -> std::process::Output {
    Command::cargo_bin("composer")
        .unwrap()
        .env("COMPOSER_HOME", project.join("composer-home"))
        .arg("home")
        .arg("--show")
        .args(packages)
        .arg("-d")
        .arg(project)
        .output()
        .unwrap()
}

#[test]
fn composer_home_command_show_mode_covers_the_upstream_data_provider() {
    let invalid = project(
        json!({
            "repositories": {
                "packages": {
                    "type": "package",
                    "package": [{"name": "vendor/package", "description": "generic description", "version": "1.0.0"}]
                }
            },
            "require": {"vendor/package": "^1.0"}
        }),
        &[],
    );
    let output = home(invalid.path(), &["vendor/package"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        "Invalid or missing repository URL for vendor/package"
    );

    let root = project(json!({"repositories": []}), &[]);
    let output = home(root.path(), &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        "No package specified, opening homepage for the root package\nInvalid or missing repository URL for __root__"
    );

    let output = home(root.path(), &["vendor/anotherpackage"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        "Package vendor/anotherpackage not found\nInvalid or missing repository URL for vendor/anotherpackage"
    );

    let valid = project(
        json!({"repositories": []}),
        &[
            ("vendor/package", "https://example.org"),
            ("vendor/devpackage", "https://example.org/dev"),
        ],
    );
    let output = home(valid.path(), &["vendor/package"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "https://example.org"
    );
    assert!(output.stderr.is_empty());

    let output = home(valid.path(), &["vendor/devpackage"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "https://example.org/dev"
    );
    assert!(output.stderr.is_empty());
}
