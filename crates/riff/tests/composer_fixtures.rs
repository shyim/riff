mod support;

include!(concat!(env!("OUT_DIR"), "/composer_fixture_cases.rs"));

// Ported from Composer\Test\InstallerTest::testInstaller. The upstream method
// has two inline root/dependency cycle provider cases; the fixture exercises
// the same published-root cycle contract through Riff's integration harness.
#[test]
fn composer_installer_inline_root_cycle_contract() {
    support::composer_fixture::run(include_str!(
        "fixtures/composer/installer/circular-dependency.test"
    ));
}
