//! Offline benchmark for version-inventory reuse. Build this same example at
//! both revisions, then alternate executions of the saved release binaries.
//! Usage: pool_optimizer_benchmark [single|tied|replacement] [iterations]

use riff_core::package::Package;
use riff_core::solver::{Policy, Pool, PoolOptimizer, Request};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::hint::black_box;
use std::time::Instant;

fn main() {
    let scenario = std::env::args().nth(1).unwrap_or_else(|| "single".into());
    assert!(matches!(
        scenario.as_str(),
        "single" | "tied" | "replacement"
    ));
    let iterations: usize = std::env::args()
        .nth(2)
        .map(|value| value.parse().unwrap())
        .unwrap_or(20);
    assert!(iterations > 0);

    let mut pool = Pool::new();
    let mut request = Request::new();
    for name_index in 0..300 {
        let name = format!("bench/package-{name_index}");
        let virtual_name = format!("virtual/package-{}", name_index / 4);
        if scenario == "replacement" {
            request.require(&virtual_name, "*");
        } else {
            request.require(&name, "^1.0");
        }
        for version_index in 0..100 {
            let copies = if scenario == "tied" && version_index == 99 {
                4
            } else {
                1
            };
            for _ in 0..copies {
                let mut package = Package::new(&name, format!("1.0.{version_index}"));
                if scenario == "replacement" {
                    package.replace.insert(virtual_name.clone(), "*".into());
                }
                pool.add_package(package);
            }
        }
    }

    let policy = Policy::new();
    let mut optimizer = PoolOptimizer::new(&policy);
    let optimized = optimizer.optimize(&request, &pool);
    assert_eq!(optimized.len(), if scenario == "tied" { 1200 } else { 300 });
    let mut fingerprint = DefaultHasher::new();
    for id in optimized.all_package_ids() {
        let entry = optimized.entry(id).unwrap();
        entry.name().hash(&mut fingerprint);
        entry.version().hash(&mut fingerprint);
        let inventory = optimized.removed_versions_by_package(id).unwrap();
        assert_eq!(inventory.len(), 100);
        inventory.hash(&mut fingerprint);
    }
    let signature = fingerprint.finish();
    drop(optimized);

    let mut elapsed_ms = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        drop(black_box(
            optimizer.optimize(black_box(&request), black_box(&pool)),
        ));
        elapsed_ms.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    println!(
        "{}",
        serde_json::json!({
            "scenario": scenario,
            "input_packages": pool.len(),
            "signature": format!("{signature:016x}"),
            "elapsed_ms": elapsed_ms,
        })
    );
}
