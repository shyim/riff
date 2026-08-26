//! Composer-compatible package metadata serialization.
//!
//! `Package` keeps both normalized and human-readable versions as separate
//! strongly typed fields. Composer's array package format calls those fields
//! `version_normalized` and `version`, respectively. Keeping this translation
//! in one place prevents repository, lock-file, and diagnostic code from each
//! growing a subtly different JSON adapter.

use serde_json::{Map, Value};

use super::Package;

/// Dump a package using Composer's array-package field names and ordering.
pub fn dump_package(package: &Package) -> serde_json::Result<Value> {
    let mut dumped = serde_json::to_value(package)?;
    let object = dumped
        .as_object_mut()
        .expect("serializing Package must produce a JSON object");

    object.remove("pretty_name");
    object.remove("stability");

    let normalized_version = object
        .remove("version")
        .unwrap_or_else(|| Value::String(package.version().to_owned()));
    let pretty_version = object
        .remove("pretty_version")
        .unwrap_or_else(|| normalized_version.clone());
    object.insert("version".to_owned(), pretty_version);
    object.insert("version_normalized".to_owned(), normalized_version);

    for key in [
        "require",
        "require-dev",
        "conflict",
        "provide",
        "replace",
        "suggest",
    ] {
        if let Some(Value::Object(values)) = object.get_mut(key) {
            sort_object(values);
        }
    }

    if let Some(Value::Array(keywords)) = object.get_mut("keywords") {
        keywords.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
    }

    Ok(dumped)
}

fn sort_object(object: &mut Map<String, Value>) {
    let mut entries: Vec<_> = std::mem::take(object).into_iter().collect();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    object.extend(entries);
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use indexmap::IndexMap;
    use serde_json::json;

    use super::*;
    use crate::package::{ArchiveConfig, Author, Funding, ScriptHandler, Support};

    #[test]
    fn composer_array_dumper_emits_required_information() {
        let mut package = Package::new("dummy/pkg", "1.0.0.0");
        package.pretty_version = Some("1.0.0".into());

        assert_eq!(
            dump_package(&package).unwrap(),
            json!({
                "name": "dummy/pkg",
                "version": "1.0.0",
                "version_normalized": "1.0.0.0",
                "type": "library",
            })
        );
    }

    #[test]
    fn composer_array_dumper_emits_supported_package_keys() {
        let mut package = Package::new("dummy/pkg", "1.0.0.0");
        package.pretty_version = Some("1.0.0".into());
        package.time = Some(Utc.with_ymd_and_hms(2012, 2, 1, 0, 0, 0).unwrap());
        package.authors = vec![Author {
            name: Some("Nils Adermann".into()),
            email: Some("naderman@naderman.de".into()),
            homepage: None,
            role: None,
        }];
        package.homepage = Some("https://getcomposer.org".to_owned());
        package.description = Some("Dependency Manager".to_owned());
        package.keywords = vec!["package".into(), "dependency".into(), "autoload".into()];
        package.bin = vec!["bin/composer".into()];
        package.license = vec!["MIT".into()];
        package.scripts.insert(
            "post-update-cmd".to_owned(),
            ScriptHandler::Single("MyVendor\\MyClass::postUpdate".to_owned()),
        );
        package.extra = Some(json!({"class": "MyVendor\\Installer"}));
        package.archive = Some(ArchiveConfig {
            name: None,
            exclude: vec!["/foo/bar".to_owned(), "baz".to_owned()],
        });
        package.require.insert("foo/bar".into(), "1.0.0".into());
        package.require.insert("bar/baz".into(), "1.0.0".into());
        package.require_dev.insert("foo/bar".into(), "1.0.0".into());
        package.suggest.insert("foo/bar".into(), "useful".into());
        package.provide.insert("virtual/pkg".into(), "1.0.0".into());
        package.replace.insert("old/pkg".into(), "1.0.0".into());
        package.conflict.insert("bad/pkg".into(), "1.0.0".into());
        package.support = Some(Support {
            issues: Some("https://example.org/issues".to_owned()),
            forum: None,
            wiki: None,
            source: None,
            email: None,
            irc: None,
            docs: None,
            rss: None,
            chat: None,
            security: None,
        });
        package.funding = vec![Funding {
            funding_type: Some("custom".into()),
            url: Some("https://example.org/fund".into()),
        }];
        package.transport_options = Some(json!({"ssl": {"local_cert": "/cert.pem"}}));

        let dumped = dump_package(&package).unwrap();

        assert_eq!(
            dumped["keywords"],
            json!(["autoload", "dependency", "package"])
        );
        assert_eq!(
            dumped["require"],
            json!({"bar/baz": "1.0.0", "foo/bar": "1.0.0"})
        );
        assert_eq!(
            dumped["scripts"]["post-update-cmd"],
            "MyVendor\\MyClass::postUpdate"
        );
        assert_eq!(dumped["archive"]["exclude"], json!(["/foo/bar", "baz"]));
        assert_eq!(dumped["support"]["issues"], "https://example.org/issues");
        assert_eq!(dumped["funding"][0]["type"], "custom");
        assert_eq!(
            dumped["transport-options"]["ssl"]["local_cert"],
            "/cert.pem"
        );
        assert_eq!(
            dumped["authors"][0],
            json!({"name": "Nils Adermann", "email": "naderman@naderman.de"})
        );
    }

    #[test]
    fn sort_object_keeps_empty_maps_valid() {
        let mut object = IndexMap::<String, Value>::new();
        object.insert("z".to_owned(), Value::Null);
        object.insert("a".to_owned(), Value::Null);
        let mut object: Map<String, Value> = object.into_iter().collect();

        sort_object(&mut object);

        assert_eq!(object.keys().collect::<Vec<_>>(), ["a", "z"]);
    }
}
