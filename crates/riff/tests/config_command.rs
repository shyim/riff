use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::{json, Value};

struct ConfigFixture {
    root: tempfile::TempDir,
    home: PathBuf,
}

impl ConfigFixture {
    fn new(document: Value) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            root.path().join("composer.json"),
            serde_json::to_vec_pretty(&document).unwrap(),
        )
        .unwrap();
        Self { root, home }
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::cargo_bin("riff").unwrap();
        command
            .env("COMPOSER_HOME", &self.home)
            .env_remove("COMPOSER_PROCESS_TIMEOUT")
            .env_remove("COMPOSER_VENDOR_DIR")
            .env_remove("COMPOSER_BIN_DIR")
            .env_remove("COMPOSER_CACHE_DIR")
            .env_remove("COMPOSER_DISCARD_CHANGES")
            .env_remove("COMPOSER_CACHE_READ_ONLY")
            .arg("config")
            .args(arguments)
            .args(["-d"])
            .arg(self.root.path());
        command
    }

    fn document(&self) -> Value {
        serde_json::from_slice(&fs::read(self.root.path().join("composer.json")).unwrap()).unwrap()
    }
}

fn assert_update(before: Value, arguments: &[&str], expected: Value) {
    let fixture = ConfigFixture::new(before);
    fixture.command(arguments).assert().success();
    assert_eq!(fixture.document(), expected, "arguments: {arguments:?}");
}

// Ported from Composer\Test\Command\ConfigCommandTest::testConfigUpdates.
#[test]
fn composer_config_command_updates_supported_settings() {
    let cases: Vec<(Value, Vec<&str>, Value)> = vec![
        (
            json!({}),
            vec!["scripts.test", "foo bar"],
            json!({"scripts": {"test": "foo bar"}}),
        ),
        (
            json!({"scripts": {"test": "foo bar", "lala": "baz"}}),
            vec!["scripts.lala", "--unset"],
            json!({"scripts": {"test": "foo bar"}}),
        ),
        (
            json!({}),
            vec!["use-github-api", "1"],
            json!({"config": {"use-github-api": true}}),
        ),
        (
            json!({}),
            vec!["github-protocols", "https", "git"],
            json!({"config": {"github-protocols": ["https", "git"]}}),
        ),
        (
            json!({}),
            vec!["version", "1.0.0"],
            json!({"version": "1.0.0"}),
        ),
        (
            json!({"version": "1.0.0"}),
            vec!["version", "--unset"],
            json!({}),
        ),
        (
            json!({"random-prop": "1.0.0"}),
            vec!["random-prop", "--unset"],
            json!({}),
        ),
        (
            json!({}),
            vec!["preferred-install.foo/*", "source"],
            json!({"config": {"preferred-install": {"foo/*": "source"}}}),
        ),
        (
            json!({"config": {"preferred-install": {"foo/*": "source"}}}),
            vec!["preferred-install.foo/*", "--unset"],
            json!({"config": {"preferred-install": {}}}),
        ),
        (
            json!({"config": {"platform": {"php": "7.2.5"}, "platform-check": false}}),
            vec!["platform.php", "--unset"],
            json!({"config": {"platform": {}, "platform-check": false}}),
        ),
        (
            json!({}),
            vec![
                "extra.patches.foo/bar",
                r#"{"123":"value"}"#,
                "--json",
                "--merge",
            ],
            json!({"extra": {"patches": {"foo/bar": {"123": "value"}}}}),
        ),
        (
            json!({"extra": {"patches": {"foo/bar": {"5": "oldvalue"}}}}),
            vec![
                "extra.patches.foo/bar",
                r#"{"123":"value"}"#,
                "--json",
                "--merge",
            ],
            json!({"extra": {"patches": {"foo/bar": {"123": "value", "5": "oldvalue"}}}}),
        ),
        (
            json!({"autoload": {"psr-4": ["test"], "classmap": ["test"]}}),
            vec!["autoload.psr-4", "--unset"],
            json!({"autoload": {"classmap": ["test"]}}),
        ),
        (
            json!({"autoload-dev": {"psr-4": ["test"], "classmap": ["test"]}}),
            vec!["autoload-dev.psr-4", "--unset"],
            json!({"autoload-dev": {"classmap": ["test"]}}),
        ),
        (
            json!({}),
            vec!["audit.ignore-unreachable", "true"],
            json!({"config": {"audit": {"ignore-unreachable": true}}}),
        ),
        (
            json!({}),
            vec!["audit.ignore-severity", "low", "medium"],
            json!({"config": {"audit": {"ignore-severity": ["low", "medium"]}}}),
        ),
        (
            json!({}),
            vec![
                "audit.ignore",
                r#"["CVE-2024-1234","GHSA-xxxx-yyyy"]"#,
                "--json",
            ],
            json!({"config": {"audit": {"ignore": ["CVE-2024-1234", "GHSA-xxxx-yyyy"]}}}),
        ),
        (
            json!({"config": {"audit": {"ignore": ["CVE-2024-1234"]}}}),
            vec!["audit.ignore", r#"["CVE-2024-5678"]"#, "--json", "--merge"],
            json!({"config": {"audit": {"ignore": ["CVE-2024-1234", "CVE-2024-5678"]}}}),
        ),
        (
            json!({"config": {"audit": {"ignore": {"CVE-2024-1234": "Old reason"}}}}),
            vec![
                "audit.ignore",
                r#"{"CVE-2024-5678":"New advisory"}"#,
                "--json",
                "--merge",
            ],
            json!({"config": {"audit": {"ignore": {
                "CVE-2024-5678": "New advisory", "CVE-2024-1234": "Old reason"
            }}}}),
        ),
        (
            json!({}),
            vec![
                "audit.ignore-abandoned",
                r#"["vendor/package1","vendor/package2"]"#,
                "--json",
            ],
            json!({"config": {"audit": {"ignore-abandoned": ["vendor/package1", "vendor/package2"]}}}),
        ),
        (
            json!({"config": {"audit": {"ignore": ["CVE-2024-1234"]}}}),
            vec!["audit.ignore", "--unset"],
            json!({"config": {"audit": {}}}),
        ),
        (
            json!({"config": {"policy": {"advisories": false}}}),
            vec!["policy", "--unset"],
            json!({"config": {}}),
        ),
        (
            json!({}),
            vec!["policy.advisories.block", "0"],
            json!({"config": {"policy": {"advisories": {"block": false}}}}),
        ),
        (
            json!({}),
            vec!["policy.advisories.audit", "report"],
            json!({"config": {"policy": {"advisories": {"audit": "report"}}}}),
        ),
        (
            json!({}),
            vec!["policy.malware.block-scope", "install"],
            json!({"config": {"policy": {"malware": {"block-scope": "install"}}}}),
        ),
        (
            json!({}),
            vec!["policy.abandoned.block", "true"],
            json!({"config": {"policy": {"abandoned": {"block": true}}}}),
        ),
        (
            json!({}),
            vec!["policy.ignore-unreachable", "true"],
            json!({"config": {"policy": {"ignore-unreachable": true}}}),
        ),
        (
            json!({}),
            vec!["policy.ignore-unreachable", "update", "install"],
            json!({"config": {"policy": {"ignore-unreachable": ["update", "install"]}}}),
        ),
        (
            json!({}),
            vec!["policy.my-list.block", "true"],
            json!({"config": {"policy": {"my-list": {"block": true}}}}),
        ),
        (
            json!({}),
            vec!["policy.my-list.audit", "report"],
            json!({"config": {"policy": {"my-list": {"audit": "report"}}}}),
        ),
        (
            json!({}),
            vec!["policy.my-list.ignore", r#"["vendor/pkg"]"#, "--json"],
            json!({"config": {"policy": {"my-list": {"ignore": ["vendor/pkg"]}}}}),
        ),
        (
            json!({}),
            vec!["policy.malware", "false"],
            json!({"config": {"policy": {"malware": false}}}),
        ),
        (
            json!({"config": {"policy": {
                "advisories": {"block": true, "audit": "fail"},
                "malware": {"block": true},
                "ignore-unreachable": true
            }}}),
            vec!["policy.advisories", "false"],
            json!({"config": {"policy": {
                "advisories": false,
                "malware": {"block": true},
                "ignore-unreachable": true
            }}}),
        ),
        (
            json!({"config": {"policy": {
                "advisories": {"block": false, "audit": "fail"},
                "malware": {"block": true}
            }}}),
            vec!["policy.advisories.block", "--unset"],
            json!({"config": {"policy": {
                "advisories": {"audit": "fail"}, "malware": {"block": true}
            }}}),
        ),
        (
            json!({"config": {"policy": {"malware": false, "advisories": {"block": true}}}}),
            vec!["policy.malware", "--unset"],
            json!({"config": {"policy": {"advisories": {"block": true}}}}),
        ),
    ];

    for (before, arguments, expected) in cases {
        assert_update(before, &arguments, expected);
    }
}

// Ported from Composer\Test\Command\ConfigCommandTest::testConfigReads.
#[test]
fn composer_config_command_reads_without_modifying_the_manifest() {
    let cases: Vec<(Value, Vec<&str>, &str)> = vec![
        (
            json!({"description": "foo bar"}),
            vec!["description"],
            "foo bar",
        ),
        (
            json!({"config": {"vendor-dir": "lala"}}),
            vec!["vendor-dir", "--source"],
            "lala (./composer.json)",
        ),
        (json!({}), vec!["vendor-dir"], "vendor"),
        (
            json!({"repositories": {
                "foo": {"type": "vcs", "url": "https://example.org"},
                "packagist.org": {"type": "composer", "url": "https://repo.packagist.org"}
            }}),
            vec!["repositories.foo"],
            r#"{"type":"vcs","url":"https://example.org"}"#,
        ),
        (
            json!({"repositories": [
                {"type": "vcs", "url": "https://example.org"},
                {"packagist.org": {"type": "composer", "url": "https://repo.packagist.org"}}
            ]}),
            vec!["repos.0"],
            r#"{"type":"vcs","url":"https://example.org"}"#,
        ),
        (
            json!({"repositories": {
                "foo": {"type": "vcs", "url": "https://example.org"},
                "packagist.org": {"type": "composer", "url": "https://repo.packagist.org"}
            }}),
            vec!["repos"],
            r#"{"foo":{"type":"vcs","url":"https://example.org"},"packagist.org":{"type":"composer","url":"https://repo.packagist.org"}}"#,
        ),
        (
            json!({"repositories": {
                "foo": {"type": "vcs", "url": "https://example.org"},
                "packagist.org": false
            }}),
            vec!["repos"],
            r#"{"foo":{"type":"vcs","url":"https://example.org"}}"#,
        ),
    ];

    for (document, arguments, expected) in cases {
        let fixture = ConfigFixture::new(document.clone());
        let output = fixture.command(&arguments).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);
        assert_eq!(fixture.document(), document);
    }
}

// Ported from Composer\Test\Command\ConfigCommandTest::
// testConfigThrowsForInvalidArgCombination.
#[test]
fn composer_config_command_rejects_file_with_global() {
    ConfigFixture::new(json!({}))
        .command(&["--file", "alt.composer.json", "--global"])
        .assert()
        .failure()
        .stderr(contains("--file and --global can not be combined"));
}

// Ported from Composer\Test\Command\ConfigCommandTest::testConfigThrowsForInvalidSeverity.
#[test]
fn composer_config_command_rejects_invalid_audit_severity() {
    ConfigFixture::new(json!({}))
        .command(&["audit.ignore-severity", "low", "invalid"])
        .assert()
        .failure()
        .stderr(contains(
            "valid severities include: low, medium, high, critical",
        ));
}

// Ported from Composer\Test\Command\ConfigCommandTest::
// testConfigThrowsWhenMergingArrayWithObject.
#[test]
fn composer_config_command_rejects_merging_an_audit_array_with_an_object() {
    ConfigFixture::new(json!({"config": {"audit": {"ignore": ["CVE-2024-1234"]}}}))
        .command(&[
            "audit.ignore",
            r#"{"CVE-2024-5678":"reason"}"#,
            "--json",
            "--merge",
        ])
        .assert()
        .failure()
        .stderr(contains("Cannot merge array and object"));
}

// Ported from Composer\Test\Command\ConfigCommandTest::
// testConfigThrowsWhenMergingPolicyArrayWithObject.
#[test]
fn composer_config_command_rejects_merging_a_policy_array_with_an_object() {
    ConfigFixture::new(json!({"config": {"policy": {"advisories": {
        "ignore": ["CVE-2024-1234"]
    }}}}))
    .command(&[
        "policy.advisories.ignore",
        r#"{"CVE-2024-5678":"reason"}"#,
        "--json",
        "--merge",
    ])
    .assert()
    .failure()
    .stderr(contains("Cannot merge array and object"));
}

// Ported from Composer\Test\Command\ConfigCommandTest::
// testConfigThrowsForInvalidPolicyAuditMode.
#[test]
fn composer_config_command_rejects_invalid_policy_audit_mode() {
    ConfigFixture::new(json!({}))
        .command(&["policy.advisories.audit", "bogus"])
        .assert()
        .failure();
}

// Ported from Composer\Test\Command\ConfigCommandTest::
// testConfigThrowsForInvalidPolicyBlockScope.
#[test]
fn composer_config_command_rejects_invalid_policy_block_scope() {
    ConfigFixture::new(json!({}))
        .command(&["policy.malware.block-scope", "bogus"])
        .assert()
        .failure();
}

// Ported from Composer\Test\Command\ConfigCommandTest::
// testConfigThrowsForInvalidPolicyIgnoreSeverity.
#[test]
fn composer_config_command_rejects_invalid_policy_severity() {
    ConfigFixture::new(json!({}))
        .command(&["policy.advisories.ignore-severity", "low", "bogus"])
        .assert()
        .failure()
        .stderr(contains(
            "valid severities include: low, medium, high, critical",
        ));
}

// Ported from Composer\Test\Command\ConfigCommandTest::
// testConfigThrowsForInvalidPolicyIgnoreUnreachableValue.
#[test]
fn composer_config_command_rejects_invalid_ignore_unreachable_scope() {
    ConfigFixture::new(json!({}))
        .command(&["policy.ignore-unreachable", r#"["bogus"]"#, "--json"])
        .assert()
        .failure();
}

// Ported from Composer\Test\Command\ConfigCommandTest::
// testConfigThrowsForInvalidPolicyListBoolValue.
#[test]
fn composer_config_command_rejects_invalid_policy_list_boolean() {
    ConfigFixture::new(json!({}))
        .command(&["policy.malware", "bogus"])
        .assert()
        .failure()
        .stderr(contains("expected a boolean"));
}

// Ported from Composer\Test\Command\ConfigCommandTest::testConfigThrowsPolicyListReserved.
#[test]
fn composer_config_command_rejects_reserved_policy_list_names() {
    for (key, value, message) in [
        ("policy.ignore-foo", "true", "reserved prefix \"ignore\""),
        (
            "policy.ignore-foo.block",
            "true",
            "reserved prefix \"ignore\"",
        ),
        ("policy.support.audit", "fail", "reserved for future use"),
        (
            "policy.ignore-foo.ignore",
            r#"["CVE-2024-1234"]"#,
            "reserved prefix \"ignore\"",
        ),
    ] {
        ConfigFixture::new(json!({}))
            .command(&[key, value])
            .assert()
            .failure()
            .stderr(contains(message));
    }
}
