use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::stream::{self, StreamExt};
use serde_json::Value;

use crate::cache::runtime_cache_dir;
use crate::downloader::SharedDownloadResources;
use crate::http::HttpClient;
use crate::installer::{InstallOptions, Installer, UpdateOptions};
use crate::output::Output;
use crate::repository::ComposerRepository;
use crate::{Riff, RiffBuilder};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ComposerRepositoryKey {
    name: String,
    url: String,
    filter: String,
}

struct RiffSessionInner {
    cache_dir: PathBuf,
    http_client: Arc<HttpClient>,
    repository_client: reqwest::Client,
    composer_repositories: Mutex<HashMap<ComposerRepositoryKey, Arc<ComposerRepository>>>,
    download_resources: SharedDownloadResources,
    audit_hook: Option<Arc<dyn ProjectAuditHook>>,
}

/// Host-provided audit integration for multi-project operations.
#[async_trait]
pub trait ProjectAuditHook: Send + Sync {
    async fn audit(
        &self,
        session: &RiffSession,
        working_dir: &Path,
        no_dev: bool,
        output: Output,
    ) -> Result<()>;
}

/// Reusable process-local resources for one or more Riff projects.
///
/// A session shares remote repository metadata, HTTP connection pools, archive
/// downloads, and resource limits. Project manifests, lockfiles, solvers,
/// transactions, and vendor directories remain isolated.
#[derive(Clone)]
pub struct RiffSession {
    inner: Arc<RiffSessionInner>,
}

impl RiffSession {
    pub fn new() -> Result<Self> {
        RiffSessionBuilder::new().build()
    }

    pub fn builder() -> RiffSessionBuilder {
        RiffSessionBuilder::new()
    }

    /// Start configuring a project that uses this session's shared resources.
    pub fn project(&self, working_dir: impl Into<PathBuf>) -> RiffBuilder {
        RiffBuilder::new(working_dir.into()).with_session(self.clone())
    }

    pub fn cache_dir(&self) -> &Path {
        &self.inner.cache_dir
    }

    pub fn supports_project_audit(&self) -> bool {
        self.inner.audit_hook.is_some()
    }

    pub async fn install_projects(
        &self,
        requests: impl IntoIterator<Item = ProjectInstallRequest>,
        options: BatchOptions,
    ) -> Vec<ProjectInstallResult> {
        let requests = requests.into_iter().collect::<Vec<_>>();
        let concurrency = options.effective_concurrency(requests.len());

        let mut results = stream::iter(requests.into_iter().enumerate())
            .map(|(index, request)| async move {
                let working_dir = request.riff.working_dir.clone();
                let output = request.riff.output().clone();
                let audit = request.audit;
                let result = if self.owns(&request.riff) {
                    let installer = Installer::new(request.riff);
                    let result = match request.operation {
                        ProjectOperation::Install(options) => installer.install(options).await,
                        ProjectOperation::Update(options) => installer.update(options).await,
                    };
                    if matches!(result, Ok(0)) {
                        if let (Some(audit), Some(hook)) = (audit, self.inner.audit_hook.as_ref()) {
                            if let Err(error) = hook
                                .audit(self, &working_dir, audit.no_dev, output.clone())
                                .await
                            {
                                crate::warnln!(output, "Warning: Audit failed: {error}");
                            }
                        }
                    }
                    result
                } else {
                    Err(anyhow::anyhow!(
                        "project at '{}' belongs to a different Riff session",
                        working_dir.display()
                    ))
                };
                (
                    index,
                    ProjectInstallResult {
                        working_dir,
                        result,
                    },
                )
            })
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await;
        results.sort_by_key(|(index, _)| *index);
        results.into_iter().map(|(_, result)| result).collect()
    }

    pub(crate) fn http_client(&self) -> Arc<HttpClient> {
        Arc::clone(&self.inner.http_client)
    }

    pub(crate) fn download_resources(&self) -> SharedDownloadResources {
        self.inner.download_resources.clone()
    }

    pub(crate) fn composer_repository(
        &self,
        name: impl Into<String>,
        url: impl Into<String>,
        filter: &Value,
    ) -> Arc<ComposerRepository> {
        let name = name.into();
        let url = url.into();
        let key = ComposerRepositoryKey {
            name: name.clone(),
            url: url.trim_end_matches('/').to_owned(),
            filter: serde_json::to_string(filter).unwrap_or_default(),
        };
        let mut repositories = self
            .inner
            .composer_repositories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(repositories.entry(key).or_insert_with(|| {
            let mut repository = ComposerRepository::with_cache_and_client(
                name,
                url,
                self.inner.cache_dir.clone(),
                self.inner.repository_client.clone(),
            );
            repository.set_user_filter_config(filter.clone());
            Arc::new(repository)
        }))
    }

    pub(crate) fn packagist_repository(&self) -> Arc<ComposerRepository> {
        self.composer_repository(
            "packagist.org",
            "https://repo.packagist.org",
            &Value::Object(Default::default()),
        )
    }

    pub(crate) fn owns(&self, riff: &Riff) -> bool {
        Arc::ptr_eq(&self.inner, &riff.session.inner)
    }
}

/// Configures resources shared by every project in a [`RiffSession`].
pub struct RiffSessionBuilder {
    cache_dir: Option<PathBuf>,
    http_client: Option<Arc<HttpClient>>,
    max_concurrent_downloads: usize,
    max_concurrent_extractions: usize,
    audit_hook: Option<Arc<dyn ProjectAuditHook>>,
}

impl RiffSessionBuilder {
    pub fn new() -> Self {
        Self {
            cache_dir: None,
            http_client: None,
            max_concurrent_downloads: 64,
            max_concurrent_extractions: 10,
            audit_hook: None,
        }
    }

    pub fn with_cache_dir(mut self, cache_dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(cache_dir.into());
        self
    }

    pub fn with_http_client(mut self, http_client: Arc<HttpClient>) -> Self {
        self.http_client = Some(http_client);
        self
    }

    pub fn max_concurrent_downloads(mut self, limit: usize) -> Self {
        self.max_concurrent_downloads = limit.max(1);
        self
    }

    pub fn max_concurrent_extractions(mut self, limit: usize) -> Self {
        self.max_concurrent_extractions = limit.max(1);
        self
    }

    pub fn with_project_audit_hook(mut self, hook: Arc<dyn ProjectAuditHook>) -> Self {
        self.audit_hook = Some(hook);
        self
    }

    pub fn build(self) -> Result<RiffSession> {
        let http_client = match self.http_client {
            Some(client) => client,
            None => Arc::new(HttpClient::new().context("Failed to create HTTP client")?),
        };
        let repository_client = reqwest::Client::builder()
            .user_agent("riff-composer/0.1.0")
            .build()
            .context("Failed to create repository HTTP client")?;
        Ok(RiffSession {
            inner: Arc::new(RiffSessionInner {
                cache_dir: self.cache_dir.unwrap_or_else(runtime_cache_dir),
                http_client,
                repository_client,
                composer_repositories: Mutex::new(HashMap::new()),
                download_resources: SharedDownloadResources::new(
                    self.max_concurrent_downloads,
                    self.max_concurrent_extractions,
                ),
                audit_hook: self.audit_hook,
            }),
        })
    }
}

impl Default for RiffSessionBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// One project operation submitted to [`RiffSession::install_projects`].
pub struct ProjectInstallRequest {
    riff: Riff,
    operation: ProjectOperation,
    audit: Option<ProjectAuditOptions>,
}

impl ProjectInstallRequest {
    pub fn install(riff: Riff, options: InstallOptions) -> Self {
        Self {
            riff,
            operation: ProjectOperation::Install(options),
            audit: None,
        }
    }

    pub fn update(riff: Riff, options: UpdateOptions) -> Self {
        Self {
            riff,
            operation: ProjectOperation::Update(options),
            audit: None,
        }
    }

    pub fn with_audit(mut self, options: ProjectAuditOptions) -> Self {
        self.audit = Some(options);
        self
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProjectAuditOptions {
    pub no_dev: bool,
}

enum ProjectOperation {
    Install(InstallOptions),
    Update(UpdateOptions),
}

/// Outcome for one project in a batch, returned in request order.
pub struct ProjectInstallResult {
    working_dir: PathBuf,
    result: Result<i32>,
}

impl ProjectInstallResult {
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    pub fn result(&self) -> Result<i32, &anyhow::Error> {
        self.result.as_ref().copied()
    }

    pub fn into_result(self) -> Result<i32> {
        self.result
    }
}

/// Controls scheduling of a multi-project install.
#[derive(Debug, Clone, Copy, Default)]
pub struct BatchOptions {
    max_concurrency: Option<usize>,
}

impl BatchOptions {
    pub fn with_max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = Some(max_concurrency.max(1));
        self
    }

    fn effective_concurrency(self, jobs: usize) -> usize {
        if jobs == 0 {
            return 1;
        }
        self.max_concurrency
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(1)
            })
            .min(jobs)
            .max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::RiffManifest;
    use crate::Platform;

    #[test]
    fn session_reuses_identical_composer_repositories() {
        let cache = tempfile::tempdir().unwrap();
        let session = RiffSession::builder()
            .with_cache_dir(cache.path())
            .build()
            .unwrap();
        let filter = serde_json::json!({"only": ["vendor/*"]});

        let first = session.composer_repository("example", "https://example.test", &filter);
        let second = session.composer_repository("example", "https://example.test/", &filter);
        let different =
            session.composer_repository("example", "https://example.test", &serde_json::json!({}));

        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &different));
    }

    #[tokio::test]
    async fn batch_rejects_foreign_projects_and_preserves_request_order() {
        let owner = RiffSession::new().unwrap();
        let runner = RiffSession::new().unwrap();
        let project = |path: &str| {
            owner
                .project(path)
                .with_manifest(RiffManifest::default())
                .with_platform(Platform::empty())
                .build()
                .unwrap()
        };
        let requests = vec![
            ProjectInstallRequest::update(project("second"), UpdateOptions::default()),
            ProjectInstallRequest::install(project("first"), InstallOptions::default()),
        ];

        let results = runner
            .install_projects(requests, BatchOptions::default())
            .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].working_dir(), Path::new("second"));
        assert_eq!(results[1].working_dir(), Path::new("first"));
        assert!(results
            .into_iter()
            .all(|result| result.into_result().is_err()));
    }
}
