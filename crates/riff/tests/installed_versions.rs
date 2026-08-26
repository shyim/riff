use std::path::Path;
use std::process::Command;

use riff_core::autoload::{AutoloadConfig, AutoloadGenerator, PackageAutoload, RootPackageInfo};
use serde_json::{json, Value};
use tempfile::TempDir;

fn package(
    name: &str,
    pretty_version: &str,
    version: &str,
    reference: Option<&str>,
) -> PackageAutoload {
    PackageAutoload {
        name: name.to_string(),
        install_path: name.to_string(),
        pretty_version: Some(pretty_version.to_string()),
        version: Some(version.to_string()),
        reference: reference.map(str::to_string),
        ..Default::default()
    }
}

fn fixture() -> TempDir {
    let temp = TempDir::new().unwrap();
    let mut provider = package("a/provider", "1.1", "1.1.0.0", Some("distref-as-no-source"));
    provider
        .provides
        .insert("foo/impl".to_string(), "^1.1".to_string());

    let mut provider2 = package(
        "a/provider2",
        "1.2",
        "1.2.0.0",
        Some("distref-as-installed-from-dist"),
    );
    provider2
        .provides
        .insert("foo/impl".to_string(), "1.2".to_string());

    let mut replacer = package("b/replacer", "2.2", "2.2.0.0", None);
    replacer
        .replaces
        .insert("foo/replaced".to_string(), "^3.0".to_string());

    let mut dev_package = package("c/c", "3.0", "3.0.0.0", None);
    dev_package.dev_requirement = true;

    let mut metapackage = package("meta/package", "1.0", "1.0.0.0", None);
    metapackage.package_type = "metapackage".to_string();

    let root = RootPackageInfo {
        name: "__root__".to_string(),
        pretty_version: "dev-master".to_string(),
        version: "dev-master".to_string(),
        reference: Some("sourceref-by-default".to_string()),
        package_type: "library".to_string(),
        aliases: vec!["1.10.x-dev".to_string()],
        dev_mode: true,
        ..Default::default()
    };
    AutoloadGenerator::new(AutoloadConfig {
        vendor_dir: temp.path().join("vendor"),
        base_dir: temp.path().to_path_buf(),
        ..Default::default()
    })
    .generate_installed_metadata(
        &[provider, provider2, replacer, dev_package, metapackage],
        Some(&root),
    )
    .unwrap();
    temp
}

fn php_binary() -> std::ffi::OsString {
    std::env::var_os("PHP_BINARY").unwrap_or_else(|| "php".into())
}

fn run_php(temp: &TempDir, body: &str) -> String {
    let composer_dir = temp.path().join("vendor/composer");
    let script = format!(
        "require $argv[1] . '/InstalledVersions.php'; error_reporting(E_ALL & ~E_DEPRECATED); {body}"
    );
    let output = Command::new(php_binary())
        .args(["-r", &script])
        .arg(composer_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "PHP exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn run_php_json(temp: &TempDir, expression: &str) -> Value {
    serde_json::from_str(&run_php(
        temp,
        &format!("echo json_encode({expression}, JSON_THROW_ON_ERROR);"),
    ))
    .unwrap()
}

#[test]
fn composer_installed_versions_lists_installed_and_virtual_packages() {
    let temp = fixture();
    assert_eq!(
        run_php_json(
            &temp,
            r"\Composer\InstalledVersions::getInstalledPackages()"
        ),
        json!([
            "__root__",
            "a/provider",
            "a/provider2",
            "b/replacer",
            "c/c",
            "foo/impl",
            "foo/replaced",
            "meta/package"
        ])
    );
}

#[test]
fn composer_installed_versions_checks_dev_and_virtual_packages() {
    let temp = fixture();
    assert_eq!(
        run_php_json(
            &temp,
            r"[
                \Composer\InstalledVersions::isInstalled('foo/impl'),
                \Composer\InstalledVersions::isInstalled('foo/replaced'),
                \Composer\InstalledVersions::isInstalled('c/c'),
                \Composer\InstalledVersions::isInstalled('c/c', false),
                \Composer\InstalledVersions::isInstalled('not/there')
            ]"
        ),
        json!([true, true, true, false, false])
    );
}

#[test]
fn composer_installed_versions_satisfies_through_the_supplied_parser() {
    let temp = fixture();
    let composer_dir = temp.path().join("vendor/composer");
    let script = r#"
namespace Composer\Semver {
    class Constraint {
        public function __construct(private string $value) {}
        public function matches(Constraint $constraint): bool { return $this->value === $constraint->value; }
    }
    class VersionParser {
        public function parseConstraints(string $value): Constraint { return new Constraint($value); }
    }
}
namespace {
    require $argv[1] . '/InstalledVersions.php';
    $parser = new \Composer\Semver\VersionParser();
    echo json_encode([
        \Composer\InstalledVersions::satisfies($parser, 'c/c', '3.0'),
        \Composer\InstalledVersions::satisfies($parser, 'c/c', '4.0'),
    ], JSON_THROW_ON_ERROR);
}
"#;
    let output = Command::new(php_binary())
        .args(["-r", script])
        .arg(composer_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "PHP exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!([true, false])
    );
}

#[test]
fn composer_installed_versions_combines_version_ranges() {
    let temp = fixture();
    assert_eq!(
        run_php_json(
            &temp,
            r"[
                \Composer\InstalledVersions::getVersionRanges('__root__'),
                \Composer\InstalledVersions::getVersionRanges('foo/impl'),
                \Composer\InstalledVersions::getVersionRanges('foo/replaced'),
                \Composer\InstalledVersions::getVersionRanges('c/c')
            ]"
        ),
        json!(["dev-master || 1.10.x-dev", "1.2 || ^1.1", "^3.0", "3.0"])
    );
}

#[test]
fn composer_installed_versions_returns_normalized_versions() {
    let temp = fixture();
    assert_eq!(
        run_php_json(
            &temp,
            r"[
                \Composer\InstalledVersions::getVersion('__root__'),
                \Composer\InstalledVersions::getVersion('foo/impl'),
                \Composer\InstalledVersions::getVersion('a/provider'),
                \Composer\InstalledVersions::getVersion('c/c')
            ]"
        ),
        json!(["dev-master", null, "1.1.0.0", "3.0.0.0"])
    );
}

#[test]
fn composer_installed_versions_returns_pretty_versions() {
    let temp = fixture();
    assert_eq!(
        run_php_json(
            &temp,
            r"[
                \Composer\InstalledVersions::getPrettyVersion('__root__'),
                \Composer\InstalledVersions::getPrettyVersion('foo/impl'),
                \Composer\InstalledVersions::getPrettyVersion('a/provider'),
                \Composer\InstalledVersions::getPrettyVersion('c/c')
            ]"
        ),
        json!(["dev-master", null, "1.1", "3.0"])
    );
}

#[test]
fn composer_installed_versions_rejects_unknown_versions() {
    let temp = fixture();
    assert_eq!(
        run_php(
            &temp,
            r"try { \Composer\InstalledVersions::getVersion('not/installed'); } catch (\Throwable $e) { echo get_class($e); }"
        ),
        "OutOfBoundsException"
    );
}

#[test]
fn composer_installed_versions_returns_root_package() {
    let temp = fixture();
    let root = run_php_json(&temp, r"\Composer\InstalledVersions::getRootPackage()");
    assert_eq!(root["name"], "__root__");
    assert_eq!(root["pretty_version"], "dev-master");
    assert_eq!(root["version"], "dev-master");
    assert_eq!(root["reference"], "sourceref-by-default");
    assert_eq!(root["type"], "library");
    assert_eq!(root["aliases"], json!(["1.10.x-dev"]));
    assert_eq!(root["dev"], true);
    let composer_dir = Path::new("vendor").join("composer");
    assert!(root["install_path"]
        .as_str()
        .unwrap()
        .ends_with(&format!("{}/../../", composer_dir.display())));
}

#[test]
fn composer_installed_versions_returns_raw_data() {
    let temp = fixture();
    let raw = run_php_json(&temp, r"\Composer\InstalledVersions::getRawData()");
    assert_eq!(raw["root"]["name"], "__root__");
    assert_eq!(raw["versions"]["a/provider"]["version"], "1.1.0.0");
    assert_eq!(raw["versions"]["foo/replaced"]["replaced"], json!(["^3.0"]));
}

#[test]
fn composer_installed_versions_returns_references() {
    let temp = fixture();
    assert_eq!(
        run_php_json(
            &temp,
            r"[
                \Composer\InstalledVersions::getReference('__root__'),
                \Composer\InstalledVersions::getReference('foo/impl'),
                \Composer\InstalledVersions::getReference('a/provider'),
                \Composer\InstalledVersions::getReference('b/replacer')
            ]"
        ),
        json!(["sourceref-by-default", null, "distref-as-no-source", null])
    );
}

#[test]
fn composer_installed_versions_filters_packages_by_type() {
    let temp = fixture();
    assert_eq!(
        run_php_json(
            &temp,
            r"\Composer\InstalledVersions::getInstalledPackagesByType('library')"
        ),
        json!(["__root__", "a/provider", "a/provider2", "b/replacer", "c/c"])
    );
}

#[test]
fn composer_installed_versions_returns_install_paths() {
    let temp = fixture();
    let paths = run_php_json(
        &temp,
        r"[
            \Composer\InstalledVersions::getInstallPath('__root__'),
            \Composer\InstalledVersions::getInstallPath('c/c'),
            \Composer\InstalledVersions::getInstallPath('foo/impl')
        ]",
    );
    let composer_dir = Path::new("vendor").join("composer");
    assert!(paths[0]
        .as_str()
        .unwrap()
        .ends_with(&format!("{}/../../", composer_dir.display())));
    assert!(paths[1]
        .as_str()
        .unwrap()
        .ends_with(&format!("{}/../c/c", composer_dir.display())));
    assert_eq!(paths[2], Value::Null);
}

#[test]
fn composer_installed_versions_can_reload_with_a_class_loader_present() {
    let temp = fixture();
    let composer_dir = temp.path().join("vendor/composer");
    let script = r#"
namespace Composer\Autoload {
    class ClassLoader { public static function getRegisteredLoaders(): array { return []; } }
}
namespace {
    require $argv[1] . '/InstalledVersions.php';
    $before = \Composer\InstalledVersions::isInstalled('foo/bar');
    \Composer\InstalledVersions::reload([
        'root' => \Composer\InstalledVersions::getRootPackage(),
        'versions' => ['foo/bar' => ['version' => '1.0.0', 'dev_requirement' => false]],
    ]);
    echo json_encode([$before, \Composer\InstalledVersions::isInstalled('foo/bar')], JSON_THROW_ON_ERROR);
}
"#;
    let output = Command::new(php_binary())
        .args(["-r", script])
        .arg(composer_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "PHP exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!([false, true])
    );
}
