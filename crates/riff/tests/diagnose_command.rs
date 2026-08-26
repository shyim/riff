#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use assert_cmd::Command;
    use predicates::prelude::*;

    fn write_manifest(project: &tempfile::TempDir, license: Option<&str>) {
        let mut manifest = serde_json::json!({
            "name": "foo/bar",
            "description": "test pkg"
        });
        if let Some(license) = license {
            manifest["license"] = serde_json::Value::String(license.to_owned());
        }
        fs::write(
            project.path().join("composer.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn diagnostic_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(3) {
                let mut stream = stream.unwrap();
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request);
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-RateLimit-Remaining: 42\r\nConnection: close\r\n\r\n{}",
                    )
                    .unwrap();
            }
        });
        format!("http://{address}/diagnose")
    }

    fn diagnose(project: &tempfile::TempDir) -> assert_cmd::assert::Assert {
        let url = diagnostic_server();
        Command::cargo_bin("riff")
            .unwrap()
            .env("NO_PROXY", "127.0.0.1")
            .env("RIFF_DIAGNOSE_PACKAGIST_HTTP_URL", &url)
            .env("RIFF_DIAGNOSE_PACKAGIST_HTTPS_URL", &url)
            .env("RIFF_DIAGNOSE_GITHUB_RATE_LIMIT_URL", &url)
            .args(["diagnose", "-d"])
            .arg(project.path())
            .assert()
    }

    // Ported from Composer\Test\Command\DiagnoseCommandTest::testCmdFail.
    #[test]
    fn composer_diagnose_command_fails_for_manifest_warning() {
        let project = tempfile::tempdir().unwrap();
        write_manifest(&project, None);

        diagnose(&project)
            .code(1)
            .stdout(predicate::str::contains("Checking composer.json: WARNING"))
            .stderr(predicate::str::contains(
                "No license specified, it is recommended to do so.",
            ))
            .stdout(predicate::str::contains(
                "Checking http connectivity to packagist: OK",
            ))
            .stdout(predicate::str::contains(
                "Checking https connectivity to packagist: OK",
            ))
            .stdout(predicate::str::contains(
                "Checking github.com rate limit: OK (42 requests remaining)",
            ));
    }

    // Ported from Composer\Test\Command\DiagnoseCommandTest::testCmdSuccess.
    #[test]
    fn composer_diagnose_command_succeeds_for_valid_manifest() {
        let project = tempfile::tempdir().unwrap();
        write_manifest(&project, Some("MIT"));

        diagnose(&project)
            .success()
            .stdout(predicate::str::contains("Checking composer.json: OK"))
            .stdout(predicate::str::contains(
                "Checking http connectivity to packagist: OK\nChecking https connectivity to packagist: OK\nChecking github.com rate limit: OK",
            ));
    }
}
