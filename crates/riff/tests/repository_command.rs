use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use serde_json::{json, Value};

struct RepositoryFixture {
    root: tempfile::TempDir,
    home: PathBuf,
}

impl RepositoryFixture {
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
            .arg("repo")
            .args(arguments)
            .args(["-d"])
            .arg(self.root.path());
        command
    }

    fn document(&self) -> Value {
        serde_json::from_slice(&fs::read(self.root.path().join("composer.json")).unwrap()).unwrap()
    }

    fn contents(&self) -> Vec<u8> {
        fs::read(self.root.path().join("composer.json")).unwrap()
    }
}

fn output(command: &mut Command) -> String {
    let assertion = command.assert().success();
    String::from_utf8(assertion.get_output().stdout.clone())
        .unwrap()
        .trim()
        .to_string()
}

fn named_repository(name: &str, repository_type: &str, url: &str) -> Value {
    json!({"name": name, "type": repository_type, "url": url})
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testListWithNoRepositories.
#[test]
fn composer_repository_command_lists_default_packagist_without_mutating_the_manifest() {
    let fixture = RepositoryFixture::new(json!({}));
    let before = fixture.contents();

    assert_eq!(
        output(&mut fixture.command(&["list"])),
        "[packagist.org] composer https://repo.packagist.org"
    );
    assert_eq!(fixture.contents(), before);
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testListWithRepositoriesAsList.
#[test]
fn composer_repository_command_lists_repositories_stored_as_a_list() {
    let fixture = RepositoryFixture::new(json!({"repositories": [
        {"type": "composer", "url": "https://first.test"},
        named_repository("foo", "vcs", "https://old.example.org"),
        named_repository("bar", "vcs", "https://other.example.org")
    ]}));

    assert_eq!(
        output(&mut fixture.command(&["list"])),
        "[0] composer https://first.test\n[foo] vcs https://old.example.org\n[bar] vcs https://other.example.org\n[packagist.org] disabled"
    );
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testListWithRepositoriesAsAssoc.
#[test]
fn composer_repository_command_lists_repositories_stored_as_an_object() {
    let fixture = RepositoryFixture::new(json!({"repositories": {
        "0": {"type": "composer", "url": "https://first.test"},
        "foo": {"type": "vcs", "url": "https://old.example.org"},
        "bar": {"type": "vcs", "url": "https://other.example.org"}
    }}));

    assert_eq!(
        output(&mut fixture.command(&["list"])),
        "[0] composer https://first.test\n[foo] vcs https://old.example.org\n[bar] vcs https://other.example.org\n[packagist.org] disabled"
    );
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testAddRepositoryWithTypeAndUrl.
#[test]
fn composer_repository_command_adds_a_repository_from_type_and_url() {
    let fixture = RepositoryFixture::new(json!({}));
    fixture
        .command(&["add", "foo", "vcs", "https://example.org/foo.git"])
        .assert()
        .success();

    assert_eq!(
        fixture.document(),
        json!({"repositories": [named_repository(
            "foo",
            "vcs",
            "https://example.org/foo.git"
        )]})
    );
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testAddRepositoryWithJson.
#[test]
fn composer_repository_command_adds_a_repository_from_json() {
    let fixture = RepositoryFixture::new(json!({}));
    fixture
        .command(&[
            "add",
            "bar",
            r#"{"type":"composer","url":"https://repo.example.org"}"#,
        ])
        .assert()
        .success();

    assert_eq!(
        fixture.document(),
        json!({"repositories": [named_repository(
            "bar",
            "composer",
            "https://repo.example.org"
        )]})
    );
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testRemoveRepository.
#[test]
fn composer_repository_command_removes_a_repository() {
    let fixture = RepositoryFixture::new(json!({"repositories": {
        "foo": {"type": "vcs", "url": "https://example.org"}
    }}));
    fixture.command(&["remove", "foo"]).assert().success();

    assert_eq!(fixture.document(), json!({}));
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testSetAndGetUrlInRepositoryAssoc.
#[test]
fn composer_repository_command_sets_and_gets_urls_without_converting_repository_objects() {
    for name in ["first", "foo", "bar"] {
        let fixture = RepositoryFixture::new(json!({"repositories": {
            "first": {"type": "composer", "url": "https://first.test"},
            "foo": {"type": "vcs", "url": "https://old.example.org"},
            "bar": {"type": "vcs", "url": "https://other.example.org"}
        }}));
        fixture
            .command(&["set-url", name, "https://new.example.org"])
            .assert()
            .success();

        assert_eq!(
            fixture.document()["repositories"][name]["url"],
            "https://new.example.org"
        );
        assert_eq!(
            output(&mut fixture.command(&["get-url", name])),
            "https://new.example.org"
        );
        assert!(fixture.document()["repositories"].is_object());
    }
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testSetAndGetUrlInRepositoryList.
#[test]
fn composer_repository_command_sets_and_gets_urls_in_repository_lists() {
    for (name, index) in [("first", 0), ("foo", 1), ("bar", 2)] {
        let fixture = RepositoryFixture::new(json!({"repositories": [
            named_repository("first", "composer", "https://first.test"),
            named_repository("foo", "vcs", "https://old.example.org"),
            named_repository("bar", "vcs", "https://other.example.org")
        ]}));
        fixture
            .command(&["set-url", name, "https://new.example.org"])
            .assert()
            .success();

        assert_eq!(fixture.document()["repositories"][index]["name"], name);
        assert_eq!(
            fixture.document()["repositories"][index]["url"],
            "https://new.example.org"
        );
        assert_eq!(
            output(&mut fixture.command(&["get-url", name])),
            "https://new.example.org"
        );
    }
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testDisableAndEnablePackagist.
#[test]
fn composer_repository_command_disables_and_enables_packagist() {
    let fixture = RepositoryFixture::new(json!({}));
    fixture
        .command(&["disable", "packagist"])
        .assert()
        .success();
    assert_eq!(
        fixture.document(),
        json!({"repositories": [{"packagist.org": false}]})
    );

    fixture.command(&["enable", "packagist"]).assert().success();
    assert_eq!(fixture.document(), json!({}));
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testInvalidArgCombinationThrows.
#[test]
fn composer_repository_command_rejects_file_with_global() {
    let fixture = RepositoryFixture::new(json!({"name": "keep/me"}));
    let before = fixture.contents();
    fixture
        .command(&["list", "--file", "alt.composer.json", "--global"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "--file and --global can not be combined",
        ));
    assert_eq!(fixture.contents(), before);
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testPrependRepositoryByNameListToAssoc.
#[test]
fn composer_repository_command_prepends_a_named_repository() {
    let fixture = RepositoryFixture::new(json!({"repositories": [
        {"type": "git", "url": "example.tld"}
    ]}));
    fixture
        .command(&["add", "foo", "path", "foo/bar"])
        .assert()
        .success();

    assert_eq!(
        fixture.document()["repositories"],
        json!([
            named_repository("foo", "path", "foo/bar"),
            {"type": "git", "url": "example.tld"}
        ])
    );
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testAppendRepositoryByNameListToAssoc.
#[test]
fn composer_repository_command_appends_a_named_repository() {
    let fixture = RepositoryFixture::new(json!({"repositories": [
        {"type": "git", "url": "example.tld"}
    ]}));
    fixture
        .command(&["add", "foo", "path", "foo/bar", "--append"])
        .assert()
        .success();

    assert_eq!(
        fixture.document()["repositories"],
        json!([
            {"type": "git", "url": "example.tld"},
            named_repository("foo", "path", "foo/bar")
        ])
    );
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testPrependRepositoryAssocWithPackagistDisabled.
#[test]
fn composer_repository_command_prepends_before_a_disabled_packagist_entry() {
    let fixture = RepositoryFixture::new(json!({"repositories": {
        "0": {"type": "git", "url": "example.tld"},
        "packagist.org": false
    }}));
    fixture
        .command(&["add", "foo", "path", "foo/bar"])
        .assert()
        .success();

    assert_eq!(
        fixture.document()["repositories"],
        json!([
            named_repository("foo", "path", "foo/bar"),
            {"type": "git", "url": "example.tld"},
            {"packagist.org": false}
        ])
    );
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testAppendRepositoryAssocWithPackagistDisabled.
#[test]
fn composer_repository_command_appends_after_a_disabled_packagist_entry() {
    let fixture = RepositoryFixture::new(json!({"repositories": {
        "0": {"type": "git", "url": "example.tld"},
        "packagist.org": false
    }}));
    fixture
        .command(&["add", "foo", "path", "foo/bar", "--append"])
        .assert()
        .success();

    assert_eq!(
        fixture.document()["repositories"],
        json!([
            {"type": "git", "url": "example.tld"},
            {"packagist.org": false},
            named_repository("foo", "path", "foo/bar")
        ])
    );
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testAddBeforeAndAfterByName.
#[test]
fn composer_repository_command_inserts_before_and_after_named_repositories() {
    let fixture = RepositoryFixture::new(json!({"repositories": [
        named_repository("alpha", "vcs", "https://example.org/a"),
        named_repository("omega", "vcs", "https://example.org/o"),
        {"packagist.org": false}
    ]}));
    fixture
        .command(&[
            "add",
            "beta",
            "vcs",
            "https://example.org/b",
            "--before",
            "omega",
        ])
        .assert()
        .success();
    fixture
        .command(&[
            "add",
            "gamma",
            "vcs",
            "https://example.org/g",
            "--after",
            "alpha",
        ])
        .assert()
        .success();

    assert_eq!(
        fixture.document()["repositories"],
        json!([
            named_repository("alpha", "vcs", "https://example.org/a"),
            named_repository("gamma", "vcs", "https://example.org/g"),
            named_repository("beta", "vcs", "https://example.org/b"),
            named_repository("omega", "vcs", "https://example.org/o"),
            {"packagist.org": false}
        ])
    );
}

// Ported from Composer\Test\Command\RepositoryCommandTest::testAddSameNameReplacesExisting.
#[test]
fn composer_repository_command_replaces_an_existing_repository_with_the_same_name() {
    let fixture = RepositoryFixture::new(json!({}));
    fixture
        .command(&["add", "foo", "vcs", "https://example.org/old"])
        .assert()
        .success();
    fixture
        .command(&["add", "foo", "vcs", "https://example.org/new", "--append"])
        .assert()
        .success();

    let repositories = fixture.document()["repositories"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(repositories.len(), 1);
    assert_eq!(repositories[0]["name"], "foo");
    assert_eq!(repositories[0]["url"], "https://example.org/new");
}
