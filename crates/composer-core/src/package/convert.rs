//! Conversions between Package and LockedPackage types.

use indexmap::IndexMap;
use sonata_semver::VersionParser;

use super::{
    Author, Autoload, AutoloadPath, DependencyMap, Dist, Funding, Package, Source, Support,
};
use crate::json::{LockAuthor, LockAutoload, LockDist, LockFunding, LockSource, LockedPackage};

/// Sort dependencies alphabetically by key (like PHP's ksort)
fn sort_dependencies(deps: &DependencyMap) -> IndexMap<String, String> {
    let mut entries: Vec<_> = deps.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

impl From<&LockedPackage> for Package {
    fn from(lp: &LockedPackage) -> Self {
        let normalized_version = VersionParser::new()
            .normalize(&lp.version)
            .unwrap_or_else(|_| lp.version.clone());

        let mut pkg = Package::new(&lp.name, &normalized_version);
        pkg.pretty_version = Some(lp.version.clone().into());
        pkg.description = lp.description.clone();
        pkg.homepage = lp.homepage.clone();
        pkg.license = lp
            .license
            .iter()
            .map(|value| value.as_str().into())
            .collect();
        pkg.keywords = lp
            .keywords
            .iter()
            .map(|value| value.as_str().into())
            .collect();
        pkg.require = lp.require.clone().into();
        pkg.require_dev = lp.require_dev.clone().into();
        pkg.conflict = lp.conflict.clone().into();
        pkg.provide = lp.provide.clone().into();
        pkg.replace = lp.replace.clone().into();
        pkg.suggest = lp.suggest.clone().into();
        pkg.bin = lp.bin.iter().map(|value| value.as_str().into()).collect();
        pkg.package_type = lp.package_type.clone().into();
        pkg.extra = lp.extra.clone();
        pkg.notification_url = lp.notification_url.clone();
        pkg.installation_source = lp.installation_source.clone();
        pkg.default_branch = lp.default_branch;
        pkg.authors = lp.authors.iter().map(Author::from).collect();
        pkg.funding = lp.funding.iter().map(Funding::from).collect();

        if !lp.support.is_empty() {
            pkg.support = Some(Support::from(&lp.support));
        }

        pkg.abandoned = match &lp.abandoned {
            serde_json::Value::Bool(true) => Some(super::Abandoned::Yes),
            serde_json::Value::String(s) if !s.is_empty() => {
                Some(super::Abandoned::Replacement(s.clone()))
            }
            _ => None,
        };

        if let Some(ref src) = lp.source {
            pkg.source = Some(Source::new(&src.source_type, &src.url, &src.reference));
        }

        if let Some(ref dist) = lp.dist {
            let mut d = Dist::new(&dist.dist_type, &dist.url);
            if let Some(ref r) = dist.reference {
                d = d.with_reference(r);
            }
            if let Some(ref s) = dist.shasum {
                d = d.with_shasum(s);
            }
            if let Some(options) = lp
                .transport_options
                .as_ref()
                .and_then(serde_json::Value::as_object)
            {
                d = d.with_transport_options(
                    options
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                );
            }
            pkg.dist = Some(d);
        }

        pkg.transport_options = lp.transport_options.clone();

        if !lp.autoload.is_empty() {
            pkg.autoload = Some(Autoload::from(&lp.autoload));
        }

        if !lp.autoload_dev.is_empty() {
            pkg.autoload_dev = Some(Autoload::from(&lp.autoload_dev));
        }

        if let Some(ref time_str) = lp.time {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(time_str) {
                pkg.time = Some(dt.with_timezone(&chrono::Utc));
            }
        }

        pkg
    }
}

impl From<LockedPackage> for Package {
    fn from(lp: LockedPackage) -> Self {
        Package::from(&lp)
    }
}

impl From<&Package> for LockedPackage {
    fn from(pkg: &Package) -> Self {
        let abandoned = match &pkg.abandoned {
            Some(super::Abandoned::Yes) => serde_json::Value::Bool(true),
            Some(super::Abandoned::Replacement(s)) => serde_json::Value::String(s.clone()),
            None => serde_json::Value::Bool(false),
        };

        LockedPackage {
            name: pkg.name.clone(),
            version: pkg.pretty_version().to_string(),
            source: pkg.source.as_ref().map(LockSource::from),
            dist: pkg.dist.as_ref().map(LockDist::from),
            require: sort_dependencies(&pkg.require),
            require_dev: sort_dependencies(&pkg.require_dev),
            conflict: sort_dependencies(&pkg.conflict),
            provide: sort_dependencies(&pkg.provide),
            replace: sort_dependencies(&pkg.replace),
            suggest: sort_dependencies(&pkg.suggest),
            bin: pkg.bin.iter().map(ToString::to_string).collect(),
            package_type: pkg.package_type.to_string(),
            extra: pkg.extra.clone(),
            autoload: pkg
                .autoload
                .as_ref()
                .map(LockAutoload::from)
                .unwrap_or_default(),
            autoload_dev: pkg
                .autoload_dev
                .as_ref()
                .map(LockAutoload::from)
                .unwrap_or_default(),
            notification_url: pkg.notification_url.clone(),
            description: pkg.description.clone(),
            homepage: pkg.homepage.clone(),
            keywords: {
                let mut kw: Vec<_> = pkg.keywords.iter().map(ToString::to_string).collect();
                kw.sort();
                kw
            },
            license: pkg.license.iter().map(ToString::to_string).collect(),
            time: pkg.time.map(|t| t.to_rfc3339()),
            installation_source: pkg.installation_source.clone(),
            default_branch: pkg.default_branch,
            transport_options: pkg.transport_options.clone().or_else(|| {
                pkg.dist
                    .as_ref()
                    .and_then(|dist| dist.transport_options.as_ref())
                    .and_then(|options| serde_json::to_value(options).ok())
            }),
            authors: pkg.authors.iter().map(LockAuthor::from).collect(),
            support: pkg
                .support
                .as_ref()
                .map(support_to_hashmap)
                .unwrap_or_default(),
            funding: pkg.funding.iter().map(LockFunding::from).collect(),
            abandoned,
            ..Default::default()
        }
    }
}

impl From<Package> for LockedPackage {
    fn from(pkg: Package) -> Self {
        LockedPackage::from(&pkg)
    }
}

impl From<&Source> for LockSource {
    fn from(s: &Source) -> Self {
        LockSource {
            source_type: s.source_type.to_string(),
            url: s.url.clone(),
            reference: s.reference.clone(),
        }
    }
}

impl From<&Dist> for LockDist {
    fn from(d: &Dist) -> Self {
        LockDist {
            dist_type: d.dist_type.to_string(),
            url: d.url.clone(),
            reference: d.reference.clone(),
            shasum: d.shasum.clone(),
        }
    }
}

impl From<&LockAutoload> for Autoload {
    fn from(lock_autoload: &LockAutoload) -> Self {
        let mut autoload = Autoload::default();

        for (ns, v) in &lock_autoload.psr4 {
            autoload.psr4.insert(ns.clone(), json_value_to_paths(v));
        }

        for (ns, v) in &lock_autoload.psr0 {
            autoload.psr0.insert(ns.clone(), json_value_to_paths(v));
        }

        autoload.classmap = lock_autoload
            .classmap
            .iter()
            .map(|value| value.as_str().into())
            .collect();
        autoload.files = lock_autoload
            .files
            .iter()
            .map(|value| value.as_str().into())
            .collect();
        autoload.exclude_from_classmap = lock_autoload
            .exclude_from_classmap
            .iter()
            .map(|value| value.as_str().into())
            .collect();

        autoload
    }
}

impl From<&Autoload> for LockAutoload {
    fn from(a: &Autoload) -> Self {
        LockAutoload {
            psr4: a
                .psr4
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        serde_json::to_value(v).unwrap_or(serde_json::Value::Null),
                    )
                })
                .collect(),
            psr0: a
                .psr0
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        serde_json::to_value(v).unwrap_or(serde_json::Value::Null),
                    )
                })
                .collect(),
            classmap: a.classmap.iter().map(ToString::to_string).collect(),
            files: a.files.iter().map(ToString::to_string).collect(),
            exclude_from_classmap: a
                .exclude_from_classmap
                .iter()
                .map(ToString::to_string)
                .collect(),
        }
    }
}

fn json_value_to_paths(value: &serde_json::Value) -> AutoloadPath {
    match value {
        serde_json::Value::String(s) => AutoloadPath::Single(s.as_str().into()),
        serde_json::Value::Array(arr) => {
            let paths: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            AutoloadPath::Multiple(paths.into_iter().map(Into::into).collect())
        }
        _ => AutoloadPath::Single("".into()),
    }
}

impl From<&LockAuthor> for Author {
    fn from(la: &LockAuthor) -> Self {
        Author {
            name: Some(la.name.as_str().into()),
            email: la.email.as_deref().map(Into::into),
            homepage: la.homepage.as_deref().map(Into::into),
            role: la.role.as_deref().map(Into::into),
        }
    }
}

impl From<&Author> for LockAuthor {
    fn from(a: &Author) -> Self {
        LockAuthor {
            name: a.name.as_deref().unwrap_or_default().to_owned(),
            email: a.email.as_deref().map(ToOwned::to_owned),
            homepage: a.homepage.as_deref().map(ToOwned::to_owned),
            role: a.role.as_deref().map(ToOwned::to_owned),
        }
    }
}

impl From<&IndexMap<String, String>> for Support {
    fn from(map: &IndexMap<String, String>) -> Self {
        Support {
            issues: map.get("issues").cloned(),
            forum: map.get("forum").cloned(),
            wiki: map.get("wiki").cloned(),
            source: map.get("source").cloned(),
            email: map.get("email").cloned(),
            irc: map.get("irc").cloned(),
            docs: map.get("docs").cloned(),
            rss: map.get("rss").cloned(),
            chat: map.get("chat").cloned(),
            security: map.get("security").cloned(),
        }
    }
}

fn support_to_hashmap(s: &Support) -> IndexMap<String, String> {
    let mut map = IndexMap::new();
    // Maintain alphabetical order like Composer does
    if let Some(ref v) = s.chat {
        map.insert("chat".to_string(), v.clone());
    }
    if let Some(ref v) = s.docs {
        map.insert("docs".to_string(), v.clone());
    }
    if let Some(ref v) = s.email {
        map.insert("email".to_string(), v.clone());
    }
    if let Some(ref v) = s.forum {
        map.insert("forum".to_string(), v.clone());
    }
    if let Some(ref v) = s.irc {
        map.insert("irc".to_string(), v.clone());
    }
    if let Some(ref v) = s.issues {
        map.insert("issues".to_string(), v.clone());
    }
    if let Some(ref v) = s.rss {
        map.insert("rss".to_string(), v.clone());
    }
    if let Some(ref v) = s.security {
        map.insert("security".to_string(), v.clone());
    }
    if let Some(ref v) = s.source {
        map.insert("source".to_string(), v.clone());
    }
    if let Some(ref v) = s.wiki {
        map.insert("wiki".to_string(), v.clone());
    }
    map
}

impl From<&LockFunding> for Funding {
    fn from(lf: &LockFunding) -> Self {
        Funding {
            funding_type: Some(lf.funding_type.as_str().into()),
            url: Some(lf.url.as_str().into()),
        }
    }
}

impl From<&Funding> for LockFunding {
    fn from(f: &Funding) -> Self {
        LockFunding {
            url: f.url.as_deref().unwrap_or_default().to_owned(),
            funding_type: f.funding_type.as_deref().unwrap_or_default().to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_locked_package_to_package() {
        let locked = LockedPackage {
            name: "vendor/package".to_string(),
            version: "1.0.0".to_string(),
            package_type: "library".to_string(),
            description: Some("A test package".to_string()),
            ..Default::default()
        };

        let pkg = Package::from(&locked);
        assert_eq!(pkg.name, "vendor/package");
        assert_eq!(pkg.version, "1.0.0.0");
        assert_eq!(pkg.pretty_version.as_deref(), Some("1.0.0"));
        assert_eq!(pkg.description, Some("A test package".to_string()));
    }

    #[test]
    fn test_package_to_locked_package() {
        let mut pkg = Package::new("vendor/package", "1.0.0");
        pkg.description = Some("A test package".to_string());

        let locked = LockedPackage::from(&pkg);
        assert_eq!(locked.name, "vendor/package");
        assert_eq!(locked.version, "1.0.0");
        assert_eq!(locked.description, Some("A test package".to_string()));
    }

    #[test]
    fn test_roundtrip_conversion() {
        let mut original = Package::new("vendor/package", "1.2.3");
        original.description = Some("Test description".to_string());
        original.homepage = Some("https://example.com".to_string());
        original.license = vec!["MIT".into()];
        original
            .require
            .insert("php".to_string(), ">=8.0".to_string());
        original
            .require
            .insert("other/pkg".to_string(), "^1.0".to_string());

        let locked = LockedPackage::from(&original);
        let converted = Package::from(&locked);

        assert_eq!(converted.name, original.name);
        assert_eq!(converted.description, original.description);
        assert_eq!(converted.homepage, original.homepage);
        assert_eq!(converted.license, original.license);
        assert_eq!(converted.require, original.require);
    }

    #[test]
    fn test_notification_url_roundtrip() {
        let mut original = Package::new("vendor/package", "1.0.0");
        original.notification_url = Some("https://packagist.org/downloads/".to_string());

        let locked = LockedPackage::from(&original);
        assert_eq!(
            locked.notification_url,
            Some("https://packagist.org/downloads/".to_string())
        );

        let converted = Package::from(&locked);
        assert_eq!(converted.notification_url, original.notification_url);
    }

    #[test]
    fn test_authors_roundtrip() {
        let mut original = Package::new("vendor/package", "1.0.0");
        original.authors = vec![
            Author {
                name: Some("John Doe".into()),
                email: Some("john@example.com".into()),
                homepage: Some("https://johndoe.com".into()),
                role: Some("Developer".into()),
            },
            Author {
                name: Some("Jane Smith".into()),
                email: None,
                homepage: None,
                role: Some("Maintainer".into()),
            },
        ];

        let locked = LockedPackage::from(&original);
        assert_eq!(locked.authors.len(), 2);
        assert_eq!(locked.authors[0].name, "John Doe");
        assert_eq!(
            locked.authors[0].email,
            Some("john@example.com".to_string())
        );
        assert_eq!(
            locked.authors[0].homepage,
            Some("https://johndoe.com".to_string())
        );
        assert_eq!(locked.authors[0].role, Some("Developer".to_string()));
        assert_eq!(locked.authors[1].name, "Jane Smith");
        assert_eq!(locked.authors[1].role, Some("Maintainer".to_string()));

        let converted = Package::from(&locked);
        assert_eq!(converted.authors.len(), 2);
        assert_eq!(converted.authors[0].name.as_deref(), Some("John Doe"));
        assert_eq!(
            converted.authors[0].email.as_deref(),
            Some("john@example.com")
        );
        assert_eq!(converted.authors[1].name.as_deref(), Some("Jane Smith"));
    }

    #[test]
    fn test_support_roundtrip() {
        let mut original = Package::new("vendor/package", "1.0.0");
        original.support = Some(Support {
            issues: Some("https://github.com/vendor/package/issues".to_string()),
            source: Some("https://github.com/vendor/package".to_string()),
            docs: Some("https://docs.example.com".to_string()),
            email: Some("support@example.com".to_string()),
            forum: None,
            wiki: None,
            irc: None,
            rss: None,
            chat: Some("https://discord.gg/example".to_string()),
            security: Some("https://example.com/security".to_string()),
        });

        let locked = LockedPackage::from(&original);
        assert_eq!(
            locked.support.get("issues"),
            Some(&"https://github.com/vendor/package/issues".to_string())
        );
        assert_eq!(
            locked.support.get("source"),
            Some(&"https://github.com/vendor/package".to_string())
        );
        assert_eq!(
            locked.support.get("docs"),
            Some(&"https://docs.example.com".to_string())
        );
        assert_eq!(
            locked.support.get("email"),
            Some(&"support@example.com".to_string())
        );
        assert_eq!(
            locked.support.get("chat"),
            Some(&"https://discord.gg/example".to_string())
        );
        assert_eq!(
            locked.support.get("security"),
            Some(&"https://example.com/security".to_string())
        );
        assert_eq!(locked.support.get("forum"), None);

        let converted = Package::from(&locked);
        let support = converted.support.unwrap();
        assert_eq!(
            support.issues,
            Some("https://github.com/vendor/package/issues".to_string())
        );
        assert_eq!(
            support.source,
            Some("https://github.com/vendor/package".to_string())
        );
        assert_eq!(support.docs, Some("https://docs.example.com".to_string()));
        assert_eq!(support.chat, Some("https://discord.gg/example".to_string()));
        assert_eq!(support.forum, None);
    }

    #[test]
    fn test_funding_roundtrip() {
        let mut original = Package::new("vendor/package", "1.0.0");
        original.funding = vec![
            Funding {
                funding_type: Some("github".into()),
                url: Some("https://github.com/sponsors/johndoe".into()),
            },
            Funding {
                funding_type: Some("patreon".into()),
                url: Some("https://patreon.com/johndoe".into()),
            },
            Funding {
                funding_type: Some("opencollective".into()),
                url: Some("https://opencollective.com/package".into()),
            },
        ];

        let locked = LockedPackage::from(&original);
        assert_eq!(locked.funding.len(), 3);
        assert_eq!(locked.funding[0].funding_type, "github");
        assert_eq!(locked.funding[0].url, "https://github.com/sponsors/johndoe");
        assert_eq!(locked.funding[1].funding_type, "patreon");
        assert_eq!(locked.funding[1].url, "https://patreon.com/johndoe");
        assert_eq!(locked.funding[2].funding_type, "opencollective");

        let converted = Package::from(&locked);
        assert_eq!(converted.funding.len(), 3);
        assert_eq!(converted.funding[0].funding_type.as_deref(), Some("github"));
        assert_eq!(
            converted.funding[0].url.as_deref(),
            Some("https://github.com/sponsors/johndoe")
        );
        assert_eq!(
            converted.funding[1].funding_type.as_deref(),
            Some("patreon")
        );
    }

    #[test]
    fn test_complete_metadata_roundtrip() {
        let mut original = Package::new("vendor/package", "1.0.0");
        original.notification_url = Some("https://packagist.org/downloads/".to_string());
        original.authors = vec![Author {
            name: Some("Test Author".into()),
            email: Some("test@example.com".into()),
            homepage: None,
            role: None,
        }];
        original.support = Some(Support {
            issues: Some("https://github.com/vendor/package/issues".to_string()),
            source: Some("https://github.com/vendor/package".to_string()),
            docs: None,
            email: None,
            forum: None,
            wiki: None,
            irc: None,
            rss: None,
            chat: None,
            security: None,
        });
        original.funding = vec![Funding {
            funding_type: Some("github".into()),
            url: Some("https://github.com/sponsors/test".into()),
        }];

        let locked = LockedPackage::from(&original);
        assert!(locked.notification_url.is_some());
        assert!(!locked.authors.is_empty());
        assert!(!locked.support.is_empty());
        assert!(!locked.funding.is_empty());

        let converted = Package::from(&locked);
        assert_eq!(converted.notification_url, original.notification_url);
        assert_eq!(converted.authors.len(), original.authors.len());
        assert!(converted.support.is_some());
        assert_eq!(converted.funding.len(), original.funding.len());
    }

    #[test]
    fn test_abandoned_bool_true_to_package() {
        let locked = LockedPackage {
            name: "vendor/abandoned".to_string(),
            version: "1.0.0".to_string(),
            package_type: "library".to_string(),
            abandoned: serde_json::Value::Bool(true),
            ..Default::default()
        };

        let pkg = Package::from(&locked);
        assert!(pkg.is_abandoned());
        assert_eq!(pkg.abandoned, Some(super::super::Abandoned::Yes));
    }

    #[test]
    fn test_abandoned_string_to_package() {
        let locked = LockedPackage {
            name: "vendor/abandoned".to_string(),
            version: "1.0.0".to_string(),
            package_type: "library".to_string(),
            abandoned: serde_json::Value::String("vendor/replacement".to_string()),
            ..Default::default()
        };

        let pkg = Package::from(&locked);
        assert!(pkg.is_abandoned());
        assert_eq!(
            pkg.abandoned,
            Some(super::super::Abandoned::Replacement(
                "vendor/replacement".to_string()
            ))
        );
    }

    #[test]
    fn test_abandoned_false_to_package() {
        let locked = LockedPackage {
            name: "vendor/normal".to_string(),
            version: "1.0.0".to_string(),
            package_type: "library".to_string(),
            abandoned: serde_json::Value::Bool(false),
            ..Default::default()
        };

        let pkg = Package::from(&locked);
        assert!(!pkg.is_abandoned());
        assert_eq!(pkg.abandoned, None);
    }

    #[test]
    fn test_package_abandoned_yes_to_locked() {
        let mut pkg = Package::new("vendor/abandoned", "1.0.0");
        pkg.abandoned = Some(super::super::Abandoned::Yes);

        let locked = LockedPackage::from(&pkg);
        assert_eq!(locked.abandoned, serde_json::Value::Bool(true));
    }

    #[test]
    fn test_package_abandoned_replacement_to_locked() {
        let mut pkg = Package::new("vendor/abandoned", "1.0.0");
        pkg.abandoned = Some(super::super::Abandoned::Replacement(
            "vendor/new".to_string(),
        ));

        let locked = LockedPackage::from(&pkg);
        assert_eq!(
            locked.abandoned,
            serde_json::Value::String("vendor/new".to_string())
        );
    }

    #[test]
    fn test_package_not_abandoned_to_locked() {
        let pkg = Package::new("vendor/normal", "1.0.0");

        let locked = LockedPackage::from(&pkg);
        assert_eq!(locked.abandoned, serde_json::Value::Bool(false));
    }

    #[test]
    fn test_abandoned_roundtrip_with_replacement() {
        let mut original = Package::new("vendor/old", "1.0.0");
        original.abandoned = Some(super::super::Abandoned::Replacement(
            "vendor/new".to_string(),
        ));

        let locked = LockedPackage::from(&original);
        let converted = Package::from(&locked);

        assert_eq!(converted.abandoned, original.abandoned);
    }

    #[test]
    fn test_abandoned_roundtrip_without_replacement() {
        let mut original = Package::new("vendor/old", "1.0.0");
        original.abandoned = Some(super::super::Abandoned::Yes);

        let locked = LockedPackage::from(&original);
        let converted = Package::from(&locked);

        assert_eq!(converted.abandoned, original.abandoned);
    }
}
