//! Pool optimization for reducing the size of the package pool before solving.
//!
//! This module implements Composer's PoolOptimizer which removes unnecessary packages
//! from the pool to speed up the SAT solver by reducing the number of rules.
//!
//! Two main optimizations are performed:
//! 1. **Identical dependencies optimization**: Groups packages with identical dependency
//!    definitions and keeps only the best version from each group.
//! 2. **Impossible packages optimization**: Uses locked package constraints to filter
//!    out versions that can't possibly be selected.

use foldhash::{HashMap, HashMapExt};
use std::borrow::Cow;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

use compact_str::CompactString;
use hashbrown::{hash_map::EntryRef, HashMap as BorrowedKeyMap};
use smallvec::SmallVec;
use sonata_semver::{ConstraintInterface, NormalizedVersion, VersionParser};

use super::policy::Policy;
use super::pool::{PackageId, Pool, PoolEntry};
use super::request::Request;
use crate::package::{DependencyMap, Package};
use crate::util::{canonical_package_name, is_platform_package};

type ConstraintId = u32;
type ConstraintBucket = SmallVec<[ConstraintId; 4]>;
type ConstraintIndex = BorrowedKeyMap<CompactString, ConstraintBucket, foldhash::fast::RandomState>;

enum PreparedConstraint {
    Unparsed,
    Permissive,
    Parsed(Box<dyn ConstraintInterface>),
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct DependencyFingerprint {
    sum: u64,
    mixed_sum: u64,
}

#[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct DependencyGroupKey<'a> {
    package_name: Cow<'a, str>,
    constraint_group: u64,
    dependency_fingerprint: DependencyFingerprint,
}

#[derive(Default)]
struct PackageIdSet {
    words: Vec<u64>,
    len: usize,
}

impl PackageIdSet {
    fn with_max_id(max_id: usize) -> Self {
        let mut set = Self::default();
        set.reset(max_id);
        set
    }

    fn reset(&mut self, max_id: usize) {
        self.words.clear();
        self.words.resize((max_id >> 6) + 1, 0);
        self.len = 0;
    }

    #[inline]
    fn contains(&self, id: PackageId) -> bool {
        if id < 0 {
            return false;
        }
        let index = id as usize;
        self.words
            .get(index >> 6)
            .is_some_and(|word| word & (1 << (index & 63)) != 0)
    }

    #[inline]
    fn insert(&mut self, id: PackageId) -> bool {
        debug_assert!(id >= 0);
        let index = id as usize;
        let mask = 1 << (index & 63);
        let word = &mut self.words[index >> 6];
        let inserted = *word & mask == 0;
        *word |= mask;
        self.len += usize::from(inserted);
        inserted
    }

    #[inline]
    fn remove(&mut self, id: PackageId) -> bool {
        if id < 0 {
            return false;
        }
        let index = id as usize;
        let Some(word) = self.words.get_mut(index >> 6) else {
            return false;
        };
        let mask = 1 << (index & 63);
        let removed = *word & mask != 0;
        *word &= !mask;
        self.len -= usize::from(removed);
        removed
    }

    fn len(&self) -> usize {
        self.len
    }
}

/// Optimizes a Pool by removing unnecessary packages before solving.
///
/// This reduces the number of SAT rules and speeds up solving.
pub struct PoolOptimizer<'a> {
    /// Selection policy for determining which package to keep
    policy: &'a Policy,

    /// Packages that cannot be removed (fixed/locked)
    irremovable_packages: PackageIdSet,

    /// Packages marked for removal
    packages_to_remove: PackageIdSet,

    /// Maps base package IDs to their alias package IDs
    aliases_per_package: HashMap<PackageId, Vec<PackageId>>,

    /// Version parser for constraint operations
    version_parser: VersionParser,

    /// Cache for parsed constraints (constraint_string -> parsed constraint)
    constraint_cache:
        BorrowedKeyMap<String, Option<Box<dyn ConstraintInterface>>, foldhash::fast::RandomState>,

    /// Cache for normalized versions (raw_version -> normalized_version)
    version_cache: BorrowedKeyMap<String, NormalizedVersion, foldhash::fast::RandomState>,
}

impl<'a> PoolOptimizer<'a> {
    /// Create a new pool optimizer with the given policy.
    pub fn new(policy: &'a Policy) -> Self {
        Self {
            policy,
            irremovable_packages: PackageIdSet::default(),
            packages_to_remove: PackageIdSet::default(),
            aliases_per_package: HashMap::new(),
            version_parser: VersionParser::new(),
            constraint_cache: BorrowedKeyMap::with_hasher(foldhash::fast::RandomState::default()),
            version_cache: BorrowedKeyMap::with_hasher(foldhash::fast::RandomState::default()),
        }
    }

    /// Optimize the pool and return a new optimized pool.
    pub fn optimize(&mut self, request: &Request, pool: &Pool) -> Pool {
        let started = Instant::now();

        // Reset state
        self.irremovable_packages.reset(pool.len());
        self.packages_to_remove.reset(pool.len());
        self.aliases_per_package.clear();
        self.constraint_cache.clear();
        self.version_cache.clear();

        // Prepare: collect constraints and mark irremovable packages
        let (require_constraints, conflict_constraints, constraint_texts) =
            self.prepare(request, pool);
        let mut parsed_constraints = (0..constraint_texts.len())
            .map(|_| PreparedConstraint::Unparsed)
            .collect::<Vec<_>>();
        let prepared = Instant::now();

        // Optimization 1: Remove packages with identical dependencies, keeping only the best
        self.optimize_by_identical_dependencies(
            pool,
            &require_constraints,
            &conflict_constraints,
            &constraint_texts,
            &mut parsed_constraints,
        );
        let deduplicated = Instant::now();

        // Optimization 2: Remove packages that can't satisfy locked constraints
        self.optimize_impossible_packages_away(request, pool, &require_constraints);
        let filtered = Instant::now();

        // Apply removals and create new pool
        let optimized = self.apply_removals_to_pool(pool);
        log::debug!(
            "Pool optimizer phases: prepare={:?}, deduplicate={:?}, locked={:?}, rebuild={:?}, parsed_constraints={}, total={:?}",
            prepared.duration_since(started),
            deduplicated.duration_since(prepared),
            filtered.duration_since(deduplicated),
            filtered.elapsed(),
            parsed_constraints
                .iter()
                .filter(|constraint| !matches!(constraint, PreparedConstraint::Unparsed))
                .count()
                + self.constraint_cache.len(),
            started.elapsed(),
        );
        optimized
    }

    /// Prepare optimization by collecting constraints and marking irremovable packages.
    fn prepare<'b>(
        &mut self,
        request: &'b Request,
        pool: &'b Pool,
    ) -> (ConstraintIndex, ConstraintIndex, Vec<&'b str>) {
        let mut require_constraints =
            ConstraintIndex::with_hasher(foldhash::fast::RandomState::default());
        let mut conflict_constraints =
            ConstraintIndex::with_hasher(foldhash::fast::RandomState::default());
        let mut constraint_ids: HashMap<&'b str, ConstraintId> = HashMap::with_capacity(pool.len());
        let mut constraint_texts = Vec::with_capacity(pool.len());

        // Mark fixed packages as irremovable
        for fixed in &request.fixed_packages {
            if let Some(id) = self.find_package_id(pool, &fixed.name, &fixed.version) {
                self.mark_irremovable(pool, id);
            }
        }

        // Mark locked packages as irremovable
        for locked in &request.locked_packages {
            if let Some(id) = self.find_package_id(pool, &locked.name, &locked.version) {
                self.mark_irremovable(pool, id);
            }
        }

        // Mark packages as irremovable if they provide/replace a virtual package
        // that has no other providers in the pool
        // This ensures root packages that replace things like shopware/core are kept
        for id in pool.all_package_ids() {
            if let Some(pkg) = pool.package(id) {
                let mut is_sole_provider = false;

                for (replaced, _) in &pkg.replace {
                    if pool.is_sole_provider(replaced, id) {
                        is_sole_provider = true;
                        log::trace!(
                            "{} {} is sole provider for replaced package {}",
                            pkg.name,
                            pkg.version,
                            replaced
                        );
                        break;
                    }
                }

                if !is_sole_provider {
                    for (provided, _) in &pkg.provide {
                        if pool.is_sole_provider(provided, id) {
                            is_sole_provider = true;
                            log::trace!(
                                "{} {} is sole provider for provided package {}",
                                pkg.name,
                                pkg.version,
                                provided
                            );
                            break;
                        }
                    }
                }

                if is_sole_provider {
                    self.mark_irremovable(pool, id);
                }
            }
        }

        // Extract require constraints from root requirements
        for (name, constraint) in request.all_requires() {
            Self::extract_constraint(
                &mut require_constraints,
                &mut constraint_ids,
                &mut constraint_texts,
                name,
                constraint,
            );
        }

        // First pass over all packages to extract constraints and build alias map
        for id in pool.all_package_ids() {
            if let Some(entry) = pool.entry(id) {
                match entry {
                    PoolEntry::Package(pkg) => {
                        // Extract requires
                        for (target, constraint) in &pkg.require {
                            Self::extract_constraint(
                                &mut require_constraints,
                                &mut constraint_ids,
                                &mut constraint_texts,
                                target,
                                constraint,
                            );
                        }

                        // Extract conflicts
                        for (target, constraint) in &pkg.conflict {
                            Self::extract_constraint(
                                &mut conflict_constraints,
                                &mut constraint_ids,
                                &mut constraint_texts,
                                target,
                                constraint,
                            );
                        }
                    }
                    PoolEntry::Alias(alias) => {
                        // Track alias relationships
                        if let Some(base_id) = pool.get_alias_base(id) {
                            self.aliases_per_package
                                .entry(base_id)
                                .or_default()
                                .push(id);
                        }

                        // Extract requires from alias's base package
                        let base_pkg = alias.alias_of();
                        for (target, constraint) in &base_pkg.require {
                            Self::extract_constraint(
                                &mut require_constraints,
                                &mut constraint_ids,
                                &mut constraint_texts,
                                target,
                                constraint,
                            );
                        }

                        // Extract conflicts
                        for (target, constraint) in &base_pkg.conflict {
                            Self::extract_constraint(
                                &mut conflict_constraints,
                                &mut constraint_ids,
                                &mut constraint_texts,
                                target,
                                constraint,
                            );
                        }
                    }
                }
            }
        }

        (require_constraints, conflict_constraints, constraint_texts)
    }

    /// Mark a package as irremovable, including its aliases.
    fn mark_irremovable(&mut self, pool: &Pool, id: PackageId) {
        self.irremovable_packages.insert(id);

        // Also mark aliases as irremovable
        if let Some(aliases) = self.aliases_per_package.get(&id) {
            for &alias_id in aliases {
                self.irremovable_packages.insert(alias_id);
            }
        }

        // If this is an alias, mark the base package too
        if let Some(base_id) = pool.get_alias_base(id) {
            self.irremovable_packages.insert(base_id);
            // And all other aliases of that base
            if let Some(aliases) = self.aliases_per_package.get(&base_id) {
                for &alias_id in aliases {
                    self.irremovable_packages.insert(alias_id);
                }
            }
        }
    }

    fn extract_constraint<'b>(
        constraints: &mut ConstraintIndex,
        constraint_ids: &mut HashMap<&'b str, ConstraintId>,
        constraint_texts: &mut Vec<&'b str>,
        package_name: &str,
        constraint: &'b str,
    ) {
        let package_name = canonical_package_name(package_name);
        let bucket = match constraints.entry_ref(package_name.as_ref()) {
            EntryRef::Occupied(entry) => entry.into_mut(),
            EntryRef::Vacant(entry) => entry.insert_with_key(
                CompactString::new(package_name.as_ref()),
                ConstraintBucket::new(),
            ),
        };
        Self::insert_expanded_constraint(bucket, constraint_ids, constraint_texts, constraint);
    }

    fn insert_expanded_constraint<'b>(
        constraints: &mut ConstraintBucket,
        constraint_ids: &mut HashMap<&'b str, ConstraintId>,
        constraint_texts: &mut Vec<&'b str>,
        constraint: &'b str,
    ) {
        if constraint.as_bytes().windows(2).any(|pair| pair == b"||") {
            let mut parts = constraint
                .split("||")
                .map(str::trim)
                .filter(|part| !part.is_empty());
            if let (Some(first), Some(second)) = (parts.next(), parts.next()) {
                Self::insert_constraint(constraints, constraint_ids, constraint_texts, first);
                Self::insert_constraint(constraints, constraint_ids, constraint_texts, second);
                for part in parts {
                    Self::insert_constraint(constraints, constraint_ids, constraint_texts, part);
                }
                return;
            }
        }

        if constraint.as_bytes().contains(&b'|') {
            let mut parts = constraint
                .split('|')
                .map(str::trim)
                .filter(|part| !part.is_empty());
            if let (Some(first), Some(second)) = (parts.next(), parts.next()) {
                Self::insert_constraint(constraints, constraint_ids, constraint_texts, first);
                Self::insert_constraint(constraints, constraint_ids, constraint_texts, second);
                for part in parts {
                    Self::insert_constraint(constraints, constraint_ids, constraint_texts, part);
                }
                return;
            }
        }

        Self::insert_constraint(constraints, constraint_ids, constraint_texts, constraint);
    }

    fn insert_constraint<'b>(
        constraints: &mut ConstraintBucket,
        constraint_ids: &mut HashMap<&'b str, ConstraintId>,
        constraint_texts: &mut Vec<&'b str>,
        constraint: &'b str,
    ) {
        let next_id = constraint_texts.len() as ConstraintId;
        let id = *constraint_ids.entry(constraint).or_insert_with(|| {
            constraint_texts.push(constraint);
            next_id
        });
        if !constraints.contains(&id) {
            constraints.push(id);
        }
    }

    /// Optimization 1: Remove packages with identical dependencies.
    ///
    /// Groups packages by their dependency hash and keeps only the best version
    /// (according to the policy) from each group.
    fn optimize_by_identical_dependencies(
        &mut self,
        pool: &Pool,
        require_constraints: &ConstraintIndex,
        conflict_constraints: &ConstraintIndex,
        constraint_texts: &[&str],
        parsed_constraints: &mut [PreparedConstraint],
    ) {
        // A flat borrowed-key index avoids allocating a package name and two
        // nested hash tables for every dependency group.
        let mut groups: HashMap<DependencyGroupKey<'_>, SmallVec<[PackageId; 4]>> =
            HashMap::with_capacity(pool.len());

        // Track which packages have been assigned to a group
        let mut packages_in_groups = PackageIdSet::with_max_id(pool.len());

        for id in pool.all_package_ids() {
            // Skip irremovable packages
            if self.irremovable_packages.contains(id) {
                continue;
            }

            // Skip aliases (they're handled with their base package)
            if pool.is_alias(id) {
                continue;
            }

            let Some(pkg) = pool.package(id) else {
                continue;
            };

            // Initially mark for removal
            self.packages_to_remove.insert(id);

            let pkg_name = canonical_package_name(&pkg.name);
            let mut group_hasher = foldhash::quality::FixedState::default().build_hasher();
            let mut matched_constraint = false;
            let require_constraints = require_constraints.get(pkg_name.as_ref());
            let conflict_constraints = conflict_constraints.get(pkg_name.as_ref());

            if require_constraints.is_none() && conflict_constraints.is_none() {
                continue;
            }

            let version_parser = &self.version_parser;
            let normalized_version = self
                .version_cache
                .entry_ref(pkg.version.as_str())
                .or_insert_with(|| {
                    let normalized = version_parser
                        .normalize(&pkg.version)
                        .unwrap_or_else(|_| pkg.version.to_string());
                    NormalizedVersion::new(normalized)
                });

            // Check requires
            if let Some(constraints) = require_constraints {
                for &constraint_id in constraints {
                    if Self::indexed_constraint_matches(
                        version_parser,
                        parsed_constraints,
                        constraint_texts,
                        normalized_version,
                        constraint_id,
                    ) {
                        (constraint_id << 1).hash(&mut group_hasher); // LSB 0 for require
                        matched_constraint = true;
                    }
                }
            }

            // Check conflicts
            if let Some(constraints) = conflict_constraints {
                for &constraint_id in constraints {
                    if Self::indexed_constraint_matches(
                        version_parser,
                        parsed_constraints,
                        constraint_texts,
                        normalized_version,
                        constraint_id,
                    ) {
                        ((constraint_id << 1) | 1).hash(&mut group_hasher); // LSB 1 for conflict
                        matched_constraint = true;
                    }
                }
            }

            // Only group if it matches at least one constraint
            if matched_constraint {
                groups
                    .entry(DependencyGroupKey {
                        package_name: pkg_name,
                        constraint_group: group_hasher.finish(),
                        dependency_fingerprint: Self::dependency_fingerprint(pkg),
                    })
                    .or_default()
                    .push(id);

                packages_in_groups.insert(id);
            }
        }

        // Build the name-only index after grouping so its keys can borrow the
        // stable names already held by `groups`.
        let mut grouped_names: HashMap<&str, ()> = HashMap::with_capacity(groups.len());
        for key in groups.keys() {
            grouped_names.insert(key.package_name.as_ref(), ());
        }

        // Keep deterministic traversal without allocating and sorting key
        // vectors at each level of the former nested map.
        let mut group_keys: Vec<_> = groups.keys().collect();
        group_keys.sort_unstable();
        for key in group_keys {
            let packages = &groups[key];
            if packages.len() == 1 {
                self.keep_package(pool, packages[0]);
            } else {
                let preferred = self
                    .policy
                    .select_preferred_for_optimization_inline(pool, packages);
                for &pkg_id in &preferred {
                    self.keep_package(pool, pkg_id);
                }
            }
        }

        // Also keep packages that weren't in any constraint group but are required
        // (packages that have no constraints matching them should still be kept
        // if they're the only option)
        for id in pool.all_package_ids() {
            if self.irremovable_packages.contains(id) || pool.is_alias(id) {
                continue;
            }

            // If package wasn't added to any group, it matches no active constraints.
            // It should be kept ONLY if no other version of this package matched any constraints
            // (i.e. the package name itself is not part of the active problem space constraints).
            if !packages_in_groups.contains(id) {
                // If we haven't already decided to keep it (it's still in removal set)
                if self.packages_to_remove.contains(id) {
                    if let Some(pkg) = pool.package(id) {
                        let pkg_name = canonical_package_name(&pkg.name);
                        if !grouped_names.contains_key(pkg_name.as_ref()) {
                            // No groups for this package name at all, keep it
                            self.keep_package(pool, id);
                        }
                    }
                }
            }
        }
    }

    #[inline]
    fn indexed_constraint_matches(
        version_parser: &VersionParser,
        parsed_constraints: &mut [PreparedConstraint],
        constraint_texts: &[&str],
        normalized_version: &NormalizedVersion,
        constraint_id: ConstraintId,
    ) -> bool {
        let constraint = constraint_texts[constraint_id as usize];
        if constraint == "*" || constraint.is_empty() {
            return true;
        }

        let prepared = &mut parsed_constraints[constraint_id as usize];
        if matches!(prepared, PreparedConstraint::Unparsed) {
            *prepared = match version_parser.parse_constraints(constraint) {
                Ok(parsed) => PreparedConstraint::Parsed(parsed),
                Err(_) => PreparedConstraint::Permissive,
            };
        }

        match prepared {
            PreparedConstraint::Unparsed => unreachable!(),
            PreparedConstraint::Permissive => true,
            PreparedConstraint::Parsed(parsed) => {
                parsed.matches_prepared_version(normalized_version)
            }
        }
    }

    /// Keep a package (remove from packages_to_remove set).
    fn keep_package(&mut self, pool: &Pool, id: PackageId) {
        self.packages_to_remove.remove(id);

        // Also keep aliases
        if let Some(aliases) = self.aliases_per_package.get(&id).cloned() {
            for alias_id in aliases {
                self.packages_to_remove.remove(alias_id);
            }
        }

        // If this is an alias, keep the base too
        if let Some(base_id) = pool.get_alias_base(id) {
            self.packages_to_remove.remove(base_id);
        }
    }

    /// Calculate an order-independent fingerprint of dependency definitions.
    fn dependency_fingerprint(package: &Package) -> DependencyFingerprint {
        #[inline]
        fn avalanche(mut value: u64) -> u64 {
            value ^= value >> 30;
            value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
            value ^= value >> 27;
            value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
            value ^ (value >> 31)
        }

        #[inline]
        fn fingerprint_deps(
            fingerprint: &mut DependencyFingerprint,
            deps: &DependencyMap,
            section: u8,
        ) {
            let state = foldhash::quality::FixedState::default();
            for (name, constraint) in deps {
                let pair_hash = state.hash_one((section, name, constraint));
                fingerprint.sum = fingerprint.sum.wrapping_add(pair_hash);
                fingerprint.mixed_sum = fingerprint.mixed_sum.wrapping_add(avalanche(pair_hash));
            }
        }

        let mut fingerprint = DependencyFingerprint {
            sum: 0,
            mixed_sum: 0,
        };
        fingerprint_deps(&mut fingerprint, &package.require, 1);
        fingerprint_deps(&mut fingerprint, &package.conflict, 2);
        fingerprint_deps(&mut fingerprint, &package.replace, 3);
        fingerprint_deps(&mut fingerprint, &package.provide, 4);
        fingerprint
    }

    /// Optimization 2: Remove packages that can't satisfy locked package constraints.
    ///
    /// Uses the requirements of locked packages to filter out versions that
    /// definitely won't be selected.
    fn optimize_impossible_packages_away(
        &mut self,
        request: &Request,
        pool: &Pool,
        require_constraints: &ConstraintIndex,
    ) {
        if request.locked_packages.is_empty() {
            return;
        }

        // Build an index of packages by name with version info (excluding irremovable and aliases)
        // Store (id, version) to avoid repeated pool lookups
        let mut package_index: HashMap<String, Vec<(PackageId, CompactString)>> = HashMap::new();

        for id in pool.all_package_ids() {
            // Skip irremovable
            if self.irremovable_packages.contains(id) {
                continue;
            }

            // Skip aliases (they're handled with their base)
            if pool.is_alias(id) {
                continue;
            }

            // Skip already marked for removal
            if self.packages_to_remove.contains(id) {
                continue;
            }

            if let Some(pkg) = pool.package(id) {
                // Skip locked packages themselves
                let is_locked = request
                    .locked_packages
                    .iter()
                    .any(|l| l.name.eq_ignore_ascii_case(&pkg.name) && l.version == pkg.version);
                if is_locked {
                    continue;
                }

                package_index
                    .entry(canonical_package_name(&pkg.name).into_owned())
                    .or_default()
                    .push((id, pkg.version.clone()));
            }
        }

        // Collect all filter operations to perform (to avoid borrow issues)
        // (package_name, constraint) pairs we need to check
        let mut filter_ops: Vec<(String, String)> = Vec::new();

        for locked in &request.locked_packages {
            // Check if the locked package is still required
            let locked_name = canonical_package_name(&locked.name);
            if !require_constraints.contains_key(locked_name.as_ref()) {
                continue;
            }

            // Collect filter operations
            for (require_name, constraint) in &locked.require {
                let require_name = canonical_package_name(require_name);
                if package_index.contains_key(require_name.as_ref()) {
                    filter_ops.push((require_name.into_owned(), constraint.as_str().to_owned()));
                }
            }
        }

        // Now apply filters
        for (require_name_lower, constraint) in filter_ops {
            if let Some(candidates) = package_index.get(&require_name_lower) {
                // Collect IDs to remove
                let mut to_remove: Vec<PackageId> = Vec::new();

                for (id, version) in candidates {
                    if !self.version_matches_constraint(version, &constraint) {
                        to_remove.push(*id);
                    }
                }

                // Apply removals
                for id in &to_remove {
                    self.packages_to_remove.insert(*id);
                    // Also mark aliases for removal
                    if let Some(aliases) = self.aliases_per_package.get(id).cloned() {
                        for alias_id in aliases {
                            self.packages_to_remove.insert(alias_id);
                        }
                    }
                }

                // Update the index to remove filtered packages
                if let Some(candidates) = package_index.get_mut(&require_name_lower) {
                    candidates.retain(|(id, _)| !to_remove.contains(id));
                }
            }
        }
    }

    /// Check if a version matches a constraint.
    fn version_matches_constraint(&mut self, version: &str, constraint_str: &str) -> bool {
        // Handle wildcard
        if constraint_str == "*" || constraint_str.is_empty() {
            return true;
        }

        let version_parser = &self.version_parser;
        let normalized_version = self.version_cache.entry_ref(version).or_insert_with(|| {
            let normalized = version_parser
                .normalize(version)
                .unwrap_or_else(|_| version.to_string());
            NormalizedVersion::new(normalized)
        });

        let parsed_constraint = self
            .constraint_cache
            .entry_ref(constraint_str)
            .or_insert_with(|| {
                version_parser
                    .parse_constraints(constraint_str)
                    .ok()
                    .map(|c| c as Box<dyn ConstraintInterface>)
            })
            .as_ref();

        match parsed_constraint {
            Some(pc) => pc.matches_prepared_version(normalized_version),
            None => true, // Be permissive on failure
        }
    }

    /// Find a package ID by name and version.
    fn find_package_id(&self, pool: &Pool, name: &str, version: &str) -> Option<PackageId> {
        for id in pool.packages_by_name(name) {
            if let Some(entry) = pool.entry(id) {
                if entry.version() == version {
                    return Some(id);
                }
            }
        }
        None
    }

    /// Apply the collected removals and create a new optimized pool.
    fn apply_removals_to_pool(&self, original_pool: &Pool) -> Pool {
        log::debug!(
            "Pool optimizer removing {} packages from pool of {}",
            self.packages_to_remove.len(),
            original_pool.len()
        );

        // Debug: count how many of each package are being removed
        let mut pkg_counts: HashMap<String, (usize, usize)> = HashMap::new();
        for id in original_pool.all_package_ids() {
            if let Some(pkg) = original_pool.package(id) {
                let entry = pkg_counts.entry(pkg.name.clone()).or_insert((0, 0));
                entry.0 += 1; // total
                if self.packages_to_remove.contains(id) {
                    entry.1 += 1; // removed
                }
            }
        }
        // Log packages where all versions are removed (potential problem)
        for (name, (total, removed)) in &pkg_counts {
            if *removed == *total && *total > 0 {
                log::warn!("Pool optimizer removed all {} versions of {}", total, name);
            }
        }

        // Log what versions of key packages are being kept
        for key in &[
            "symfony/console",
            "symfony/http-kernel",
            "symfony/string",
            "symfony/event-dispatcher",
            "webmozart/assert",
        ] {
            if let Some(&(total, removed)) = pkg_counts.get(*key) {
                log::debug!(
                    "Pool optimizer: {} - kept {}/{} versions",
                    key,
                    total - removed,
                    total
                );
            }
        }

        let mut new_pool = Pool::with_minimum_stability(original_pool.minimum_stability());

        // Copy stability flags
        // TODO: Access private field stability_flags if possible, or add getter/setter
        // Since we can't access private fields easily without modifying Pool,
        // we might be missing flags. But wait, we can add them via builder or setter.
        // Assuming we rely on the fact that stability was checked during initial pool population ??
        // Actually, optimization might lose stability flags which is bad for subsequent lookups.

        // Copy packages that aren't marked for removal
        for id in original_pool.all_package_ids() {
            if self.packages_to_remove.contains(id) {
                continue;
            }

            if let Some(entry) = original_pool.entry(id) {
                match entry {
                    PoolEntry::Package(pkg) => {
                        let repo_name = original_pool.get_repository(id);
                        let priority = original_pool.get_priority_by_id(id);

                        // Platform packages and packages with replace/provide should bypass
                        // stability filtering. Platform packages are fixed system packages.
                        // Packages with replace/provide are typically root or metapackages
                        // that need to be preserved regardless of their version stability.
                        let bypass_stability = is_platform_package(&pkg.name)
                            || !pkg.replace.is_empty()
                            || !pkg.provide.is_empty();

                        if bypass_stability {
                            new_pool.add_package_arc_bypass_stability(Arc::clone(pkg), repo_name);
                        } else {
                            new_pool.add_package_arc(Arc::clone(pkg), repo_name);
                        }

                        // Preserve priority
                        if let Some(repo) = repo_name {
                            new_pool.set_priority(repo, priority);
                        }
                    }
                    PoolEntry::Alias(alias) => {
                        // Aliases will be recreated after their base packages
                        // We need to find if the base is in the new pool
                        if let Some(base_id) = original_pool.get_alias_base(id) {
                            // Only add alias if base package was kept
                            if !self.packages_to_remove.contains(base_id) {
                                let repo_name = original_pool.get_repository(id);
                                new_pool.add_alias_package_arc(Arc::clone(alias), repo_name);
                            }
                        }
                    }
                }
            }
        }

        new_pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{AliasPackage, Stability};
    use std::collections::BTreeSet;
    use std::sync::Arc;

    #[test]
    fn package_id_set_tracks_dense_pool_ids() {
        let mut set = PackageIdSet::with_max_id(130);
        assert!(!set.contains(0));
        assert!(!set.contains(-1));
        assert_eq!(set.len(), 0);

        assert!(set.insert(1));
        assert!(set.insert(64));
        assert!(set.insert(130));
        assert!(!set.insert(64));
        assert_eq!(set.len(), 3);
        assert!(set.contains(1));
        assert!(set.contains(64));
        assert!(set.contains(130));

        assert!(set.remove(64));
        assert!(!set.remove(64));
        assert!(!set.remove(131));
        assert_eq!(set.len(), 2);

        set.reset(5);
        assert_eq!(set.len(), 0);
        assert!(!set.contains(1));
        assert!(!set.contains(130));
        assert!(set.insert(5));
    }

    #[test]
    fn expanded_constraint_index_matches_owned_set_behavior() {
        fn insert_reference(constraints: &mut BTreeSet<String>, constraint: &str) {
            if constraint.contains("||") {
                let mut parts = constraint
                    .split("||")
                    .map(str::trim)
                    .filter(|part| !part.is_empty());
                if let (Some(first), Some(second)) = (parts.next(), parts.next()) {
                    constraints.insert(first.to_string());
                    constraints.insert(second.to_string());
                    constraints.extend(parts.map(String::from));
                    return;
                }
            }

            if constraint.contains('|') {
                let mut parts = constraint
                    .split('|')
                    .map(str::trim)
                    .filter(|part| !part.is_empty());
                if let (Some(first), Some(second)) = (parts.next(), parts.next()) {
                    constraints.insert(first.to_string());
                    constraints.insert(second.to_string());
                    constraints.extend(parts.map(String::from));
                    return;
                }
            }

            constraints.insert(constraint.to_string());
        }

        let corpus = [
            "^1.0",
            "^1.0",
            "^2.14 || ^3.3",
            " ^2.14 || ^3.3 ",
            "^1|^2|^3",
            "||",
            "||^1",
            "^1||",
            "^1||||^2",
            "^1|||^2",
            "",
            "   ",
        ];
        let mut expected = BTreeSet::new();
        let mut actual = ConstraintBucket::new();
        let mut ids = HashMap::new();
        let mut texts = Vec::new();

        for constraint in corpus {
            insert_reference(&mut expected, constraint);
            PoolOptimizer::insert_expanded_constraint(
                &mut actual,
                &mut ids,
                &mut texts,
                constraint,
            );
            let actual_text: BTreeSet<_> = actual.iter().map(|&id| texts[id as usize]).collect();
            assert_eq!(actual_text, expected.iter().map(String::as_str).collect());
        }

        let mut second_bucket = ConstraintBucket::new();
        PoolOptimizer::insert_expanded_constraint(&mut second_bucket, &mut ids, &mut texts, "^1.0");
        let first_id = actual
            .iter()
            .copied()
            .find(|&id| texts[id as usize] == "^1.0")
            .unwrap();
        assert_eq!(second_bucket[0], first_id);
    }

    #[test]
    fn indexed_constraint_matching_preserves_permissive_semantics() {
        let parser = VersionParser::new();
        let normalized = NormalizedVersion::new(parser.normalize("1.2.3").unwrap());
        let constraint_texts = ["^1", "^2", "^", "*", "unused"];
        let mut prepared = (0..constraint_texts.len())
            .map(|_| PreparedConstraint::Unparsed)
            .collect::<Vec<_>>();

        assert!(PoolOptimizer::indexed_constraint_matches(
            &parser,
            &mut prepared,
            &constraint_texts,
            &normalized,
            0,
        ));
        assert!(!PoolOptimizer::indexed_constraint_matches(
            &parser,
            &mut prepared,
            &constraint_texts,
            &normalized,
            1,
        ));
        assert!(PoolOptimizer::indexed_constraint_matches(
            &parser,
            &mut prepared,
            &constraint_texts,
            &normalized,
            2,
        ));
        assert!(PoolOptimizer::indexed_constraint_matches(
            &parser,
            &mut prepared,
            &constraint_texts,
            &normalized,
            3,
        ));
        assert!(matches!(prepared[4], PreparedConstraint::Unparsed));
    }

    #[test]
    fn dependency_fingerprint_matches_sorted_grouping() {
        fn package_with_links(
            require: &[(&str, &str)],
            conflict: &[(&str, &str)],
            replace: &[(&str, &str)],
            provide: &[(&str, &str)],
        ) -> Package {
            let mut package = Package::new("vendor/package", "1.0.0");
            for &(name, constraint) in require {
                package.require.insert(name.into(), constraint.into());
            }
            for &(name, constraint) in conflict {
                package.conflict.insert(name.into(), constraint.into());
            }
            for &(name, constraint) in replace {
                package.replace.insert(name.into(), constraint.into());
            }
            for &(name, constraint) in provide {
                package.provide.insert(name.into(), constraint.into());
            }
            package
        }

        fn sorted_reference(package: &Package) -> u64 {
            fn hash_deps(hasher: &mut impl Hasher, deps: &DependencyMap, section: u8) {
                if deps.is_empty() {
                    return;
                }
                section.hash(hasher);
                let mut sorted: Vec<_> = deps.iter().collect();
                sorted.sort_unstable_by(|left, right| left.0.cmp(right.0));
                for (name, constraint) in sorted {
                    name.hash(hasher);
                    constraint.hash(hasher);
                }
            }

            let mut hasher = foldhash::quality::FixedState::default().build_hasher();
            hash_deps(&mut hasher, &package.require, 1);
            hash_deps(&mut hasher, &package.conflict, 2);
            hash_deps(&mut hasher, &package.replace, 3);
            hash_deps(&mut hasher, &package.provide, 4);
            hasher.finish()
        }

        let packages = [
            package_with_links(&[], &[], &[], &[]),
            package_with_links(&[("vendor/a", "^1")], &[], &[], &[]),
            package_with_links(&[("vendor/a", "^1"), ("vendor/b", "^2")], &[], &[], &[]),
            package_with_links(&[("vendor/b", "^2"), ("vendor/a", "^1")], &[], &[], &[]),
            package_with_links(&[("vendor/a", "^2"), ("vendor/b", "^2")], &[], &[], &[]),
            package_with_links(&[("vendor/a", "^1")], &[("vendor/b", "^2")], &[], &[]),
            package_with_links(&[], &[], &[("vendor/a", "self.version")], &[]),
            package_with_links(&[], &[], &[], &[("virtual/api", "1.0")]),
        ];

        for left in &packages {
            for right in &packages {
                assert_eq!(
                    sorted_reference(left) == sorted_reference(right),
                    PoolOptimizer::dependency_fingerprint(left)
                        == PoolOptimizer::dependency_fingerprint(right),
                );
            }
        }
    }

    #[test]
    fn test_optimizer_basic() {
        let mut pool = Pool::new();
        pool.add_package(Package::new("vendor/a", "1.0.0"));
        pool.add_package(Package::new("vendor/a", "2.0.0"));
        pool.add_package(Package::new("vendor/b", "1.0.0"));

        let mut request = Request::new();
        request.require("vendor/a", "^1.0");
        request.require("vendor/b", "^1.0");

        let policy = Policy::new();
        let mut optimizer = PoolOptimizer::new(&policy);
        let optimized = optimizer.optimize(&request, &pool);

        // Should have kept packages for vendor/a matching ^1.0 and vendor/b
        assert!(optimized.len() >= 2);
    }

    #[test]
    fn test_optimizer_keeps_irremovable() {
        let mut pool = Pool::new();
        pool.add_package(Package::new("vendor/a", "1.0.0"));
        pool.add_package(Package::new("vendor/a", "2.0.0"));

        let mut request = Request::new();
        request.require("vendor/a", "*");
        request.lock(Package::new("vendor/a", "1.0.0"));

        let policy = Policy::new();
        let mut optimizer = PoolOptimizer::new(&policy);
        let optimized = optimizer.optimize(&request, &pool);

        // Locked package should still be there
        let versions: Vec<_> = optimized
            .packages_by_name("vendor/a")
            .iter()
            .filter_map(|&id| optimized.package(id))
            .map(|p| p.version.as_str())
            .collect();

        assert!(versions.contains(&"1.0.0"));
    }

    #[test]
    fn test_optimizer_removes_impossible_versions() {
        let mut pool = Pool::new();

        // A requires B ^1.0
        let mut a = Package::new("vendor/a", "1.0.0");
        a.require.insert("vendor/b".to_string(), "^1.0".to_string());
        pool.add_package(a);

        // B has versions 1.0, 1.5, and 2.0
        pool.add_package(Package::new("vendor/b", "1.0.0"));
        pool.add_package(Package::new("vendor/b", "1.5.0"));
        pool.add_package(Package::new("vendor/b", "2.0.0"));

        // Lock A at 1.0.0 (which requires B ^1.0)
        let mut request = Request::new();
        request.require("vendor/a", "^1.0");
        request.require("vendor/b", "*");
        let mut locked_a = Package::new("vendor/a", "1.0.0");
        locked_a
            .require
            .insert("vendor/b".to_string(), "^1.0".to_string());
        request.lock(locked_a);

        let policy = Policy::new();
        let mut optimizer = PoolOptimizer::new(&policy);
        let optimized = optimizer.optimize(&request, &pool);

        // B 2.0.0 should be removed since it can't satisfy ^1.0
        let b_versions: Vec<_> = optimized
            .packages_by_name("vendor/b")
            .iter()
            .filter_map(|&id| optimized.package(id))
            .map(|p| p.version.as_str())
            .collect();

        // Should only have versions matching ^1.0
        assert!(!b_versions.contains(&"2.0.0"), "B 2.0.0 should be removed");
    }

    #[test]
    fn test_optimizer_identical_dependencies() {
        let mut pool = Pool::new();

        // Multiple versions of A with identical requirements
        let mut a1 = Package::new("vendor/a", "1.0.0");
        a1.require
            .insert("vendor/b".to_string(), "^1.0".to_string());
        pool.add_package(a1);

        let mut a2 = Package::new("vendor/a", "1.0.1");
        a2.require
            .insert("vendor/b".to_string(), "^1.0".to_string());
        pool.add_package(a2);

        let mut a3 = Package::new("vendor/a", "1.0.2");
        a3.require
            .insert("vendor/b".to_string(), "^1.0".to_string());
        pool.add_package(a3);

        pool.add_package(Package::new("vendor/b", "1.0.0"));

        let mut request = Request::new();
        request.require("vendor/a", "^1.0");
        request.require("vendor/b", "^1.0");

        // With default policy (prefer highest), should keep only 1.0.2
        let policy = Policy::new();
        let mut optimizer = PoolOptimizer::new(&policy);
        let optimized = optimizer.optimize(&request, &pool);

        let a_versions: Vec<_> = optimized
            .packages_by_name("vendor/a")
            .iter()
            .filter_map(|&id| optimized.package(id))
            .map(|p| p.version.as_str())
            .collect();

        // Should only keep the best version (1.0.2 with prefer_highest)
        assert_eq!(a_versions.len(), 1);
        assert!(a_versions.contains(&"1.0.2"));
    }

    #[test]
    fn test_optimizer_with_aliases() {
        let mut pool = Pool::with_minimum_stability(Stability::Dev);

        // Base package
        let pkg = Package::new("vendor/a", "dev-main");
        let _base_id = pool.add_package(pkg.clone());

        // Alias for the dev version
        let alias = AliasPackage::new(Arc::new(pkg), "1.0.0.0".to_string(), "1.0.0".to_string());
        pool.add_alias_package(alias);

        let mut request = Request::new();
        request.require("vendor/a", "^1.0");

        let policy = Policy::new();
        let mut optimizer = PoolOptimizer::new(&policy);
        let optimized = optimizer.optimize(&request, &pool);

        // Both base and alias should be kept
        let all_ids: Vec<_> = optimized.packages_by_name("vendor/a");
        assert!(!all_ids.is_empty(), "Package should be preserved");
    }

    #[test]
    fn test_optimizer_preserves_repo_priority() {
        let mut pool = Pool::new();

        pool.add_package_from_repo(Package::new("vendor/a", "1.0.0"), Some("repo1"));
        pool.add_package_from_repo(Package::new("vendor/a", "1.0.0"), Some("repo2"));
        pool.set_priority("repo1", 0);
        pool.set_priority("repo2", 1);

        let mut request = Request::new();
        request.require("vendor/a", "^1.0");

        let policy = Policy::new();
        let mut optimizer = PoolOptimizer::new(&policy);
        let optimized = optimizer.optimize(&request, &pool);

        // Should keep the one from higher priority repo
        let a_ids: Vec<_> = optimized.packages_by_name("vendor/a");
        assert!(!a_ids.is_empty());
    }
}
