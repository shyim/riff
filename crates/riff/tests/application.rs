#[cfg(unix)]
mod unix {
    use std::fs;

    use assert_cmd::Command;
    use predicates::prelude::*;

    fn project_with_script(name: &str, command: &str) -> tempfile::TempDir {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("composer.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "fixture/project",
                "scripts": {name: command}
            }))
            .unwrap(),
        )
        .unwrap();
        project
    }

    // Ported from Composer\Test\Console\ApplicationTest::testDevWarning.
    #[test]
    fn composer_application_emits_configured_development_warning() {
        Command::cargo_bin("riff")
            .unwrap()
            .env("RIFF_DEV_WARNING_TIME", "1")
            .arg("about")
            .assert()
            .success()
            .stderr(predicate::str::contains(
                "development build of Riff is over 60 days old",
            ));
    }

    // Ported from Composer\Test\Console\ApplicationTest::testDevWarningSuppressedForSelfUpdate.
    #[test]
    fn composer_application_suppresses_dev_warning_for_self_update_process() {
        Command::cargo_bin("riff")
            .unwrap()
            .env("RIFF_DEV_WARNING_TIME", "1")
            .arg("self-update")
            .assert()
            .failure()
            .stderr(predicate::str::contains("development build").not());
    }

    // Ported from Composer\Test\Console\ApplicationTest::testProcessIsolationWorksMultipleTimes.
    #[test]
    fn composer_application_process_isolation_is_repeatable() {
        for _ in 0..2 {
            Command::cargo_bin("riff")
                .unwrap()
                .arg("about")
                .assert()
                .success()
                .stdout(predicate::str::contains("Riff"));
        }
    }

    // Ported from Composer\Test\Console\ApplicationTest::
    // testScriptCommandTakesPriorityOverAbbreviatedBuiltinCommand.
    #[test]
    fn composer_application_prefers_dynamic_script_to_builtin_prefix() {
        let project = project_with_script("check", "printf 'hello-from-script\\n'");

        Command::cargo_bin("riff")
            .unwrap()
            .args(["check", "--no-plugins", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicate::str::contains("hello-from-script"));
    }
}
