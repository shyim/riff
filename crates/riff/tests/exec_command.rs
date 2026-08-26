use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;

fn write_json(path: &Path, value: serde_json::Value) {
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn exec_list(project: &Path) -> Command {
    let mut command = Command::cargo_bin("riff").unwrap();
    command.args(["exec", "--list", "-d"]).arg(project);
    command
}

// Ported from Composer\Test\Command\ExecCommandTest::
// testListThrowsIfNoBinariesExist.
#[test]
fn composer_exec_list_requires_at_least_one_binary() {
    let project = tempfile::tempdir().unwrap();
    write_json(&project.path().join("composer.json"), json!({}));

    exec_list(project.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "No binaries found in composer.json or in bin-dir",
        ))
        .stderr(predicate::str::contains(
            Path::new("vendor").join("bin").display().to_string(),
        ));
}

// Ported from Composer\Test\Command\ExecCommandTest::testList.
#[test]
fn composer_exec_lists_vendor_and_root_binaries_without_bat_copies() {
    let project = tempfile::tempdir().unwrap();
    write_json(&project.path().join("composer.json"), json!({"bin": ["a"]}));
    let bin_dir = project.path().join("vendor/bin");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::write(bin_dir.join("b"), "").unwrap();
    fs::write(bin_dir.join("b.bat"), "").unwrap();
    fs::write(bin_dir.join("c"), "").unwrap();

    exec_list(project.path())
        .assert()
        .success()
        .stdout(predicate::eq(
            "Available binaries:\n- b\n- c\n- a (local)\n",
        ));
}
