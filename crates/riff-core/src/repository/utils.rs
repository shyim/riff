use std::collections::HashSet;
use std::sync::Arc;

use crate::json::RiffManifest;
use crate::Package;

pub struct RepositoryUtils;

impl RepositoryUtils {
    /// Keep repository candidates Composer would make solver-visible for a
    /// requested package name.
    ///
    /// A provider/replacer is not selected merely because it is the only
    /// package mentioning a missing virtual name. A provider becomes eligible
    /// when its own package name is independently selected. A replacer may also
    /// be considered beside a real package of the requested name.
    pub fn filter_solver_candidates(
        requested_name: &str,
        packages: Vec<Arc<Package>>,
        is_selected_name: impl Fn(&str) -> bool,
    ) -> Vec<Arc<Package>> {
        let has_direct = packages
            .iter()
            .any(|package| package.name.eq_ignore_ascii_case(requested_name));

        packages
            .into_iter()
            .filter(|package| {
                package.name.eq_ignore_ascii_case(requested_name)
                    || is_selected_name(&package.name)
                    || (has_direct
                        && package
                            .replace
                            .keys()
                            .any(|name| name.eq_ignore_ascii_case(requested_name)))
            })
            .collect()
    }

    pub fn filter_required_packages(
        packages: &[Arc<Package>],
        manifest: &RiffManifest,
    ) -> Vec<Arc<Package>> {
        Self::filter_required_packages_internal(packages, manifest, false)
    }

    pub fn filter_required_packages_with_dev(
        packages: &[Arc<Package>],
        manifest: &RiffManifest,
    ) -> Vec<Arc<Package>> {
        Self::filter_required_packages_internal(packages, manifest, true)
    }

    fn filter_required_packages_internal(
        packages: &[Arc<Package>],
        manifest: &RiffManifest,
        include_require_dev: bool,
    ) -> Vec<Arc<Package>> {
        let mut required_names: HashSet<String> =
            manifest.require.keys().map(|s| s.to_lowercase()).collect();

        if include_require_dev {
            required_names.extend(manifest.require_dev.keys().map(|s| s.to_lowercase()));
        }

        let mut name_to_packages: std::collections::HashMap<String, Vec<Arc<Package>>> =
            std::collections::HashMap::new();

        for package in packages {
            name_to_packages
                .entry(package.name.to_lowercase())
                .or_default()
                .push(Arc::clone(package));

            for provided in package.provide.keys() {
                name_to_packages
                    .entry(provided.as_str().to_lowercase())
                    .or_default()
                    .push(Arc::clone(package));
            }

            for replaced in package.replace.keys() {
                name_to_packages
                    .entry(replaced.as_str().to_lowercase())
                    .or_default()
                    .push(Arc::clone(package));
            }
        }

        let mut result: Vec<Arc<Package>> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut to_process: Vec<String> = required_names.into_iter().collect();

        while let Some(name) = to_process.pop() {
            if let Some(candidates) = name_to_packages.get(&name) {
                for package in candidates {
                    let pkg_key = package.name.to_lowercase();
                    if seen.contains(&pkg_key) {
                        continue;
                    }
                    seen.insert(pkg_key);
                    result.push(Arc::clone(package));

                    for dep_name in package.require.keys() {
                        let dep_lower = dep_name.as_str().to_lowercase();
                        if !seen.contains(&dep_lower) {
                            to_process.push(dep_lower);
                        }
                    }
                }
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_package(name: &str, requires: &[&str]) -> Arc<Package> {
        let mut pkg = Package::new(name, "1.0.0");
        for req in requires {
            pkg.require.insert(req.to_string(), "*".to_string());
        }
        Arc::new(pkg)
    }

    #[test]
    fn test_filter_required_packages_simple() {
        let packages = vec![
            make_package("vendor/a", &[]),
            make_package("vendor/b", &[]),
            make_package("vendor/c", &[]),
        ];

        let mut manifest = RiffManifest::default();
        manifest
            .require
            .insert("vendor/a".to_string(), "*".to_string());

        let filtered = RepositoryUtils::filter_required_packages(&packages, &manifest);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "vendor/a");
    }

    #[test]
    fn test_filter_required_packages_transitive() {
        let packages = vec![
            make_package("vendor/a", &["vendor/b"]),
            make_package("vendor/b", &["vendor/c"]),
            make_package("vendor/c", &[]),
            make_package("vendor/d", &[]),
        ];

        let mut manifest = RiffManifest::default();
        manifest
            .require
            .insert("vendor/a".to_string(), "*".to_string());

        let filtered = RepositoryUtils::filter_required_packages(&packages, &manifest);
        assert_eq!(filtered.len(), 3);

        let names: HashSet<_> = filtered.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains("vendor/a"));
        assert!(names.contains("vendor/b"));
        assert!(names.contains("vendor/c"));
        assert!(!names.contains("vendor/d"));
    }

    #[test]
    fn test_filter_required_packages_with_dev() {
        let packages = vec![
            make_package("vendor/a", &[]),
            make_package("vendor/dev", &[]),
        ];

        let mut manifest = RiffManifest::default();
        manifest
            .require
            .insert("vendor/a".to_string(), "*".to_string());
        manifest
            .require_dev
            .insert("vendor/dev".to_string(), "*".to_string());

        let filtered = RepositoryUtils::filter_required_packages(&packages, &manifest);
        assert_eq!(filtered.len(), 1);

        let filtered_with_dev =
            RepositoryUtils::filter_required_packages_with_dev(&packages, &manifest);
        assert_eq!(filtered_with_dev.len(), 2);
    }

    #[test]
    fn test_filter_required_packages_circular() {
        let mut pkg_a = Package::new("vendor/a", "1.0.0");
        pkg_a
            .require
            .insert("vendor/b".to_string(), "*".to_string());

        let mut pkg_b = Package::new("vendor/b", "1.0.0");
        pkg_b
            .require
            .insert("vendor/a".to_string(), "*".to_string());

        let packages = vec![Arc::new(pkg_a), Arc::new(pkg_b)];

        let mut manifest = RiffManifest::default();
        manifest
            .require
            .insert("vendor/a".to_string(), "*".to_string());

        let filtered = RepositoryUtils::filter_required_packages(&packages, &manifest);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_filter_required_packages_with_provides() {
        let mut pkg_impl = Package::new("vendor/impl", "1.0.0");
        pkg_impl
            .provide
            .insert("vendor/interface".to_string(), "1.0.0".to_string());

        let packages = vec![Arc::new(pkg_impl), make_package("vendor/other", &[])];

        let mut manifest = RiffManifest::default();
        manifest
            .require
            .insert("vendor/interface".to_string(), "*".to_string());

        let filtered = RepositoryUtils::filter_required_packages(&packages, &manifest);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "vendor/impl");
    }

    #[test]
    fn composer_filter_required_packages_data_provider() {
        let packages = vec![
            make_package("required/a", &[]),
            make_package("required/b", &["required/c"]),
            make_package("required/c", &[]),
            make_package("required/circular", &["required/circular-b"]),
            make_package("required/circular-b", &["required/circular"]),
            make_package("dummy/pkg", &[]),
        ];
        let names = |filtered: Vec<Arc<Package>>| {
            filtered
                .into_iter()
                .map(|package| package.name.clone())
                .collect::<HashSet<_>>()
        };

        let empty = RiffManifest::default();
        assert!(RepositoryUtils::filter_required_packages(&packages, &empty).is_empty());

        let mut dev = RiffManifest::default();
        dev.require_dev
            .insert("required/a".to_string(), "*".to_string());
        assert!(RepositoryUtils::filter_required_packages(&packages, &dev).is_empty());
        assert_eq!(
            names(RepositoryUtils::filter_required_packages_with_dev(
                &packages, &dev
            )),
            HashSet::from(["required/a".to_string()])
        );

        let mut transitive = RiffManifest::default();
        transitive
            .require
            .insert("required/b".to_string(), "dev-lala".to_string());
        assert_eq!(
            names(RepositoryUtils::filter_required_packages(
                &packages,
                &transitive
            )),
            HashSet::from(["required/b".to_string(), "required/c".to_string()])
        );

        let mut circular = RiffManifest::default();
        circular
            .require
            .insert("required/circular".to_string(), "*".to_string());
        assert_eq!(
            names(RepositoryUtils::filter_required_packages(
                &packages, &circular
            )),
            HashSet::from([
                "required/circular".to_string(),
                "required/circular-b".to_string()
            ])
        );
    }

    #[test]
    fn solver_candidates_do_not_auto_select_an_unrequested_provider() {
        let provider = Arc::new(Package::new("vendor/provider", "1.0.0"));

        assert!(
            RepositoryUtils::filter_solver_candidates("virtual/api", vec![provider], |_| false,)
                .is_empty()
        );
    }

    #[test]
    fn solver_candidates_keep_providers_loaded_through_another_name() {
        let provider = Arc::new(Package::new("vendor/provider", "1.0.0"));

        assert_eq!(
            RepositoryUtils::filter_solver_candidates("virtual/api", vec![provider], |name| name
                == "vendor/provider",)
            .len(),
            1
        );
    }

    #[test]
    fn solver_candidates_do_not_auto_select_a_provider_next_to_a_direct_package() {
        let direct = Arc::new(Package::new("virtual/api", "1.0.0"));
        let mut provider = Package::new("vendor/provider", "1.0.0");
        provider
            .provide
            .insert("virtual/api".to_string(), "1.0.0".to_string());

        let selected = RepositoryUtils::filter_solver_candidates(
            "virtual/api",
            vec![direct, Arc::new(provider)],
            |_| false,
        );
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "virtual/api");
    }

    #[test]
    fn solver_candidates_keep_a_replacer_next_to_a_direct_package() {
        let direct = Arc::new(Package::new("virtual/api", "1.0.0"));
        let mut replacer = Package::new("vendor/replacer", "1.0.0");
        replacer
            .replace
            .insert("virtual/api".to_string(), "1.0.0".to_string());

        assert_eq!(
            RepositoryUtils::filter_solver_candidates(
                "virtual/api",
                vec![direct, Arc::new(replacer)],
                |_| false,
            )
            .len(),
            2
        );
    }
}
