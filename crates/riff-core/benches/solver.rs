//! Benchmarks for the SAT-based dependency solver.
//!
//! The solver is the hot path of `riff install`/`riff update`: it builds a pool
//! from repository metadata, generates SAT rules and resolves a complete
//! dependency set. The benchmarks below build synthetic package universes that
//! mimic a Composer project (many packages, several versions each, transitive
//! requirements) so the measurements stay deterministic and offline.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use riff_core::package::Package;
use riff_core::solver::{Policy, Pool, Request, Solver};

/// Build a package with the given requirements.
fn package(name: &str, version: &str, requires: &[(String, String)]) -> Package {
    let mut package = Package::new(name, version);
    for (dependency, constraint) in requires {
        package
            .require
            .insert(dependency.clone(), constraint.clone());
    }
    package
}

/// Build a layered package universe.
///
/// Every layer depends on the layer below it, so the solver has to walk the
/// whole graph and pick a version for each package, just like a real project
/// with transitive dependencies.
fn universe(layers: usize, packages_per_layer: usize, versions: usize) -> Vec<Package> {
    let mut result = Vec::with_capacity(layers * packages_per_layer * versions);

    for layer in 0..layers {
        for index in 0..packages_per_layer {
            let name = format!("vendor{layer}/package{index}");
            for version in 0..versions {
                let requires: Vec<(String, String)> = if layer == 0 {
                    Vec::new()
                } else {
                    // Depend on two packages of the previous layer.
                    (0..2)
                        .map(|offset| {
                            let dependency = (index + offset) % packages_per_layer;
                            (
                                format!("vendor{}/package{dependency}", layer - 1),
                                format!(">=1.{version}.0"),
                            )
                        })
                        .collect()
                };
                result.push(package(&name, &format!("1.{version}.0"), &requires));
            }
        }
    }

    result
}

fn build_pool(packages: &[Package]) -> Pool {
    let mut pool = Pool::new();
    for package in packages {
        pool.add_package(package.clone());
    }
    pool
}

fn root_request(layers: usize, packages_per_layer: usize) -> Request {
    let mut request = Request::new();
    for index in 0..packages_per_layer {
        request.require(format!("vendor{}/package{index}", layers - 1), "*");
    }
    request
}

fn bench_pool_build(c: &mut Criterion) {
    let packages = universe(4, 12, 6);

    let mut group = c.benchmark_group("solver");
    group.bench_function(BenchmarkId::new("pool_build", packages.len()), |b| {
        b.iter(|| black_box(build_pool(black_box(&packages))).len())
    });
    group.finish();
}

fn bench_solve(c: &mut Criterion) {
    let mut group = c.benchmark_group("solver");

    for (label, layers, packages_per_layer, versions) in [
        ("small", 3usize, 5usize, 3usize),
        ("medium", 4, 12, 6),
        ("large", 5, 20, 8),
    ] {
        let packages = universe(layers, packages_per_layer, versions);
        let pool = build_pool(&packages);
        let request = root_request(layers, packages_per_layer);
        let policy = Policy::new();

        group.bench_function(BenchmarkId::new("solve", label), |b| {
            b.iter(|| {
                let solver = Solver::new(black_box(&pool), &policy);
                let result = solver.solve(black_box(&request)).expect("solvable request");
                black_box(result.packages.len())
            })
        });
    }

    group.finish();
}

fn bench_what_provides(c: &mut Criterion) {
    let packages = universe(4, 12, 6);
    let pool = build_pool(&packages);
    let queries: Vec<(String, &str)> = (0..12)
        .map(|index| (format!("vendor3/package{index}"), "^1.0"))
        .collect();

    let mut group = c.benchmark_group("solver");
    group.bench_function("pool_what_provides", |b| {
        b.iter(|| {
            for (name, constraint) in &queries {
                black_box(pool.what_provides(black_box(name), Some(black_box(constraint))));
            }
        })
    });
    group.finish();
}

criterion_group!(benches, bench_pool_build, bench_solve, bench_what_provides);
criterion_main!(benches);
