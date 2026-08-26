use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::{json, Value};

fn write_json(path: &Path, value: Value) {
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn string_map(entries: &[(&str, &str)]) -> serde_json::Map<String, Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_string(), Value::String((*value).to_string())))
        .collect()
}

fn package(
    name: &str,
    suggests: &[(&str, &str)],
    require: &[(&str, &str)],
    require_dev: &[(&str, &str)],
) -> Value {
    json!({
        "name": name,
        "version": "1.0.0",
        "version_normalized": "1.0.0.0",
        "type": "library",
        "suggest": string_map(suggests),
        "require": string_map(require),
        "require-dev": string_map(require_dev),
    })
}

fn suggestion_project() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({
            "name": "fixture/root",
            "require": {"vendor1/package1": "^1"},
            "require-dev": {"vendor2/package2": "^1"}
        }),
    );
    let production = vec![
        package(
            "vendor1/package1",
            &[("vendor3/suggested", "helpful for vendor1/package1")],
            &[("vendor6/package6", "^1.0")],
            &[
                ("vendor3/suggested", "^1.0"),
                ("vendor4/dev-suggested", "^1.0"),
            ],
        ),
        package(
            "vendor6/package6",
            &[("vendor7/transitive", "helpful for vendor6/package6")],
            &[],
            &[],
        ),
    ];
    let development = vec![
        package(
            "vendor2/package2",
            &[("vendor4/dev-suggested", "helpful for vendor2/package2")],
            &[("vendor5/dev-package", "^1.0")],
            &[],
        ),
        package(
            "vendor5/dev-package",
            &[("vendor8/dev-transitive", "helpful for vendor5/dev-package")],
            &[],
            &[("vendor8/dev-transitive", "^1.0")],
        ),
    ];
    fs::create_dir_all(project.path().join("vendor/composer")).unwrap();
    write_json(
        &project.path().join("vendor/composer/installed.json"),
        json!({
            "packages": production.iter().chain(&development).collect::<Vec<_>>(),
            "dev": true,
            "dev-package-names": ["vendor2/package2", "vendor5/dev-package"]
        }),
    );
    write_json(
        &project.path().join("composer.lock"),
        json!({
            "content-hash": "fixture",
            "packages": production,
            "packages-dev": development
        }),
    );
    project
}

fn suggests(project: &Path, arguments: &[&str]) -> String {
    let output = Command::cargo_bin("riff")
        .unwrap()
        .env("COMPOSER_HOME", project.join("composer-home"))
        .arg("suggest")
        .args(arguments)
        .args(["-d"])
        .arg(project)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "riff exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

// Ported from Composer\Test\Command\SuggestsCommandTest::
// testInstalledPackagesWithNoSuggestions.
#[test]
fn composer_suggests_command_is_silent_without_suggestions() {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({
            "require": {
                "vendor1/package1": "1.*",
                "vendor2/package2": "1.*"
            }
        }),
    );
    let packages = [
        package("vendor1/package1", &[], &[], &[]),
        package("vendor2/package2", &[], &[], &[]),
    ];
    write_json(
        &project.path().join("composer.lock"),
        json!({"packages": packages, "packages-dev": []}),
    );

    assert_eq!(suggests(project.path(), &[]), "");
}

// Ported from Composer\Test\Command\SuggestsCommandTest::testSuggest.
#[test]
fn composer_suggests_command_supports_scope_grouping_filters_and_lists() {
    let project = suggestion_project();
    let by_package = "vendor1/package1 suggests:\n - vendor3/suggested: helpful for vendor1/package1\n\nvendor2/package2 suggests:\n - vendor4/dev-suggested: helpful for vendor2/package2";
    let by_package_prod =
        "vendor1/package1 suggests:\n - vendor3/suggested: helpful for vendor1/package1";
    let all_by_package = "vendor1/package1 suggests:\n - vendor3/suggested: helpful for vendor1/package1\n\nvendor2/package2 suggests:\n - vendor4/dev-suggested: helpful for vendor2/package2\n\nvendor5/dev-package suggests:\n - vendor8/dev-transitive: helpful for vendor5/dev-package\n\nvendor6/package6 suggests:\n - vendor7/transitive: helpful for vendor6/package6";
    let all_by_package_prod = "vendor1/package1 suggests:\n - vendor3/suggested: helpful for vendor1/package1\n\nvendor6/package6 suggests:\n - vendor7/transitive: helpful for vendor6/package6";
    let by_suggestion = "vendor3/suggested is suggested by:\n - vendor1/package1: helpful for vendor1/package1\n\nvendor4/dev-suggested is suggested by:\n - vendor2/package2: helpful for vendor2/package2";
    let by_suggestion_prod =
        "vendor3/suggested is suggested by:\n - vendor1/package1: helpful for vendor1/package1";
    let hint = "2 additional suggestions by transitive dependencies can be shown with --all";
    let hint_prod = "1 additional suggestions by transitive dependencies can be shown with --all";
    let separator = "-".repeat(78);
    let both = format!("{by_package}\n\n{separator}\n{by_suggestion}");
    let both_prod = format!("{by_package_prod}\n\n{separator}\n{by_suggestion_prod}");

    let cases =
        vec![
        (vec![], format!("{by_package}\n\n{hint}"), format!("{by_package}\n\n{hint}")),
        (
            vec!["--no-dev"],
            format!("{by_package_prod}\n\n{hint_prod}"),
            format!("{by_package}\n\n{hint}"),
        ),
        (vec!["--all"], all_by_package.to_string(), all_by_package.to_string()),
        (
            vec!["--all", "--no-dev"],
            all_by_package_prod.to_string(),
            all_by_package.to_string(),
        ),
        (
            vec!["--by-package"],
            format!("{by_package}\n\n{hint}"),
            format!("{by_package}\n\n{hint}"),
        ),
        (
            vec!["--by-package", "--no-dev"],
            format!("{by_package_prod}\n\n{hint_prod}"),
            format!("{by_package}\n\n{hint}"),
        ),
        (
            vec!["--by-suggestion"],
            format!("{by_suggestion}\n\n{hint}"),
            format!("{by_suggestion}\n\n{hint}"),
        ),
        (
            vec!["--by-suggestion", "--no-dev"],
            format!("{by_suggestion_prod}\n\n{hint_prod}"),
            format!("{by_suggestion}\n\n{hint}"),
        ),
        (
            vec!["--by-package", "--by-suggestion"],
            format!("{both}\n\n{hint}"),
            format!("{both}\n\n{hint}"),
        ),
        (
            vec!["--by-package", "--by-suggestion", "--no-dev"],
            format!("{both_prod}\n\n{hint_prod}"),
            format!("{both}\n\n{hint}"),
        ),
        (
            vec!["vendor2/package2"],
            "vendor2/package2 suggests:\n - vendor4/dev-suggested: helpful for vendor2/package2"
                .to_string(),
            "vendor2/package2 suggests:\n - vendor4/dev-suggested: helpful for vendor2/package2"
                .to_string(),
        ),
        (
            vec!["--list"],
            "vendor3/suggested\nvendor4/dev-suggested".to_string(),
            "vendor3/suggested\nvendor4/dev-suggested".to_string(),
        ),
        (
            vec!["--list", "--no-dev"],
            "vendor3/suggested".to_string(),
            "vendor3/suggested\nvendor4/dev-suggested".to_string(),
        ),
        (
            vec!["--list", "--all"],
            "vendor3/suggested\nvendor4/dev-suggested\nvendor7/transitive\nvendor8/dev-transitive"
                .to_string(),
            "vendor3/suggested\nvendor4/dev-suggested\nvendor7/transitive\nvendor8/dev-transitive"
                .to_string(),
        ),
        (
            vec!["--list", "--all", "--no-dev"],
            "vendor3/suggested\nvendor7/transitive".to_string(),
            "vendor3/suggested\nvendor4/dev-suggested\nvendor7/transitive\nvendor8/dev-transitive"
                .to_string(),
        ),
    ];

    for (arguments, locked, _) in &cases {
        assert_eq!(
            suggests(project.path(), arguments),
            locked.as_str(),
            "{arguments:?}"
        );
    }

    fs::remove_file(project.path().join("composer.lock")).unwrap();
    for (arguments, _, unlocked) in &cases {
        assert_eq!(
            suggests(project.path(), arguments),
            unlocked.as_str(),
            "without lock: {arguments:?}"
        );
    }
}
