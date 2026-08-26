use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::json;

fn write_json(path: &Path, value: serde_json::Value) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn write_lock(project: &Path, packages: serde_json::Value) {
    write_json(
        &project.join("composer.lock"),
        json!({"content-hash": "", "packages": packages, "packages-dev": []}),
    );
}

fn write_installed(project: &Path, packages: serde_json::Value) {
    write_json(
        &project.join("vendor/composer/installed.json"),
        json!({"packages": packages, "dev": true, "dev-package-names": []}),
    );
}

fn command(project: &Path) -> Command {
    let mut command = Command::cargo_bin("riff").unwrap();
    command.env("RIFF_CACHE_DIR", project.join("riff-cache"));
    command
}

fn output(project: &Path, arguments: &[&str]) -> std::process::Output {
    command(project)
        .arg("show")
        .args(arguments)
        .args(["-d"])
        .arg(project)
        .output()
        .unwrap()
}

// Ported from Composer\Test\Command\ShowCommandTest::testInvalidOptionCombinations.
#[test]
fn composer_show_rejects_invalid_option_combinations() {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({"name": "fixture/project"}),
    );
    for options in [
        &["--direct", "--all"][..],
        &["--direct", "--available"],
        &["--direct", "--platform"],
        &["--tree", "--all"],
        &["--tree", "--available"],
        &["--tree", "--latest"],
        &["--tree", "--path"],
        &["--format", "test"],
        &["--patch-only", "--minor-only"],
        &["--minor-only", "--major-only"],
    ] {
        assert!(
            !output(project.path(), options).status.success(),
            "{options:?}"
        );
    }
}

// Ported from Composer\Test\Command\ShowCommandTest::testIgnoredOptionCombinations.
#[test]
fn composer_show_warns_for_deprecated_or_ineffective_options() {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({"name": "fixture/project"}),
    );
    let installed = output(project.path(), &["--installed"]);
    assert!(installed.status.success());
    assert!(String::from_utf8_lossy(&installed.stderr).contains("deprecated option \"installed\""));

    let ignored = output(project.path(), &["--ignore", "vendor/package"]);
    assert!(ignored.status.success());
    assert!(String::from_utf8_lossy(&ignored.stderr).contains("option \"ignore\""));
}

// Ported from Composer\Test\Command\ShowCommandTest::testSelf.
#[test]
fn composer_show_displays_root_package_metadata() {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({
            "name": "vendor/package",
            "version": "1.2.3",
            "time": "2026-08-25"
        }),
    );
    let run = output(project.path(), &["--self"]);
    assert!(run.status.success());
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("name     : vendor/package"));
    assert!(stdout.contains("version  : 1.2.3"));
    assert!(stdout.contains("type     : library"));
}

// Ported from Composer\Test\Command\ShowCommandTest::testNotExistingPackage.
#[test]
fn composer_show_reports_missing_packages_by_source() {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({"name": "fixture/project", "require": {"vendor/package": "1.0.0"}}),
    );
    write_lock(project.path(), json!([]));
    write_installed(project.path(), json!([]));

    for (arguments, expected) in [
        (&["not/existing"][..], "--available"),
        (&["not/existing", "--all"], "not found."),
        (&["not/existing", "--locked"], "not found in lock file"),
        (&["ext-nonexisting", "--platform"], "--available"),
        (&["ext-nonexisting"], "--platform"),
    ] {
        let run = output(project.path(), arguments);
        assert!(!run.status.success(), "{arguments:?}");
        assert!(
            String::from_utf8_lossy(&run.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
    }
}

// Ported from Composer\Test\Command\ShowCommandTest::testNotExistingPackageWithWorkingDir.
#[test]
fn composer_show_missing_package_mentions_explicit_working_directory() {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({"name": "fixture/project"}),
    );
    // Preserve the path exactly as passed through -d, even when resolving the
    // directory for file access removes a redundant component.
    let requested_working_dir = project.path().join(".");
    let run = output(&requested_working_dir, &["not/existing"]);
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains(&format!(
        "not found in {}/composer.json",
        requested_working_dir.display()
    )));
}

// Ported from Composer\Test\Command\ShowCommandTest::testSpecificPackageAndTree.
#[test]
fn composer_show_specific_package_tree_supports_dependencies_platform_and_json() {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({"name": "fixture/project", "require": {"vendor/package": "1.0.0"}}),
    );
    write_installed(
        project.path(),
        json!([
            {
                "name": "vendor/package",
                "version": "1.0.0",
                "version_normalized": "1.0.0.0",
                "type": "library",
                "require": {"vendor/required-package": "1.0.0", "php": "8.2.0"},
                "install-path": "../vendor/package"
            },
            {
                "name": "vendor/required-package",
                "version": "1.0.0",
                "version_normalized": "1.0.0.0",
                "type": "library",
                "install-path": "../vendor/required-package"
            }
        ]),
    );
    let tree = output(project.path(), &["vendor/package", "--tree"]);
    assert!(tree.status.success());
    let stdout = String::from_utf8_lossy(&tree.stdout);
    assert!(stdout.contains("vendor/package 1.0.0"));
    assert!(stdout.contains("vendor/required-package 1.0.0"));
    assert!(stdout.contains("php (8.2.0)"));

    let json = output(
        project.path(),
        &["vendor/package", "--tree", "--format", "json"],
    );
    assert!(json.status.success());
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["installed"][0]["name"], "vendor/package");
}

fn outdated_fixture(requirement: serde_json::Value) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({
            "name": "fixture/project",
            "repositories": [
                {"packagist.org": false},
                {"type": "package", "package": requirement}
            ]
        }),
    );
    write_installed(
        project.path(),
        json!([{
            "name": "vendor/package",
            "version": "1.1.0",
            "version_normalized": "1.1.0.0",
            "type": "library",
            "install-path": "../vendor/package"
        }]),
    );
    project
}

// Ported from Composer\Test\Command\ShowCommandTest::
// testOutdatedFiltersAccordingToPlatformReqsAndWarns.
#[test]
fn composer_outdated_filters_missing_platform_requirements_and_warns() {
    let project = outdated_fixture(json!([
        {"name": "vendor/package", "version": "1.0.0"},
        {"name": "vendor/package", "version": "1.1.0", "require": {"ext-missing": "3"}},
        {"name": "vendor/package", "version": "1.2.0", "require": {"ext-missing": "3"}},
        {"name": "vendor/package", "version": "1.3.0", "require": {"ext-missing": "3"}}
    ]));
    let run = command(project.path())
        .args(["outdated", "-d"])
        .arg(project.path())
        .output()
        .unwrap();
    assert!(run.status.success());
    assert!(String::from_utf8_lossy(&run.stdout).contains("1.0.0"));
    assert!(String::from_utf8_lossy(&run.stderr)
        .contains("Cannot use vendor/package 1.1.0 as it requires ext-missing 3"));

    let verbose = command(project.path())
        .args(["outdated", "--verbose", "-d"])
        .arg(project.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&verbose.stderr);
    for version in ["1.1.0", "1.2.0", "1.3.0"] {
        assert!(stderr.contains(version), "{stderr}");
    }
}

// Ported from Composer\Test\Command\ShowCommandTest::
// testOutdatedFiltersAccordingToPlatformReqsWithoutWarningForHigherVersions.
#[test]
fn composer_outdated_silently_filters_higher_platform_requirements() {
    let project = outdated_fixture(json!([
        {"name": "vendor/package", "version": "1.0.0"},
        {"name": "vendor/package", "version": "1.1.0"},
        {"name": "vendor/package", "version": "1.2.0"},
        {"name": "vendor/package", "version": "1.3.0", "require": {"php": "^99"}}
    ]));
    let run = command(project.path())
        .args(["outdated", "-d"])
        .arg(project.path())
        .output()
        .unwrap();
    assert!(run.status.success());
    assert!(String::from_utf8_lossy(&run.stdout).contains("1.2.0"));
    assert!(!String::from_utf8_lossy(&run.stderr).contains("Cannot use"));
}

// Ported from Composer\Test\Command\ShowCommandTest::testShowAllShowsAllSections.
#[test]
fn composer_show_all_lists_every_section() {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({
            "name": "fixture/project",
            "repositories": [
                {"packagist.org": false},
                {"type": "package", "package": {"name": "vendor/available", "description": "generic description", "version": "1.0.0"}}
            ]
        }),
    );
    write_lock(
        project.path(),
        json!([{"name": "vendor/locked", "version": "3.0.0", "description": "locked"}]),
    );
    write_installed(
        project.path(),
        json!([{
            "name": "vendor/installed",
            "version": "2.0.0",
            "version_normalized": "2.0.0.0",
            "description": "installed",
            "type": "library",
            "install-path": "../vendor/installed"
        }]),
    );
    let run = output(project.path(), &["--all"]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    for expected in [
        "platform:",
        "locked:",
        "vendor/locked",
        "available:",
        "vendor/available",
        "installed:",
        "vendor/installed",
    ] {
        assert!(stdout.contains(expected), "missing {expected} in {stdout}");
    }
}

// Ported from Composer\Test\Command\ShowCommandTest::testShow, including its
// installed, self, locked, available, direct, outdated, SemVer-filter and
// age-sort data-provider modes.
#[test]
fn composer_show_covers_installed_available_direct_self_and_outdated_modes() {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        json!({
            "name": "root/pkg",
            "version": "1.2.3",
            "require": {"outdated/major": "*"},
            "repositories": [
                {"packagist.org": false},
                {"type": "package", "package": [
                    {"name": "vendor/package", "description": "generic description", "version": "1.0.0"},
                    {"name": "outdated/major", "description": "major one", "version": "1.0.0"},
                    {"name": "outdated/major", "description": "major two", "version": "2.0.0"},
                    {"name": "outdated/minor", "description": "minor one", "version": "1.0.0"},
                    {"name": "outdated/minor", "description": "minor latest", "version": "1.1.1"},
                    {"name": "outdated/patch", "description": "patch one", "version": "1.0.0"},
                    {"name": "outdated/patch", "description": "patch latest", "version": "1.0.1"}
                ]}
            ]
        }),
    );
    write_installed(
        project.path(),
        json!([
            {"name": "vendor/package", "version": "1.0.0", "version_normalized": "1.0.0.0", "description": "installed description", "type": "library", "install-path": "../vendor/package"},
            {"name": "outdated/major", "version": "1.0.0", "version_normalized": "1.0.0.0", "time": "2026-08-25T00:00:00+00:00", "type": "library", "install-path": "../outdated/major"},
            {"name": "outdated/minor", "version": "1.0.0", "version_normalized": "1.0.0.0", "time": "2024-08-25T00:00:00+00:00", "type": "library", "install-path": "../outdated/minor"},
            {"name": "outdated/patch", "version": "1.0.0", "version_normalized": "1.0.0.0", "time": "2026-08-11T00:00:00+00:00", "type": "library", "install-path": "../outdated/patch"}
        ]),
    );
    write_lock(
        project.path(),
        json!([{"name": "vendor/locked", "version": "3.0.0", "description": "locked description"}]),
    );

    let installed = output(project.path(), &[]);
    assert!(installed.status.success());
    let installed = String::from_utf8_lossy(&installed.stdout);
    for name in [
        "vendor/package",
        "outdated/major",
        "outdated/minor",
        "outdated/patch",
    ] {
        assert!(installed.contains(name));
    }

    let available = output(project.path(), &["--available"]);
    assert!(available.status.success());
    let available = String::from_utf8_lossy(&available.stdout);
    assert!(available.contains("major two"));
    assert!(available.contains("minor latest"));
    assert!(available.contains("patch latest"));

    let direct = output(project.path(), &["--direct"]);
    let direct = String::from_utf8_lossy(&direct.stdout);
    assert!(direct.contains("outdated/major"));
    assert!(!direct.contains("outdated/minor"));

    for options in [&["--installed", "--self"][..], &["--locked", "--self"]] {
        let run = output(project.path(), options);
        assert!(run.status.success());
        assert!(String::from_utf8_lossy(&run.stdout).contains("root/pkg"));
    }

    for (filter, expected, absent) in [
        (None, "outdated/major", None),
        (
            Some("--major-only"),
            "outdated/major",
            Some("outdated/minor"),
        ),
        (
            Some("--minor-only"),
            "outdated/minor",
            Some("outdated/major"),
        ),
        (
            Some("--patch-only"),
            "outdated/patch",
            Some("outdated/minor"),
        ),
    ] {
        let mut arguments = vec!["outdated"];
        if let Some(filter) = filter {
            arguments.push(filter);
        }
        arguments.extend(["-d", project.path().to_str().unwrap()]);
        let run = command(project.path()).args(&arguments).output().unwrap();
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(stdout.contains(expected), "{stdout}");
        if let Some(absent) = absent {
            assert!(!stdout.contains(absent), "{stdout}");
        }
    }

    let age_sorted = command(project.path())
        .args(["outdated", "--sort-by-age", "-d"])
        .arg(project.path())
        .output()
        .unwrap();
    assert!(age_sorted.status.success());
    let age_sorted = String::from_utf8_lossy(&age_sorted.stdout);
    assert!(
        age_sorted.find("outdated/minor") < age_sorted.find("outdated/patch"),
        "{age_sorted}"
    );
}
