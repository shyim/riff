#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use assert_cmd::Command;
    use predicates::prelude::*;

    fn write_json(path: &Path, value: serde_json::Value) {
        fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    fn fake_php(project: &Path) -> std::path::PathBuf {
        let php = project.join("php-probe");
        fs::write(
            &php,
            "#!/bin/sh\nprintf '%s' '{\"php_version\":\"8.5.0\",\"php_version_id\":80500,\"int_size\":8,\"zts\":false,\"debug\":false,\"ipv6\":true,\"extensions\":{},\"libraries\":{}}'\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&php).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&php, permissions).unwrap();
        php
    }

    fn package(name: &str, version: &str) -> serde_json::Value {
        serde_json::json!({"name": name, "version": version, "type": "metapackage"})
    }

    fn platform_project() -> tempfile::TempDir {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({
                "name": "fixture/project",
                "require": {"ext-foobar": "^2.0"},
                "require-dev": {"ext-barbaz": "~4.0"}
            }),
        );
        fs::create_dir_all(project.path().join("vendor/composer")).unwrap();
        write_json(
            &project.path().join("vendor/composer/installed.json"),
            serde_json::json!({
                "packages": [package("ext-foobar", "2.3.4"), package("ext-barbaz", "2.3.4.5")],
                "dev": true,
                "dev-package-names": ["ext-barbaz"]
            }),
        );
        write_json(
            &project.path().join("composer.lock"),
            serde_json::json!({
                "content-hash": "",
                "packages": [package("ext-foobar", "2.3.4")],
                "packages-dev": [package("ext-barbaz", "2.3.4.5")]
            }),
        );
        project
    }

    // Ported from Composer\Test\Command\CheckPlatformReqsCommandTest::
    // testPlatformReqsAreSatisfied.
    #[test]
    fn composer_check_platform_reqs_supports_no_dev_and_lock_sources() {
        let project = platform_project();
        let php = fake_php(project.path());

        Command::cargo_bin("riff")
            .unwrap()
            .arg("--php")
            .arg(&php)
            .args(["check-platform-reqs", "--no-dev", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicate::str::contains("ext-foobar"))
            .stdout(predicate::str::contains("success"))
            .stdout(predicate::str::contains("ext-barbaz").not());

        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({
                "name": "fixture/project",
                "require": {"ext-foobar": "^2.3"},
                "require-dev": {"ext-barbaz": "~2.0"}
            }),
        );
        Command::cargo_bin("riff")
            .unwrap()
            .arg("--php")
            .arg(&php)
            .args(["check-platform-reqs", "--lock", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicate::str::contains("ext-foobar"))
            .stdout(predicate::str::contains("ext-barbaz"));
    }

    // Ported from Composer\Test\Command\CheckPlatformReqsCommandTest::
    // testExceptionThrownIfNoLockfileFound.
    #[test]
    fn composer_check_platform_reqs_requires_lock_when_vendor_is_absent() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({"name": "fixture/project"}),
        );
        let php = fake_php(project.path());

        Command::cargo_bin("riff")
            .unwrap()
            .arg("--php")
            .arg(php)
            .args(["check-platform-reqs", "-d"])
            .arg(project.path())
            .assert()
            .failure()
            .stderr(predicate::str::contains("No composer.lock found"));
    }

    // Ported from Composer\Test\Command\CheckPlatformReqsCommandTest::
    // testFailedPlatformRequirement.
    #[test]
    fn composer_check_platform_reqs_reports_failed_requirement_as_json() {
        let project = platform_project();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({
                "name": "fixture/project",
                "require": {"ext-foobar": "^0.3"},
                "require-dev": {"ext-barbaz": "^2.3"}
            }),
        );
        let php = fake_php(project.path());

        Command::cargo_bin("riff")
            .unwrap()
            .arg("--php")
            .arg(php)
            .args(["check-platform-reqs", "--format", "json", "-d"])
            .arg(project.path())
            .assert()
            .code(1)
            .stdout(predicate::str::contains("\"name\": \"ext-foobar\""))
            .stdout(predicate::str::contains("\"status\": \"failed\""))
            .stdout(predicate::str::contains("\"constraint\": \"^0.3\""));
    }
}
