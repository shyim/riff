use chrono::{DateTime, Utc};
use compact_str::CompactString;
use indexmap::IndexMap;
use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::package::{
    Abandoned, ArchiveConfig, Author, Autoload, AutoloadPath, DependencyMap, Dist, Funding, Mirror,
    Package, Scripts, Source, Stability, Support,
};

/// Solver-hot package fields archived directly in the filtered repository cache.
///
/// Cold install metadata stays in a separate fixed-layout MessagePack payload
/// so the solver can retain it as an opaque byte range until hydration.
#[derive(Debug, Archive, RkyvSerialize, RkyvDeserialize)]
pub(super) struct CachedPackage {
    name: String,
    version: String,
    pretty_version: Option<String>,
    stability: u8,
    require: Vec<(String, String)>,
    conflict: Vec<(String, String)>,
    provide: Vec<(String, String)>,
    replace: Vec<(String, String)>,
    metadata: Vec<u8>,
}

/// Fixed-layout representation for the deferred MessagePack payload.
///
/// `Package` uses conditional fields for Composer JSON compatibility, which
/// makes tuple serialization unsafe: omitted fields shift every later value.
/// This mirror always serializes every field in a stable order.
#[derive(Debug, Serialize, Deserialize)]
struct CachedPackageMetadata {
    pretty_name: Option<String>,
    package_type: CompactString,
    source: Option<CachedSource>,
    dist: Option<CachedDist>,
    require_dev: DependencyMap,
    suggest: DependencyMap,
    autoload: Option<CachedAutoload>,
    autoload_dev: Option<CachedAutoload>,
    include_path: Vec<String>,
    target_dir: Option<String>,
    bin: Vec<CompactString>,
    extra: Option<Value>,
    notification_url: Option<String>,
    installation_source: Option<String>,
    time: Option<CachedDateTime>,
    description: Option<String>,
    homepage: Option<String>,
    license: Vec<CompactString>,
    keywords: Vec<CompactString>,
    authors: Vec<CachedAuthor>,
    support: Option<CachedSupport>,
    funding: Vec<CachedFunding>,
    scripts: Scripts,
    abandoned: Option<Abandoned>,
    archive: Option<CachedArchiveConfig>,
    default_branch: Option<bool>,
    transport_options: Option<Value>,
}

/// Minimal cold-metadata projection needed to plan a dry-run transaction.
///
/// The cache uses tuple-style struct encoding, so every field remains present
/// and in the same order. `IgnoredAny` consumes cold values without allocating
/// them while package type and source/dist references retain transaction and
/// plugin ordering semantics.
#[allow(dead_code)]
#[derive(Deserialize)]
struct CachedTransactionMetadata {
    pretty_name: IgnoredAny,
    package_type: CompactString,
    source: Option<CachedSource>,
    dist: Option<CachedDist>,
    require_dev: IgnoredAny,
    suggest: IgnoredAny,
    autoload: IgnoredAny,
    autoload_dev: IgnoredAny,
    include_path: IgnoredAny,
    target_dir: IgnoredAny,
    bin: IgnoredAny,
    extra: IgnoredAny,
    notification_url: IgnoredAny,
    installation_source: IgnoredAny,
    time: IgnoredAny,
    description: IgnoredAny,
    homepage: IgnoredAny,
    license: IgnoredAny,
    keywords: IgnoredAny,
    authors: IgnoredAny,
    support: IgnoredAny,
    funding: IgnoredAny,
    scripts: IgnoredAny,
    abandoned: IgnoredAny,
    archive: IgnoredAny,
    default_branch: IgnoredAny,
    transport_options: IgnoredAny,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedSource {
    source_type: CompactString,
    url: String,
    reference: String,
    mirrors: Option<Vec<Mirror>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedDist {
    dist_type: CompactString,
    url: String,
    reference: Option<String>,
    shasum: Option<String>,
    sha256: Option<String>,
    mirrors: Option<Vec<Mirror>>,
    transport_options: Option<HashMap<String, Value>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedAutoload {
    psr4: IndexMap<String, AutoloadPath>,
    psr0: IndexMap<String, AutoloadPath>,
    classmap: Vec<CompactString>,
    files: Vec<CompactString>,
    exclude_from_classmap: Vec<CompactString>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedDateTime {
    seconds: i64,
    nanoseconds: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedAuthor {
    name: Option<CompactString>,
    email: Option<CompactString>,
    homepage: Option<CompactString>,
    role: Option<CompactString>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedSupport {
    issues: Option<String>,
    forum: Option<String>,
    wiki: Option<String>,
    source: Option<String>,
    email: Option<String>,
    irc: Option<String>,
    docs: Option<String>,
    rss: Option<String>,
    chat: Option<String>,
    security: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedFunding {
    funding_type: Option<CompactString>,
    url: Option<CompactString>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedArchiveConfig {
    name: Option<String>,
    exclude: Vec<String>,
}

impl CachedPackage {
    pub(super) fn from_package(package: &Package) -> Option<Self> {
        let metadata = rmp_serde::to_vec(&CachedPackageMetadata::from(package)).ok()?;
        Some(Self {
            name: package.name.clone(),
            version: package.version.to_string(),
            pretty_version: package.pretty_version.as_ref().map(ToString::to_string),
            stability: encode_stability(package.stability),
            require: cache_dependencies(&package.require),
            conflict: cache_dependencies(&package.conflict),
            provide: cache_dependencies(&package.provide),
            replace: cache_dependencies(&package.replace),
            metadata,
        })
    }

    pub(super) fn hydrate(mut package: Package, metadata: &[u8]) -> Option<Package> {
        let metadata: CachedPackageMetadata = rmp_serde::from_slice(metadata).ok()?;
        metadata.apply_to(&mut package)?;
        Some(package)
    }

    pub(super) fn hydrate_for_transaction(
        mut package: Package,
        metadata: &[u8],
    ) -> Option<Package> {
        let metadata: CachedTransactionMetadata = rmp_serde::from_slice(metadata).ok()?;
        package.package_type = metadata.package_type;
        package.source = metadata.source.map(Source::from);
        package.dist = metadata.dist.map(Dist::from);
        Some(package)
    }
}

impl ArchivedCachedPackage {
    pub(super) fn to_solver_package(&self) -> Option<(Package, &[u8])> {
        let package = Package {
            name: self.name.as_str().to_owned(),
            pretty_name: None,
            version: self.version.as_str().into(),
            pretty_version: self
                .pretty_version
                .as_ref()
                .map(|version| version.as_str().into()),
            package_type: "library".into(),
            stability: decode_stability(self.stability)?,
            source: None,
            dist: None,
            require: archived_dependencies(&self.require),
            require_dev: DependencyMap::default(),
            conflict: archived_dependencies(&self.conflict),
            provide: archived_dependencies(&self.provide),
            replace: archived_dependencies(&self.replace),
            suggest: DependencyMap::default(),
            autoload: None,
            autoload_dev: None,
            include_path: Vec::new(),
            target_dir: None,
            bin: Vec::new(),
            extra: None,
            notification_url: None,
            installation_source: None,
            time: None,
            description: None,
            homepage: None,
            license: Vec::new(),
            keywords: Vec::new(),
            authors: Vec::new(),
            support: None,
            funding: Vec::new(),
            scripts: Scripts::default(),
            abandoned: None,
            archive: None,
            default_branch: None,
            transport_options: None,
        };
        Some((package, self.metadata.as_slice()))
    }
}

fn cache_dependencies(dependencies: &DependencyMap) -> Vec<(String, String)> {
    dependencies
        .iter()
        .map(|(name, constraint)| (name.to_string(), constraint.to_string()))
        .collect()
}

fn archived_dependencies(
    dependencies: &rkyv::vec::ArchivedVec<
        rkyv::tuple::ArchivedTuple2<rkyv::string::ArchivedString, rkyv::string::ArchivedString>,
    >,
) -> DependencyMap {
    DependencyMap::from_ordered_iter(
        dependencies
            .iter()
            .map(|dependency| (dependency.0.as_str(), dependency.1.as_str())),
    )
}

fn encode_stability(stability: Option<Stability>) -> u8 {
    match stability {
        None => 0,
        Some(Stability::Dev) => 1,
        Some(Stability::Alpha) => 2,
        Some(Stability::Beta) => 3,
        Some(Stability::RC) => 4,
        Some(Stability::Stable) => 5,
    }
}

fn decode_stability(stability: u8) -> Option<Option<Stability>> {
    match stability {
        0 => Some(None),
        1 => Some(Some(Stability::Dev)),
        2 => Some(Some(Stability::Alpha)),
        3 => Some(Some(Stability::Beta)),
        4 => Some(Some(Stability::RC)),
        5 => Some(Some(Stability::Stable)),
        _ => None,
    }
}

impl From<&Package> for CachedPackageMetadata {
    fn from(package: &Package) -> Self {
        Self {
            pretty_name: package.pretty_name.clone(),
            package_type: package.package_type.clone(),
            source: package.source.as_ref().map(CachedSource::from),
            dist: package.dist.as_ref().map(CachedDist::from),
            require_dev: package.require_dev.clone(),
            suggest: package.suggest.clone(),
            autoload: package.autoload.as_ref().map(CachedAutoload::from),
            autoload_dev: package.autoload_dev.as_ref().map(CachedAutoload::from),
            include_path: package.include_path.clone(),
            target_dir: package.target_dir.clone(),
            bin: package.bin.clone(),
            extra: package.extra.clone(),
            notification_url: package.notification_url.clone(),
            installation_source: package.installation_source.clone(),
            time: package.time.as_ref().map(CachedDateTime::from),
            description: package.description.clone(),
            homepage: package.homepage.clone(),
            license: package.license.clone(),
            keywords: package.keywords.clone(),
            authors: package.authors.iter().map(CachedAuthor::from).collect(),
            support: package.support.as_ref().map(CachedSupport::from),
            funding: package.funding.iter().map(CachedFunding::from).collect(),
            scripts: package.scripts.clone(),
            abandoned: package.abandoned.clone(),
            archive: package.archive.as_ref().map(CachedArchiveConfig::from),
            default_branch: package.default_branch,
            transport_options: package.transport_options.clone(),
        }
    }
}

impl CachedPackageMetadata {
    fn apply_to(self, package: &mut Package) -> Option<()> {
        package.pretty_name = self.pretty_name;
        package.package_type = self.package_type;
        package.source = self.source.map(Source::from);
        package.dist = self.dist.map(Dist::from);
        package.require_dev = self.require_dev;
        package.suggest = self.suggest;
        package.autoload = self.autoload.map(Autoload::from);
        package.autoload_dev = self.autoload_dev.map(Autoload::from);
        package.include_path = self.include_path;
        package.target_dir = self.target_dir;
        package.bin = self.bin;
        package.extra = self.extra;
        package.notification_url = self.notification_url;
        package.installation_source = self.installation_source;
        package.time = match self.time {
            Some(time) => Some(DateTime::<Utc>::from_timestamp(
                time.seconds,
                time.nanoseconds,
            )?),
            None => None,
        };
        package.description = self.description;
        package.homepage = self.homepage;
        package.license = self.license;
        package.keywords = self.keywords;
        package.authors = self.authors.into_iter().map(Author::from).collect();
        package.support = self.support.map(Support::from);
        package.funding = self.funding.into_iter().map(Funding::from).collect();
        package.scripts = self.scripts;
        package.abandoned = self.abandoned;
        package.archive = self.archive.map(ArchiveConfig::from);
        package.default_branch = self.default_branch;
        package.transport_options = self.transport_options;
        Some(())
    }
}

impl From<&Source> for CachedSource {
    fn from(source: &Source) -> Self {
        Self {
            source_type: source.source_type.clone(),
            url: source.url.clone(),
            reference: source.reference.clone(),
            mirrors: source.mirrors.clone(),
        }
    }
}

impl From<CachedSource> for Source {
    fn from(source: CachedSource) -> Self {
        Self {
            source_type: source.source_type,
            url: source.url,
            reference: source.reference,
            mirrors: source.mirrors,
        }
    }
}

impl From<&Dist> for CachedDist {
    fn from(dist: &Dist) -> Self {
        Self {
            dist_type: dist.dist_type.clone(),
            url: dist.url.clone(),
            reference: dist.reference.clone(),
            shasum: dist.shasum.clone(),
            sha256: dist.sha256.clone(),
            mirrors: dist.mirrors.clone(),
            transport_options: dist.transport_options.clone(),
        }
    }
}

impl From<CachedDist> for Dist {
    fn from(dist: CachedDist) -> Self {
        Self {
            dist_type: dist.dist_type,
            url: dist.url,
            reference: dist.reference,
            shasum: dist.shasum,
            sha256: dist.sha256,
            mirrors: dist.mirrors,
            transport_options: dist.transport_options,
        }
    }
}

impl From<&Autoload> for CachedAutoload {
    fn from(autoload: &Autoload) -> Self {
        Self {
            psr4: autoload.psr4.clone(),
            psr0: autoload.psr0.clone(),
            classmap: autoload.classmap.clone(),
            files: autoload.files.clone(),
            exclude_from_classmap: autoload.exclude_from_classmap.clone(),
        }
    }
}

impl From<CachedAutoload> for Autoload {
    fn from(autoload: CachedAutoload) -> Self {
        Self {
            psr4: autoload.psr4,
            psr0: autoload.psr0,
            classmap: autoload.classmap,
            files: autoload.files,
            exclude_from_classmap: autoload.exclude_from_classmap,
        }
    }
}

impl From<&DateTime<Utc>> for CachedDateTime {
    fn from(time: &DateTime<Utc>) -> Self {
        Self {
            seconds: time.timestamp(),
            nanoseconds: time.timestamp_subsec_nanos(),
        }
    }
}

impl From<&Author> for CachedAuthor {
    fn from(author: &Author) -> Self {
        Self {
            name: author.name.clone(),
            email: author.email.clone(),
            homepage: author.homepage.clone(),
            role: author.role.clone(),
        }
    }
}

impl From<CachedAuthor> for Author {
    fn from(author: CachedAuthor) -> Self {
        Self {
            name: author.name,
            email: author.email,
            homepage: author.homepage,
            role: author.role,
        }
    }
}

impl From<&Support> for CachedSupport {
    fn from(support: &Support) -> Self {
        Self {
            issues: support.issues.clone(),
            forum: support.forum.clone(),
            wiki: support.wiki.clone(),
            source: support.source.clone(),
            email: support.email.clone(),
            irc: support.irc.clone(),
            docs: support.docs.clone(),
            rss: support.rss.clone(),
            chat: support.chat.clone(),
            security: support.security.clone(),
        }
    }
}

impl From<CachedSupport> for Support {
    fn from(support: CachedSupport) -> Self {
        Self {
            issues: support.issues,
            forum: support.forum,
            wiki: support.wiki,
            source: support.source,
            email: support.email,
            irc: support.irc,
            docs: support.docs,
            rss: support.rss,
            chat: support.chat,
            security: support.security,
        }
    }
}

impl From<&Funding> for CachedFunding {
    fn from(funding: &Funding) -> Self {
        Self {
            funding_type: funding.funding_type.clone(),
            url: funding.url.clone(),
        }
    }
}

impl From<CachedFunding> for Funding {
    fn from(funding: CachedFunding) -> Self {
        Self {
            funding_type: funding.funding_type,
            url: funding.url,
        }
    }
}

impl From<&ArchiveConfig> for CachedArchiveConfig {
    fn from(archive: &ArchiveConfig) -> Self {
        Self {
            name: archive.name.clone(),
            exclude: archive.exclude.clone(),
        }
    }
}

impl From<CachedArchiveConfig> for ArchiveConfig {
    fn from(archive: CachedArchiveConfig) -> Self {
        Self {
            name: archive.name,
            exclude: archive.exclude,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Mirror, ScriptHandler};
    use serde_json::json;

    fn round_trip(package: &Package) {
        let encoded =
            rkyv::to_bytes::<rkyv::rancor::Error>(&CachedPackage::from_package(package).unwrap())
                .unwrap();
        let cached = rkyv::access::<ArchivedCachedPackage, rkyv::rancor::Error>(&encoded).unwrap();
        let (solver_package, metadata) = cached.to_solver_package().unwrap();
        assert_eq!(
            CachedPackage::hydrate(solver_package, metadata).unwrap(),
            *package
        );
    }

    #[test]
    fn borrowed_cache_metadata_points_into_encoded_buffer() {
        let package = Package::new("vendor/package", "1.2.3");
        let encoded =
            rkyv::to_bytes::<rkyv::rancor::Error>(&CachedPackage::from_package(&package).unwrap())
                .unwrap();
        let cached = rkyv::access::<ArchivedCachedPackage, rkyv::rancor::Error>(&encoded).unwrap();
        let buffer = encoded.as_ptr_range();
        let metadata = cached.metadata.as_slice().as_ptr_range();

        assert!(metadata.start >= buffer.start);
        assert!(metadata.end <= buffer.end);
        let (solver_package, metadata) = cached.to_solver_package().unwrap();
        assert_eq!(
            CachedPackage::hydrate(solver_package, metadata).unwrap(),
            package
        );
    }

    #[test]
    fn transaction_projection_keeps_only_planning_metadata() {
        let mut package = Package::new("vendor/package", "1.2.3");
        package.package_type = "composer-plugin".into();
        package.source = Some(Source::git("https://example.test/source.git", "source-ref"));
        package.dist =
            Some(Dist::zip("https://example.test/package.zip").with_reference("dist-ref"));
        package
            .require
            .insert("dependency/package".into(), "^1".into());
        package
            .require_dev
            .insert("dev/package".into(), "^2".into());
        package.description = Some("cold description".into());
        package.authors.push(Author {
            name: Some("Developer".into()),
            email: None,
            homepage: None,
            role: None,
        });

        let encoded =
            rkyv::to_bytes::<rkyv::rancor::Error>(&CachedPackage::from_package(&package).unwrap())
                .unwrap();
        let cached = rkyv::access::<ArchivedCachedPackage, rkyv::rancor::Error>(&encoded).unwrap();
        let (solver_package, metadata) = cached.to_solver_package().unwrap();
        let projected = CachedPackage::hydrate_for_transaction(solver_package, metadata).unwrap();

        assert_eq!(projected.name, package.name);
        assert_eq!(projected.version, package.version);
        assert_eq!(projected.require, package.require);
        assert_eq!(projected.package_type, package.package_type);
        assert_eq!(projected.source, package.source);
        assert_eq!(projected.dist, package.dist);
        assert!(projected.require_dev.is_empty());
        assert!(projected.description.is_none());
        assert!(projected.authors.is_empty());
    }

    #[test]
    fn sparse_package_round_trips_through_archived_cache() {
        round_trip(&Package::new("vendor/package", "1.2.3"));
    }

    #[test]
    fn dependency_map_preserves_cold_metadata_wire_format() {
        let standard = IndexMap::from([
            ("php".to_string(), "^8.2".to_string()),
            ("ext-json".to_string(), "*".to_string()),
        ]);
        let fast = DependencyMap::from(standard.clone());

        let previous_bytes = rmp_serde::to_vec(&standard).unwrap();
        assert_eq!(rmp_serde::to_vec(&fast).unwrap(), previous_bytes);

        let decoded: DependencyMap = rmp_serde::from_slice(&previous_bytes).unwrap();
        assert!(decoded
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .eq(standard
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))));
        assert!(decoded
            .iter()
            .all(|(key, value)| !key.is_heap_allocated() && !value.is_heap_allocated()));
    }

    #[test]
    fn compact_metadata_preserves_cold_metadata_wire_format() {
        #[derive(Serialize)]
        struct PreviousAuthor {
            name: Option<String>,
            email: Option<String>,
            homepage: Option<String>,
            role: Option<String>,
        }

        #[derive(Serialize)]
        struct PreviousFunding {
            funding_type: Option<String>,
            url: Option<String>,
        }

        let author_bytes = rmp_serde::to_vec(&PreviousAuthor {
            name: Some("Jane Doe".into()),
            email: Some("jane@example.test".into()),
            homepage: None,
            role: Some("Maintainer".into()),
        })
        .unwrap();
        let author: CachedAuthor = rmp_serde::from_slice(&author_bytes).unwrap();
        assert_eq!(rmp_serde::to_vec(&author).unwrap(), author_bytes);
        assert!(author
            .name
            .as_ref()
            .is_some_and(|value| !value.is_heap_allocated()));
        assert!(author
            .email
            .as_ref()
            .is_some_and(|value| !value.is_heap_allocated()));

        let funding_bytes = rmp_serde::to_vec(&PreviousFunding {
            funding_type: Some("github".into()),
            url: Some("https://a.test".into()),
        })
        .unwrap();
        let funding: CachedFunding = rmp_serde::from_slice(&funding_bytes).unwrap();
        assert_eq!(rmp_serde::to_vec(&funding).unwrap(), funding_bytes);
        assert!(funding
            .funding_type
            .as_ref()
            .is_some_and(|value| !value.is_heap_allocated()));
        assert!(funding
            .url
            .as_ref()
            .is_some_and(|value| !value.is_heap_allocated()));
    }

    #[test]
    fn planning_metadata_preserves_cold_metadata_wire_format() {
        #[derive(Serialize)]
        struct PreviousSource {
            source_type: String,
            url: String,
            reference: String,
            mirrors: Option<Vec<Mirror>>,
        }

        #[derive(Serialize)]
        struct PreviousDist {
            dist_type: String,
            url: String,
            reference: Option<String>,
            shasum: Option<String>,
            sha256: Option<String>,
            mirrors: Option<Vec<Mirror>>,
            transport_options: Option<HashMap<String, Value>>,
        }

        let previous_source = PreviousSource {
            source_type: "git".into(),
            url: "https://example.test/package.git".into(),
            reference: "0123456789012345678901234567890123456789".into(),
            mirrors: None,
        };
        let previous_source_bytes = rmp_serde::to_vec(&previous_source).unwrap();
        let source: CachedSource = rmp_serde::from_slice(&previous_source_bytes).unwrap();
        assert_eq!(rmp_serde::to_vec(&source).unwrap(), previous_source_bytes);
        assert!(!source.source_type.is_heap_allocated());

        let previous_dist = PreviousDist {
            dist_type: "zip".into(),
            url: "https://example.test/package.zip".into(),
            reference: Some("0123456789012345678901234567890123456789".into()),
            shasum: None,
            sha256: None,
            mirrors: None,
            transport_options: None,
        };
        let previous_dist_bytes = rmp_serde::to_vec(&previous_dist).unwrap();
        let dist: CachedDist = rmp_serde::from_slice(&previous_dist_bytes).unwrap();
        assert_eq!(rmp_serde::to_vec(&dist).unwrap(), previous_dist_bytes);
        assert!(!dist.dist_type.is_heap_allocated());
    }

    #[test]
    fn compact_lists_preserve_cold_metadata_wire_format() {
        #[derive(Serialize)]
        struct PreviousAutoload {
            psr4: IndexMap<String, Value>,
            psr0: IndexMap<String, Value>,
            classmap: Vec<String>,
            files: Vec<String>,
            exclude_from_classmap: Vec<String>,
        }

        let previous_lists = (
            vec!["bin/tool".to_string()],
            vec!["MIT".to_string()],
            vec!["package".to_string(), "composer".to_string()],
        );
        let previous_list_bytes = rmp_serde::to_vec(&previous_lists).unwrap();
        let compact_lists: (Vec<CompactString>, Vec<CompactString>, Vec<CompactString>) =
            rmp_serde::from_slice(&previous_list_bytes).unwrap();
        assert_eq!(
            rmp_serde::to_vec(&compact_lists).unwrap(),
            previous_list_bytes
        );
        assert!(compact_lists
            .0
            .iter()
            .chain(&compact_lists.1)
            .chain(&compact_lists.2)
            .all(|value| !value.is_heap_allocated()));

        let previous_autoload = PreviousAutoload {
            psr4: IndexMap::from([("Vendor\\Package\\".into(), json!("src"))]),
            psr0: IndexMap::from([("Legacy_".into(), json!(["lib", "src/legacy"]))]),
            classmap: vec!["classes".into()],
            files: vec!["bootstrap.php".into()],
            exclude_from_classmap: vec!["tests".into()],
        };
        let previous_autoload_bytes = rmp_serde::to_vec(&previous_autoload).unwrap();
        let autoload: CachedAutoload = rmp_serde::from_slice(&previous_autoload_bytes).unwrap();
        assert_eq!(
            rmp_serde::to_vec(&autoload).unwrap(),
            previous_autoload_bytes
        );
        assert!(autoload
            .classmap
            .iter()
            .chain(&autoload.files)
            .chain(&autoload.exclude_from_classmap)
            .all(|value| !value.is_heap_allocated()));
        assert!(autoload
            .psr4
            .values()
            .chain(autoload.psr0.values())
            .flat_map(AutoloadPath::iter)
            .all(|value| !value.is_heap_allocated()));
    }

    #[test]
    fn populated_package_round_trips_through_archived_cache() {
        let mut package = Package::new("vendor/package", "1.2.3");
        package.source = Some(Source {
            source_type: "git".into(),
            url: "https://example.test/source.git".into(),
            reference: "source-ref".into(),
            mirrors: Some(vec![Mirror::preferred("https://mirror.test/source.git")]),
        });
        package.dist = Some(Dist {
            dist_type: "zip".into(),
            url: "https://example.test/package.zip".into(),
            reference: Some("dist-ref".into()),
            shasum: Some("sha1".into()),
            sha256: Some("sha256".into()),
            mirrors: Some(vec![Mirror::fallback("https://mirror.test/package.zip")]),
            transport_options: Some(HashMap::from([("timeout".into(), json!(30))])),
        });
        package.require.insert("php".into(), "^8.2".into());
        package
            .require_dev
            .insert("phpunit/phpunit".into(), "^11".into());
        package
            .conflict
            .insert("legacy/package".into(), "<2".into());
        package
            .provide
            .insert("virtual/package".into(), "1.0".into());
        package
            .replace
            .insert("old/package".into(), "self.version".into());
        package
            .suggest
            .insert("ext-test".into(), "For tests".into());
        package.autoload = Some(Autoload {
            psr4: IndexMap::from([("Vendor\\Package\\".into(), "src".into())]),
            psr0: IndexMap::from([("Legacy_".into(), "lib".into())]),
            classmap: vec!["classes".into()],
            files: vec!["bootstrap.php".into()],
            exclude_from_classmap: vec!["tests".into()],
        });
        package.autoload_dev = Some(Autoload::default());
        package.include_path = vec!["include".into()];
        package.target_dir = Some("target".into());
        package.bin = vec!["bin/tool".into()];
        package.extra = Some(json!({"branch-alias": {"dev-main": "1.x-dev"}}));
        package.notification_url = Some("https://example.test/notify".into());
        package.installation_source = Some("dist".into());
        package.time = DateTime::from_timestamp(1_700_000_000, 123_456_789);
        package.description = Some("Description".into());
        package.homepage = Some("https://example.test".into());
        package.license = vec!["MIT".into()];
        package.keywords = vec!["package".into()];
        package.authors = vec![Author {
            name: Some("Developer".into()),
            email: Some("dev@example.test".into()),
            homepage: Some("https://example.test/dev".into()),
            role: Some("Maintainer".into()),
        }];
        package.support = Some(Support {
            issues: Some("https://example.test/issues".into()),
            forum: Some("https://example.test/forum".into()),
            wiki: Some("https://example.test/wiki".into()),
            source: Some("https://example.test/source".into()),
            email: Some("support@example.test".into()),
            irc: Some("#package".into()),
            docs: Some("https://example.test/docs".into()),
            rss: Some("https://example.test/rss".into()),
            chat: Some("https://example.test/chat".into()),
            security: Some("https://example.test/security".into()),
        });
        package.funding = vec![Funding {
            funding_type: Some("github".into()),
            url: Some("https://example.test/sponsor".into()),
        }];
        package.scripts.insert(
            "post-install-cmd".into(),
            ScriptHandler::Multiple(vec!["echo one".into(), "echo two".into()]),
        );
        package.abandoned = Some(Abandoned::Replacement("new/package".into()));
        package.archive = Some(ArchiveConfig {
            name: Some("package".into()),
            exclude: vec!["tests".into()],
        });
        package.default_branch = Some(true);
        package.transport_options = Some(json!({"http": {"timeout": 30}}));

        round_trip(&package);
    }
}
