use assert_cmd::Command;

// Ported from Composer\Test\Command\AboutCommandTest::testAbout.
#[test]
fn composer_about_command_describes_riff() {
    Command::cargo_bin("riff")
        .unwrap()
        .arg("--php")
        .arg("/this/path/must/not/be-executed")
        .arg("about")
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "Riff - Composer-compatible Dependency Manager for PHP - version {}",
            env!("CARGO_PKG_VERSION")
        )))
        .stdout(predicates::str::contains(
            "Riff is a fast, standalone package manager tracking local dependencies of your projects and libraries.",
        ))
        .stdout(predicates::str::contains(
            "See https://github.com/shyim/riff for more information.",
        ));
}
