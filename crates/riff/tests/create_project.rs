#[cfg(unix)]
mod unix {
    use std::fs;
    use std::path::Path;
    use std::process::Command as ProcessCommand;

    use assert_cmd::Command;

    fn git(directory: &Path, args: &[&str]) -> String {
        let output = ProcessCommand::new("git")
            .current_dir(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn source_repository(workspace: &Path) -> (std::path::PathBuf, String) {
        let source = workspace.join("source");
        fs::create_dir(&source).unwrap();
        git(&source, &["init", "--quiet"]);
        git(&source, &["config", "user.email", "test@example.com"]);
        git(&source, &["config", "user.name", "Riff Test"]);
        git(&source, &["config", "commit.gpgsign", "false"]);
        fs::write(
            source.join("composer.json"),
            r#"{"name":"vendor/project","description":"fixture"}"#,
        )
        .unwrap();
        fs::write(source.join("README.md"), "created by Riff\n").unwrap();
        git(&source, &["add", "composer.json", "README.md"]);
        git(&source, &["commit", "--quiet", "-m", "fixture"]);
        let reference = git(&source, &["rev-parse", "HEAD"]);
        (source, reference)
    }

    fn write_repository(
        workspace: &Path,
        source: &Path,
        reference: &str,
        pretty_version: &str,
        normalized_version: &str,
    ) {
        let packages = serde_json::json!([{
            "name": "vendor/project",
            "description": "fixture",
            "version": pretty_version,
            "version_normalized": normalized_version,
            "source": {
                "type": "git",
                "url": source,
                "reference": reference
            },
            "type": "project"
        }]);
        fs::write(
            workspace.join("packages.json"),
            serde_json::to_vec_pretty(&packages).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn composer_create_project_command_clones_the_requested_source_package() {
        let workspace = tempfile::tempdir().unwrap();
        let (source, reference) = source_repository(workspace.path());
        write_repository(workspace.path(), &source, &reference, "1.0.0", "1.0.0.0");

        let output = Command::cargo_bin("riff")
            .unwrap()
            .current_dir(workspace.path())
            .args([
                "create-project",
                "--repository=packages.json",
                "vendor/project",
                "created",
                "1.0.0",
                "--prefer-source",
                "-n",
            ])
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "create-project failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains(&format!(
            "- Installing vendor/project (1.0.0): Cloning {}",
            &reference[..10]
        )));
        assert_eq!(
            fs::read_to_string(workspace.path().join("created/README.md")).unwrap(),
            "created by Riff\n"
        );
        assert!(workspace.path().join("created/composer.lock").exists());
    }

    #[test]
    fn composer_create_project_shows_full_hash_for_verbose_dev_package() {
        let workspace = tempfile::tempdir().unwrap();
        let (source, reference) = source_repository(workspace.path());
        write_repository(
            workspace.path(),
            &source,
            &reference,
            "dev-main",
            "dev-main",
        );

        let output = Command::cargo_bin("riff")
            .unwrap()
            .current_dir(workspace.path())
            .args([
                "create-project",
                "--repository=packages.json",
                "-v",
                "vendor/project",
                "created",
                "dev-main",
                "-n",
            ])
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "create-project failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8(output.stdout).unwrap().contains(&format!(
            "- Installing vendor/project (dev-main): Cloning {reference}"
        )));
        assert!(workspace.path().join("created/README.md").exists());
    }
}
