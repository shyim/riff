use std::ffi::OsString;
use std::fs;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

const CLASS_LOADER: &str = include_str!("../src/autoload/ClassLoader.php.template");

fn php_binary() -> OsString {
    std::env::var_os("PHP_BINARY").unwrap_or_else(|| "php".into())
}

fn fixture() -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("ClassLoader.php"), CLASS_LOADER).unwrap();
    for (path, contents) in [
        (
            "Fixtures/Namespaced/Foo.php",
            "<?php namespace Namespaced; class Foo {}",
        ),
        ("Fixtures/Pearlike/Foo.php", "<?php class Pearlike_Foo {}"),
        (
            "Fixtures/SubNamespace/Foo.php",
            "<?php namespace ShinyVendor\\ShinyPackage\\SubNamespace; class Foo {}",
        ),
    ] {
        let path = temp.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    temp
}

fn run_php_json(temp: &TempDir, body: &str) -> Value {
    let script = format!("require $argv[1] . '/ClassLoader.php'; {body}");
    let output = Command::new(php_binary())
        .args(["-d", "apc.enabled=0", "-r", &script])
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "PHP exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

// Ported from Composer\Test\Autoload\ClassLoaderTest::testLoadClass.
#[test]
fn composer_class_loader_loads_psr0_and_psr4_classes() {
    let temp = fixture();
    let loaded = run_php_json(
        &temp,
        r#"
        $loader = new \Composer\Autoload\ClassLoader();
        $fixtures = $argv[1] . '/Fixtures';
        $loader->add('Namespaced\\', $fixtures);
        $loader->add('Pearlike_', $fixtures);
        $loader->addPsr4('ShinyVendor\\ShinyPackage\\', $fixtures);
        $loaded = array();
        foreach (array(
            'Namespaced\\Foo',
            'Pearlike_Foo',
            'ShinyVendor\\ShinyPackage\\SubNamespace\\Foo'
        ) as $class) {
            $loaded[$class] = $loader->loadClass($class) && class_exists($class, false);
        }
        echo json_encode($loaded, JSON_THROW_ON_ERROR);
        "#,
    );

    assert_eq!(
        loaded,
        serde_json::json!({
            "Namespaced\\Foo": true,
            "Pearlike_Foo": true,
            "ShinyVendor\\ShinyPackage\\SubNamespace\\Foo": true,
        })
    );
}

// Ported from Composer\Test\Autoload\ClassLoaderTest::
// testGetPrefixesWithNoPSR0Configuration.
#[test]
fn composer_class_loader_has_no_psr0_prefixes_by_default() {
    let temp = fixture();
    let prefixes = run_php_json(
        &temp,
        r#"
        $loader = new \Composer\Autoload\ClassLoader();
        echo json_encode($loader->getPrefixes(), JSON_THROW_ON_ERROR);
        "#,
    );

    assert_eq!(prefixes, serde_json::json!([]));
}

// Ported from Composer\Test\Autoload\ClassLoaderTest::testSerializability.
#[test]
fn composer_class_loader_preserves_configuration_when_serialized() {
    let temp = fixture();
    let result = run_php_json(
        &temp,
        r#"
        $loader = new \Composer\Autoload\ClassLoader();
        $loader->add('Pearlike_', $argv[1] . '/Fixtures');
        $loader->add('', $argv[1] . '/FALLBACK');
        $loader->addPsr4('ShinyVendor\\ShinyPackage\\', $argv[1] . '/Fixtures');
        $loader->addPsr4('', $argv[1] . '/FALLBACKPSR4');
        $loader->addClassMap(array('A' => '', 'B' => 'path'));
        $loader->setApcuPrefix('prefix');
        $loader->setClassMapAuthoritative(true);
        $loader->setUseIncludePath(true);

        $copy = unserialize(serialize($loader));
        echo json_encode(array(
            'instance' => $copy instanceof \Composer\Autoload\ClassLoader,
            'apcu' => $loader->getApcuPrefix() === $copy->getApcuPrefix(),
            'classMap' => $loader->getClassMap() === $copy->getClassMap(),
            'fallbackPsr0' => $loader->getFallbackDirs() === $copy->getFallbackDirs(),
            'fallbackPsr4' => $loader->getFallbackDirsPsr4() === $copy->getFallbackDirsPsr4(),
            'prefixesPsr0' => $loader->getPrefixes() === $copy->getPrefixes(),
            'prefixesPsr4' => $loader->getPrefixesPsr4() === $copy->getPrefixesPsr4(),
            'includePath' => $loader->getUseIncludePath() === $copy->getUseIncludePath(),
            'authoritative' => $loader->isClassMapAuthoritative() === $copy->isClassMapAuthoritative(),
        ), JSON_THROW_ON_ERROR);
        "#,
    );

    assert_eq!(
        result,
        serde_json::json!({
            "instance": true,
            "apcu": true,
            "classMap": true,
            "fallbackPsr0": true,
            "fallbackPsr4": true,
            "prefixesPsr0": true,
            "prefixesPsr4": true,
            "includePath": true,
            "authoritative": true,
        })
    );
}
