#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use assert_cmd::Command;

    fn make_executable(path: &std::path::Path, content: &str) {
        fs::write(path, content).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn write_json(path: &Path, value: serde_json::Value) {
        fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    fn write_lock(project: &Path, packages: serde_json::Value, packages_dev: serde_json::Value) {
        write_json(
            &project.join("composer.lock"),
            serde_json::json!({
                "content-hash": "",
                "packages": packages,
                "packages-dev": packages_dev
            }),
        );
    }

    #[test]
    fn pure_command_does_not_execute_php() {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("composer.json"),
            r#"{"name":"fixture/project","description":"test","license":"MIT"}"#,
        )
        .unwrap();

        Command::cargo_bin("riff")
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

        Command::cargo_bin("riff")
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

    #[test]
    fn json_output_is_newline_delimited_events() {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("composer.json"),
            r#"{"name":"fixture/project","description":"test","license":"MIT"}"#,
        )
        .unwrap();

        let output = Command::cargo_bin("riff")
            .unwrap()
            .args(["--output", "json", "validate", "--no-check-publish", "-d"])
            .arg(project.path())
            .output()
            .unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(!stdout.trim().is_empty());
        assert!(!stdout.contains('\x1b'));
        for line in stdout.lines() {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(event["level"].is_string());
            assert!(event["message"].is_string());
        }
    }

    #[test]
    fn quiet_mode_suppresses_informational_output() {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("composer.json"),
            r#"{"name":"fixture/project","description":"test","license":"MIT"}"#,
        )
        .unwrap();

        Command::cargo_bin("riff")
            .unwrap()
            .args(["--quiet", "validate", "--no-check-publish", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout("");
    }

    #[test]
    fn ansi_flags_are_rendered_per_cli_invocation() {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("composer.json"),
            r#"{"name":"fixture/project"}"#,
        )
        .unwrap();

        let stderr = |flag| {
            let output = Command::cargo_bin("riff")
                .unwrap()
                .args([flag, "run", "missing", "-d"])
                .arg(project.path())
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            String::from_utf8(output.stderr).unwrap()
        };

        assert!(stderr("--ansi").contains("\x1b["));
        assert!(!stderr("--no-ansi").contains("\x1b["));
    }

    #[test]
    fn dry_runs_do_not_execute_lifecycle_scripts() {
        let project = tempfile::tempdir().unwrap();
        let install_marker = project.path().join("install-ran");
        let update_marker = project.path().join("update-ran");
        fs::write(
            project.path().join("composer.json"),
            format!(
                r#"{{"name":"fixture/project","scripts":{{"pre-install-cmd":"sh -c 'touch {}'","pre-update-cmd":"sh -c 'touch {}'"}}}}"#,
                install_marker.display(),
                update_marker.display(),
            ),
        )
        .unwrap();
        fs::write(project.path().join("composer.lock"), "{}").unwrap();

        Command::cargo_bin("riff")
            .unwrap()
            .args(["install", "--dry-run", "--no-audit", "-d"])
            .arg(project.path())
            .assert()
            .success();
        Command::cargo_bin("riff")
            .unwrap()
            .args(["update", "--dry-run", "--no-audit", "-d"])
            .arg(project.path())
            .assert()
            .success();

        assert!(!install_marker.exists());
        assert!(!update_marker.exists());
    }

    #[test]
    fn lifecycle_scripts_receive_composer_dev_mode() {
        let project = tempfile::tempdir().unwrap();
        let marker = project.path().join("dev-mode");
        fs::write(
            project.path().join("composer.json"),
            serde_json::json!({
                "name": "fixture/project",
                "scripts": {
                    "pre-install-cmd": format!(
                        "printf %s \"$COMPOSER_DEV_MODE\" > {}",
                        marker.display()
                    )
                }
            })
            .to_string(),
        )
        .unwrap();
        fs::write(project.path().join("composer.lock"), "{}").unwrap();

        Command::cargo_bin("riff")
            .unwrap()
            .args(["install", "--no-dev", "--no-audit", "-d"])
            .arg(project.path())
            .assert()
            .success();

        assert_eq!(fs::read_to_string(marker).unwrap(), "0");
    }

    fn write_inline_path_project(project: &std::path::Path) {
        let source = project.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("composer.json"),
            r#"{"name":"fixture/dependency","version":"1.0.0"}"#,
        )
        .unwrap();
        fs::write(
            project.join("composer.json"),
            serde_json::json!({
                "name": "fixture/project",
                "require": {"fixture/dependency": "^1.0"},
                "repositories": [
                    {
                        "type": "package",
                        "package": {
                            "name": "fixture/dependency",
                            "version": "1.0.0",
                            "dist": {"type": "path", "url": source}
                        }
                    },
                    {"packagist.org": false}
                ]
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn update_no_install_writes_the_lock_without_creating_vendor() {
        let project = tempfile::tempdir().unwrap();
        write_inline_path_project(project.path());

        Command::cargo_bin("riff")
            .unwrap()
            .args(["update", "--no-install", "--no-audit", "--no-scripts", "-d"])
            .arg(project.path())
            .assert()
            .success();

        assert!(project.path().join("composer.lock").exists());
        assert!(!project.path().join("vendor").exists());
    }

    #[test]
    fn download_only_writes_the_lock_without_creating_vendor() {
        let project = tempfile::tempdir().unwrap();
        write_inline_path_project(project.path());

        Command::cargo_bin("riff")
            .unwrap()
            .args([
                "install",
                "--download-only",
                "--no-audit",
                "--no-scripts",
                "-d",
            ])
            .arg(project.path())
            .assert()
            .success();

        assert!(project.path().join("composer.lock").exists());
        assert!(!project.path().join("vendor").exists());
    }

    #[test]
    fn update_dry_run_projects_bump_without_writing() {
        let project = tempfile::tempdir().unwrap();
        let source = project.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("composer.json"),
            r#"{"name":"fixture/dependency","version":"1.2.3"}"#,
        )
        .unwrap();
        let manifest = serde_json::json!({
            "name": "fixture/project",
            "require": {"fixture/dependency": "^1.0"},
            "repositories": [
                {
                    "type": "package",
                    "package": {
                        "name": "fixture/dependency",
                        "version": "1.2.3",
                        "dist": {"type": "path", "url": source}
                    }
                },
                {"packagist.org": false}
            ]
        })
        .to_string();
        let manifest_path = project.path().join("composer.json");
        fs::write(&manifest_path, &manifest).unwrap();

        Command::cargo_bin("riff")
            .unwrap()
            .args([
                "update",
                "--dry-run",
                "--bump-after-update",
                "--no-audit",
                "--no-scripts",
                "-d",
            ])
            .arg(project.path())
            .assert()
            .code(1)
            .stdout(predicates::str::contains(
                "composer.json would be updated with",
            ));

        assert_eq!(fs::read_to_string(manifest_path).unwrap(), manifest);
        assert!(!project.path().join("composer.lock").exists());
        assert!(!project.path().join("vendor").exists());
    }

    #[test]
    fn config_dry_run_does_not_write_the_project_manifest() {
        let project = tempfile::tempdir().unwrap();
        let manifest = project.path().join("composer.json");
        fs::write(&manifest, r#"{"name":"fixture/project"}"#).unwrap();
        let before = fs::read(&manifest).unwrap();

        Command::cargo_bin("riff")
            .unwrap()
            .args(["config", "--dry-run", "optimize-autoloader", "true", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicates::str::contains("would be updated"));

        assert_eq!(fs::read(&manifest).unwrap(), before);
    }

    #[test]
    fn dump_autoload_dry_run_skips_scripts_and_vendor_writes() {
        let project = tempfile::tempdir().unwrap();
        let marker = project.path().join("autoload-ran");
        fs::write(
            project.path().join("composer.json"),
            format!(
                r#"{{"name":"fixture/project","scripts":{{"pre-autoload-dump":"sh -c 'touch {}'"}}}}"#,
                marker.display(),
            ),
        )
        .unwrap();

        Command::cargo_bin("riff")
            .unwrap()
            .args(["dump-autoload", "--dry-run", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicates::str::contains("Would generate autoload files"));

        assert!(!marker.exists());
        assert!(!project.path().join("vendor").exists());
    }

    #[test]
    fn composer_dump_autoload_command_generates_autoload_files() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({"name": "fixture/project"}),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["dump-autoload", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicates::str::contains("Generating autoload files"));

        assert!(project.path().join("vendor/autoload.php").is_file());
    }

    #[test]
    fn composer_dump_autoload_command_includes_dev_autoload_by_default() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({
                "name": "fixture/project",
                "autoload": {"files": ["prod.php"]},
                "autoload-dev": {"files": ["dev.php"]}
            }),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["dump-autoload", "-d"])
            .arg(project.path())
            .assert()
            .success();

        let files =
            fs::read_to_string(project.path().join("vendor/composer/autoload_files.php")).unwrap();
        assert!(files.contains("prod.php"));
        assert!(files.contains("dev.php"));
    }

    #[test]
    fn composer_dump_autoload_command_excludes_dev_autoload_with_no_dev() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({
                "name": "fixture/project",
                "autoload": {"files": ["prod.php"]},
                "autoload-dev": {"files": ["dev.php"]}
            }),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["dump-autoload", "--no-dev", "-d"])
            .arg(project.path())
            .assert()
            .success();

        let files =
            fs::read_to_string(project.path().join("vendor/composer/autoload_files.php")).unwrap();
        assert!(files.contains("prod.php"));
        assert!(!files.contains("dev.php"));
    }

    #[test]
    fn composer_dump_autoload_command_generates_authoritative_classmap() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({"name": "fixture/project"}),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["dump-autoload", "--classmap-authoritative", "-d"])
            .arg(project.path())
            .assert()
            .success();

        let real =
            fs::read_to_string(project.path().join("vendor/composer/autoload_real.php")).unwrap();
        assert!(real.contains("setClassMapAuthoritative(true)"));
    }

    #[test]
    fn composer_dump_autoload_command_uses_lock_content_hash_as_suffix() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({"name": "foo/bar"}),
        );
        write_json(
            &project.path().join("composer.lock"),
            serde_json::json!({
                "content-hash": "2d4a6be9a93712c5d6a119b26734a047",
                "packages": [],
                "packages-dev": []
            }),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["dump-autoload", "-d"])
            .arg(project.path())
            .assert()
            .success();

        let autoload = fs::read_to_string(project.path().join("vendor/autoload.php")).unwrap();
        assert!(autoload.contains("ComposerAutoloaderInit2d4a6be9a93712c5d6a119b26734a047"));
    }

    #[test]
    fn composer_show_command_locked_requires_a_valid_lock_file() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({"name": "fixture/project"}),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "--locked", "-d"])
            .arg(project.path())
            .assert()
            .code(1)
            .stderr(predicates::str::contains(
                "valid composer.json and composer.lock",
            ));
    }

    #[test]
    fn composer_show_command_lists_all_locked_packages() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({"name": "fixture/project"}),
        );
        write_lock(
            project.path(),
            serde_json::json!([
                {"name": "vendor/locked", "version": "3.0.0", "description": "first"},
                {"name": "vendor/locked2", "version": "2.0.0", "description": "second"}
            ]),
            serde_json::json!([]),
        );

        let assert = Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "--locked", "-d"])
            .arg(project.path())
            .assert()
            .success();
        let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
        assert!(stdout.contains("vendor/locked"));
        assert!(stdout.contains("vendor/locked2"));
        assert!(stdout.contains("3.0.0"));
        assert!(stdout.contains("2.0.0"));
    }

    #[test]
    fn composer_show_command_rejects_invalid_option_combinations() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({"name": "fixture/project"}),
        );

        for options in [
            &["--direct", "--all"][..],
            &["--direct", "--available"],
            &["--direct", "--platform"],
            &["--tree", "--all"],
            &["--tree", "--available"],
            &["--tree", "--latest"],
            &["--tree", "--path"],
        ] {
            Command::cargo_bin("riff")
                .unwrap()
                .arg("show")
                .args(options)
                .arg("-d")
                .arg(project.path())
                .assert()
                .code(1);
        }

        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "--format", "unsupported", "-d"])
            .arg(project.path())
            .assert()
            .code(1);
    }

    #[test]
    fn composer_show_command_self_and_name_only_prints_root_name() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({"name": "vendor/package", "version": "1.2.3"}),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "--self", "--name-only", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout("vendor/package\n");
    }

    #[test]
    fn composer_show_command_rejects_self_with_package() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({"name": "vendor/package"}),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "vendor/package", "--self", "-d"])
            .arg(project.path())
            .assert()
            .code(1)
            .stderr(predicates::str::contains(
                "Cannot use --self together with a package name",
            ));
    }

    #[test]
    fn composer_show_command_displays_root_package() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({
                "name": "vendor/package",
                "version": "1.2.3",
                "description": "fixture root"
            }),
        );

        let assert = Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "--self", "-d"])
            .arg(project.path())
            .assert()
            .success();
        let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
        assert!(stdout.contains("name     : vendor/package"));
        assert!(stdout.contains("version  : 1.2.3"));
        assert!(stdout.contains("descrip. : fixture root"));
    }

    #[test]
    fn composer_show_command_warns_when_dependencies_are_not_installed() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({
                "name": "fixture/project",
                "require": {"vendor/package": "1.0.0"},
                "require-dev": {"vendor/package-dev": "1.0.0"}
            }),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stderr(predicates::str::contains("No dependencies installed"));
    }

    #[test]
    fn composer_show_command_no_dev_excludes_locked_dev_packages() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({
                "name": "fixture/project",
                "require": {"vendor/package": "1.0.0"},
                "require-dev": {"vendor/package-dev": "1.0.0"}
            }),
        );
        write_lock(
            project.path(),
            serde_json::json!([{"name": "vendor/package", "version": "1.0.0"}]),
            serde_json::json!([{"name": "vendor/package-dev", "version": "1.0.0"}]),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "--locked", "--no-dev", "--name-only", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout("vendor/package\n");
    }

    #[test]
    fn composer_show_command_filters_exact_and_wildcard_package_names() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({"name": "fixture/project"}),
        );
        write_lock(
            project.path(),
            serde_json::json!([
                {"name": "vendor/package", "version": "1.0.0"},
                {"name": "vendor/other-package", "version": "1.0.0"},
                {"name": "company/package", "version": "1.0.0"},
                {"name": "company/other-package", "version": "1.0.0"}
            ]),
            serde_json::json!([]),
        );

        let exact = Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "vendor/package", "--locked", "-d"])
            .arg(project.path())
            .output()
            .unwrap();
        assert!(exact.status.success());
        let stdout = String::from_utf8(exact.stdout).unwrap();
        assert!(stdout.contains("name     : vendor/package"));
        assert!(!stdout.contains("vendor/other-package"));

        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "company/*", "--locked", "--name-only", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout("company/other-package\ncompany/package\n");
    }

    #[test]
    fn composer_show_command_reports_a_missing_package() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({"name": "fixture/project"}),
        );
        write_lock(project.path(), serde_json::json!([]), serde_json::json!([]));

        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "not/existing", "--locked", "-d"])
            .arg(project.path())
            .assert()
            .code(1)
            .stderr(predicates::str::contains(
                "Package \"not/existing\" not found",
            ));
    }

    #[test]
    fn composer_show_command_direct_package_rejects_transitive_dependency() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({
                "name": "fixture/project",
                "require": {"direct/dependent": "*"}
            }),
        );
        write_lock(
            project.path(),
            serde_json::json!([
                {"name": "direct/dependent", "version": "1.0.0", "require": {"vendor/package": "*"}},
                {"name": "vendor/package", "version": "1.0.0"}
            ]),
            serde_json::json!([]),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "vendor/package", "--locked", "--direct", "-d"])
            .arg(project.path())
            .assert()
            .code(1)
            .stderr(predicates::str::contains("is not a direct dependency"));
    }

    #[test]
    fn composer_show_command_direct_package_accepts_prod_and_dev_dependencies() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({
                "name": "fixture/project",
                "require": {"direct/dependent": "*"},
                "require-dev": {"direct/dependent2": "*"}
            }),
        );
        write_lock(
            project.path(),
            serde_json::json!([{"name": "direct/dependent", "version": "1.0.0"}]),
            serde_json::json!([{"name": "direct/dependent2", "version": "1.0.0"}]),
        );

        for package in ["direct/dependent", "direct/dependent2"] {
            Command::cargo_bin("riff")
                .unwrap()
                .arg("show")
                .arg(package)
                .args(["--locked", "--direct", "-d"])
                .arg(project.path())
                .assert()
                .success()
                .stdout(predicates::str::contains(format!("name     : {package}")));
        }
    }

    #[test]
    fn composer_show_command_specific_package_tree_displays_requirements() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({
                "name": "fixture/project",
                "require": {"vendor/package": "1.0.0"}
            }),
        );
        write_lock(
            project.path(),
            serde_json::json!([
                {"name": "vendor/package", "version": "1.0.0", "require": {"vendor/required-package": "1.0.0"}},
                {"name": "vendor/required-package", "version": "1.0.0"}
            ]),
            serde_json::json!([]),
        );

        let assert = Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "vendor/package", "--locked", "--tree", "-d"])
            .arg(project.path())
            .assert()
            .success();
        let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
        assert!(stdout.contains("vendor/package 1.0.0"));
        assert!(stdout.contains("vendor/required-package 1.0.0"));
    }

    #[test]
    fn composer_show_command_name_only_has_no_trailing_whitespace() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({"name": "fixture/project"}),
        );
        write_lock(
            project.path(),
            serde_json::json!([
                {"name": "vendor/apackage", "version": "1.0.0"},
                {"name": "vendor/longpackagename", "version": "1.0.0"},
                {"name": "vendor/somepackage", "version": "1.0.0"}
            ]),
            serde_json::json!([]),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "--locked", "--name-only", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout("vendor/apackage\nvendor/longpackagename\nvendor/somepackage\n");
    }

    #[test]
    fn composer_show_command_text_and_json_formats_list_packages() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({"name": "fixture/project"}),
        );
        write_lock(
            project.path(),
            serde_json::json!([{
                "name": "vendor/package",
                "version": "1.0.0",
                "description": "fixture package"
            }]),
            serde_json::json!([]),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "--locked", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicates::str::contains("vendor/package"));
        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "--locked", "--format", "json", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicates::str::contains("\"name\": \"vendor/package\""));
    }

    #[test]
    fn composer_show_command_platform_only_lists_platform_packages() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({"name": "fixture/project"}),
        );

        let assert = Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "--platform", "-d"])
            .arg(project.path())
            .assert()
            .success();
        let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
        assert!(!stdout.trim().is_empty());
        assert!(stdout.lines().all(|line| {
            let name = line.split_whitespace().next().unwrap_or_default();
            name == "php"
                || name.starts_with("php-")
                || name == "composer"
                || name.starts_with("composer-")
                || name.starts_with("ext-")
                || name.starts_with("lib-")
        }));
    }

    #[test]
    fn composer_show_command_platform_works_without_composer_json() {
        let project = tempfile::tempdir().unwrap();

        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "--platform", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicates::str::contains("php"));
        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "php", "--platform", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicates::str::starts_with("php"));
        Command::cargo_bin("riff")
            .unwrap()
            .args(["show", "php", "--platform", "--format", "json", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicates::str::contains("\"name\": \"php\""));
    }

    #[test]
    fn composer_remove_command_requires_at_least_one_package() {
        Command::cargo_bin("riff")
            .unwrap()
            .arg("remove")
            .assert()
            .failure();
    }

    #[test]
    fn composer_remove_command_warns_for_nonexistent_package() {
        let project = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!({"name": "fixture/project"});
        write_json(&project.path().join("composer.json"), manifest.clone());

        Command::cargo_bin("riff")
            .unwrap()
            .args(["remove", "vendor1/package1", "--no-update", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stderr(predicates::str::contains(
                "vendor1/package1 is not required in your composer.json and has not been removed",
            ));

        let actual: serde_json::Value =
            serde_json::from_slice(&fs::read(project.path().join("composer.json")).unwrap())
                .unwrap();
        assert_eq!(actual, manifest);
    }

    #[test]
    fn composer_remove_command_warns_for_package_in_wrong_dependency_type() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({
                "name": "fixture/project",
                "require": {"root/req": "1.*"}
            }),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["remove", "root/req", "--dev", "--no-update", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stderr(predicates::str::contains(
                "root/req could not be found in require-dev but it is present in require",
            ));

        let actual: serde_json::Value =
            serde_json::from_slice(&fs::read(project.path().join("composer.json")).unwrap())
                .unwrap();
        assert_eq!(actual["require"]["root/req"], "1.*");
    }

    #[test]
    fn composer_remove_command_removes_package_by_name() {
        let project = tempfile::tempdir().unwrap();
        write_json(
            &project.path().join("composer.json"),
            serde_json::json!({
                "name": "fixture/project",
                "require": {"root/req": "1.*", "root/another": "1.*"}
            }),
        );

        Command::cargo_bin("riff")
            .unwrap()
            .args(["remove", "root/req", "--no-update", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicates::str::contains("root/req"));

        let actual: serde_json::Value =
            serde_json::from_slice(&fs::read(project.path().join("composer.json")).unwrap())
                .unwrap();
        assert!(actual["require"].get("root/req").is_none());
        assert_eq!(actual["require"]["root/another"], "1.*");
    }

    #[test]
    fn composer_remove_command_dry_run_preserves_manifest() {
        let project = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!({
            "name": "fixture/project",
            "require": {"root/req": "1.*", "root/another": "1.*"}
        });
        write_json(&project.path().join("composer.json"), manifest.clone());
        let before = fs::read(project.path().join("composer.json")).unwrap();

        Command::cargo_bin("riff")
            .unwrap()
            .args(["remove", "root/req", "--dry-run", "--no-update", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout(predicates::str::contains("would be updated"));

        assert_eq!(
            fs::read(project.path().join("composer.json")).unwrap(),
            before
        );
    }

    #[test]
    fn composer_process_executor_forwards_uncaptured_output() {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("composer.json"),
            r#"{"name":"fixture/project","scripts":{"probe":"printf 'foo\\n'"}}"#,
        )
        .unwrap();

        Command::cargo_bin("riff")
            .unwrap()
            .args(["--quiet", "run", "probe", "-d"])
            .arg(project.path())
            .assert()
            .success()
            .stdout("foo\n");
    }
}
