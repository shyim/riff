mod support;

use std::path::Path;

use assert_cmd::Command;
use serde_json::{json, Value};

fn run_fixture(source: &str) {
    support::composer_fixture::run(source);
}

// Ported from Composer\Test\Command\UpdateCommandTest::testUpdate.
#[test]
fn composer_update_command_covers_the_upstream_provider_semantics() {
    let base = |run: &str, expected: &str| {
        run_fixture(&format!(
            r#"
--TEST--
Update root and transitive dependencies from a deterministic inline repository
--COMPOSER--
{{
  "repositories":[{{"type":"package","package":[
    {{"name":"root/req","version":"1.0.0","type":"metapackage","require":{{"dep/pkg":"^1"}}}},
    {{"name":"dep/pkg","version":"1.0.0","type":"metapackage","replace":{{"replaced/pkg":"1.0.0"}}}},
    {{"name":"dep/pkg","version":"1.0.1","type":"metapackage","replace":{{"replaced/pkg":"1.0.1"}}}},
    {{"name":"dep/pkg","version":"1.0.2","type":"metapackage","replace":{{"replaced/pkg":"1.0.2"}}}}
  ]}}],
  "require":{{"root/req":"1.*"}}
}}
--RUN--
{run}
--EXPECT--
{expected}
"#,
        ));
    };
    base(
        "update",
        "Installing dep/pkg (1.0.2)\nInstalling root/req (1.0.0)",
    );
    base(
        "update -vv",
        "Installing dep/pkg (1.0.2)\nInstalling root/req (1.0.0)",
    );
    base(
        "update --with dep/pkg:1.0.0 --no-install",
        "Installing dep/pkg (1.0.0)\nInstalling root/req (1.0.0)",
    );
    base(
        "update --bump-after-update=dev",
        "Installing dep/pkg (1.0.2)\nInstalling root/req (1.0.0)",
    );

    run_fixture(
        r#"
--TEST--
A temporary transitive constraint can make the root graph unsatisfiable
--COMPOSER--
{"repositories":[{"type":"package","package":[{"name":"root/req","version":"1.0.0","type":"metapackage","require":{"dep/pkg":"^1"}},{"name":"dep/pkg","version":"1.0.0","type":"metapackage"},{"name":"dep/pkg","version":"1.0.2","type":"metapackage"}]}],"require":{"root/req":"1.*"}}
--RUN--
update --with dep/pkg:^2
--EXPECT-EXIT-CODE--
2
--EXPECT-OUTPUT--
Could not resolve dependencies for dep/pkg
--EXPECT--

"#,
    );
    run_fixture(
        r#"
--TEST--
A temporary root constraint must intersect composer.json
--COMPOSER--
{"repositories":[{"type":"package","package":[{"name":"root/req","version":"1.0.0","type":"metapackage"},{"name":"root/req","version":"2.0.0","type":"metapackage"}]}],"require":{"root/req":"1.*"}}
--RUN--
update --with root/req:^2
--EXPECT-EXIT-CODE--
1
--EXPECT-EXCEPTION--
temporary constraint
--EXPECT--
temporary constraint "^2" for root/req does not intersect the root constraint "1.*"
"#,
    );
    run_fixture(
        r#"
--TEST--
A temporary constraint for a replaced capability filters incompatible providers
--COMPOSER--
{"repositories":[{"type":"package","package":[{"name":"root/req","version":"1.0.0","type":"metapackage","require":{"dep/pkg":"^1"}},{"name":"dep/pkg","version":"1.0.0","type":"metapackage","replace":{"replaced/pkg":"1.0.0"}},{"name":"dep/pkg","version":"1.0.2","type":"metapackage","replace":{"replaced/pkg":"1.0.2"}}]}],"require":{"root/req":"1.*"}}
--RUN--
update --with replaced/pkg:^2 --bump-after-update
--EXPECT-EXIT-CODE--
2
--EXPECT-OUTPUT--
replaced/pkg
--EXPECT--

"#,
    );
    run_fixture(
        r#"
--TEST--
Lock metadata updates do not resolve new packages or run bumping
--COMPOSER--
{"repositories":[{"type":"package","package":[{"name":"root/req","version":"1.0.0","type":"metapackage"}]}],"require":{"root/req":"1.*"}}
--LOCK--
{"packages":[],"packages-dev":[]}
--INSTALLED--
[]
--RUN--
update --lock --bump-after-update
--EXPECT--

"#,
    );

    let two_dependencies = |run: &str, expected: &str, exit: i32| {
        let expected_output = if exit == 0 { "" } else { expected };
        let expected_operations = if exit == 0 { expected } else { "" };
        run_fixture(&format!(
            r#"
--TEST--
Partial and complete updates preserve locked dependency constraints
--COMPOSER--
{{"type":"project","repositories":[{{"type":"package","package":[{{"name":"acme/foo","version":"1.0.0","type":"metapackage"}},{{"name":"acme/foo","version":"1.2.0","type":"metapackage"}},{{"name":"acme/bar","version":"1.0.0","type":"metapackage","require":{{"acme/foo":"^1.0.0"}}}},{{"name":"acme/bar","version":"1.2.0","type":"metapackage","require":{{"acme/foo":"^1.2.0"}}}}]}}],"require":{{"acme/foo":"^1.0.0","acme/bar":"^1.0.0"}}}}
--LOCK--
{{"packages":[{{"name":"acme/foo","version":"1.0.0","type":"metapackage"}},{{"name":"acme/bar","version":"1.0.0","type":"metapackage","require":{{"acme/foo":"^1.0.0"}}}}],"packages-dev":[]}}
--INSTALLED--
[{{"name":"acme/foo","version":"1.0.0","type":"metapackage"}},{{"name":"acme/bar","version":"1.0.0","type":"metapackage","require":{{"acme/foo":"^1.0.0"}}}}]
--RUN--
{run}
--EXPECT-EXIT-CODE--
{exit}
--EXPECT-OUTPUT--
{expected_output}
--EXPECT--
{expected_operations}
"#,
        ));
    };
    two_dependencies(
        "update --bump-after-update",
        "Upgrading acme/foo (1.0.0 => 1.2.0)\nUpgrading acme/bar (1.0.0 => 1.2.0)",
        0,
    );
    two_dependencies("update acme/bar:^1.2 --bump-after-update", "acme/foo", 2);
    two_dependencies(
        "update acme/bar:^1.2 --with-all-dependencies --bump-after-update",
        "Upgrading acme/foo (1.0.0 => 1.2.0)\nUpgrading acme/bar (1.0.0 => 1.2.0)",
        0,
    );
}

// Ported from Composer\Test\Command\UpdateCommandTest::testUpdateWithPatchOnly.
#[test]
fn composer_update_patch_only_restricts_locked_packages_to_their_patch_series() {
    run_fixture(
        r#"
--TEST--
Patch-only intersects with temporary constraints
--COMPOSER--
{"repositories":[{"type":"package","package":[{"name":"root/req","version":"1.0.0","type":"metapackage"},{"name":"root/req","version":"1.0.1","type":"metapackage"},{"name":"root/req","version":"1.1.0","type":"metapackage"}]}],"require":{"root/req":"1.*"}}
--LOCK--
{"packages":[{"name":"root/req","version":"1.0.0","type":"metapackage"}],"packages-dev":[]}
--INSTALLED--
[{"name":"root/req","version":"1.0.0","type":"metapackage"}]
--RUN--
update --patch-only --with root/req:^1.1 root/req
--EXPECT-EXIT-CODE--
2
--EXPECT-OUTPUT--
Could not resolve dependencies
--EXPECT--

"#,
    );
    run_fixture(
        r#"
--TEST--
Patch-only updates allowlisted packages but keeps an unlisted package fixed
--COMPOSER--
{"repositories":[{"type":"package","package":[{"name":"root/req","version":"1.0.0","type":"metapackage"},{"name":"root/req","version":"1.0.1","type":"metapackage"},{"name":"root/req","version":"1.1.0","type":"metapackage"},{"name":"root/req2","version":"1.0.0","type":"metapackage"},{"name":"root/req2","version":"1.0.1","type":"metapackage"},{"name":"root/req2","version":"1.1.0","type":"metapackage"},{"name":"root/req3","version":"1.0.0","type":"metapackage"},{"name":"root/req3","version":"1.0.1","type":"metapackage"},{"name":"root/req3","version":"1.1.0","type":"metapackage"}]}],"require":{"root/req":"1.*","root/req2":"1.*","root/req3":"1.*"}}
--LOCK--
{"packages":[{"name":"root/req","version":"1.0.0","type":"metapackage"},{"name":"root/req2","version":"1.0.0","type":"metapackage"},{"name":"root/req3","version":"1.0.0","type":"metapackage"}],"packages-dev":[]}
--INSTALLED--
[{"name":"root/req","version":"1.0.0","type":"metapackage"},{"name":"root/req2","version":"1.0.0","type":"metapackage"},{"name":"root/req3","version":"1.0.0","type":"metapackage"}]
--RUN--
update --patch-only --with root/req:^1.0.1 root/req root/req2
--EXPECT--
Upgrading root/req (1.0.0 => 1.0.1)
Upgrading root/req2 (1.0.0 => 1.0.1)
"#,
    );
}

fn write_json(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn update_project(versions: &[&str]) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        &json!({
            "repositories": [
                {"type":"package", "package": versions.iter().map(|version| json!({"name":"root/req","version":version,"type":"metapackage"})).collect::<Vec<_>>()},
                {"packagist.org": false}
            ],
            "require":{"root/req":"1.*"}
        }),
    );
    write_json(
        &project.path().join("composer.lock"),
        &json!({"packages":[{"name":"root/req","version":"1.0.0","type":"metapackage"}],"packages-dev":[]}),
    );
    project
}

fn interactive_provider_project(include_another: bool) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        &json!({
            "repositories": [
                {"type":"package", "package":[
                    {"name":"root/req","version":"1.0.0","type":"metapackage","require":{"dep/pkg":"^1","another-dep/pkg":"^1"}},
                    {"name":"dep/pkg","version":"1.0.0","type":"metapackage"},
                    {"name":"dep/pkg","version":"1.0.1","type":"metapackage"},
                    {"name":"dep/pkg","version":"1.0.2","type":"metapackage"},
                    {"name":"another-dep/pkg","version":"1.0.2","type":"metapackage"}
                ]},
                {"packagist.org": false}
            ],
            "require":{"root/req":"1.*"}
        }),
    );
    let mut packages = vec![
        json!({"name":"root/req","version":"1.0.0","type":"metapackage","require":{"dep/pkg":"^1","another-dep/pkg":"^1"}}),
        json!({"name":"dep/pkg","version":"1.0.1","type":"metapackage"}),
    ];
    if include_another {
        packages.push(json!({"name":"another-dep/pkg","version":"1.0.2","type":"metapackage"}));
    }
    write_json(
        &project.path().join("composer.lock"),
        &json!({"packages":packages,"packages-dev":[]}),
    );
    project
}

fn update_command(project: &Path) -> Command {
    let mut command = Command::cargo_bin("composer").unwrap();
    command
        .env("COMPOSER_HOME", project.join("composer-home"))
        .env("RIFF_CACHE_DIR", project.join("cache"))
        .args([
            "update",
            "--interactive",
            "--no-audit",
            "--no-install",
            "-d",
        ])
        .arg(project);
    command
}

// Ported from Composer\Test\Command\UpdateCommandTest::testInteractiveModeThrowsIfNoPackageToUpdate.
#[test]
fn composer_interactive_update_reports_when_no_new_versions_exist() {
    let project = update_project(&["1.0.0"]);
    let output = update_command(project.path())
        .write_stdin("\n")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("Could not find any package with new versions available"));
}

// Ported from Composer\Test\Command\UpdateCommandTest::testInteractiveModeThrowsIfNoPackageEntered.
#[test]
fn composer_interactive_update_rejects_an_empty_first_selection() {
    let project = update_project(&["1.0.0", "1.0.1"]);
    let output = update_command(project.path())
        .write_stdin("\n")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("No package named \"\" is installed."));
}

// Ported from Composer\Test\Command\UpdateCommandTest::testInteractiveTmp.
#[test]
fn composer_interactive_update_uses_the_confirmed_package_selection() {
    for (include_another, input) in [
        (false, "dep/pkg\n\nyes\n"),
        (true, "dep/pkg\nanother-dep/pkg\n\nyes\n"),
    ] {
        let project = interactive_provider_project(include_another);
        let output = update_command(project.path())
            .write_stdin(input)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let lock: Value =
            serde_json::from_slice(&std::fs::read(project.path().join("composer.lock")).unwrap())
                .unwrap();
        let packages = lock["packages"].as_array().unwrap();
        let version = |name: &str| {
            packages
                .iter()
                .find(|package| package["name"] == name)
                .and_then(|package| package["version"].as_str())
        };
        assert_eq!(version("dep/pkg"), Some("1.0.2"));
        assert_eq!(version("another-dep/pkg"), Some("1.0.2"));
        assert_eq!(version("root/req"), Some("1.0.0"));
    }
}

// Ported from Composer\Test\Command\UpdateCommandTest::testNoSecurityBlockingAllowsInsecurePackages.
#[test]
fn composer_update_no_security_blocking_controls_advisory_exclusion() {
    let fixture = |flag: &str, version: &str| {
        run_fixture(&format!(
            r#"
--TEST--
Security blocking can be disabled explicitly
--COMPOSER--
{{"repositories":[{{"type":"package","package":[{{"name":"vulnerable/pkg","version":"1.0.0","type":"metapackage"}},{{"name":"vulnerable/pkg","version":"1.1.0","type":"metapackage"}}],"security-advisories":{{"vulnerable/pkg":[{{"advisoryId":"PKSA-test-001","packageName":"vulnerable/pkg","affectedVersions":">=1.1.0,<2.0.0"}}]}}}}],"require":{{"vulnerable/pkg":"^1.0"}}}}
--RUN--
update {flag}
--EXPECT--
Installing vulnerable/pkg ({version})
"#,
        ));
    };
    fixture("", "1.0.0");
    fixture("--no-security-blocking", "1.1.0");
}

// Ported from Composer\Test\Command\UpdateCommandTest::testNoBlockingAllowsMalwareFlaggedPackages.
#[test]
fn composer_update_no_blocking_controls_malware_exclusion() {
    let fixture = |flag: &str, version: &str| {
        run_fixture(&format!(
            r#"
--TEST--
Malware blocking can be disabled explicitly
--COMPOSER--
{{"repositories":[{{"type":"package","package":[{{"name":"malicious/pkg","version":"1.0.0","type":"metapackage"}},{{"name":"malicious/pkg","version":"1.1.0","type":"metapackage"}}],"filter":{{"malware":[{{"package":"malicious/pkg","constraint":">=1.1.0","reason":"malware"}}]}}}}],"require":{{"malicious/pkg":"^1.0"}}}}
--RUN--
update {flag}
--EXPECT--
Installing malicious/pkg ({version})
"#,
        ));
    };
    fixture("", "1.0.0");
    fixture("--no-security-blocking", "1.1.0");
    fixture("--no-blocking", "1.1.0");
}

// Ported from Composer\Test\Command\UpdateCommandTest::testBumpAfterUpdateWithoutLockfile.
#[test]
fn composer_update_bumps_projected_versions_when_lock_writing_is_disabled() {
    let project = tempfile::tempdir().unwrap();
    write_json(
        &project.path().join("composer.json"),
        &json!({
            "repositories":[{"type":"package","package":[{"name":"root/a","version":"1.0.0","type":"metapackage"},{"name":"root/a","version":"1.1.0","type":"metapackage"}]},{"packagist.org":false}],
            "require-dev":{"root/a":"^1.0.0"},
            "config":{"lock":false}
        }),
    );
    let output = Command::cargo_bin("composer")
        .unwrap()
        .env("COMPOSER_HOME", project.path().join("composer-home"))
        .env("RIFF_CACHE_DIR", project.path().join("cache"))
        .args([
            "update",
            "--dry-run",
            "--no-audit",
            "--no-install",
            "--bump-after-update=dev",
            "-d",
        ])
        .arg(project.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("require-dev.root/a: ^1.1.0"), "{stdout}");
    assert!(!project.path().join("composer.lock").exists());
}

// Ported from Composer\Test\Command\UpdateCommandTest::testUpdateWithTemporaryConstraintUsingWildcard.
#[test]
fn composer_update_wildcard_temporary_constraints_apply_to_every_matching_root() {
    let fixture = |constraint: &str, expected: &str| {
        run_fixture(&format!(
            r#"
--TEST--
Wildcard temporary constraints expand against matching root requirements
--COMPOSER--
{{"repositories":[{{"type":"package","package":[{{"name":"root/a","version":"1.0.0","type":"metapackage"}},{{"name":"root/a","version":"2.0.0","type":"metapackage"}},{{"name":"root/ab","version":"1.0.0","type":"metapackage"}},{{"name":"root/ab","version":"2.0.0","type":"metapackage"}},{{"name":"root/abc","version":"1.0.0","type":"metapackage"}},{{"name":"root/abc","version":"2.0.0","type":"metapackage"}}]}}],"require":{{"root/a":"^1 || ^2","root/ab":"^1 || ^2","root/abc":"^1 || ^2"}}}}
--LOCK--
{{"packages":[{{"name":"root/a","version":"2.0.0","type":"metapackage"}},{{"name":"root/ab","version":"2.0.0","type":"metapackage"}},{{"name":"root/abc","version":"2.0.0","type":"metapackage"}}],"packages-dev":[]}}
--INSTALLED--
[{{"name":"root/a","version":"2.0.0","type":"metapackage"}},{{"name":"root/ab","version":"2.0.0","type":"metapackage"}},{{"name":"root/abc","version":"2.0.0","type":"metapackage"}}]
--RUN--
update --with {constraint}
--EXPECT--
{expected}
"#,
        ));
    };
    fixture(
        "root/*:^1",
        "Downgrading root/a (2.0.0 => 1.0.0)\nDowngrading root/ab (2.0.0 => 1.0.0)\nDowngrading root/abc (2.0.0 => 1.0.0)",
    );
    fixture(
        "root/ab*:^1",
        "Downgrading root/ab (2.0.0 => 1.0.0)\nDowngrading root/abc (2.0.0 => 1.0.0)",
    );
}

// Ported from Composer\Test\Command\UpdateCommandTest::testUpdateWithTemporaryConstraintWildcardFailsIntersection.
#[test]
fn composer_update_wildcard_temporary_constraint_reports_root_intersection_errors() {
    run_fixture(
        r#"
--TEST--
A wildcard temporary constraint must intersect every matching root requirement
--COMPOSER--
{"repositories":[{"type":"package","package":[{"name":"root/a","version":"1.0.0","type":"metapackage"},{"name":"root/a","version":"2.0.0","type":"metapackage"},{"name":"root/ab","version":"1.0.0","type":"metapackage"},{"name":"root/ab","version":"2.0.0","type":"metapackage"}]}],"require":{"root/a":"^1","root/ab":"^1"}}
--LOCK--
{"packages":[{"name":"root/a","version":"1.0.0","type":"metapackage"},{"name":"root/ab","version":"1.0.0","type":"metapackage"}],"packages-dev":[]}
--RUN--
update --with root/*:^2
--EXPECT-EXIT-CODE--
1
--EXPECT-EXCEPTION--
temporary constraint
--EXPECT--
temporary constraint "^2" for root/a does not intersect the root constraint "^1"
"#,
    );
}

// Ported from Composer\Test\Command\UpdateCommandTest::testUpdateWithTemporaryConstraintWildcardMatchingNothing.
#[test]
fn composer_update_wildcard_temporary_constraint_matching_nothing_is_a_noop() {
    run_fixture(
        r#"
--TEST--
A wildcard temporary constraint matching no roots leaves the lock unchanged
--COMPOSER--
{"repositories":[{"type":"package","package":[{"name":"root/a","version":"1.0.0","type":"metapackage"},{"name":"root/a","version":"2.0.0","type":"metapackage"}]}],"require":{"root/a":"^1 || ^2"}}
--LOCK--
{"packages":[{"name":"root/a","version":"2.0.0","type":"metapackage"}],"packages-dev":[]}
--INSTALLED--
[{"name":"root/a","version":"2.0.0","type":"metapackage"}]
--RUN--
update --with other/*:^1
--EXPECT--

"#,
    );
}
