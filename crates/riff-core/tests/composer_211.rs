//! Regression contracts from Composer 2.11.0 and 2.11.1.
use riff_core::{
    archive::{read_composer_json_from_archive, ComposerArchiveFormat},
    config::AuthConfig,
    downloader::ArchiveExtractor,
    http::authentication_options,
    json::{JsonManipulator, LockedPackage},
    package::{validate_package_metadata, Dist},
    repository::InstalledRepository,
    solver::{package_matches_platform_requirements, PlatformRequirementMatcher},
    Package,
};
use serde_json::json;
use std::{fs, io::Write};

#[tokio::test]
async fn installed_names_cannot_escape_vendor_in_either_reader() {
    let tmp = tempfile::tempdir().unwrap();
    let vendor = tmp.path().join("vendor");
    fs::create_dir_all(vendor.join("composer")).unwrap();
    let sentinel = tmp.path().join("keep");
    fs::write(&sentinel, "unchanged").unwrap();
    for name in [
        "../keep",
        "a/../../keep",
        "/tmp/keep",
        "a/b/c",
        "a/..",
        "a/.",
        "a\\b/c",
        "ext-../../keep",
    ] {
        let packages = json!([{"name":name,"version":"1.0.0","version_normalized":"1.0.0.0"}]);
        for data in [packages.clone(), json!({"packages":packages,"dev":false})] {
            fs::write(vendor.join("composer/installed.json"), data.to_string()).unwrap();
            let repository = InstalledRepository::new(&vendor);
            assert!(repository.load().await.is_err(), "{name}");
            assert!(repository.load_transaction_packages().is_err(), "{name}");
            assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged");
        }
    }
}

#[test]
fn package_validation_rejects_paths_and_keeps_runtime_extension_names() {
    for name in ["a/b/c", "../b", "a/..", "a/.", "a/-b"] {
        assert!(validate_package_metadata(&Package::new(name, "1.0")).is_err());
    }
    for target in ["../outside", "/outside", "C:\\outside", "a\\..\\outside"] {
        let mut package = Package::new("a/b", "1.0");
        package.target_dir = Some(target.into());
        assert!(validate_package_metadata(&package).is_err());
    }
    assert!(validate_package_metadata(&Package::new("ext-unusual(extension)", "1.0")).is_ok());
}

#[test]
fn lock_conversion_removes_headers_but_preserves_transport_settings() {
    let options = json!({"http":{"header":["Authorization: fake-secret"],"timeout":30},"symlink":false,"relative":true});
    for on_dist in [false, true] {
        let mut package = Package::new("a/b", "1.0");
        if on_dist {
            package.dist = Some(
                Dist::zip("https://example.org/a.zip")
                    .with_transport_options(serde_json::from_value(options.clone()).unwrap()),
            );
        } else {
            package.transport_options = Some(options.clone());
        }
        let locked = LockedPackage::from(&package);
        let serialized = serde_json::to_string(&locked).unwrap();
        assert!(!serialized.contains("fake-secret"));
        let retained = locked.transport_options.unwrap();
        assert_eq!(retained["http"]["timeout"], 30);
        assert_eq!(retained["symlink"], false);
        assert_eq!(retained["relative"], true);
        if !on_dist {
            assert_eq!(package.transport_options.as_ref().unwrap(), &options);
        }
    }
}

#[test]
fn archive_metadata_siblings_do_not_prevent_unwrapping_or_manifest_discovery() {
    for metadata in [
        "__MACOSX/._package",
        ".DS_Store",
        "Thumbs.db",
        "DESKTOP.INI",
        "._package",
        ".AppleDouble/info",
        ".Spotlight-V100/info",
    ] {
        for metadata_first in [true, false] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("archive.zip");
            let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
            let entries = if metadata_first {
                [metadata, "package/composer.json"]
            } else {
                ["package/composer.json", metadata]
            };
            for entry in entries {
                zip.start_file(entry, zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(b"{\"name\":\"a/b\"}").unwrap();
            }
            zip.finish().unwrap();
            assert!(
                read_composer_json_from_archive(&path, ComposerArchiveFormat::Zip)
                    .unwrap()
                    .is_some(),
                "{metadata}"
            );
            let dest = tmp.path().join("dest");
            ArchiveExtractor::extract(&path, &dest).unwrap();
            assert!(dest.join("composer.json").is_file(), "{metadata}");
            assert!(!dest.join("package").exists());
        }
    }
}

#[test]
fn job_tokens_use_basic_only_for_gitlab_composer_registry_paths() {
    let config: AuthConfig = serde_json::from_value(json!({"gitlab-token":{"code.example.org":{"username":"gitlab-ci-token","token":"fake-token"}}})).unwrap();
    for path in [
        "/api/v4/group/1/-/packages/composer/packages.json",
        "/api/v4/projects/2/packages/composer/archives/a.zip",
        "/api/v4/projects/2/repository/archive.zip",
        "/api/v4/projects/2/packages/composer-other",
    ] {
        let options = authentication_options(
            &config,
            "code.example.org",
            &format!("https://code.example.org{path}"),
            &[],
        )
        .unwrap();
        let header = &options.headers()[0];
        if path.contains("/composer/") {
            assert_eq!(header.name(), "authorization");
            assert!(header.value().to_str().unwrap().starts_with("Basic "));
        } else {
            assert_eq!(header.name(), "job-token");
            assert_eq!(header.value(), "fake-token");
        }
    }
    let options = authentication_options(
        &config,
        "unrelated.example.org",
        "https://unrelated.example.org",
        &[],
    )
    .unwrap();
    assert!(options.headers().is_empty());
}

#[test]
fn malformed_custom_headers_are_ignored_atomically_without_secret_diagnostics() {
    for value in [
        r#"["X-Valid: secret", 42]"#,
        r#"["X-Valid: secret", "not-a-header"]"#,
        "invalid-secret",
    ] {
        let config: AuthConfig = serde_json::from_value(
            json!({"http-basic":{"example.org":{"username":value,"password":"custom-headers"}}}),
        )
        .unwrap();
        let options = authentication_options(
            &config,
            "example.org",
            "https://example.org",
            &["Accept: application/json"],
        )
        .unwrap();
        assert_eq!(options.header_lines(), ["Accept: application/json"]);
        assert!(!options.warning().unwrap().contains("secret"));
    }
}

#[test]
fn removal_preserves_preexisting_empty_sections() {
    let input = r#"{"require":{},"require-dev":{},"repositories":[]}"#;
    let mut document = JsonManipulator::new(input).unwrap();
    document.remove_link("require", "a/b").unwrap();
    document.remove_link("require-dev", "a/b").unwrap();
    document.remove_repository("unused").unwrap();
    assert_eq!(document.contents().trim_end(), input);
}

#[test]
fn platform_match_cache_preserves_failures_and_isolated_runtime_results() {
    let platforms = [Package::new("php", "8.2.0.0")];
    let older = [Package::new("php", "7.4.0.0")];
    let mut cached = PlatformRequirementMatcher::new(&platforms);
    let mut cached_older = PlatformRequirementMatcher::new(&older);
    for _ in 0..3 {
        for (name, constraint) in [
            ("php", "^8.0"),
            ("php", "^7.0"),
            ("php", "bad constraint!"),
            ("ext-missing", "*"),
        ] {
            let mut package = Package::new("a/b", "1.0");
            package.require.insert(name.into(), constraint.into());
            assert_eq!(
                cached.matches(&package),
                package_matches_platform_requirements(&package, &platforms)
            );
            assert_eq!(
                cached_older.matches(&package),
                package_matches_platform_requirements(&package, &older)
            );
        }
    }
}

#[test]
fn old_lock_headers_are_not_reemitted_on_serialization() {
    let package: LockedPackage = serde_json::from_value(json!({"name":"a/b","version":"1.0","transport-options":{"http":{"header":["X-Secret: fake-secret"],"timeout":20}}})).unwrap();
    let encoded = serde_json::to_value(&package).unwrap();
    assert!(encoded["transport-options"]["http"].get("header").is_none());
    assert_eq!(encoded["transport-options"]["http"]["timeout"], 20);
}

#[test]
fn custom_header_auth_json_supports_valid_lists_and_ignores_bad_entries() {
    for headers in [json!(["X-Token: example"]), json!([42]), json!("invalid")] {
        let config: AuthConfig =
            serde_json::from_value(json!({"custom-headers":{"example.org":headers}})).unwrap();
        let options =
            authentication_options(&config, "example.org", "https://example.org/", &[]).unwrap();
        if headers == json!(["X-Token: example"]) {
            assert_eq!(options.header_lines(), ["X-Token: example"]);
        } else {
            assert!(options.headers().is_empty());
            assert!(options.warning().is_some());
        }
    }
}

#[test]
fn svn_and_url_credentials_are_redacted_in_shell_and_command_debug_forms() {
    for command in [
        "svn --password 'secret with spaces' https://user:fake-secret@example.org",
        r#""svn" "--password" "secret with spaces" "https://user:fake-secret@example.org""#,
        "svn --password=secret-value",
    ] {
        let diagnostic = riff_core::process::redact_command(command);
        assert!(!diagnostic.contains("secret"), "{diagnostic}");
    }
}

#[test]
fn tar_metadata_siblings_are_ignored_in_manifest_discovery_and_extraction() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("archive.tar");
    let mut archive = tar::Builder::new(fs::File::create(&path).unwrap());
    for name in ["__MACOSX/._package", "Thumbs.db", "package/composer.json"] {
        let content = br#"{"name":"a/b"}"#;
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, name, &content[..])
            .unwrap();
    }
    archive.finish().unwrap();
    assert!(
        read_composer_json_from_archive(&path, ComposerArchiveFormat::Tar)
            .unwrap()
            .is_some()
    );
    let dest = tmp.path().join("dest");
    ArchiveExtractor::extract(&path, &dest).unwrap();
    assert!(dest.join("composer.json").exists());
    assert!(!dest.join("._package").exists());
}

#[tokio::test]
async fn project_auth_reaches_repository_and_archive_requests_without_cross_project_reuse() {
    use riff_core::{Platform, RiffBuilder, RiffSession};
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut requests = Vec::new();
        for _ in 0..4 {
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    Err(error) => {
                        panic!("local auth fixture did not receive all requests: {error}")
                    }
                }
            };
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut request = String::new();
            let mut byte = [0];
            while !request.ends_with("\r\n\r\n") {
                assert_eq!(socket.read(&mut byte).unwrap(), 1);
                request.push(byte[0] as char);
            }
            let body = r#"{"packages":{"a/b":{"1.0.0":{"name":"a/b","version":"1.0.0","type":"metapackage"}}}}"#;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            requests.push(request);
        }
        requests
    });
    let tmp = tempfile::tempdir().unwrap();
    let session = RiffSession::builder()
        .with_cache_dir(tmp.path().join("cache"))
        .build()
        .unwrap();
    let url = format!("http://{address}/api/v4/group/1/-/packages/composer");
    for token in ["first-token", "second-token"] {
        let project = tmp.path().join(token);
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("auth.json"),
            json!({"gitlab-token":{"127.0.0.1":{"username":"gitlab-ci-token","token":token}}})
                .to_string(),
        )
        .unwrap();
        let manifest = serde_json::from_value(
            json!({"repositories":[{"type":"composer","url":url},{"packagist.org":false}]}),
        )
        .unwrap();
        let riff = RiffBuilder::new(project)
            .with_session(session.clone())
            .with_platform(Platform::empty())
            .with_manifest(manifest)
            .disable_packagist(true)
            .build()
            .unwrap();
        let packages = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            riff.repository_manager.find_packages("a/b"),
        )
        .await
        .unwrap();
        assert_eq!(packages.len(), 1);
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            riff.http_client.get(&format!(
                "http://{address}/api/v4/projects/1/repository/archive.zip"
            )),
        )
        .await
        .unwrap()
        .unwrap();
    }
    let requests = server.join().unwrap();
    use base64::Engine;
    for (pair, token) in requests
        .as_chunks::<2>()
        .0
        .iter()
        .zip(["first-token", "second-token"])
    {
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!("gitlab-ci-token:{token}"));
        assert!(pair[0].contains(&format!("authorization: Basic {basic}")));
        assert!(pair[1].contains(&format!("job-token: {token}")));
        assert!(!pair[0].contains("private-token"));
    }
}

#[tokio::test]
async fn invalid_transaction_aborts_before_removing_any_package() {
    use riff_core::{
        http::HttpClient, solver::Operation, InstallConfig, InstallationManager, Transaction,
    };
    use std::sync::Arc;
    let tmp = tempfile::tempdir().unwrap();
    let vendor = tmp.path().join("vendor");
    let sentinel = vendor.join("good/package/keep");
    fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
    fs::write(&sentinel, "unchanged").unwrap();
    let manager = InstallationManager::new(
        Arc::new(HttpClient::new().unwrap()),
        InstallConfig {
            vendor_dir: vendor,
            bin_dir: tmp.path().join("bin"),
            cache_dir: tmp.path().join("cache"),
            ..Default::default()
        },
    );
    for operation in [
        Operation::Reinstall(Arc::new(Package::new("../outside", "1.0"))),
        Operation::Install(Arc::new(Package::new("a/../../outside", "1.0"))),
    ] {
        let mut transaction = Transaction::new();
        transaction.operations = vec![
            Operation::Uninstall(Arc::new(Package::new("good/package", "1.0"))),
            operation,
        ];
        assert!(manager.execute(&transaction).await.is_err());
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged");
    }
}

#[tokio::test]
async fn private_repository_does_not_forward_custom_tokens_across_origins() {
    use riff_core::repository::{ComposerRepository, Repository};
    use std::io::Read;
    let destination = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    destination.set_nonblocking(true).unwrap();
    let destination_address = destination.local_addr().unwrap();
    let source = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let source_address = source.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = source.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut byte = [0];
        let mut request = String::new();
        while !request.ends_with("\r\n\r\n") {
            assert_eq!(socket.read(&mut byte).unwrap(), 1);
            request.push(byte[0] as char);
        }
        assert!(request.contains("x-token: fake-token"));
        write!(socket,"HTTP/1.1 302 Found\r\nLocation: http://{destination_address}/packages.json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let mut repo = ComposerRepository::new("private", format!("http://{source_address}"));
    repo.set_auth(
        serde_json::from_value(json!({"custom-headers":{"127.0.0.1":["X-Token: fake-token"]}}))
            .unwrap(),
    );
    assert!(tokio::time::timeout(
        std::time::Duration::from_secs(15),
        repo.find_packages("a/b")
    )
    .await
    .unwrap()
    .is_empty());
    server.join().unwrap();
    assert_eq!(
        destination.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
