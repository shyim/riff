//! Benchmarks for composer.json / composer.lock handling.
//!
//! Every Riff command starts by reading and validating a manifest, and most of
//! them also read or rewrite a lock file. These benchmarks cover parsing,
//! schema validation, content hashing and repository metadata loading.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use riff_core::json::{
    parse_manifest, validate_composer_manifest, ManifestValidationOptions, RiffLockfile,
};
use riff_core::package::load_package_config;
use riff_core::util::compute_content_hash;
use serde_json::{json, Value};

const COMPOSER_MANIFEST: &str = include_str!("../tests/fixtures/composer-main.json");

/// Build a lock file with `count` packages, similar in shape to what Packagist
/// metadata produces for a mid-sized Symfony project.
fn lockfile_json(count: usize) -> String {
    let packages: Vec<Value> = (0..count)
        .map(|index| {
            json!({
                "name": format!("vendor{index}/package{index}"),
                "version": format!("1.{index}.0"),
                "source": {
                    "type": "git",
                    "url": format!("https://github.com/vendor{index}/package{index}.git"),
                    "reference": "0123456789abcdef0123456789abcdef01234567",
                },
                "dist": {
                    "type": "zip",
                    "url": format!("https://api.github.com/repos/vendor{index}/package{index}/zipball/0123456789abcdef"),
                    "reference": "0123456789abcdef0123456789abcdef01234567",
                    "shasum": "",
                },
                "require": {
                    "php": ">=8.1",
                    format!("vendor{}/package{}", index / 2, index / 2): "^1.0",
                },
                "require-dev": {
                    "phpunit/phpunit": "^10.0",
                },
                "type": "library",
                "autoload": {
                    "psr-4": { format!("Vendor{index}\\Package{index}\\"): "src/" },
                },
                "license": ["MIT"],
                "description": "A synthetic package used for benchmarking",
                "time": "2024-01-01T00:00:00+00:00",
            })
        })
        .collect();

    serde_json::to_string(&json!({
        "_readme": ["This file locks the dependencies of your project to a known state"],
        "content-hash": "0123456789abcdef0123456789abcdef",
        "packages": packages,
        "packages-dev": [],
        "aliases": [],
        "minimum-stability": "stable",
        "stability-flags": {},
        "prefer-stable": true,
        "prefer-lowest": false,
        "platform": { "php": ">=8.1" },
        "platform-dev": {},
    }))
    .expect("serialize lock file")
}

/// Repository metadata entries as returned by a Composer v2 repository.
fn repository_metadata(count: usize) -> Vec<Value> {
    (0..count)
        .map(|index| {
            json!({
                "name": format!("vendor{index}/package{index}"),
                "version": format!("1.{index}.0"),
                "version_normalized": format!("1.{index}.0.0"),
                "type": "library",
                "license": ["MIT"],
                "require": {
                    "php": "^8.1",
                    "psr/log": "^1.0 || ^2.0 || ^3.0",
                },
                "autoload": {
                    "psr-4": { format!("Vendor{index}\\Package{index}\\"): "src/" },
                    "classmap": ["src/legacy"],
                },
                "dist": {
                    "type": "zip",
                    "url": format!("https://example.com/vendor{index}.zip"),
                    "reference": "0123456789abcdef0123456789abcdef01234567",
                },
            })
        })
        .collect()
}

fn bench_manifest(c: &mut Criterion) {
    let mut group = c.benchmark_group("manifest");

    group.bench_function("parse", |b| {
        b.iter(|| black_box(parse_manifest(black_box(COMPOSER_MANIFEST)).expect("valid manifest")))
    });

    group.bench_function("content_hash", |b| {
        b.iter(|| black_box(compute_content_hash(black_box(COMPOSER_MANIFEST))))
    });

    group.bench_function("validate", |b| {
        b.iter(|| {
            black_box(validate_composer_manifest(
                black_box(COMPOSER_MANIFEST),
                "composer.json",
                ManifestValidationOptions::default(),
            ))
        })
    });

    group.finish();
}

fn bench_lockfile(c: &mut Criterion) {
    let mut group = c.benchmark_group("lockfile");

    for count in [25usize, 200] {
        let content = lockfile_json(count);

        group.bench_function(BenchmarkId::new("parse", count), |b| {
            b.iter(|| {
                black_box(RiffLockfile::from_str(black_box(&content)).expect("valid lock file"))
            })
        });

        let lockfile = RiffLockfile::from_str(&content).expect("valid lock file");
        group.bench_function(BenchmarkId::new("serialize", count), |b| {
            b.iter(|| black_box(black_box(&lockfile).to_json().expect("serializable")))
        });
    }

    group.finish();
}

fn bench_package_loading(c: &mut Criterion) {
    let metadata = repository_metadata(200);

    let mut group = c.benchmark_group("repository");
    group.bench_function(
        BenchmarkId::new("load_package_config", metadata.len()),
        |b| {
            b.iter(|| {
                for entry in &metadata {
                    black_box(load_package_config(black_box(entry)).expect("valid package config"));
                }
            })
        },
    );
    group.finish();
}

criterion_group!(
    benches,
    bench_manifest,
    bench_lockfile,
    bench_package_loading
);
criterion_main!(benches);
