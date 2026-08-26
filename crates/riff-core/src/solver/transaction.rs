use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::package::{AliasPackage, Package};

#[derive(Debug, Clone, Default)]
pub struct Transaction {
    /// Operations to perform
    pub operations: Vec<Operation>,
    /// Transactions generated from complete package sets are already ordered
    /// using Composer's dependency traversal. Manually assembled transactions
    /// still need the best-effort operation sorter.
    ordered: bool,
}

/// A single operation in a transaction
#[derive(Debug, Clone)]
pub enum Operation {
    /// Install a new package
    Install(Arc<Package>),
    /// Update a package from one version to another
    Update {
        from: Arc<Package>,
        to: Arc<Package>,
    },
    /// Reinstall the same package identity because its materialized contents changed.
    Reinstall(Arc<Package>),
    /// Remove a package
    Uninstall(Arc<Package>),
    /// Mark a package as not needed (but keep it)
    MarkUnneeded(Arc<Package>),
    /// Mark an alias as installed (the alias package itself is not installed,
    /// but requirements matching the alias version are satisfied)
    MarkAliasInstalled(Arc<AliasPackage>),
    /// Mark an alias as uninstalled
    MarkAliasUninstalled(Arc<AliasPackage>),
}

#[derive(Clone)]
enum ResultPackage {
    Package(Arc<Package>),
    Alias(Arc<AliasPackage>),
}

impl ResultPackage {
    fn name(&self) -> &str {
        match self {
            Self::Package(package) => package.name(),
            Self::Alias(alias) => alias.name(),
        }
    }

    fn version(&self) -> &str {
        match self {
            Self::Package(package) => package.version(),
            Self::Alias(alias) => alias.version(),
        }
    }

    fn is_alias(&self) -> bool {
        matches!(self, Self::Alias(_))
    }

    fn names(&self) -> Vec<String> {
        match self {
            Self::Package(package) => package.get_names(true),
            Self::Alias(alias) => alias.alias_of().get_names(true),
        }
    }
}

impl Transaction {
    /// Create a new empty transaction
    pub fn new() -> Self {
        Self {
            operations: Vec::new(),
            ordered: false,
        }
    }

    pub fn from_packages(
        present_packages: Vec<Arc<Package>>,
        result_packages: Vec<Arc<Package>>,
        result_aliases: Vec<Arc<AliasPackage>>,
    ) -> Self {
        Self::from_package_sets(
            present_packages,
            Vec::new(),
            result_packages,
            result_aliases,
        )
    }

    pub fn from_package_sets(
        present_packages: Vec<Arc<Package>>,
        present_aliases: Vec<Arc<AliasPackage>>,
        result_packages: Vec<Arc<Package>>,
        result_aliases: Vec<Arc<AliasPackage>>,
    ) -> Self {
        let mut tx = Self::new();
        tx.calculate_operations(
            present_packages,
            present_aliases,
            result_packages,
            result_aliases,
        );
        tx
    }

    fn calculate_operations(
        &mut self,
        present_packages: Vec<Arc<Package>>,
        present_aliases: Vec<Arc<AliasPackage>>,
        result_packages: Vec<Arc<Package>>,
        result_aliases: Vec<Arc<AliasPackage>>,
    ) {
        let mut present_package_map: HashMap<String, Arc<Package>> = HashMap::new();
        let mut remove_map: HashMap<String, Arc<Package>> = HashMap::new();
        let mut present_package_order = Vec::new();

        let mut present_alias_map: HashMap<String, Arc<AliasPackage>> = HashMap::new();
        let mut remove_alias_map: HashMap<String, Arc<AliasPackage>> = HashMap::new();

        for package in &present_packages {
            let name_lower = package.name.to_lowercase();
            present_package_map.insert(name_lower.clone(), package.clone());
            remove_map.insert(name_lower.clone(), package.clone());
            present_package_order.push(name_lower);
        }

        let mut present_alias_order = Vec::new();
        for alias in &present_aliases {
            let alias_key = format!("{}::{}", alias.name().to_lowercase(), alias.version());
            present_alias_map.insert(alias_key.clone(), alias.clone());
            remove_alias_map.insert(alias_key.clone(), alias.clone());
            present_alias_order.push(alias_key);
        }

        // Composer first sorts the complete result set by descending package
        // name (aliases before their base package), then walks it from roots in
        // dependency order. This stable DFS order is observable in install
        // output and is also needed for providers and aliases.
        let mut result: Vec<ResultPackage> = result_packages
            .iter()
            .cloned()
            .map(ResultPackage::Package)
            .chain(result_aliases.iter().cloned().map(ResultPackage::Alias))
            .collect();
        result.sort_by(|left, right| {
            right
                .name()
                .cmp(left.name())
                .then_with(|| match (left.is_alias(), right.is_alias()) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => right.version().cmp(left.version()),
                })
        });

        // A same-identity package does not produce an update operation, so its
        // installed dependency metadata remains authoritative for ordering.
        // This also mirrors Composer partial updates, where non-allowlisted
        // packages are traversed from the lock repository rather than refreshed
        // metadata for the same version.
        let result_requires: Vec<Vec<String>> = result
            .iter()
            .map(|candidate| match candidate {
                ResultPackage::Package(package) => present_package_map
                    .get(&package.name.to_lowercase())
                    .filter(|present| !self.needs_update(present, package))
                    .map_or(&package.require, |present| &present.require),
                ResultPackage::Alias(alias) => alias.require(),
            })
            .map(|requires| {
                requires
                    .keys()
                    .map(|required| required.as_str().to_lowercase())
                    .collect()
            })
            .collect();

        let mut packages_by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, package) in result.iter().enumerate() {
            for name in package.names() {
                packages_by_name
                    .entry(name.to_lowercase())
                    .or_default()
                    .push(index);
            }
        }

        let mut roots = vec![true; result.len()];
        for (index, _package) in result.iter().enumerate() {
            if !roots[index] {
                continue;
            }
            for required in &result_requires[index] {
                if let Some(providers) = packages_by_name.get(required) {
                    for &provider in providers {
                        if provider != index {
                            roots[provider] = false;
                        }
                    }
                }
            }
        }

        let mut stack: Vec<usize> = roots
            .iter()
            .enumerate()
            .filter_map(|(index, is_root)| is_root.then_some(index))
            .collect();
        let mut visited = HashSet::new();
        let mut processed = HashSet::new();
        while let Some(index) = stack.pop() {
            if processed.contains(&index) {
                continue;
            }

            if visited.insert(index) {
                stack.push(index);
                match &result[index] {
                    ResultPackage::Alias(alias) => {
                        if let Some(base_index) = result.iter().position(|candidate| {
                            matches!(candidate, ResultPackage::Package(package)
                                if Arc::ptr_eq(package, &alias.alias_of_arc()))
                        }) {
                            stack.push(base_index);
                        }
                    }
                    ResultPackage::Package(package) => {
                        let _ = package;
                        for required in &result_requires[index] {
                            if let Some(providers) = packages_by_name.get(required) {
                                stack.extend(providers.iter().copied());
                            }
                        }
                    }
                }
                continue;
            }

            processed.insert(index);
            match &result[index] {
                ResultPackage::Alias(alias) => {
                    let alias_key = format!("{}::{}", alias.name().to_lowercase(), alias.version());
                    if present_alias_map.contains_key(&alias_key) {
                        remove_alias_map.remove(&alias_key);
                    } else {
                        self.operations
                            .push(Operation::MarkAliasInstalled(alias.clone()));
                    }
                }
                ResultPackage::Package(package) => {
                    let name_lower = package.name.to_lowercase();
                    if let Some(present_pkg) = present_package_map.get(&name_lower) {
                        if self.needs_update(present_pkg, package) {
                            self.operations.push(Operation::Update {
                                from: present_pkg.clone(),
                                to: package.clone(),
                            });
                        }
                    } else {
                        self.operations.push(Operation::Install(package.clone()));
                    }
                    remove_map.remove(&name_lower);
                }
            }
        }

        // Composer prepends each removal while iterating the present set, so
        // remaining packages are removed in reverse input order.
        for name in present_package_order {
            if let Some(package) = remove_map.remove(&name) {
                self.operations.insert(0, Operation::Uninstall(package));
            }
        }
        for alias_key in present_alias_order {
            if let Some(alias) = remove_alias_map.remove(&alias_key) {
                self.operations.push(Operation::MarkAliasUninstalled(alias));
            }
        }

        self.move_plugins_to_front();
        self.move_uninstalls_to_front();
        self.ordered = true;
    }

    fn needs_update(&self, present: &Package, target: &Package) -> bool {
        if present.version != target.version {
            return true;
        }

        let present_dist_ref = present.dist.as_ref().and_then(|d| d.reference.as_ref());
        let target_dist_ref = target.dist.as_ref().and_then(|d| d.reference.as_ref());
        if present_dist_ref != target_dist_ref {
            return true;
        }

        let present_source_ref = present.source.as_ref().map(|s| &s.reference);
        let target_source_ref = target.source.as_ref().map(|s| &s.reference);
        if present_source_ref != target_source_ref {
            return true;
        }

        present.abandoned != target.abandoned
    }

    /// Move plugin installations to the front (after uninstalls).
    /// Plugins need to be installed before packages that depend on them.
    fn move_plugins_to_front(&mut self) {
        let mut downloads_modifying_plugins_no_deps = Vec::new();
        let mut downloads_modifying_plugins_with_deps = Vec::new();
        let mut downloads_modifying_plugin_requires: HashSet<String> = HashSet::new();
        let mut plugins_no_deps = Vec::new();
        let mut plugins_with_deps = Vec::new();
        let mut plugin_requires: HashSet<String> = HashSet::new();
        let mut operations: Vec<Option<Operation>> = self.operations.drain(..).map(Some).collect();

        // Composer scans in reverse so that requirements encountered after a
        // plugin are promoted with it while preserving their original order.
        for index in (0..operations.len()).rev() {
            let Some(op) = operations[index].as_ref() else {
                continue;
            };
            let package = match op {
                Operation::Install(pkg) => Some(pkg.clone()),
                Operation::Update { to, .. } => Some(to.clone()),
                Operation::Reinstall(pkg) => Some(pkg.clone()),
                _ => None,
            };

            if let Some(pkg) = package {
                let modifies_downloads = pkg.package_type == "composer-plugin"
                    && pkg
                        .extra
                        .as_ref()
                        .and_then(|extra| extra.get("plugin-modifies-downloads"))
                        .and_then(serde_json::Value::as_bool)
                        == Some(true);
                let names: HashSet<_> = pkg.get_names(true).into_iter().collect();
                let modifies_downloads_dependency =
                    !names.is_disjoint(&downloads_modifying_plugin_requires);

                let requires: Vec<_> = pkg
                    .require
                    .keys()
                    .filter(|required| !is_platform_package_name(required))
                    .map(|required| required.as_str().to_lowercase())
                    .collect();

                if modifies_downloads || modifies_downloads_dependency {
                    let operation = operations[index].take().expect("operation is present");
                    if modifies_downloads && requires.is_empty() {
                        downloads_modifying_plugins_no_deps.insert(0, operation);
                    } else {
                        downloads_modifying_plugin_requires.extend(requires);
                        downloads_modifying_plugins_with_deps.insert(0, operation);
                    }
                    continue;
                }

                let is_plugin = pkg.package_type == "composer-plugin"
                    || pkg.package_type == "composer-installer";

                // Check if this is a plugin or dependency of a plugin
                let is_plugin_dep = !names.is_disjoint(&plugin_requires);

                if is_plugin || is_plugin_dep {
                    let operation = operations[index].take().expect("operation is present");
                    if is_plugin && requires.is_empty() {
                        plugins_no_deps.insert(0, operation);
                    } else {
                        plugin_requires.extend(requires);
                        plugins_with_deps.insert(0, operation);
                    }
                }
            }
        }

        self.operations.extend(downloads_modifying_plugins_no_deps);
        self.operations
            .extend(downloads_modifying_plugins_with_deps);
        self.operations.extend(plugins_no_deps);
        self.operations.extend(plugins_with_deps);
        self.operations.extend(operations.into_iter().flatten());
    }

    /// Move uninstall operations to the front.
    fn move_uninstalls_to_front(&mut self) {
        let mut uninstalls = Vec::new();
        let mut others = Vec::new();

        for op in self.operations.drain(..) {
            match &op {
                Operation::Uninstall(_) | Operation::MarkAliasUninstalled(_) => {
                    uninstalls.push(op);
                }
                _ => others.push(op),
            }
        }

        self.operations.extend(uninstalls);
        self.operations.extend(others);
    }

    /// Add an install operation
    pub fn install(&mut self, package: Arc<Package>) {
        self.operations.push(Operation::Install(package));
        self.ordered = false;
    }

    /// Add an update operation
    pub fn update(&mut self, from: Arc<Package>, to: Arc<Package>) {
        self.operations.push(Operation::Update { from, to });
        self.ordered = false;
    }

    /// Add a same-version reinstall unless the transaction already changes the package.
    pub fn reinstall(&mut self, package: Arc<Package>) {
        let name = package.name.to_lowercase();
        let already_changed = self.operations.iter().any(|operation| match operation {
            Operation::Install(candidate)
            | Operation::Reinstall(candidate)
            | Operation::Uninstall(candidate) => candidate.name.to_lowercase() == name,
            Operation::Update { to, .. } => to.name.to_lowercase() == name,
            _ => false,
        });
        if !already_changed {
            self.operations.push(Operation::Reinstall(package));
            self.ordered = false;
        }
    }

    /// Drop dev-package updates that point at the already installed reference.
    ///
    /// Composer may still record repository metadata changes in the lock file,
    /// while its installation phase skips an update when the target omits a
    /// source/dist reference or repeats the currently installed one.
    pub fn skip_same_reference_dev_updates(&mut self) {
        self.operations.retain(|operation| {
            let Operation::Update { from, to } = operation else {
                return true;
            };
            if to.stability() != crate::package::Stability::Dev || to.version != from.version {
                return true;
            }

            let source_unchanged = to
                .source
                .as_ref()
                .map(|source| &source.reference)
                .is_none_or(|target| {
                    from.source
                        .as_ref()
                        .is_some_and(|source| &source.reference == target)
                });
            let dist_unchanged = to
                .dist
                .as_ref()
                .and_then(|dist| dist.reference.as_ref())
                .is_none_or(|target| {
                    from.dist.as_ref().and_then(|dist| dist.reference.as_ref()) == Some(target)
                });

            !(source_unchanged && dist_unchanged)
        });
    }

    /// Add an uninstall operation
    pub fn uninstall(&mut self, package: Arc<Package>) {
        self.operations.push(Operation::Uninstall(package));
        self.ordered = false;
    }

    /// Add a mark alias installed operation
    pub fn mark_alias_installed(&mut self, alias: Arc<AliasPackage>) {
        self.operations.push(Operation::MarkAliasInstalled(alias));
        self.ordered = false;
    }

    /// Add a mark alias uninstalled operation
    pub fn mark_alias_uninstalled(&mut self, alias: Arc<AliasPackage>) {
        self.operations.push(Operation::MarkAliasUninstalled(alias));
        self.ordered = false;
    }

    /// Check if the transaction is empty
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Get the number of operations
    pub fn len(&self) -> usize {
        self.operations.len()
    }

    /// Get all packages that will be installed (including updates)
    pub fn installs(&self) -> impl Iterator<Item = &Arc<Package>> {
        self.operations.iter().filter_map(|op| match op {
            Operation::Install(pkg) => Some(pkg),
            Operation::Update { to, .. } => Some(to),
            Operation::Reinstall(pkg) => Some(pkg),
            _ => None,
        })
    }

    /// Get all packages that will be removed (including updates)
    pub fn uninstalls(&self) -> impl Iterator<Item = &Arc<Package>> {
        self.operations.iter().filter_map(|op| match op {
            Operation::Uninstall(pkg) => Some(pkg),
            Operation::Update { from, .. } => Some(from),
            Operation::Reinstall(pkg) => Some(pkg),
            _ => None,
        })
    }

    /// Get only new installs (not updates)
    pub fn new_installs(&self) -> impl Iterator<Item = &Arc<Package>> {
        self.operations.iter().filter_map(|op| match op {
            Operation::Install(pkg) => Some(pkg),
            _ => None,
        })
    }

    /// Get only updates
    pub fn updates(&self) -> impl Iterator<Item = (&Arc<Package>, &Arc<Package>)> {
        self.operations.iter().filter_map(|op| match op {
            Operation::Update { from, to } => Some((from, to)),
            _ => None,
        })
    }

    /// Get only same-version reinstalls.
    pub fn reinstalls(&self) -> impl Iterator<Item = &Arc<Package>> {
        self.operations
            .iter()
            .filter_map(|operation| match operation {
                Operation::Reinstall(package) => Some(package),
                _ => None,
            })
    }

    /// Get only removals (not updates)
    pub fn removals(&self) -> impl Iterator<Item = &Arc<Package>> {
        self.operations.iter().filter_map(|op| match op {
            Operation::Uninstall(pkg) => Some(pkg),
            _ => None,
        })
    }

    /// Sort operations for proper execution order.
    /// Uninstalls first, then installs (sorted by dependencies).
    pub fn sort(&mut self) {
        if self.ordered {
            return;
        }

        // Separate operations by type
        let mut uninstalls: Vec<Operation> = Vec::new();
        let mut installs_updates: Vec<Operation> = Vec::new();
        let mut mark_unneeded: Vec<Operation> = Vec::new();
        let mut alias_installs: Vec<Operation> = Vec::new();
        let mut alias_uninstalls: Vec<Operation> = Vec::new();

        for op in self.operations.drain(..) {
            match &op {
                Operation::Uninstall(_) => uninstalls.push(op),
                Operation::Update { .. } | Operation::Reinstall(_) | Operation::Install(_) => {
                    installs_updates.push(op)
                }
                Operation::MarkUnneeded(_) => mark_unneeded.push(op),
                Operation::MarkAliasInstalled(_) => alias_installs.push(op),
                Operation::MarkAliasUninstalled(_) => alias_uninstalls.push(op),
            }
        }

        // Sort all materialization operations together so a newly installed
        // dependency can precede an update of the package that requires it.
        let sorted_installs_updates = topological_sort_operations(installs_updates);

        // Reconstruct operations: uninstalls first, then dependency-ordered
        // materializations, alias ops, and finally mark-unneeded operations.
        self.operations.extend(uninstalls);
        self.operations.extend(sorted_installs_updates);
        for alias in alias_uninstalls {
            insert_alias_after_base(&mut self.operations, alias, false);
        }
        for alias in alias_installs {
            insert_alias_after_base(&mut self.operations, alias, true);
        }
        self.operations.extend(mark_unneeded);
        self.move_plugins_to_front();
        self.move_uninstalls_to_front();
        self.ordered = true;
    }

    /// Get a summary of the transaction
    pub fn summary(&self) -> TransactionSummary {
        let mut summary = TransactionSummary::default();

        for op in &self.operations {
            match op {
                Operation::Install(_) => summary.installs += 1,
                Operation::Update { .. } => summary.updates += 1,
                Operation::Reinstall(_) => summary.reinstalls += 1,
                Operation::Uninstall(_) => summary.uninstalls += 1,
                Operation::MarkUnneeded(_) => summary.mark_unneeded += 1,
                Operation::MarkAliasInstalled(_) => summary.alias_installs += 1,
                Operation::MarkAliasUninstalled(_) => summary.alias_uninstalls += 1,
            }
        }

        summary
    }

    /// Get all alias packages that will be marked as installed
    pub fn alias_installs(&self) -> impl Iterator<Item = &Arc<AliasPackage>> {
        self.operations.iter().filter_map(|op| match op {
            Operation::MarkAliasInstalled(alias) => Some(alias),
            _ => None,
        })
    }
}

fn insert_alias_after_base(operations: &mut Vec<Operation>, operation: Operation, installed: bool) {
    let alias = match &operation {
        Operation::MarkAliasInstalled(alias) | Operation::MarkAliasUninstalled(alias) => alias,
        _ => return,
    };
    let base_name = alias.name();
    let base_version = alias.alias_of().version.as_str();
    let position = operations.iter().rposition(|candidate| match candidate {
        Operation::Install(package) | Operation::Reinstall(package) if installed => {
            package.name.eq_ignore_ascii_case(base_name) && package.version == base_version
        }
        Operation::Update { to, .. } if installed => {
            to.name.eq_ignore_ascii_case(base_name) && to.version == base_version
        }
        Operation::Uninstall(package) if !installed => {
            package.name.eq_ignore_ascii_case(base_name) && package.version == base_version
        }
        _ => false,
    });
    if let Some(position) = position {
        operations.insert(position + 1, operation);
    } else {
        operations.push(operation);
    }
}

fn is_platform_package_name(name: &str) -> bool {
    let name = name.to_lowercase();
    name == "php"
        || name == "hhvm"
        || name == "composer"
        || name == "composer-runtime-api"
        || name == "composer-plugin-api"
        || name.starts_with("ext-")
        || name.starts_with("lib-")
}

/// Summary of a transaction
#[derive(Debug, Clone, Default)]
pub struct TransactionSummary {
    pub installs: usize,
    pub updates: usize,
    pub reinstalls: usize,
    pub uninstalls: usize,
    pub mark_unneeded: usize,
    pub alias_installs: usize,
    pub alias_uninstalls: usize,
}

impl std::fmt::Display for TransactionSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();

        if self.installs > 0 {
            parts.push(format!("{} install(s)", self.installs));
        }
        if self.updates > 0 {
            parts.push(format!("{} update(s)", self.updates));
        }
        if self.reinstalls > 0 {
            parts.push(format!("{} reinstall(s)", self.reinstalls));
        }
        if self.uninstalls > 0 {
            parts.push(format!("{} removal(s)", self.uninstalls));
        }

        if parts.is_empty() {
            write!(f, "Nothing to do")
        } else {
            write!(f, "{}", parts.join(", "))
        }
    }
}

/// Sort operations using topological sort based on package dependencies.
/// Dependencies are installed before the packages that depend on them.
fn topological_sort_operations(operations: Vec<Operation>) -> Vec<Operation> {
    if operations.is_empty() {
        return operations;
    }

    // Build a map of package name -> operation index
    let mut name_to_index: HashMap<String, usize> = HashMap::new();
    let mut packages: Vec<Arc<Package>> = Vec::new();

    for (idx, op) in operations.iter().enumerate() {
        let pkg = match op {
            Operation::Install(p) => p.clone(),
            Operation::Update { to, .. } => to.clone(),
            Operation::Reinstall(p) => p.clone(),
            _ => continue,
        };
        name_to_index.insert(pkg.name.to_lowercase(), idx);
        packages.push(pkg);
    }

    // Build adjacency list for dependencies
    // If A depends on B, then edge: A -> B (B must be installed before A)
    let mut in_degree: Vec<usize> = vec![0; operations.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); operations.len()];

    for (idx, pkg) in packages.iter().enumerate() {
        for (dep_name, _) in &pkg.require {
            let dep_lower = dep_name.as_str().to_lowercase();
            // Skip platform requirements
            if dep_lower == "php" || dep_lower.starts_with("ext-") || dep_lower.starts_with("lib-")
            {
                continue;
            }
            if let Some(&dep_idx) = name_to_index.get(&dep_lower) {
                // pkg depends on dep, so dep must be installed first
                // Edge: dep_idx -> idx (when dep is installed, it unblocks idx)
                dependents[dep_idx].push(idx);
                in_degree[idx] += 1;
            }
        }
    }

    // Kahn's algorithm for topological sort
    let mut queue: VecDeque<usize> = VecDeque::new();
    let mut result: Vec<usize> = Vec::new();

    // Start with packages that have no dependencies (in the transaction)
    for (idx, &degree) in in_degree.iter().enumerate() {
        if degree == 0 {
            queue.push_back(idx);
        }
    }

    while let Some(idx) = queue.pop_front() {
        result.push(idx);

        for &dependent_idx in &dependents[idx] {
            in_degree[dependent_idx] -= 1;
            if in_degree[dependent_idx] == 0 {
                queue.push_back(dependent_idx);
            }
        }
    }

    // If there's a cycle (result.len() != operations.len()), append remaining items
    // This shouldn't happen with valid dependency resolution, but handle gracefully
    if result.len() != operations.len() {
        let in_result: HashSet<usize> = result.iter().copied().collect();
        for idx in 0..operations.len() {
            if !in_result.contains(&idx) {
                result.push(idx);
            }
        }
    }

    // Reorder operations according to topological order
    let operations_vec: Vec<Operation> = operations;
    result
        .into_iter()
        .map(|idx| operations_vec[idx].clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transaction_new() {
        let tx = Transaction::new();
        assert!(tx.is_empty());
        assert_eq!(tx.len(), 0);
    }

    #[test]
    fn test_transaction_install() {
        let mut tx = Transaction::new();
        let pkg = Arc::new(Package::new("vendor/package", "1.0.0"));
        tx.install(pkg.clone());

        assert_eq!(tx.len(), 1);
        assert_eq!(tx.new_installs().count(), 1);
    }

    #[test]
    fn test_transaction_update() {
        let mut tx = Transaction::new();
        let from = Arc::new(Package::new("vendor/package", "1.0.0"));
        let to = Arc::new(Package::new("vendor/package", "2.0.0"));
        tx.update(from.clone(), to.clone());

        assert_eq!(tx.updates().count(), 1);
        assert_eq!(tx.installs().count(), 1); // Update counts as install
        assert_eq!(tx.uninstalls().count(), 1); // And uninstall
    }

    #[test]
    fn reinstall_is_added_only_when_package_is_otherwise_unchanged() {
        let package = Arc::new(Package::new("vendor/package", "1.0.0"));
        let mut tx =
            Transaction::from_packages(vec![package.clone()], vec![package.clone()], Vec::new());
        tx.reinstall(package.clone());
        tx.reinstall(package);
        assert_eq!(tx.reinstalls().count(), 1);
        assert_eq!(tx.summary().reinstalls, 1);
    }

    #[test]
    fn test_transaction_summary() {
        let mut tx = Transaction::new();
        tx.install(Arc::new(Package::new("a", "1.0.0")));
        tx.install(Arc::new(Package::new("b", "1.0.0")));
        tx.uninstall(Arc::new(Package::new("c", "1.0.0")));

        let summary = tx.summary();
        assert_eq!(summary.installs, 2);
        assert_eq!(summary.uninstalls, 1);
        assert_eq!(summary.updates, 0);
    }

    #[test]
    fn test_transaction_sort() {
        let mut tx = Transaction::new();
        tx.install(Arc::new(Package::new("a", "1.0.0")));
        tx.uninstall(Arc::new(Package::new("b", "1.0.0")));
        tx.install(Arc::new(Package::new("c", "1.0.0")));

        tx.sort();

        // Uninstalls should come first
        assert!(matches!(tx.operations[0], Operation::Uninstall(_)));
    }

    #[test]
    fn test_transaction_sort_by_dependencies() {
        let mut tx = Transaction::new();

        // Package c depends on b, b depends on a
        // Expected install order: a, b, c
        let pkg_a = Package::new("vendor/a", "1.0.0");
        let mut pkg_b = Package::new("vendor/b", "1.0.0");
        pkg_b
            .require
            .insert("vendor/a".to_string(), "^1.0".to_string());
        let mut pkg_c = Package::new("vendor/c", "1.0.0");
        pkg_c
            .require
            .insert("vendor/b".to_string(), "^1.0".to_string());

        // Add in wrong order
        tx.install(Arc::new(pkg_c));
        tx.install(Arc::new(pkg_a));
        tx.install(Arc::new(pkg_b));

        tx.sort();

        // Check that installs are in dependency order
        let install_names: Vec<String> = tx
            .operations
            .iter()
            .filter_map(|op| match op {
                Operation::Install(p) => Some(p.name.clone()),
                _ => None,
            })
            .collect();

        // a should come before b, b should come before c
        let a_pos = install_names.iter().position(|n| n == "vendor/a").unwrap();
        let b_pos = install_names.iter().position(|n| n == "vendor/b").unwrap();
        let c_pos = install_names.iter().position(|n| n == "vendor/c").unwrap();

        assert!(a_pos < b_pos, "a should be installed before b");
        assert!(b_pos < c_pos, "b should be installed before c");
    }

    #[test]
    fn test_transaction_sorts_new_dependency_before_dependent_update() {
        let dependency = Arc::new(Package::new("vendor/dependency", "1.0.0"));
        let from = Arc::new(Package::new("vendor/package", "1.0.0"));
        let mut target = Package::new("vendor/package", "2.0.0");
        target
            .require
            .insert("vendor/dependency".to_string(), "^1.0".to_string());

        let mut transaction = Transaction::new();
        transaction.update(from, Arc::new(target));
        transaction.install(dependency);
        transaction.sort();

        assert!(matches!(
            &transaction.operations[..],
            [Operation::Install(dependency), Operation::Update { to, .. }]
                if dependency.name == "vendor/dependency" && to.name == "vendor/package"
        ));
    }

    #[test]
    fn test_transaction_sort_uninstalls_before_installs() {
        let mut tx = Transaction::new();

        tx.install(Arc::new(Package::new("vendor/new", "1.0.0")));
        tx.uninstall(Arc::new(Package::new("vendor/old", "1.0.0")));
        tx.install(Arc::new(Package::new("vendor/another", "1.0.0")));

        tx.sort();

        // Find positions of first uninstall and first install
        let first_uninstall = tx
            .operations
            .iter()
            .position(|op| matches!(op, Operation::Uninstall(_)));
        let first_install = tx
            .operations
            .iter()
            .position(|op| matches!(op, Operation::Install(_)));

        assert!(
            first_uninstall.unwrap() < first_install.unwrap(),
            "Uninstalls should come before installs"
        );
    }

    #[test]
    fn test_transaction_from_packages_new_install() {
        // No present packages, one result package -> should generate Install operation
        let present = vec![];
        let result = vec![Arc::new(Package::new("vendor/a", "1.0.0"))];
        let aliases = vec![];

        let tx = Transaction::from_packages(present, result, aliases);

        assert_eq!(tx.new_installs().count(), 1);
        assert_eq!(tx.updates().count(), 0);
        assert_eq!(tx.removals().count(), 0);
    }

    #[test]
    fn test_transaction_from_packages_update() {
        // Present has v1.0.0, result has v2.0.0 -> should generate Update operation
        let present = vec![Arc::new(Package::new("vendor/a", "1.0.0"))];
        let result = vec![Arc::new(Package::new("vendor/a", "2.0.0"))];
        let aliases = vec![];

        let tx = Transaction::from_packages(present, result, aliases);

        assert_eq!(tx.new_installs().count(), 0);
        assert_eq!(tx.updates().count(), 1);
        assert_eq!(tx.removals().count(), 0);
    }

    #[test]
    fn test_transaction_from_packages_no_change() {
        // Same package version -> should generate no operations
        let present = vec![Arc::new(Package::new("vendor/a", "1.0.0"))];
        let result = vec![Arc::new(Package::new("vendor/a", "1.0.0"))];
        let aliases = vec![];

        let tx = Transaction::from_packages(present, result, aliases);

        assert_eq!(tx.new_installs().count(), 0);
        assert_eq!(tx.updates().count(), 0);
        assert_eq!(tx.removals().count(), 0);
    }

    #[test]
    fn test_transaction_from_packages_uninstall() {
        // Present has a package, result doesn't -> should generate Uninstall operation
        let present = vec![Arc::new(Package::new("vendor/a", "1.0.0"))];
        let result = vec![];
        let aliases = vec![];

        let tx = Transaction::from_packages(present, result, aliases);

        assert_eq!(tx.new_installs().count(), 0);
        assert_eq!(tx.updates().count(), 0);
        assert_eq!(tx.removals().count(), 1);
    }

    #[test]
    fn transaction_keeps_aliases_in_composer_dfs_order() {
        let dep = Arc::new(Package::new("current/dep", "dev-master"));
        let old_dep2 = Arc::new(Package::new("current/dep2", "dev-foo"));
        let new_dep2 = Arc::new(Package::new("current/dep2", "dev-master"));
        let mut new_package = Package::new("new/pkg", "1.0.0");
        new_package
            .require
            .insert("current/dep".to_string(), "^1.1".to_string());
        new_package
            .require
            .insert("current/dep2".to_string(), "^1.1".to_string());
        let mut current_package = Package::new("current/pkg", "1.0.0");
        current_package
            .require
            .insert("current/dep".to_string(), "<1.2.0".to_string());
        let current_package = Arc::new(current_package);

        let old_alias = Arc::new(AliasPackage::new(
            old_dep2.clone(),
            "1.0.x-dev".to_string(),
            "1.0.x-dev".to_string(),
        ));
        let dep_branch_alias = Arc::new(AliasPackage::new(
            dep.clone(),
            "1.0.x-dev".to_string(),
            "1.0.x-dev".to_string(),
        ));
        let mut dep_root_alias =
            AliasPackage::new(dep.clone(), "1.1.0.0".to_string(), "1.1.0".to_string());
        dep_root_alias.set_root_package_alias(true);
        let mut dep2_root_alias =
            AliasPackage::new(new_dep2.clone(), "1.1.2.0".to_string(), "1.1.2".to_string());
        dep2_root_alias.set_root_package_alias(true);
        let dep2_branch_alias = Arc::new(AliasPackage::new(
            new_dep2.clone(),
            "2.9999999.9999999-dev".to_string(),
            "2.x-dev".to_string(),
        ));

        let transaction = Transaction::from_package_sets(
            vec![dep.clone(), old_dep2, current_package.clone()],
            vec![dep_branch_alias.clone(), old_alias],
            vec![dep, new_dep2, current_package, Arc::new(new_package)],
            vec![
                Arc::new(dep_root_alias),
                dep_branch_alias,
                Arc::new(dep2_root_alias),
                dep2_branch_alias,
            ],
        );
        let operations: Vec<_> = transaction
            .operations
            .iter()
            .map(|operation| match operation {
                Operation::Update { to, .. } => format!("update:{}", to.name),
                Operation::Install(package) => format!("install:{}", package.name),
                Operation::MarkAliasInstalled(alias) => {
                    format!("alias+:{}:{}", alias.name(), alias.pretty_version())
                }
                Operation::MarkAliasUninstalled(alias) => {
                    format!("alias-:{}:{}", alias.name(), alias.pretty_version())
                }
                _ => unreachable!("unexpected operation"),
            })
            .collect();

        assert_eq!(
            operations,
            [
                "alias-:current/dep2:1.0.x-dev",
                "alias+:current/dep:1.1.0",
                "update:current/dep2",
                "alias+:current/dep2:1.1.2",
                "alias+:current/dep2:2.x-dev",
                "install:new/pkg",
            ]
        );
    }

    #[test]
    fn composer_transaction_generation_and_sorting() {
        // Ported from Composer's DependencyResolver/TransactionTest.php.
        let package_a = Arc::new(Package::new("a/a", "dev-master"));
        let package_a_alias = Arc::new(AliasPackage::new(
            package_a.clone(),
            "1.0.x-dev".to_string(),
            "1.0.x-dev".to_string(),
        ));
        let package_b = Arc::new(Package::new("b/b", "1.0.0"));
        let package_e = Arc::new(Package::new("e/e", "dev-foo"));
        let package_e_alias = Arc::new(AliasPackage::new(
            package_e.clone(),
            "1.0.x-dev".to_string(),
            "1.0.x-dev".to_string(),
        ));
        let package_c = Arc::new(Package::new("c/c", "1.0.0"));

        let package_b_new = Arc::new(Package::new("b/b", "2.1.3"));
        let mut package_d = Package::new("d/d", "1.2.3");
        package_d
            .require
            .insert("f/f".to_string(), ">0.2".to_string());
        package_d
            .require
            .insert("g/provider".to_string(), ">0.2".to_string());
        let package_d = Arc::new(package_d);
        let package_f = Arc::new(Package::new("f/f", "1.0.0"));
        let package_f_alias_1 = Arc::new(AliasPackage::new(
            package_f.clone(),
            "dev-foo".to_string(),
            "dev-foo".to_string(),
        ));
        let package_f_alias_2 = Arc::new(AliasPackage::new(
            package_f.clone(),
            "dev-bar".to_string(),
            "dev-bar".to_string(),
        ));
        let mut package_g = Package::new("g/g", "1.0.0");
        package_g
            .provide
            .insert("g/provider".to_string(), "1.0.0".to_string());
        let package_g = Arc::new(package_g);
        let package_a0 = Arc::new(Package::new("a0/first", "1.2.3"));

        let mut plugin = Package::new("x/plugin", "1.0.0");
        plugin.package_type = "composer-installer".into();
        let plugin = Arc::new(plugin);
        let plugin_2_dependency = Arc::new(Package::new("x/plugin2-dep", "1.0.0"));
        let mut plugin_2 = Package::new("x/plugin2", "1.0.0");
        plugin_2.package_type = "composer-plugin".into();
        plugin_2
            .require
            .insert("x/plugin2-dep".to_string(), "1.0.0".to_string());
        let plugin_2 = Arc::new(plugin_2);
        let mut modifying_plugin = Package::new("x/downloads-modifying", "1.0.0");
        modifying_plugin.package_type = "composer-plugin".into();
        modifying_plugin.extra = Some(serde_json::json!({"plugin-modifies-downloads": true}));
        let modifying_plugin = Arc::new(modifying_plugin);
        let modifying_dependency = Arc::new(Package::new("x/downloads-modifying2-dep", "1.0.0"));
        let mut modifying_plugin_2 = Package::new("x/downloads-modifying2", "1.0.0");
        modifying_plugin_2.package_type = "composer-plugin".into();
        modifying_plugin_2.extra = Some(serde_json::json!({"plugin-modifies-downloads": true}));
        modifying_plugin_2.require.insert(
            "x/downloads-modifying2-dep".to_string(),
            "1.0.0".to_string(),
        );
        let modifying_plugin_2 = Arc::new(modifying_plugin_2);

        let mut transaction = Transaction::from_package_sets(
            vec![
                package_a.clone(),
                package_b.clone(),
                package_e.clone(),
                package_c.clone(),
            ],
            vec![package_a_alias.clone(), package_e_alias.clone()],
            vec![
                package_a,
                package_b_new.clone(),
                package_d.clone(),
                package_f.clone(),
                package_g.clone(),
                package_a0.clone(),
                plugin.clone(),
                plugin_2_dependency.clone(),
                plugin_2.clone(),
                modifying_plugin.clone(),
                modifying_dependency.clone(),
                modifying_plugin_2.clone(),
            ],
            vec![package_a_alias, package_f_alias_1, package_f_alias_2],
        );
        transaction.sort();

        let names: Vec<_> = transaction
            .operations
            .iter()
            .map(|operation| match operation {
                Operation::Install(package) => format!("install:{}", package.name),
                Operation::Update { to, .. } => format!("update:{}", to.name),
                Operation::Uninstall(package) => format!("uninstall:{}", package.name),
                Operation::MarkAliasInstalled(alias) => {
                    format!("alias+:{}:{}", alias.name(), alias.version())
                }
                Operation::MarkAliasUninstalled(alias) => {
                    format!("alias-:{}:{}", alias.name(), alias.version())
                }
                Operation::Reinstall(package) => format!("reinstall:{}", package.name),
                Operation::MarkUnneeded(package) => format!("unneeded:{}", package.name),
            })
            .collect();
        assert_eq!(
            names,
            [
                "uninstall:c/c",
                "uninstall:e/e",
                "alias-:e/e:1.0.x-dev",
                "install:x/downloads-modifying",
                "install:x/downloads-modifying2-dep",
                "install:x/downloads-modifying2",
                "install:x/plugin",
                "install:x/plugin2-dep",
                "install:x/plugin2",
                "install:a0/first",
                "update:b/b",
                "install:g/g",
                "install:f/f",
                "alias+:f/f:dev-bar",
                "alias+:f/f:dev-foo",
                "install:d/d",
            ]
        );
    }
}
