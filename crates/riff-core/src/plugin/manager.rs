use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use compact_str::CompactString;
use indexmap::IndexMap;

use crate::config::AllowPlugins;
use crate::event::{EventDispatcher, EventListener, EventType, RiffEvent};
use crate::json::RiffManifest;
use crate::package::Package;
use crate::riff::Riff;
use crate::runtime::RuntimeContext;
use crate::solver::Transaction;

use super::policy::PluginPolicy;

type VersionValidator = fn(&Package) -> Result<()>;

#[derive(Clone, Copy)]
pub(crate) struct PluginDescriptor {
    pub package: &'static str,
    pub validate_version: Option<VersionValidator>,
}

impl PluginDescriptor {
    pub(crate) const fn new(package: &'static str) -> Self {
        Self {
            package,
            validate_version: None,
        }
    }

    pub(crate) const fn with_version_validator(
        package: &'static str,
        validate_version: VersionValidator,
    ) -> Self {
        Self {
            package,
            validate_version: Some(validate_version),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageOperation {
    Require,
    Update,
}

#[async_trait]
pub(crate) trait PackageArgumentHook: Send + Sync {
    async fn transform_arguments(
        &self,
        riff: &Riff,
        operation: PackageOperation,
        arguments: &[String],
    ) -> Result<Vec<String>>;
}

#[async_trait]
pub(crate) trait RootManifestHook: Send + Sync {
    async fn transform_manifest(
        &self,
        riff: &mut Riff,
        operation: PackageOperation,
    ) -> Result<Vec<String>>;
}

pub(crate) trait SolverConstraintRule: Send + Sync {
    fn rewrite(&self, package: &str, constraint: &str) -> Option<String>;
}

#[async_trait]
pub(crate) trait SolverConstraintHook: Send + Sync {
    async fn prepare(&self, riff: &Riff) -> Result<Option<Box<dyn SolverConstraintRule>>>;
}

#[async_trait]
pub(crate) trait OperationHook: Send + Sync {
    async fn prepare(
        &self,
        riff: &Riff,
        transaction: &Transaction,
        desired_packages: &[Arc<Package>],
    ) -> Result<Option<Box<dyn PreparedPluginOperation>>>;
}

pub(crate) trait PreparedPluginOperation: Send {
    fn apply(self: Box<Self>, riff: &Riff) -> Result<()>;
}

pub(crate) struct ScriptPluginContext<'a> {
    pub manifest: &'a RiffManifest,
    pub working_dir: &'a Path,
    pub runtime: &'a RuntimeContext,
    pub output: &'a crate::output::Output,
}

pub(crate) trait ComposerCommandHook: Send + Sync {
    fn execute(
        &self,
        command: &str,
        extra_args: &[String],
        context: &ScriptPluginContext<'_>,
    ) -> Result<i32>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ObjectScriptAction {
    Execute { display: String, command: String },
    Warning(String),
}

pub(crate) trait ObjectScriptHook: Send + Sync {
    fn expand(
        &self,
        configuration: &IndexMap<String, serde_json::Value>,
        arguments: &[String],
        context: &ScriptPluginContext<'_>,
    ) -> Result<Option<Vec<ObjectScriptAction>>>;
}

pub(crate) trait PackageLayoutHook: Send + Sync {
    fn is_fileless(&self, package: &Package) -> bool;
}

struct EventRegistration {
    package: &'static str,
    event_type: EventType,
    listener: Arc<dyn EventListener>,
}

struct PackageArgumentRegistration {
    package: &'static str,
    hook: Arc<dyn PackageArgumentHook>,
}

struct RootManifestRegistration {
    package: &'static str,
    hook: Arc<dyn RootManifestHook>,
}

struct SolverConstraintRegistration {
    package: &'static str,
    hook: Arc<dyn SolverConstraintHook>,
}

struct OperationRegistration {
    package: &'static str,
    hook: Arc<dyn OperationHook>,
}

struct ComposerCommandRegistration {
    package: &'static str,
    command: &'static str,
    hook: Arc<dyn ComposerCommandHook>,
}

struct ObjectScriptRegistration {
    package: &'static str,
    script: &'static str,
    hook: Arc<dyn ObjectScriptHook>,
}

struct PackageLayoutRegistration {
    package: &'static str,
    hook: Arc<dyn PackageLayoutHook>,
}

#[derive(Default)]
pub(crate) struct PluginRegistrar {
    descriptors: Vec<PluginDescriptor>,
    events: Vec<EventRegistration>,
    package_arguments: Vec<PackageArgumentRegistration>,
    root_manifests: Vec<RootManifestRegistration>,
    solver_constraints: Vec<SolverConstraintRegistration>,
    operations: Vec<OperationRegistration>,
    composer_commands: Vec<ComposerCommandRegistration>,
    object_scripts: Vec<ObjectScriptRegistration>,
    package_layouts: Vec<PackageLayoutRegistration>,
}

impl PluginRegistrar {
    pub(crate) fn descriptor(&mut self, descriptor: PluginDescriptor) {
        self.descriptors.push(descriptor);
    }

    pub(crate) fn event(
        &mut self,
        package: &'static str,
        event_type: EventType,
        listener: Arc<dyn EventListener>,
    ) {
        self.events.push(EventRegistration {
            package,
            event_type,
            listener,
        });
    }

    pub(crate) fn package_arguments(
        &mut self,
        package: &'static str,
        hook: Arc<dyn PackageArgumentHook>,
    ) {
        self.package_arguments
            .push(PackageArgumentRegistration { package, hook });
    }

    pub(crate) fn root_manifest(&mut self, package: &'static str, hook: Arc<dyn RootManifestHook>) {
        self.root_manifests
            .push(RootManifestRegistration { package, hook });
    }

    pub(crate) fn solver_constraints(
        &mut self,
        package: &'static str,
        hook: Arc<dyn SolverConstraintHook>,
    ) {
        self.solver_constraints
            .push(SolverConstraintRegistration { package, hook });
    }

    pub(crate) fn operations(&mut self, package: &'static str, hook: Arc<dyn OperationHook>) {
        self.operations
            .push(OperationRegistration { package, hook });
    }

    pub(crate) fn composer_command(
        &mut self,
        package: &'static str,
        command: &'static str,
        hook: Arc<dyn ComposerCommandHook>,
    ) {
        self.composer_commands.push(ComposerCommandRegistration {
            package,
            command,
            hook,
        });
    }

    pub(crate) fn object_script(
        &mut self,
        package: &'static str,
        script: &'static str,
        hook: Arc<dyn ObjectScriptHook>,
    ) {
        self.object_scripts.push(ObjectScriptRegistration {
            package,
            script,
            hook,
        });
    }

    pub(crate) fn package_layout(
        &mut self,
        package: &'static str,
        hook: Arc<dyn PackageLayoutHook>,
    ) {
        self.package_layouts
            .push(PackageLayoutRegistration { package, hook });
    }
}

struct PluginManagerInner {
    policy: PluginPolicy,
    descriptors: HashMap<&'static str, PluginDescriptor>,
    events: Vec<EventRegistration>,
    package_arguments: Vec<PackageArgumentRegistration>,
    root_manifests: Vec<RootManifestRegistration>,
    solver_constraints: Vec<SolverConstraintRegistration>,
    operations: Vec<OperationRegistration>,
    composer_commands: Vec<ComposerCommandRegistration>,
    object_scripts: Vec<ObjectScriptRegistration>,
    package_layouts: Vec<PackageLayoutRegistration>,
}

/// Facade for Riff's compiled-in native Composer plugin adapters.
///
/// The capability traits are intentionally private so this type does not
/// promise a dynamically loadable Rust plugin API.
#[derive(Clone)]
pub struct PluginManager {
    inner: Arc<PluginManagerInner>,
}

impl PluginManager {
    pub fn builtins(enabled: bool, allow: AllowPlugins) -> Result<Self> {
        let mut registrar = PluginRegistrar::default();
        super::composer_bin::register(&mut registrar);
        super::composer_patches::register(&mut registrar);
        super::php_http_discovery::register(&mut registrar);
        super::phpstan_extension_installer::register(&mut registrar);
        super::symfony_flex::register(&mut registrar);
        super::symfony_runtime::register(&mut registrar);
        Self::from_registrar(PluginPolicy::new(enabled, allow), registrar)
    }

    fn from_registrar(policy: PluginPolicy, registrar: PluginRegistrar) -> Result<Self> {
        let mut descriptors = HashMap::new();
        for descriptor in registrar.descriptors {
            if descriptors.insert(descriptor.package, descriptor).is_some() {
                bail!(
                    "Native plugin '{}' was registered twice",
                    descriptor.package
                );
            }
        }
        let require_descriptor = |package: &str| -> Result<()> {
            if !descriptors.contains_key(package) {
                bail!("Native plugin capability references unknown package '{package}'");
            }
            Ok(())
        };
        for registration in &registrar.events {
            require_descriptor(registration.package)?;
        }
        for registration in &registrar.package_arguments {
            require_descriptor(registration.package)?;
        }
        for registration in &registrar.root_manifests {
            require_descriptor(registration.package)?;
        }
        for registration in &registrar.solver_constraints {
            require_descriptor(registration.package)?;
        }
        for registration in &registrar.operations {
            require_descriptor(registration.package)?;
        }
        for registration in &registrar.composer_commands {
            require_descriptor(registration.package)?;
        }
        for registration in &registrar.object_scripts {
            require_descriptor(registration.package)?;
        }
        for registration in &registrar.package_layouts {
            require_descriptor(registration.package)?;
        }
        for registration in &registrar.composer_commands {
            if registrar
                .composer_commands
                .iter()
                .filter(|candidate| candidate.command == registration.command)
                .count()
                > 1
            {
                bail!(
                    "Native plugins registered duplicate @composer handler '{}'",
                    registration.command
                );
            }
        }
        for registration in &registrar.object_scripts {
            if registrar
                .object_scripts
                .iter()
                .filter(|candidate| candidate.script == registration.script)
                .count()
                > 1
            {
                bail!(
                    "Native plugins registered duplicate object script handler '{}'",
                    registration.script
                );
            }
        }

        Ok(Self {
            inner: Arc::new(PluginManagerInner {
                policy,
                descriptors,
                events: registrar.events,
                package_arguments: registrar.package_arguments,
                root_manifests: registrar.root_manifests,
                solver_constraints: registrar.solver_constraints,
                operations: registrar.operations,
                composer_commands: registrar.composer_commands,
                object_scripts: registrar.object_scripts,
                package_layouts: registrar.package_layouts,
            }),
        })
    }

    pub fn is_enabled(&self, package: &str) -> bool {
        self.inner.policy.allows(package)
    }

    pub fn validate<'a>(&self, packages: impl IntoIterator<Item = &'a Package>) -> Result<()> {
        for package in packages {
            if !package.is_composer_plugin() || !self.is_enabled(&package.name) {
                continue;
            }
            let Some(descriptor) = self.inner.descriptors.get(package.name.as_str()) else {
                bail!(
                    "Composer plugin {} is enabled but cannot run in riff; disable it with config.allow-plugins or --no-plugins",
                    package.name
                );
            };
            if let Some(validate) = descriptor.validate_version {
                validate(package)?;
            }
        }
        Ok(())
    }

    pub async fn transform_package_arguments(
        &self,
        riff: &Riff,
        operation: PackageOperation,
        arguments: &[String],
    ) -> Result<Vec<String>> {
        let mut transformed = arguments.to_vec();
        for registration in &self.inner.package_arguments {
            if self.is_enabled(registration.package) {
                transformed = registration
                    .hook
                    .transform_arguments(riff, operation, &transformed)
                    .await?;
            }
        }
        Ok(transformed)
    }

    pub async fn transform_root_manifest(
        &self,
        riff: &mut Riff,
        operation: PackageOperation,
    ) -> Result<Vec<String>> {
        let mut messages = Vec::new();
        for registration in &self.inner.root_manifests {
            if self.is_enabled(registration.package) {
                messages.extend(
                    registration
                        .hook
                        .transform_manifest(riff, operation)
                        .await?,
                );
            }
        }
        Ok(messages)
    }

    pub(crate) async fn prepare_solver_constraints(
        &self,
        riff: &Riff,
    ) -> Result<SolverConstraintSet> {
        let mut rules = Vec::new();
        for registration in &self.inner.solver_constraints {
            if self.is_enabled(registration.package) {
                if let Some(rule) = registration.hook.prepare(riff).await? {
                    rules.push(rule);
                }
            }
        }
        Ok(SolverConstraintSet { rules })
    }

    pub(crate) async fn prepare_operations(
        &self,
        riff: &Riff,
        transaction: &Transaction,
        desired_packages: &[Arc<Package>],
    ) -> Result<PreparedPluginOperations> {
        let mut plans = Vec::new();
        for registration in &self.inner.operations {
            if self.is_enabled(registration.package) {
                if let Some(plan) = registration
                    .hook
                    .prepare(riff, transaction, desired_packages)
                    .await?
                {
                    plans.push(plan);
                }
            }
        }
        Ok(PreparedPluginOperations { plans })
    }

    pub(crate) fn register_events(&self, dispatcher: &mut EventDispatcher) {
        for registration in &self.inner.events {
            dispatcher.add_listener(
                registration.event_type,
                Arc::new(ManagedEventListener {
                    manager: self.clone(),
                    package: registration.package,
                    listener: registration.listener.clone(),
                }),
            );
        }
    }

    pub(crate) fn execute_composer_command(
        &self,
        command: &str,
        extra_args: &[String],
        context: &ScriptPluginContext<'_>,
    ) -> Result<Option<i32>> {
        let (name, remainder) = command
            .split_once(char::is_whitespace)
            .unwrap_or((command, ""));
        let Some(registration) = self.inner.composer_commands.iter().find(|registration| {
            registration.command == name && self.is_enabled(registration.package)
        }) else {
            return Ok(None);
        };
        Ok(Some(registration.hook.execute(
            remainder.trim_start(),
            extra_args,
            context,
        )?))
    }

    pub(crate) fn expand_object_script(
        &self,
        script: &str,
        configuration: &IndexMap<String, serde_json::Value>,
        arguments: &[String],
        context: &ScriptPluginContext<'_>,
    ) -> Result<Option<Vec<ObjectScriptAction>>> {
        let Some(registration) = self.inner.object_scripts.iter().find(|registration| {
            registration.script == script && self.is_enabled(registration.package)
        }) else {
            return Ok(None);
        };
        registration.hook.expand(configuration, arguments, context)
    }

    pub(crate) fn package_layouts<'a>(
        &self,
        packages: impl IntoIterator<Item = &'a Package>,
    ) -> PackageLayouts {
        let packages = packages.into_iter().collect::<Vec<_>>();
        let mut fileless = HashSet::new();
        for registration in &self.inner.package_layouts {
            let plugin_installed = packages
                .iter()
                .any(|package| package.name.eq_ignore_ascii_case(registration.package));
            if !plugin_installed || !self.is_enabled(registration.package) {
                continue;
            }
            fileless.extend(
                packages
                    .iter()
                    .filter(|package| registration.hook.is_fileless(package))
                    .map(|package| package.name.to_ascii_lowercase()),
            );
        }
        PackageLayouts { fileless }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PackageLayouts {
    fileless: HashSet<String>,
}

impl PackageLayouts {
    pub(crate) fn is_fileless(&self, package: &Package) -> bool {
        package.is_metapackage() || self.fileless.contains(package.name.as_str())
    }
}

pub(crate) struct SolverConstraintSet {
    rules: Vec<Box<dyn SolverConstraintRule>>,
}

impl SolverConstraintSet {
    pub(crate) fn rewrite(&self, package: &str, constraint: CompactString) -> CompactString {
        self.rules.iter().fold(constraint, |constraint, rule| {
            rule.rewrite(package, &constraint)
                .map(CompactString::new)
                .unwrap_or(constraint)
        })
    }
}

pub(crate) struct PreparedPluginOperations {
    plans: Vec<Box<dyn PreparedPluginOperation>>,
}

impl PreparedPluginOperations {
    pub(crate) fn apply(self, riff: &Riff) -> Result<()> {
        for plan in self.plans {
            plan.apply(riff)?;
        }
        Ok(())
    }
}

struct ManagedEventListener {
    manager: PluginManager,
    package: &'static str,
    listener: Arc<dyn EventListener>,
}

impl EventListener for ManagedEventListener {
    fn handle(&self, event: &dyn RiffEvent, riff: &Riff) -> Result<i32> {
        if !self.manager.is_enabled(self.package) {
            return Ok(0);
        }
        self.listener.handle(event, riff)
    }

    fn priority(&self) -> i32 {
        self.listener.priority()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Platform;
    use std::sync::Mutex;

    struct AppendArgument(&'static str);

    #[async_trait]
    impl PackageArgumentHook for AppendArgument {
        async fn transform_arguments(
            &self,
            _riff: &Riff,
            _operation: PackageOperation,
            arguments: &[String],
        ) -> Result<Vec<String>> {
            let mut arguments = arguments.to_vec();
            arguments.push(self.0.to_owned());
            Ok(arguments)
        }
    }

    struct NoopObjectScript;

    impl ObjectScriptHook for NoopObjectScript {
        fn expand(
            &self,
            _configuration: &IndexMap<String, serde_json::Value>,
            _arguments: &[String],
            _context: &ScriptPluginContext<'_>,
        ) -> Result<Option<Vec<ObjectScriptAction>>> {
            Ok(Some(Vec::new()))
        }
    }

    struct RecordedOperationHook {
        name: &'static str,
        fail: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    struct RecordedOperationPlan {
        name: &'static str,
        fail: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl OperationHook for RecordedOperationHook {
        async fn prepare(
            &self,
            _riff: &Riff,
            _transaction: &Transaction,
            _desired_packages: &[Arc<Package>],
        ) -> Result<Option<Box<dyn PreparedPluginOperation>>> {
            Ok(Some(Box::new(RecordedOperationPlan {
                name: self.name,
                fail: self.fail,
                calls: self.calls.clone(),
            })))
        }
    }

    impl PreparedPluginOperation for RecordedOperationPlan {
        fn apply(self: Box<Self>, _riff: &Riff) -> Result<()> {
            self.calls.lock().unwrap().push(self.name);
            if self.fail {
                anyhow::bail!("{} failed", self.name);
            }
            Ok(())
        }
    }

    fn plugin_package(name: &str, version: &str) -> Package {
        let mut package = Package::new(name, version);
        package.package_type = "composer-plugin".into();
        package
    }

    #[test]
    fn builtins_validate_supported_plugins_and_versions() {
        let manager = PluginManager::builtins(true, AllowPlugins::Bool(true)).unwrap();
        for (package, version) in [
            ("bamarni/composer-bin-plugin", "2.1.0.0"),
            ("cweagans/composer-patches", "2.0.0.0"),
            ("php-http/discovery", "1.20.0.0"),
            ("phpstan/extension-installer", "1.4.0.0"),
            ("symfony/flex", "2.11.0.0"),
            ("symfony/runtime", "7.4.0.0"),
        ] {
            assert!(manager
                .validate([&plugin_package(package, version)])
                .is_ok());
        }

        let error = manager
            .validate([&plugin_package("symfony/flex", "1.22.4.0")])
            .unwrap_err();
        assert!(error.to_string().contains("only Flex 2.x"));

        let error = manager
            .validate([&plugin_package("vendor/unknown", "1.0.0")])
            .unwrap_err();
        assert!(error.to_string().contains("cannot run in riff"));
    }

    #[test]
    fn disabled_plugins_do_not_fail_validation() {
        let manager = PluginManager::builtins(false, AllowPlugins::Bool(true)).unwrap();
        assert!(manager
            .validate([&plugin_package("vendor/unknown", "1.0.0")])
            .is_ok());
    }

    #[test]
    fn flex_declares_symfony_packs_fileless_through_package_layout_capability() {
        let manager = PluginManager::builtins(true, AllowPlugins::Bool(true)).unwrap();
        let flex = plugin_package("symfony/flex", "2.11.0.0");
        let mut pack = Package::new("symfony/apache-pack", "1.0.1.0");
        pack.package_type = "symfony-pack".into();
        let library = Package::new("symfony/console", "8.1.0.0");

        let layouts = manager.package_layouts([&flex, &pack, &library]);
        assert!(layouts.is_fileless(&pack));
        assert!(!layouts.is_fileless(&library));

        let layouts_without_flex = manager.package_layouts([&pack, &library]);
        assert!(!layouts_without_flex.is_fileless(&pack));
    }

    #[tokio::test]
    async fn package_argument_hooks_run_in_registration_order() {
        let mut registrar = PluginRegistrar::default();
        registrar.descriptor(PluginDescriptor::new("test/first"));
        registrar.descriptor(PluginDescriptor::new("test/second"));
        registrar.package_arguments("test/first", Arc::new(AppendArgument("first")));
        registrar.package_arguments("test/second", Arc::new(AppendArgument("second")));
        let manager = PluginManager::from_registrar(
            PluginPolicy::new(true, AllowPlugins::Bool(true)),
            registrar,
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let riff = Riff::builder(root.path().to_path_buf())
            .with_manifest(RiffManifest::default())
            .with_platform(Platform::empty())
            .build()
            .unwrap();

        assert_eq!(
            manager
                .transform_package_arguments(
                    &riff,
                    PackageOperation::Require,
                    &["root".to_owned()],
                )
                .await
                .unwrap(),
            ["root", "first", "second"]
        );
    }

    #[test]
    fn duplicate_exclusive_handlers_are_rejected() {
        let mut registrar = PluginRegistrar::default();
        registrar.descriptor(PluginDescriptor::new("test/first"));
        registrar.descriptor(PluginDescriptor::new("test/second"));
        registrar.object_script("test/first", "owned", Arc::new(NoopObjectScript));
        registrar.object_script("test/second", "owned", Arc::new(NoopObjectScript));

        assert!(PluginManager::from_registrar(
            PluginPolicy::new(true, AllowPlugins::Bool(true)),
            registrar,
        )
        .is_err());
    }

    #[tokio::test]
    async fn prepared_operations_apply_in_order_and_stop_on_failure() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registrar = PluginRegistrar::default();
        for (package, fail) in [
            ("test/first", false),
            ("test/second", true),
            ("test/third", false),
        ] {
            registrar.descriptor(PluginDescriptor::new(package));
            registrar.operations(
                package,
                Arc::new(RecordedOperationHook {
                    name: package,
                    fail,
                    calls: calls.clone(),
                }),
            );
        }
        let manager = PluginManager::from_registrar(
            PluginPolicy::new(true, AllowPlugins::Bool(true)),
            registrar,
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let riff = Riff::builder(root.path().to_path_buf())
            .with_manifest(RiffManifest::default())
            .with_platform(Platform::empty())
            .build()
            .unwrap();

        let plans = manager
            .prepare_operations(&riff, &Transaction::new(), &[])
            .await
            .unwrap();
        assert!(plans.apply(&riff).is_err());
        assert_eq!(*calls.lock().unwrap(), ["test/first", "test/second"]);
    }

    #[test]
    fn generic_core_modules_do_not_depend_on_flex() {
        let generic_sources = [
            include_str!("../installer/installer.rs"),
            include_str!("../scripts.rs"),
            include_str!("../event.rs"),
            include_str!("policy.rs"),
        ];
        for source in generic_sources {
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            for forbidden in ["symfony/flex", "symfony_flex", "FlexPlan", "SYMFONY_FLEX"] {
                assert!(
                    !production.contains(forbidden),
                    "generic core module contains Flex-specific identifier {forbidden}"
                );
            }
        }
    }
}
