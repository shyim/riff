use std::fmt;
use std::sync::Arc;

use super::pool::{PackageId, Pool};
use crate::package::Package;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) struct LiteralFingerprint {
    sum: u64,
    mixed_sum: u64,
    len: usize,
}

#[inline]
fn avalanche(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// A literal in SAT terms - positive means "install", negative means "don't install"
pub type Literal = i32;

/// Types of rules generated during dependency resolution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleType {
    /// Root composer.json requirement
    RootRequire,
    /// Fixed package that must stay installed (e.g., platform packages)
    Fixed,
    /// A locked/fixed package was removed from the pool by dependency policy.
    LockedFilterListRemoved,
    /// Package dependency: if A is installed, then B|C|D must be installed
    PackageRequires,
    /// Package conflict: A and B cannot both be installed
    PackageConflict,
    /// Multiple versions of same package: only one can be installed (binary conflict)
    PackageSameName,
    /// Multiple versions of same package: at most one can be installed (n-ary multi-conflict)
    /// This is more efficient than O(n²) binary conflicts for packages with many versions
    MultiConflict,
    /// Alias must require its target
    PackageAlias,
    /// Target must require its alias
    PackageInverseAlias,
    /// Learned clause from conflict analysis
    Learned,
}

impl RuleType {
    /// Get the priority of this rule type (lower = higher priority)
    pub fn priority(&self) -> u8 {
        match self {
            RuleType::RootRequire | RuleType::Fixed | RuleType::LockedFilterListRemoved => 1, // Request rules
            RuleType::PackageRequires
            | RuleType::PackageConflict
            | RuleType::PackageSameName
            | RuleType::MultiConflict
            | RuleType::PackageAlias
            | RuleType::PackageInverseAlias => 0, // Package rules
            RuleType::Learned => 4,
        }
    }

    /// Check if this is a multi-conflict rule type
    pub fn is_multi_conflict(&self) -> bool {
        matches!(self, RuleType::MultiConflict)
    }
}

/// A SAT rule (clause) representing a dependency constraint.
///
/// Rules are disjunctions (OR) of literals. A rule is satisfied when
/// at least one of its literals is true.
///
/// # Examples
///
/// - `[A]` - Package A must be installed (assertion)
/// - `[-A]` - Package A must not be installed
/// - `[-A, B, C]` - If A is installed, then B or C must be installed
/// - `[-A, -B]` - A and B cannot both be installed (conflict)
#[derive(Clone)]
pub struct Rule {
    /// The literals in this rule
    literals: Vec<Literal>,
    /// Type of rule
    rule_type: RuleType,
    /// Rule ID (assigned by RuleSet)
    id: u32,
    /// Source package ID (for error messages)
    source_package: Option<PackageId>,
    /// Target package name (for error messages)
    target_name: Option<String>,
    /// Constraint string (for error messages)
    constraint: Option<String>,
    /// Whether this rule is disabled
    disabled: bool,
    /// Locked package retained for policy-removal diagnostics.
    locked_package: Option<Arc<Package>>,
}

impl Rule {
    /// Create a new rule with the given literals
    pub fn new(literals: Vec<Literal>, rule_type: RuleType) -> Self {
        Self {
            literals,
            rule_type,
            id: 0,
            source_package: None,
            target_name: None,
            constraint: None,
            disabled: false,
            locked_package: None,
        }
    }

    /// Create an assertion rule (single literal that must be true)
    pub fn assertion(literal: Literal, rule_type: RuleType) -> Self {
        Self::new(vec![literal], rule_type)
    }

    /// Create a requirement rule: if source is installed, one of targets must be
    pub fn requires(source: PackageId, targets: Vec<PackageId>) -> Self {
        let mut literals = vec![-source];
        literals.extend(targets);
        Self::new(literals, RuleType::PackageRequires)
    }

    /// Create a conflict rule: these packages cannot all be installed together
    pub fn conflict(packages: Vec<PackageId>) -> Self {
        let literals: Vec<_> = packages.into_iter().map(|p| -p).collect();
        Self::new(literals, RuleType::PackageConflict)
    }

    /// Create a same-name rule: only one of these versions can be installed (binary conflict)
    pub fn same_name(packages: Vec<PackageId>) -> Self {
        let literals: Vec<_> = packages.into_iter().map(|p| -p).collect();
        Self::new(literals, RuleType::PackageSameName)
    }

    /// Create a multi-conflict rule: at most one of these packages can be installed
    /// This is more efficient than O(n²) binary conflicts for packages with many versions.
    /// The rule watches all literals and triggers when any becomes true.
    pub fn multi_conflict(packages: Vec<PackageId>) -> Self {
        let literals: Vec<_> = packages.into_iter().map(|p| -p).collect();
        Self::new(literals, RuleType::MultiConflict)
    }

    /// Check if this is a multi-conflict rule
    pub fn is_multi_conflict(&self) -> bool {
        self.rule_type.is_multi_conflict()
    }

    /// Create a root requirement rule
    pub fn root_require(targets: Vec<PackageId>) -> Self {
        Self::new(targets, RuleType::RootRequire)
    }

    /// Create a fixed package rule
    pub fn fixed(package: PackageId) -> Self {
        Self::assertion(package, RuleType::Fixed)
    }

    /// Create an unsatisfiable rule for a policy-removed locked package.
    pub fn locked_filter_list_removed(package: Arc<Package>) -> Self {
        Self {
            target_name: Some(package.name.clone()),
            locked_package: Some(package),
            ..Self::new(Vec::new(), RuleType::LockedFilterListRemoved)
        }
    }

    /// Create a learned rule from conflict analysis
    pub fn learned(literals: Vec<Literal>) -> Self {
        Self::new(literals, RuleType::Learned)
    }

    /// Set the rule ID
    pub fn set_id(&mut self, id: u32) {
        self.id = id;
    }

    /// Get the rule ID
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Set source package for error messages
    pub fn with_source(mut self, package: PackageId) -> Self {
        self.source_package = Some(package);
        self
    }

    /// Set target name for error messages
    pub fn with_target(mut self, name: impl AsRef<str>) -> Self {
        self.target_name = Some(name.as_ref().to_owned());
        self
    }

    /// Set constraint for error messages
    pub fn with_constraint(mut self, constraint: impl AsRef<str>) -> Self {
        self.constraint = Some(constraint.as_ref().to_owned());
        self
    }

    /// Get the rule type
    pub fn rule_type(&self) -> RuleType {
        self.rule_type
    }

    /// Get the literals
    pub fn literals(&self) -> &[Literal] {
        &self.literals
    }

    /// Get a mutable reference to literals
    pub fn literals_mut(&mut self) -> &mut Vec<Literal> {
        &mut self.literals
    }

    /// Get source package ID
    pub fn source_package(&self) -> Option<PackageId> {
        self.source_package
    }

    /// Get target name
    pub fn target_name(&self) -> Option<&str> {
        self.target_name.as_deref()
    }

    /// Get constraint
    pub fn constraint(&self) -> Option<&str> {
        self.constraint.as_deref()
    }

    /// Package name whose availability this rule requires.
    pub fn required_package(&self) -> Option<&str> {
        self.locked_package
            .as_ref()
            .map(|package| package.name.as_str())
            .or(self.target_name.as_deref())
    }

    pub(crate) fn locked_package(&self) -> Option<&Arc<Package>> {
        self.locked_package.as_ref()
    }

    /// Composer-style diagnostic for this rule.
    pub fn pretty_string(&self, pool: &Pool) -> String {
        match self.rule_type {
            RuleType::LockedFilterListRemoved => self.locked_package.as_ref().map_or_else(
                || "A locked package was removed by dependency policy and cannot be installed."
                    .to_owned(),
                |package| {
                    format!(
                        "{} {} was removed by a dependency policy (e.g. malware) and cannot be installed.",
                        package.name,
                        package.pretty_version()
                    )
                },
            ),
            RuleType::PackageRequires => {
                let source = self
                    .source_package
                    .and_then(|id| pool.entry(id))
                    .map(|entry| format!("{} {}", entry.name(), entry.pretty_version()))
                    .unwrap_or_else(|| "unknown package".to_owned());
                let target = self.target_name.as_deref().unwrap_or("unknown");
                let constraint = self.constraint.as_deref().unwrap_or("*");
                let versions = pool
                    .what_provides(target, Some(constraint))
                    .into_iter()
                    .filter_map(|id| pool.entry(id))
                    .map(|entry| entry.pretty_version().to_owned())
                    .collect::<Vec<_>>();
                if versions.is_empty() {
                    format!("{source} relates to {target} {constraint}.")
                } else {
                    format!(
                        "{source} relates to {target} {constraint} -> satisfiable by {target}[{}].",
                        versions.join(", ")
                    )
                }
            }
            RuleType::RootRequire if self.literals.is_empty() => format!(
                "No package found to satisfy root composer.json require {}",
                self.target_name.as_deref().unwrap_or("unknown")
            ),
            _ => self.to_string(),
        }
    }

    /// Check if this is an assertion (single literal)
    pub fn is_assertion(&self) -> bool {
        self.literals.len() == 1
    }

    /// Check if this rule is disabled
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Disable this rule
    pub fn disable(&mut self) {
        self.disabled = true;
    }

    /// Enable this rule
    pub fn enable(&mut self) {
        self.disabled = false;
    }

    /// Get the number of literals
    pub fn len(&self) -> usize {
        self.literals.len()
    }

    /// Check if the rule is empty
    pub fn is_empty(&self) -> bool {
        self.literals.is_empty()
    }

    /// Get a hash of this rule's literals for deduplication
    pub fn literal_hash(&self) -> u64 {
        let fingerprint = self.literal_fingerprint();
        avalanche(
            fingerprint.sum
                ^ fingerprint.mixed_sum.rotate_left(29)
                ^ avalanche(fingerprint.len as u64),
        )
    }

    pub(crate) fn literal_fingerprint(&self) -> LiteralFingerprint {
        let mut fingerprint = LiteralFingerprint {
            sum: 0,
            mixed_sum: 0,
            len: self.literals.len(),
        };
        for &literal in &self.literals {
            let hash = avalanche((literal as i64 as u64) ^ 0x9e37_79b9_7f4a_7c15);
            fingerprint.sum = fingerprint.sum.wrapping_add(hash);
            fingerprint.mixed_sum = fingerprint.mixed_sum.wrapping_add(avalanche(hash));
        }
        fingerprint
    }

    /// Check if two rules have the same literals (regardless of order)
    pub fn equals_literals(&self, other: &Rule) -> bool {
        if self.literals == other.literals {
            return true;
        }
        if self.literals.len() != other.literals.len() {
            return false;
        }

        for (index, literal) in self.literals.iter().enumerate() {
            if self.literals[..index].contains(literal) {
                continue;
            }
            let own_count = self
                .literals
                .iter()
                .filter(|candidate| *candidate == literal)
                .count();
            let other_count = other
                .literals
                .iter()
                .filter(|candidate| *candidate == literal)
                .count();
            if own_count != other_count {
                return false;
            }
        }
        true
    }
}

impl fmt::Debug for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Rule({:?}, {:?})", self.rule_type, self.literals)
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let literals: Vec<String> = self
            .literals
            .iter()
            .map(|&l| {
                if l > 0 {
                    format!("+{}", l)
                } else {
                    format!("{}", l)
                }
            })
            .collect();

        write!(f, "({}) [{}]", self.rule_type_str(), literals.join(" | "))
    }
}

impl Rule {
    fn rule_type_str(&self) -> &'static str {
        match self.rule_type {
            RuleType::RootRequire => "root-require",
            RuleType::Fixed => "fixed",
            RuleType::LockedFilterListRemoved => "locked-filter-list-removed",
            RuleType::PackageRequires => "requires",
            RuleType::PackageConflict => "conflict",
            RuleType::PackageSameName => "same-name",
            RuleType::MultiConflict => "multi-conflict",
            RuleType::PackageAlias => "alias",
            RuleType::PackageInverseAlias => "inverse-alias",
            RuleType::Learned => "learned",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_rule_hash_is_stable_for_the_literal_multiset() {
        let first = Rule::new(vec![123, -7, 42], RuleType::RootRequire);
        let reordered = Rule::new(vec![42, 123, -7], RuleType::RootRequire);

        assert_eq!(first.literal_hash(), first.literal_hash());
        assert_eq!(first.literal_hash(), reordered.literal_hash());
    }

    #[test]
    fn composer_rule_equality_rejects_different_literals() {
        let first = Rule::new(vec![1, 2], RuleType::RootRequire);
        let second = Rule::new(vec![1, 3], RuleType::RootRequire);

        assert!(!first.equals_literals(&second));
    }

    #[test]
    fn composer_rule_equality_rejects_different_literal_counts() {
        let first = Rule::new(vec![1, 12], RuleType::RootRequire);
        let second = Rule::new(vec![1], RuleType::RootRequire);

        assert!(!first.equals_literals(&second));
    }

    #[test]
    fn composer_rule_equality_accepts_the_same_literals() {
        let first = Rule::new(vec![1, 12], RuleType::RootRequire);
        let second = Rule::new(vec![12, 1], RuleType::RootRequire);

        assert!(first.equals_literals(&second));
    }

    #[test]
    fn composer_rule_type_is_exposed_from_construction() {
        let rule = Rule::new(Vec::new(), RuleType::RootRequire);

        assert_eq!(rule.rule_type(), RuleType::RootRequire);
    }

    #[test]
    fn composer_rule_can_be_enabled_after_disabling() {
        let mut rule = Rule::new(Vec::new(), RuleType::RootRequire);
        rule.disable();
        rule.enable();

        assert!(!rule.is_disabled());
    }

    #[test]
    fn composer_rule_can_be_disabled_after_enabling() {
        let mut rule = Rule::new(Vec::new(), RuleType::RootRequire);
        rule.enable();
        rule.disable();

        assert!(rule.is_disabled());
    }

    #[test]
    fn composer_rule_assertion_has_one_literal() {
        assert!(!Rule::new(vec![1, 12], RuleType::RootRequire).is_assertion());
        assert!(Rule::new(vec![1], RuleType::RootRequire).is_assertion());
    }

    #[test]
    fn test_rule_assertion() {
        let rule = Rule::assertion(5, RuleType::Fixed);
        assert!(rule.is_assertion());
        assert_eq!(rule.literals(), &[5]);
    }

    #[test]
    fn test_rule_requires() {
        let rule = Rule::requires(1, vec![2, 3, 4]);
        assert_eq!(rule.literals(), &[-1, 2, 3, 4]);
        assert_eq!(rule.rule_type(), RuleType::PackageRequires);
    }

    #[test]
    fn test_rule_conflict() {
        let rule = Rule::conflict(vec![1, 2]);
        assert_eq!(rule.literals(), &[-1, -2]);
        assert_eq!(rule.rule_type(), RuleType::PackageConflict);
    }

    #[test]
    fn test_rule_same_name() {
        let rule = Rule::same_name(vec![1, 2, 3]);
        assert_eq!(rule.literals(), &[-1, -2, -3]);
        assert_eq!(rule.rule_type(), RuleType::PackageSameName);
    }

    #[test]
    fn test_rule_literal_hash() {
        let rule1 = Rule::new(vec![1, 2, 3], RuleType::PackageRequires);
        let rule2 = Rule::new(vec![3, 1, 2], RuleType::PackageRequires);
        let rule3 = Rule::new(vec![1, 2, 4], RuleType::PackageRequires);

        assert_eq!(rule1.literal_hash(), rule2.literal_hash());
        assert_ne!(rule1.literal_hash(), rule3.literal_hash());
        assert_eq!(rule1.literal_fingerprint(), rule2.literal_fingerprint());
        assert_ne!(rule1.literal_fingerprint(), rule3.literal_fingerprint());
    }

    #[test]
    fn test_rule_equals_literals() {
        let rule1 = Rule::new(vec![1, 2, 3], RuleType::PackageRequires);
        let rule2 = Rule::new(vec![3, 1, 2], RuleType::PackageConflict);
        let rule3 = Rule::new(vec![1, 2], RuleType::PackageRequires);

        assert!(rule1.equals_literals(&rule2));
        assert!(!rule1.equals_literals(&rule3));

        let duplicates = Rule::new(vec![1, 1, 2], RuleType::PackageRequires);
        let reordered = Rule::new(vec![2, 1, 1], RuleType::PackageConflict);
        let different_counts = Rule::new(vec![1, 2, 2], RuleType::PackageConflict);
        assert!(duplicates.equals_literals(&reordered));
        assert!(!duplicates.equals_literals(&different_counts));
    }

    #[test]
    fn test_rule_display() {
        let rule = Rule::requires(1, vec![2, 3]);
        let display = format!("{}", rule);
        assert!(display.contains("requires"));
    }

    #[test]
    fn composer_rule_pretty_string_describes_satisfying_packages() {
        let mut pool = Pool::new();
        let foo = pool.add_package(Package::new("foo", "2.1"));
        let baz = pool.add_package(Package::new("baz", "1.1"));
        let rule = Rule::new(vec![foo, -baz], RuleType::PackageRequires)
            .with_source(baz)
            .with_target("foo")
            .with_constraint("*");

        assert_eq!(
            rule.pretty_string(&pool),
            "baz 1.1 relates to foo * -> satisfiable by foo[2.1]."
        );
    }

    #[test]
    fn composer_rule_returns_the_required_policy_removed_package() {
        let package = Arc::new(Package::new("vendor/malware", "1.0"));
        let rule = Rule::locked_filter_list_removed(package);

        assert_eq!(rule.required_package(), Some("vendor/malware"));
    }

    #[test]
    fn composer_rule_pretty_string_describes_policy_removed_locked_packages() {
        let package = Arc::new(Package::new("vendor/malware", "1.0"));
        let rule = Rule::locked_filter_list_removed(package);

        assert_eq!(
            rule.pretty_string(&Pool::new()),
            "vendor/malware 1.0 was removed by a dependency policy (e.g. malware) and cannot be installed."
        );
    }
}
