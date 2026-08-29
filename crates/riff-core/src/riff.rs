use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{Config, PreferredInstall};
use crate::event::EventDispatcher;
use crate::http::HttpClient;
use crate::installer::InstallConfig;
use crate::installer::InstallationManager;
use crate::json::{Repositories, Repository as JsonRepository, RiffLockfile, RiffManifest};
use crate::output::Output;
use crate::plugin::PluginManager;
use crate::policy_config::{PackagePolicyConfig, PolicyEnvironment};
use crate::repository::{Repository, RepositoryManager};
use crate::runtime::RuntimeContext;
use crate::session::{RiffSession, RiffSessionBuilder};
use crate::Platform;

/// The central Riff application object.
pub struct Riff {
    pub config: Config,
    pub manifest: RiffManifest,
    pub lockfile: Option<RiffLockfile>,
    pub repository_manager: Arc<RepositoryManager>,
    pub installation_manager: Arc<InstallationManager>,
    pub http_client: Arc<HttpClient>,
    pub working_dir: PathBuf,
    pub platform_packages: Vec<crate::package::Package>,
    pub event_dispatcher: EventDispatcher,
    pub runtime: RuntimeContext,
    pub package_policy: PackagePolicyConfig,
    pub output: Output,
    pub(crate) session: RiffSession,
    pub(crate) platform: Platform,
    pub(crate) policy_environment: PolicyEnvironment,
    pub(crate) plugins_enabled: bool,
    pub(crate) audit_enabled: bool,
    plugin_manager: PluginManager,
}

impl Riff {
    /// Create a new Riff instance using the builder pattern.
    pub fn builder(working_dir: PathBuf) -> RiffBuilder {
        RiffBuilder::new(working_dir)
    }

    /// Create a new Riff instance directly.
    pub fn new(
        working_dir: PathBuf,
        config: Config,
        manifest: RiffManifest,
        lockfile: Option<RiffLockfile>,
        platform: Platform,
    ) -> Result<Self> {
        RiffBuilder::new(working_dir)
            .with_config(config)
            .with_manifest(manifest)
            .with_lockfile(lockfile)
            .with_platform(platform)
            .build()
    }

    /// Dispatch a typed event and return the exit code.
    pub async fn dispatch<E: crate::event::RiffEvent>(&self, event: &E) -> anyhow::Result<i32> {
        self.event_dispatcher.dispatch(event, self).await
    }

    /// Get the vendor directory path.
    pub fn vendor_dir(&self) -> std::path::PathBuf {
        self.working_dir.join(&self.config.vendor_dir)
    }

    pub fn plugins(&self) -> &PluginManager {
        &self.plugin_manager
    }

    pub fn output(&self) -> &Output {
        &self.output
    }
}

/// Builder for creating Riff instances.
pub struct RiffBuilder {
    working_dir: PathBuf,
    config: Option<Config>,
    manifest: Option<RiffManifest>,
    lockfile: Option<RiffLockfile>,
    http_client: Option<Arc<HttpClient>>,
    repository_manager: Option<RepositoryManager>,
    additional_repositories: Vec<Arc<dyn Repository>>,
    session: Option<RiffSession>,

    // Installation options (override config)
    prefer_source: Option<bool>,
    prefer_dist: Option<bool>,
    dry_run: bool,
    no_dev: bool,
    prefer_lowest: bool,
    prefer_stable: bool,
    download_only: bool,

    // Caller-supplied platform facts (php, ext-*, lib-*)
    platform: Option<Platform>,

    // Executables used by @php and @composer scripts.
    runtime: RuntimeContext,
    plugins_enabled: bool,
    audit_enabled: bool,
    policy_environment: PolicyEnvironment,
    output: Output,

    // Repository options
    disable_packagist: Option<bool>,
}

impl RiffBuilder {
    /// Create a new builder with the given working directory.
    pub fn new(working_dir: PathBuf) -> Self {
        Self {
            working_dir,
            config: None,
            manifest: None,
            lockfile: None,
            http_client: None,
            repository_manager: None,
            additional_repositories: Vec::new(),
            session: None,
            prefer_source: None,
            prefer_dist: None,
            dry_run: false,
            no_dev: false,
            prefer_lowest: false,
            prefer_stable: false,
            download_only: false,
            platform: None,
            runtime: RuntimeContext::default(),
            plugins_enabled: true,
            audit_enabled: true,
            policy_environment: PolicyEnvironment::new(),
            output: Output::silent(),
            disable_packagist: None,
        }
    }

    pub fn with_config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_manifest(mut self, manifest: RiffManifest) -> Self {
        self.manifest = Some(manifest);
        self
    }

    pub fn with_lockfile(mut self, lockfile: Option<RiffLockfile>) -> Self {
        self.lockfile = lockfile;
        self
    }

    pub fn with_http_client(mut self, http_client: Arc<HttpClient>) -> Self {
        self.http_client = Some(http_client);
        self
    }

    /// Use resources shared with other projects created from the same session.
    pub fn with_session(mut self, session: RiffSession) -> Self {
        self.session = Some(session);
        self
    }

    pub fn with_repository_manager(mut self, repository_manager: RepositoryManager) -> Self {
        self.repository_manager = Some(repository_manager);
        self
    }

    pub fn add_repository(mut self, repo: Arc<dyn Repository>) -> Self {
        self.additional_repositories.push(repo);
        self
    }

    pub fn prefer_source(mut self, prefer: bool) -> Self {
        self.prefer_source = Some(prefer);
        if prefer {
            self.prefer_dist = Some(false);
        }
        self
    }

    pub fn prefer_dist(mut self, prefer: bool) -> Self {
        self.prefer_dist = Some(prefer);
        if prefer {
            self.prefer_source = Some(false);
        }
        self
    }

    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    pub fn no_dev(mut self, no_dev: bool) -> Self {
        self.no_dev = no_dev;
        self
    }

    pub fn prefer_lowest(mut self, prefer: bool) -> Self {
        self.prefer_lowest = prefer;
        self
    }

    pub fn prefer_stable(mut self, prefer: bool) -> Self {
        self.prefer_stable = prefer;
        self
    }

    pub fn prefer_auto(mut self) -> Self {
        self.prefer_source = Some(false);
        self.prefer_dist = Some(false);
        self
    }

    pub fn download_only(mut self, download_only: bool) -> Self {
        self.download_only = download_only;
        self
    }

    pub fn with_platform(mut self, platform: Platform) -> Self {
        self.platform = Some(platform);
        self
    }

    #[deprecated(note = "use with_platform(Platform::from_packages(packages))")]
    pub fn with_platform_packages(mut self, packages: Vec<crate::package::Package>) -> Self {
        self.platform = Some(Platform::from_packages(packages));
        self
    }

    pub fn with_runtime(mut self, runtime: RuntimeContext) -> Self {
        self.runtime = runtime;
        self
    }

    pub fn plugins_enabled(mut self, enabled: bool) -> Self {
        self.plugins_enabled = enabled;
        self
    }

    /// Declare whether nested package-manager operations should run audits.
    pub fn audit_enabled(mut self, enabled: bool) -> Self {
        self.audit_enabled = enabled;
        self
    }

    /// Supply the dependency-policy environment explicitly. CLI callers use a
    /// process snapshot while embedders can remain fully deterministic.
    pub fn with_policy_environment(mut self, environment: PolicyEnvironment) -> Self {
        self.policy_environment = environment;
        self
    }

    /// Route Riff-generated output through an instance-scoped handle.
    pub fn with_output(mut self, output: Output) -> Self {
        self.output = output;
        self
    }

    pub fn disable_packagist(mut self, disable: bool) -> Self {
        self.disable_packagist = Some(disable);
        self
    }

    pub fn build(mut self) -> Result<Riff> {
        let manifest = self
            .manifest
            .take()
            .ok_or_else(|| anyhow::anyhow!("composer.json is required"))?;

        let config = self
            .config
            .take()
            .unwrap_or_else(|| Config::with_base_dir(&self.working_dir));
        let platform = self.platform.take().ok_or_else(|| {
            anyhow::anyhow!(
                "platform information is required; use with_platform(...) or Platform::empty()"
            )
        })?;
        let platform_packages = platform.to_packages(&config.platform)?;
        let policy_environment = self.policy_environment.clone();
        let package_policy = PackagePolicyConfig::from_raw(
            &config.policy,
            &config.audit_policy,
            &policy_environment,
        )?;

        let session = match (self.session.take(), self.http_client.take()) {
            (Some(session), None) => session,
            (Some(_), Some(_)) => {
                anyhow::bail!(
                    "with_http_client(...) cannot be combined with a shared Riff session; configure the client on RiffSessionBuilder"
                )
            }
            (None, http_client) => {
                let mut builder = RiffSessionBuilder::new();
                if let Some(http_client) = http_client {
                    builder = builder.with_http_client(http_client);
                }
                builder.build()?
            }
        };
        let http_client = session.http_client();

        let repository_manager = self.build_repository_manager(&manifest, &session)?;
        let install_config = self.build_install_config(&config, &session);
        let plugin_manager =
            PluginManager::builtins(self.plugins_enabled, config.allow_plugins.clone())?;

        let installation_manager = Arc::new(InstallationManager::new_with_output_and_resources(
            http_client.clone(),
            install_config,
            self.output.clone(),
            session.download_resources(),
        ));

        // Create event dispatcher with script listeners and plugins
        let mut event_dispatcher = EventDispatcher::with_scripts();
        plugin_manager.register_events(&mut event_dispatcher);

        Ok(Riff {
            config,
            manifest,
            lockfile: self.lockfile.take(),
            repository_manager: Arc::new(repository_manager),
            installation_manager,
            http_client,
            working_dir: self.working_dir.clone(),
            platform_packages,
            event_dispatcher,
            runtime: self.runtime,
            package_policy,
            output: self.output,
            session,
            platform,
            policy_environment,
            plugins_enabled: self.plugins_enabled,
            audit_enabled: self.audit_enabled,
            plugin_manager,
        })
    }

    fn build_repository_manager(
        &mut self,
        manifest: &RiffManifest,
        session: &RiffSession,
    ) -> Result<RepositoryManager> {
        if let Some(manager) = self.repository_manager.take() {
            return Ok(manager.with_output(self.output.clone()));
        }

        let mut repository_manager = RepositoryManager::new().with_output(self.output.clone());

        for repo in manifest.repositories.as_vec() {
            if repository_is_pear(&repo) {
                anyhow::bail!("The PEAR repository has been removed from Composer 2.x");
            }
            repository_manager.add_from_json_repository_at_in_session(
                &repo,
                &self.working_dir,
                session,
            );
        }

        for repo in &self.additional_repositories {
            repository_manager.add_repository(repo.clone());
        }

        let packagist_disabled = self
            .disable_packagist
            .unwrap_or_else(|| is_packagist_disabled(&manifest.repositories));

        if !packagist_disabled {
            repository_manager.add_repository(session.packagist_repository());
        }

        Ok(repository_manager)
    }

    fn build_install_config(&self, config: &Config, session: &RiffSession) -> InstallConfig {
        let (prefer_source, prefer_dist) = match (self.prefer_source, self.prefer_dist) {
            (Some(src), Some(dst)) => (src, dst),
            (Some(src), None) => (src, !src),
            (None, Some(dst)) => (!dst, dst),
            (None, None) => match config.preferred_install {
                PreferredInstall::Source => (true, false),
                PreferredInstall::Dist => (false, true),
                PreferredInstall::Auto => (false, false),
                PreferredInstall::Patterns(_) => (false, false),
            },
        };

        InstallConfig {
            base_dir: self.working_dir.clone(),
            vendor_dir: self.working_dir.join(&config.vendor_dir),
            bin_dir: self.working_dir.join(&config.bin_dir),
            bin_compat: config.bin_compat.clone(),
            cache_dir: session.cache_dir().to_path_buf(),
            prefer_source,
            prefer_dist,
            dry_run: self.dry_run,
            no_dev: self.no_dev,
            prefer_lowest: self.prefer_lowest,
            prefer_stable: self.prefer_stable,
            download_only: self.download_only,
        }
    }
}

impl Clone for RiffBuilder {
    fn clone(&self) -> Self {
        Self {
            working_dir: self.working_dir.clone(),
            config: self.config.clone(),
            manifest: self.manifest.clone(),
            lockfile: self.lockfile.clone(),
            http_client: self.http_client.clone(),
            repository_manager: None, // RepositoryManager doesn't implement Clone
            additional_repositories: self.additional_repositories.clone(),
            session: self.session.clone(),
            prefer_source: self.prefer_source,
            prefer_dist: self.prefer_dist,
            dry_run: self.dry_run,
            no_dev: self.no_dev,
            prefer_lowest: self.prefer_lowest,
            prefer_stable: self.prefer_stable,
            download_only: self.download_only,
            platform: self.platform.clone(),
            runtime: self.runtime.clone(),
            plugins_enabled: self.plugins_enabled,
            audit_enabled: self.audit_enabled,
            policy_environment: self.policy_environment.clone(),
            output: self.output.clone(),
            disable_packagist: self.disable_packagist,
        }
    }
}

fn repository_is_pear(repository: &JsonRepository) -> bool {
    match repository {
        JsonRepository::Pear { .. } => true,
        JsonRepository::Filtered { repository, .. } => repository_is_pear(repository),
        _ => false,
    }
}

/// Check if packagist.org is disabled in the repositories configuration
fn is_packagist_disabled(repositories: &Repositories) -> bool {
    match repositories {
        Repositories::None => false,
        Repositories::Array(repos) => {
            // In array format, check for Disabled(false) entries
            // (though this is unusual - disabling is typically done in object format)
            repos.iter().any(|repository| {
                matches!(repository, JsonRepository::Disabled(false))
                    || matches!(
                        repository,
                        JsonRepository::NamedDisabled { name, disabled: false }
                            if name == "packagist.org" || name == "packagist"
                    )
            })
        }
        Repositories::Object(map) => {
            // In object format, packagist.org is disabled if key exists with false value
            map.iter().any(|(key, val)| {
                (key == "packagist.org" || key == "packagist")
                    && matches!(val, JsonRepository::Disabled(false))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::runtime_cache_dir;
    use indexmap::IndexMap;

    fn create_minimal_manifest() -> RiffManifest {
        RiffManifest {
            name: Some("test/package".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_builder_minimal() {
        let working_dir = PathBuf::from("/tmp/test");
        let manifest = create_minimal_manifest();

        let result = RiffBuilder::new(working_dir.clone())
            .with_manifest(manifest)
            .with_platform(Platform::empty())
            .build();

        assert!(result.is_ok());
        let riff = result.unwrap();
        assert_eq!(riff.working_dir, working_dir);
    }

    #[test]
    fn test_builder_missing_manifest() {
        let working_dir = PathBuf::from("/tmp/test");

        let result = RiffBuilder::new(working_dir).build();

        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("composer.json is required"));
    }

    #[test]
    fn test_builder_requires_explicit_platform() {
        let error = RiffBuilder::new(PathBuf::from("/tmp/test"))
            .with_manifest(create_minimal_manifest())
            .build()
            .err()
            .expect("platform information must be explicit");

        assert!(error
            .to_string()
            .contains("platform information is required"));
    }

    #[test]
    fn test_builder_with_dry_run() {
        let working_dir = PathBuf::from("/tmp/test");
        let manifest = create_minimal_manifest();

        let _riff = RiffBuilder::new(working_dir)
            .with_manifest(manifest)
            .with_platform(Platform::empty())
            .dry_run(true)
            .build()
            .unwrap();
    }

    #[test]
    fn test_builder_with_no_dev() {
        let working_dir = PathBuf::from("/tmp/test");
        let manifest = create_minimal_manifest();

        let _riff = RiffBuilder::new(working_dir)
            .with_manifest(manifest)
            .with_platform(Platform::empty())
            .no_dev(true)
            .build()
            .unwrap();
    }

    #[test]
    fn test_builder_prefer_source() {
        let working_dir = PathBuf::from("/tmp/test");
        let manifest = create_minimal_manifest();

        let _riff = RiffBuilder::new(working_dir)
            .with_manifest(manifest)
            .with_platform(Platform::empty())
            .prefer_source(true)
            .build()
            .unwrap();
    }

    #[test]
    fn test_builder_prefer_dist() {
        let working_dir = PathBuf::from("/tmp/test");
        let manifest = create_minimal_manifest();

        let _riff = RiffBuilder::new(working_dir)
            .with_manifest(manifest)
            .with_platform(Platform::empty())
            .prefer_dist(true)
            .build()
            .unwrap();
    }

    #[test]
    fn test_builder_disable_packagist() {
        let working_dir = PathBuf::from("/tmp/test");
        let manifest = create_minimal_manifest();

        let riff = RiffBuilder::new(working_dir)
            .with_manifest(manifest)
            .with_platform(Platform::empty())
            .disable_packagist(true)
            .build()
            .unwrap();

        let repos = riff.repository_manager.repositories();
        let has_packagist = repos.iter().any(|r| r.name().contains("packagist"));
        assert!(!has_packagist);
    }

    #[test]
    fn test_builder_with_config() {
        let working_dir = PathBuf::from("/tmp/test");
        let manifest = create_minimal_manifest();
        let config = Config::with_base_dir(&working_dir);

        let riff = RiffBuilder::new(working_dir.clone())
            .with_config(config)
            .with_manifest(manifest)
            .with_platform(Platform::empty())
            .build()
            .unwrap();

        assert_eq!(riff.config.base_dir(), Some(working_dir.as_path()));
    }

    #[test]
    fn pear_repository_matches_composer_two_removal_error() {
        let working_dir = PathBuf::from("/tmp/test");
        let mut manifest = create_minimal_manifest();
        manifest.repositories = Repositories::Array(vec![JsonRepository::Pear {
            url: "https://pear.example.test".to_string(),
        }]);

        let error = RiffBuilder::new(working_dir)
            .with_manifest(manifest)
            .with_platform(Platform::empty())
            .build()
            .err()
            .expect("PEAR repositories must be rejected");
        assert!(error
            .to_string()
            .contains("PEAR repository has been removed from Composer 2.x"));
    }

    #[test]
    fn test_builder_uses_riff_cache_instead_of_composer_cache() {
        let working_dir = PathBuf::from("/tmp/test");
        let manifest = create_minimal_manifest();
        let mut config = Config::with_base_dir(&working_dir);
        config.cache_dir = Some(working_dir.join("composer-cache"));

        let riff = RiffBuilder::new(working_dir)
            .with_config(config)
            .with_manifest(manifest)
            .with_platform(Platform::empty())
            .build()
            .unwrap();

        assert_eq!(
            riff.installation_manager.config().cache_dir,
            runtime_cache_dir()
        );
        assert_ne!(
            riff.installation_manager.config().cache_dir,
            riff.config.cache_dir.unwrap()
        );
    }

    #[test]
    fn test_builder_with_lock() {
        let working_dir = PathBuf::from("/tmp/test");
        let manifest = create_minimal_manifest();
        let lockfile = RiffLockfile::default();

        let riff = RiffBuilder::new(working_dir)
            .with_manifest(manifest)
            .with_lockfile(Some(lockfile))
            .with_platform(Platform::empty())
            .build()
            .unwrap();

        assert!(riff.lockfile.is_some());
    }

    #[test]
    fn test_builder_clone() {
        let working_dir = PathBuf::from("/tmp/test");
        let manifest = create_minimal_manifest();

        let builder = RiffBuilder::new(working_dir)
            .with_manifest(manifest)
            .with_platform(Platform::empty())
            .dry_run(true)
            .no_dev(true);

        let cloned = builder.clone();
        assert!(cloned.dry_run);
        assert!(cloned.no_dev);
    }

    #[test]
    fn test_riff_builder_static_method() {
        let working_dir = PathBuf::from("/tmp/test");
        let manifest = create_minimal_manifest();

        let riff = Riff::builder(working_dir.clone())
            .with_manifest(manifest)
            .with_platform(Platform::empty())
            .build()
            .unwrap();

        assert_eq!(riff.working_dir, working_dir);
    }

    #[test]
    fn test_is_packagist_disabled_none() {
        let repos = Repositories::None;
        assert!(!is_packagist_disabled(&repos));
    }

    #[test]
    fn test_is_packagist_disabled_empty_array() {
        let repos = Repositories::Array(vec![]);
        assert!(!is_packagist_disabled(&repos));
    }

    #[test]
    fn test_is_packagist_disabled_array_with_disabled() {
        let repos = Repositories::Array(vec![JsonRepository::Disabled(false)]);
        assert!(is_packagist_disabled(&repos));
    }

    #[test]
    fn test_is_packagist_disabled_empty_object() {
        let repos = Repositories::Object(IndexMap::new());
        assert!(!is_packagist_disabled(&repos));
    }

    #[test]
    fn test_is_packagist_disabled_object_packagist_org_false() {
        let mut map = IndexMap::new();
        map.insert("packagist.org".to_string(), JsonRepository::Disabled(false));
        let repos = Repositories::Object(map);
        assert!(is_packagist_disabled(&repos));
    }

    #[test]
    fn test_is_packagist_disabled_object_packagist_false() {
        let mut map = IndexMap::new();
        map.insert("packagist".to_string(), JsonRepository::Disabled(false));
        let repos = Repositories::Object(map);
        assert!(is_packagist_disabled(&repos));
    }

    #[test]
    fn test_is_packagist_disabled_object_other_repo() {
        let mut map = IndexMap::new();
        map.insert("other-repo".to_string(), JsonRepository::Disabled(false));
        let repos = Repositories::Object(map);
        assert!(!is_packagist_disabled(&repos));
    }
}
