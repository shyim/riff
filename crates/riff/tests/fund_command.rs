use std::path::Path;

use assert_cmd::Command;
use serde_json::{json, Value};

fn write_json(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn installed_package(name: &str, funding: Option<(&str, &str)>) -> Value {
    let mut package = json!({
        "name": name,
        "version": "1.0.0",
        "version_normalized": "1.0.0.0",
        "type": "library"
    });
    if let Some((funding_type, url)) = funding {
        package["funding"] = json!([{"type": funding_type, "url": url}]);
    }
    package
}

fn project(manifest: Value, fundings: &[(&str, &str)]) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    write_json(&project.path().join("composer.json"), &manifest);
    let funding = |name: &str| {
        fundings
            .iter()
            .find(|(package, _)| *package == name)
            .map(|(_, url)| ("github", *url))
    };
    write_json(
        &project.path().join("vendor/composer/installed.json"),
        &json!({
            "packages": [
                installed_package("first/pkg", funding("first/pkg")),
                installed_package("stable/pkg", funding("stable/pkg")),
                installed_package("dev/pkg", funding("dev/pkg"))
            ],
            "dev": true,
            "dev-package-names": ["dev/pkg"]
        }),
    );
    project
}

fn fund(project: &Path, arguments: &[&str]) -> std::process::Output {
    Command::cargo_bin("composer")
        .unwrap()
        .env("COMPOSER_HOME", project.join("composer-home"))
        .arg("fund")
        .args(arguments)
        .arg("-d")
        .arg(project)
        .output()
        .unwrap()
}

#[test]
fn composer_fund_command_covers_local_remote_empty_and_json_cases() {
    let empty = project(
        json!({
            "repositories": [],
            "require": {"first/pkg": "^2.0"},
            "require-dev": {"dev/pkg": "~4.0"}
        }),
        &[],
    );
    let output = fund(empty.path(), &[]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "No funding links were found in your package dependencies. This doesn't mean they don't need your support!"
    );

    let local = project(
        json!({
            "repositories": [],
            "require": {"first/pkg": "^2.0"},
            "require-dev": {"dev/pkg": "~4.0"}
        }),
        &[
            ("first/pkg", "https://github.com/composer-test-data"),
            ("dev/pkg", "https://github.com/composer-test-data-dev"),
        ],
    );
    let output = fund(local.path(), &[]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "The following packages were found in your dependencies which publish funding information:\n\n\
dev\n  pkg\n    https://github.com/sponsors/composer-test-data-dev\n\n\
first\n    https://github.com/sponsors/composer-test-data\n\n\
Please consider following these links and sponsoring the work of package authors!\nThank you!"
    );

    let remote = project(
        json!({
            "repositories": [{
                "type": "package",
                "package": [
                    {"name": "first/pkg", "version": "dev-foo", "funding": [{"type": "github", "url": "https://github.com/test-should-not-be-used"}]},
                    {"name": "first/pkg", "version": "dev-main", "default-branch": true, "funding": [{"type": "custom", "url": "https://example.org"}]},
                    {"name": "dev/pkg", "version": "dev-foo", "default-branch": true, "funding": [{"type": "github", "url": "https://github.com/org"}]},
                    {"name": "stable/pkg", "version": "1.0.0", "funding": [{"type": "github", "url": "org2"}]}
                ]
            }],
            "require": {"first/pkg": "^2.0", "stable/pkg": "^1.0"},
            "require-dev": {"dev/pkg": "~4.0"}
        }),
        &[
            ("first/pkg", "https://github.com/composer-test-data"),
            ("dev/pkg", "https://github.com/composer-test-data-dev"),
            ("stable/pkg", "https://github.com/composer-test-data-stable"),
        ],
    );
    let output = fund(remote.path(), &[]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "The following packages were found in your dependencies which publish funding information:\n\n\
dev\n  pkg\n    https://github.com/sponsors/org\n\n\
first\n    https://example.org\n\n\
stable\n    https://github.com/sponsors/composer-test-data-stable\n\n\
Please consider following these links and sponsoring the work of package authors!\nThank you!"
    );

    let output = fund(local.path(), &["--format", "json"]);
    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json,
        json!({
            "dev": {"https://github.com/sponsors/composer-test-data-dev": ["pkg"]},
            "first": {"https://github.com/sponsors/composer-test-data": ["pkg"]}
        })
    );
}
