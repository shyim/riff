use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;

fn cache_fixture() -> tempfile::TempDir {
    let cache = tempfile::tempdir().unwrap();
    fs::create_dir_all(cache.path().join("files/vendor/package")).unwrap();
    fs::write(
        cache.path().join("files/vendor/package/archive.zip"),
        b"cached",
    )
    .unwrap();
    cache
}

// Ported from Composer\Test\Command\ClearCacheCommandTest::testClearCacheCommandSuccess.
#[test]
fn composer_clear_cache_command_removes_riff_cache_entries() {
    let cache = cache_fixture();
    Command::cargo_bin("riff")
        .unwrap()
        .env("RIFF_CACHE_DIR", cache.path())
        .arg("clear-cache")
        .assert()
        .success()
        .stdout(predicate::str::contains("All caches cleared."));
    assert!(fs::read_dir(cache.path()).unwrap().next().is_none());
}

// Ported from Composer\Test\Command\ClearCacheCommandTest::
// testClearCacheCommandWithOptionGarbageCollection.
#[test]
fn composer_clear_cache_command_supports_garbage_collection() {
    let cache = cache_fixture();
    Command::cargo_bin("riff")
        .unwrap()
        .env("RIFF_CACHE_DIR", cache.path())
        .args(["clear-cache", "--gc"])
        .assert()
        .success()
        .stdout(predicate::str::contains("All caches garbage-collected."));
    assert!(cache
        .path()
        .join("files/vendor/package/archive.zip")
        .is_file());
}

// Ported from Composer\Test\Command\ClearCacheCommandTest::
// testClearCacheCommandWithOptionNoCache.
#[test]
fn composer_clear_cache_command_honors_disabled_cache() {
    let cache = cache_fixture();
    Command::cargo_bin("riff")
        .unwrap()
        .env("RIFF_CACHE_DIR", cache.path())
        .args(["clear-cache", "--no-cache"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Cache is not enabled"));
    assert!(cache
        .path()
        .join("files/vendor/package/archive.zip")
        .is_file());
}
