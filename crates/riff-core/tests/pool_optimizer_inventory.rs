use riff_core::package::Package;
use riff_core::solver::{Policy, Pool, PoolOptimizer, Request};
use std::collections::BTreeMap;

#[test]
fn tied_candidates_preserve_pretty_versions_and_prior_inventories() {
    let mut pool = Pool::new();
    for (version, pretty) in [("1.0.0", "v1.0.0"), ("1.1.0", "1.1.0"), ("1.1.0", "v1.1.0")] {
        let mut package = Package::new("vendor/a", version);
        package.pretty_version = Some(pretty.into());
        pool.add_package(package);
    }
    let mut request = Request::new();
    request.require("vendor/a", "^1.0");
    let policy = Policy::new();
    let mut optimizer = PoolOptimizer::new(&policy);
    let expected = BTreeMap::from([
        ("1.0.0".to_owned(), "v1.0.0".to_owned()),
        ("1.1.0".to_owned(), "v1.1.0".to_owned()),
    ]);

    let optimized = optimizer.optimize(&request, &pool);
    let reoptimized = optimizer.optimize(&request, &optimized);
    for result in [&optimized, &reoptimized] {
        assert_eq!(result.len(), 2);
        for id in result.all_package_ids() {
            assert_eq!(result.entry(id).unwrap().version(), "1.1.0");
            assert_eq!(result.removed_versions_by_package(id), Some(&expected));
        }
    }

    // Reusing an optimizer for another input must not carry over old inventories.
    let mut other_pool = Pool::new();
    other_pool.add_package(Package::new("vendor/a", "1.2.0"));
    let other = optimizer.optimize(&request, &other_pool);
    let id = other.all_package_ids().next().unwrap();
    assert_eq!(
        other.removed_versions_by_package(id),
        Some(&BTreeMap::from([("1.2.0".into(), "1.2.0".into())]))
    );
}
