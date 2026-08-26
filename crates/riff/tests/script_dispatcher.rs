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

    // Ported from Composer\Test\Command\RunScriptCommandTest::testCanListScripts.
    #[test]
    fn composer_run_script_lists_default_and_custom_descriptions() {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("composer.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "fixture/project",
                "scripts": {
                    "test": "@php test",
                    "fix-cs": "php-cs-fixer fix"
                },
                "scripts-descriptions": {
                    "fix-cs": "Run the codestyle fixer"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        Command::cargo_bin("riff")
            .unwrap()
            .args(["run-script", "--list", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "Runs the test script as defined in composer.json",
            ))
            .stdout(predicate::str::contains("Run the codestyle fixer"));
    }

    // Ported from Composer\Test\EventDispatcher\EventDispatcherTest::
    // testDispatcherOutputsCommand.
    #[test]
    fn composer_dispatcher_outputs_command_and_child_stdout() {
        let project = project_with_script("probe", "printf 'foo\\n'");
        Command::cargo_bin("riff")
            .unwrap()
            .args(["run", "probe", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicate::str::contains("> printf 'foo\\n'"))
            .stdout(predicate::str::contains("foo\n"));
    }

    // Ported from Composer\Test\EventDispatcher\EventDispatcherTest::
    // testDispatcherOutputsErrorOnFailedCommand.
    #[test]
    fn composer_dispatcher_reports_failed_command_and_exit_code() {
        let project = project_with_script("probe", "exit 1");
        Command::cargo_bin("riff")
            .unwrap()
            .args(["run", "probe", "-d"])
            .arg(project.path())
            .assert()
            .failure()
            .stdout(predicate::str::contains("> exit 1"))
            .stderr(predicate::str::contains(
                "Script exit 1 handling the probe event returned with error code 1",
            ));
    }

    // Ported from Composer\Test\EventDispatcher\EventDispatcherTest::
    // testDispatcherDoesntReturnSkippedScripts.
    #[test]
    fn composer_dispatcher_omits_scripts_selected_by_environment() {
        let project = project_with_script("probe", "touch should-not-exist");
        Command::cargo_bin("riff")
            .unwrap()
            .env("COMPOSER_SKIP_SCRIPTS", " probe, another-script ")
            .args(["run", "probe", "-d"])
            .arg(project.path())
            .assert()
            .success();
        assert!(!project.path().join("should-not-exist").exists());
    }
}
