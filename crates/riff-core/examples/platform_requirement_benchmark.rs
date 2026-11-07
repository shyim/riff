//! Compare uncached and selection-scoped platform checks without network or PHP.
use riff_core::{
    solver::{package_matches_platform_requirements, PlatformRequirementMatcher},
    Package, RiffLockfile,
};
use std::{
    collections::{BTreeSet, HashSet},
    hint::black_box,
    time::Instant,
};
fn main() {
    let argument = std::env::args().nth(1).unwrap_or_else(|| "repeated".into());
    let packages: Vec<Package> = if argument == "repeated" || argument == "unique" {
        (0..10_000)
            .map(|i| {
                let mut p = Package::new(format!("vendor/pkg{i}"), "1.0.0.0");
                let constraint = if argument == "unique" {
                    format!(">=7.0.{i}")
                } else {
                    ["^7.4 || ^8.0", "^8.1", ">=8.0"][i % 3].to_owned()
                };
                p.require.insert("php".into(), constraint);
                p
            })
            .collect()
    } else {
        RiffLockfile::from_file(&argument)
            .unwrap()
            .all_packages()
            .map(Package::from)
            .collect()
    };
    let mut names: BTreeSet<String> = packages
        .iter()
        .flat_map(|p| p.require.keys())
        .filter(|n| riff_core::is_platform_package(n))
        .map(ToString::to_string)
        .collect();
    names.insert("php".into());
    let platform: Vec<_> = names.iter().map(|n| Package::new(n, "8.2.0.0")).collect();
    let pairs: HashSet<_> = packages
        .iter()
        .flat_map(|p| p.require.iter())
        .filter(|(n, _)| riff_core::is_platform_package(n))
        .map(|(n, c)| (n.to_string(), c.to_string()))
        .collect();
    let calls = packages
        .iter()
        .flat_map(|p| p.require.keys())
        .filter(|n| riff_core::is_platform_package(n))
        .count();
    let mut samples = Vec::new();
    for round in 0..9 {
        for cached in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let start = Instant::now();
            let mut fingerprint = 0;
            for _ in 0..20 {
                let mut matcher = PlatformRequirementMatcher::new(&platform);
                for p in &packages {
                    fingerprint += usize::from(black_box(if cached {
                        matcher.matches(p)
                    } else {
                        package_matches_platform_requirements(p, &platform)
                    }));
                }
            }
            samples.push(serde_json::json!({"cached":cached,"ms_per_pass":start.elapsed().as_secs_f64()*1000.0/20.0,"fingerprint":fingerprint}));
        }
    }
    assert!(samples
        .iter()
        .all(|s| s["fingerprint"] == samples[0]["fingerprint"]));
    println!(
        "{}",
        serde_json::json!({"fixture":argument,"packages":packages.len(),"platform_requirement_occurrences":calls,"distinct_pairs":pairs.len(),"samples":samples})
    );
}
