#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use assert_cmd::Command;

    fn make_executable(path: &std::path::Path, content: &str) {
        fs::write(path, content).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn pure_command_does_not_execute_php() {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("composer.json"),
            r#"{"name":"fixture/project","description":"test","license":"MIT"}"#,
        )
        .unwrap();

        Command::cargo_bin("sonata")
            .unwrap()
            .arg("--php")
            .arg(project.path().join("missing-php"))
            .arg("validate")
            .arg("--no-check-publish")
            .arg("-d")
            .arg(project.path())
            .assert()
            .success();
    }

    #[test]
    fn php_script_uses_selected_executable() {
        let project = tempfile::tempdir().unwrap();
        let log = project.path().join("php-args");
        let php = project.path().join("custom-php");
        make_executable(
            &php,
            &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\n", log.display()),
        );
        fs::write(
            project.path().join("composer.json"),
            r#"{"name":"fixture/project","scripts":{"probe":"@php script.php"}}"#,
        )
        .unwrap();

        Command::cargo_bin("sonata")
            .unwrap()
            .arg("--php")
            .arg(&php)
            .arg("run")
            .arg("probe")
            .arg("-d")
            .arg(project.path())
            .arg("two words")
            .assert()
            .success();

        assert_eq!(fs::read_to_string(log).unwrap(), "script.php two words\n");
    }
}
