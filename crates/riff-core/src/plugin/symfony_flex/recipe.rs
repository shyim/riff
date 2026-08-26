use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use base64::Engine;
use futures_util::future::join_all;
use indexmap::IndexMap;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::cache::runtime_cache_dir;
use crate::http::{HttpClient, HttpRequestOptions};
use crate::json::RiffManifest;
use crate::package::Package;
use crate::riff::Riff;
use crate::solver::{Operation, Transaction};

const DEFAULT_ENDPOINTS: [&str; 2] = [
    "https://raw.githubusercontent.com/symfony/recipes/flex/main/index.json",
    "https://raw.githubusercontent.com/symfony/recipes-contrib/flex/main/index.json",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecipeJob {
    Install,
    Uninstall,
}

#[derive(Debug, Clone)]
pub(crate) struct RecipeFile {
    pub(crate) contents: String,
    pub(crate) executable: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Recipe {
    pub(crate) package: Arc<Package>,
    pub(crate) name: String,
    pub(crate) job: RecipeJob,
    pub(crate) manifest: Map<String, Value>,
    pub(crate) files: IndexMap<String, RecipeFile>,
    pub(crate) lock: Value,
    pub(crate) origin: String,
    pub(crate) is_contrib: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct EndpointIndex {
    #[serde(default)]
    aliases: Value,
    #[serde(default)]
    versions: Value,
    #[serde(default)]
    recipes: IndexMap<String, Vec<String>>,
    #[serde(default)]
    branch: String,
    #[serde(default)]
    is_contrib: bool,
    #[serde(rename = "_links", default)]
    links: EndpointLinks,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct EndpointLinks {
    #[serde(default)]
    repository: String,
    #[serde(default)]
    origin_template: String,
    #[serde(default)]
    recipe_template: String,
    #[serde(default)]
    recipe_template_relative: Option<String>,
    #[serde(default)]
    archived_recipes_template: Option<String>,
    #[serde(default)]
    archived_recipes_template_relative: Option<String>,
}

#[derive(Debug, Clone)]
struct IndexedRecipe {
    versions: Vec<String>,
    endpoint: String,
}

#[derive(Debug, Clone)]
struct Endpoint {
    url: String,
    index: EndpointIndex,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedResponse {
    body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_modified: Option<String>,
}

pub(crate) struct RecipeDownloader {
    client: Arc<HttpClient>,
    cache_dir: PathBuf,
    cache_read_only: bool,
    endpoints: Vec<Endpoint>,
    recipes: IndexMap<String, IndexedRecipe>,
    aliases: IndexMap<String, String>,
    versions: Map<String, Value>,
    legacy_endpoint: Option<String>,
}

impl RecipeDownloader {
    pub(crate) async fn new(riff: &Riff) -> Result<Self> {
        let cache_dir = runtime_cache_dir().join("repo/flex");
        let configuration = endpoint_configuration(&riff.manifest);
        let urls = configuration.modern;
        let responses = join_all(urls.iter().map(|url| {
            fetch_cached(
                riff.http_client.clone(),
                cache_dir.clone(),
                riff.config.cache_read_only,
                url.clone(),
            )
        }))
        .await;

        let mut endpoints = Vec::new();
        let mut recipes = IndexMap::new();
        let mut aliases = IndexMap::new();
        let mut versions = Map::new();
        for (url, response) in urls.into_iter().zip(responses) {
            let body =
                response.with_context(|| format!("Failed to load Symfony recipe index {url}"))?;
            let index: EndpointIndex = serde_json::from_str(&body)
                .with_context(|| format!("Invalid Symfony recipe index from {url}"))?;
            for (name, versions) in &index.recipes {
                recipes
                    .entry(name.clone())
                    .or_insert_with(|| IndexedRecipe {
                        versions: versions.clone(),
                        endpoint: url.clone(),
                    });
            }
            if let Some(index_aliases) = index.aliases.as_object() {
                for (alias, package) in index_aliases {
                    if let Some(package) = package.as_str() {
                        aliases
                            .entry(alias.clone())
                            .or_insert_with(|| package.to_owned());
                    }
                }
            }
            if let Some(index_versions) = index.versions.as_object() {
                for (name, value) in index_versions {
                    versions
                        .entry(name.clone())
                        .or_insert_with(|| value.clone());
                }
            }
            endpoints.push(Endpoint { url, index });
        }
        if let Some(endpoint) = &configuration.legacy {
            let version_url = format!("{endpoint}/versions.json");
            let alias_url = format!("{endpoint}/aliases.json");
            let (version_body, alias_body) = futures_util::future::try_join(
                fetch_cached(
                    riff.http_client.clone(),
                    cache_dir.clone(),
                    riff.config.cache_read_only,
                    version_url,
                ),
                fetch_cached(
                    riff.http_client.clone(),
                    cache_dir.clone(),
                    riff.config.cache_read_only,
                    alias_url,
                ),
            )
            .await?;
            if let Some(values) = serde_json::from_str::<Value>(&version_body)?.as_object() {
                versions.extend(values.clone());
            }
            if let Some(values) = serde_json::from_str::<Value>(&alias_body)?.as_object() {
                aliases.extend(values.iter().filter_map(|(name, package)| {
                    package
                        .as_str()
                        .map(|package| (name.clone(), package.to_owned()))
                }));
            }
        }

        Ok(Self {
            client: riff.http_client.clone(),
            cache_dir,
            cache_read_only: riff.config.cache_read_only,
            endpoints,
            recipes,
            aliases,
            versions,
            legacy_endpoint: configuration.legacy,
        })
    }

    pub(crate) fn resolve_arguments(
        &self,
        manifest: &RiffManifest,
        arguments: &[String],
        is_require: bool,
    ) -> Vec<String> {
        let symfony_requirement = manifest
            .extra
            .pointer("/symfony/require")
            .and_then(Value::as_str);
        let splits = self.versions.get("splits").and_then(Value::as_object);
        let mut resolved = IndexMap::new();
        for argument in arguments {
            let delimiter = argument.find([':', '=']);
            let (name, version) = delimiter.map_or((argument.as_str(), None), |position| {
                (&argument[..position], Some(&argument[position + 1..]))
            });
            let name = if name.contains('/') || name.contains('*') {
                name.to_owned()
            } else {
                self.aliases
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| name.to_owned())
            };
            let version = if name.starts_with("symfony/")
                && splits.is_some_and(|splits| splits.contains_key(&name))
            {
                match version {
                    Some("dev") => self
                        .versions
                        .get("dev-name")
                        .and_then(Value::as_str)
                        .map(|version| format!("^{version}@dev")),
                    Some("next") => self
                        .versions
                        .get("next")
                        .and_then(Value::as_str)
                        .map(|version| format!("^{version}@dev")),
                    Some(label @ ("lts" | "previous" | "stable")) => self
                        .versions
                        .get(label)
                        .and_then(Value::as_str)
                        .map(|version| format!("^{version}")),
                    Some("guess" | "*") | None if is_require => {
                        symfony_requirement.map(str::to_owned)
                    }
                    Some(version) => Some(version.to_owned()),
                    None => None,
                }
            } else {
                version
                    .filter(|version| *version != "guess")
                    .map(str::to_owned)
            };
            let argument = match version {
                Some(version) => format!("{name}:{version}"),
                None => name.clone(),
            };
            resolved.insert(name, argument);
        }
        resolved.into_values().collect()
    }

    pub(crate) fn symfony_splits(&self) -> Result<std::collections::HashSet<String>> {
        self.versions
            .get("splits")
            .and_then(Value::as_object)
            .map(|splits| splits.keys().cloned().collect())
            .context(
                "The Flex recipe index is missing its Symfony split-package list; include flex://defaults in extra.symfony.endpoint",
            )
    }

    pub(crate) async fn recipes_for_transaction(
        &self,
        transaction: &Transaction,
        lock: &super::lock::FlexLock,
        installed_packages: &[Arc<Package>],
    ) -> Result<Vec<Recipe>> {
        let mut requests = Vec::new();
        let mut generated = Vec::new();
        let installed_versions = Arc::new(
            installed_packages
                .iter()
                .map(|package| (package.name.clone(), package.pretty_version().to_owned()))
                .collect::<HashMap<_, _>>(),
        );
        for operation in &transaction.operations {
            let (package, job) = match operation {
                Operation::Install(package) | Operation::Reinstall(package)
                    if !lock.has(&package.name) =>
                {
                    (package.clone(), RecipeJob::Install)
                }
                Operation::Update { to: package, .. } if !lock.has(&package.name) => {
                    (package.clone(), RecipeJob::Install)
                }
                Operation::Uninstall(package) if lock.has(&package.name) => {
                    (package.clone(), RecipeJob::Uninstall)
                }
                _ => continue,
            };
            let Some(indexed) = self.recipes.get(&package.name) else {
                if let Some(endpoint) = &self.legacy_endpoint {
                    let operation = if job == RecipeJob::Uninstall {
                        'r'
                    } else {
                        'i'
                    };
                    let url = format!(
                        "{endpoint}/p/{},{operation}{}",
                        package.name.replace('/', ","),
                        package.pretty_version()
                    );
                    match fetch_cached(
                        self.client.clone(),
                        self.cache_dir.clone(),
                        self.cache_read_only,
                        url,
                    )
                    .await
                    {
                        Ok(body) => {
                            if let Some(recipe) = decode_legacy_recipe(package.clone(), job, &body)?
                            {
                                generated.push(recipe);
                            }
                        }
                        Err(error) => crate::warnln!(
                            "Warning: Failed to download recipe for {}: {error:#}",
                            package.name
                        ),
                    }
                    continue;
                }
                if let Some(recipe) = generated_bundle_recipe(package, job) {
                    generated.push(recipe);
                }
                continue;
            };
            let mut recipe_versions =
                compatible_recipe_versions(&indexed.versions, package.pretty_version());
            if recipe_versions.is_empty() {
                continue;
            }
            if job == RecipeJob::Uninstall {
                recipe_versions.truncate(1);
            }
            let endpoint = self
                .endpoints
                .iter()
                .find(|endpoint| endpoint.url == indexed.endpoint)
                .expect("indexed endpoint must exist");
            let urls = recipe_versions
                .into_iter()
                .map(|version| {
                    recipe_url(endpoint, &package.name, &version, job, lock)
                        .map(|url| (version, url))
                })
                .collect::<Result<Vec<_>>>()?;
            let client = self.client.clone();
            let cache_dir = self.cache_dir.clone();
            let cache_read_only = self.cache_read_only;
            let installed_versions = installed_versions.clone();
            requests.push(async move {
                for (recipe_version, url) in urls {
                    let body = match fetch_cached(
                        client.clone(),
                        cache_dir.clone(),
                        cache_read_only,
                        url,
                    )
                    .await
                    {
                        Ok(body) => body,
                        Err(error) => {
                            crate::warnln!(
                                "Warning: Failed to download recipe for {}: {error:#}",
                                package.name
                            );
                            return Ok(None);
                        }
                    };
                    let recipe =
                        decode_recipe(endpoint, package.clone(), job, &recipe_version, &body)?;
                    if job == RecipeJob::Install
                        && recipe_conflicts(&recipe, &installed_versions)
                    {
                        continue;
                    }
                    return Ok(Some(recipe));
                }
                crate::warnln!(
                    "Warning: Skipping recipe for {} because every compatible recipe conflicts with installed packages",
                    package.name
                );
                Ok(None)
            });
        }

        let mut recipes = join_all(requests)
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        recipes.extend(generated);
        recipes.sort_by(|left, right| recipe_order(left).cmp(&recipe_order(right)));
        Ok(recipes)
    }

    pub(crate) async fn recipes_for_update(
        &self,
        package: Arc<Package>,
        lock: &super::lock::FlexLock,
        installed_packages: &[Arc<Package>],
    ) -> Result<Option<(Recipe, Recipe)>> {
        let Some(lock_entry) = lock.get(&package.name) else {
            return Ok(None);
        };
        let Some(lock_recipe) = lock_entry.get("recipe").and_then(Value::as_object) else {
            return Ok(None);
        };
        let original_version = lock_recipe
            .get("version")
            .and_then(Value::as_str)
            .context("Installed recipe has no version")?;
        let repository = lock_recipe
            .get("repo")
            .and_then(Value::as_str)
            .context("Installed recipe has no repository")?;
        let endpoint = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.index.links.repository == repository)
            .with_context(|| format!("Recipe repository {repository} is not configured"))?;
        let original_url = recipe_url(
            endpoint,
            &package.name,
            original_version,
            RecipeJob::Uninstall,
            lock,
        )?;
        let original_body = fetch_cached(
            self.client.clone(),
            self.cache_dir.clone(),
            self.cache_read_only,
            original_url,
        )
        .await
        .context("Failed to download the installed recipe version")?;
        let original = decode_recipe(
            endpoint,
            package.clone(),
            RecipeJob::Install,
            original_version,
            &original_body,
        )?;

        let Some(indexed) = self.recipes.get(&package.name) else {
            return Ok(None);
        };
        let latest_endpoint = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.url == indexed.endpoint)
            .expect("indexed endpoint must exist");
        let installed_versions = installed_packages
            .iter()
            .map(|package| (package.name.clone(), package.pretty_version().to_owned()))
            .collect::<HashMap<_, _>>();
        for version in compatible_recipe_versions(&indexed.versions, package.pretty_version()) {
            let url = recipe_url(
                latest_endpoint,
                &package.name,
                &version,
                RecipeJob::Install,
                lock,
            )?;
            let body = fetch_cached(
                self.client.clone(),
                self.cache_dir.clone(),
                self.cache_read_only,
                url,
            )
            .await
            .context("Failed to download the latest recipe version")?;
            let latest = decode_recipe(
                latest_endpoint,
                package.clone(),
                RecipeJob::Install,
                &version,
                &body,
            )?;
            if !recipe_conflicts(&latest, &installed_versions) {
                return Ok(Some((original, latest)));
            }
        }
        Ok(None)
    }
}

struct EndpointConfiguration {
    modern: Vec<String>,
    legacy: Option<String>,
}

fn endpoint_configuration(manifest: &RiffManifest) -> EndpointConfiguration {
    endpoint_configuration_with_env(manifest, std::env::var("SYMFONY_ENDPOINT").ok())
}

fn endpoint_configuration_with_env(
    manifest: &RiffManifest,
    environment_endpoint: Option<String>,
) -> EndpointConfiguration {
    let configured = manifest
        .extra
        .get("symfony")
        .and_then(Value::as_object)
        .and_then(|symfony| symfony.get("endpoint"))
        .cloned();
    let (mut endpoints, mut legacy) = match configured {
        Some(Value::String(endpoint)) if endpoint.contains(".json") => {
            (vec![endpoint, "flex://defaults".to_owned()], None)
        }
        Some(Value::String(endpoint)) if endpoint == "flex://defaults" => (vec![endpoint], None),
        Some(Value::String(endpoint)) => (Vec::new(), Some(endpoint)),
        Some(Value::Array(endpoints)) => (
            endpoints
                .into_iter()
                .filter_map(|endpoint| endpoint.as_str().map(str::to_owned))
                .collect(),
            None,
        ),
        _ => (
            DEFAULT_ENDPOINTS
                .iter()
                .map(|endpoint| (*endpoint).to_owned())
                .collect(),
            None,
        ),
    };
    if let Some(endpoint) = environment_endpoint {
        if endpoint.contains(".json") || endpoint == "flex://defaults" {
            if endpoints.is_empty() {
                endpoints.extend(
                    DEFAULT_ENDPOINTS
                        .iter()
                        .map(|endpoint| (*endpoint).to_owned()),
                );
            }
            endpoints.insert(0, endpoint);
            legacy = None;
        } else {
            endpoints.clear();
            legacy = Some(endpoint);
        }
    }
    let mut expanded = Vec::new();
    for endpoint in endpoints.drain(..) {
        if endpoint == "flex://defaults" {
            expanded.extend(
                DEFAULT_ENDPOINTS
                    .iter()
                    .map(|endpoint| (*endpoint).to_owned()),
            );
        } else {
            expanded.push(endpoint);
        }
    }
    EndpointConfiguration {
        modern: expanded,
        legacy: legacy.map(|endpoint| endpoint.trim_end_matches('/').to_owned()),
    }
}

async fn fetch_cached(
    client: Arc<HttpClient>,
    cache_dir: PathBuf,
    cache_read_only: bool,
    url: String,
) -> Result<String> {
    let cache_file = cache_dir.join(cache_key(&url));
    let cached = std::fs::read(&cache_file)
        .ok()
        .and_then(|contents| serde_json::from_slice::<CachedResponse>(&contents).ok());
    let mut request_options = HttpRequestOptions::default();
    if let Some(cached) = &cached {
        if let Some(etag) = &cached.etag {
            request_options = request_options.with_header("if-none-match", etag)?;
        }
        if let Some(last_modified) = &cached.last_modified {
            request_options = request_options.with_header("if-modified-since", last_modified)?;
        }
    }

    match client.get_with_options(&url, &request_options).await {
        Ok(response) => {
            if response.status() == reqwest::StatusCode::NOT_MODIFIED {
                return cached
                    .map(|cached| cached.body)
                    .context("Server returned 304 without cached Symfony recipe data");
            }
            let etag = response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let last_modified = response
                .headers()
                .get(reqwest::header::LAST_MODIFIED)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let body = response
                .text()
                .await
                .context("Failed to read recipe response")?;
            serde_json::from_str::<Value>(&body)
                .with_context(|| format!("Invalid JSON downloaded from {url}"))?;
            if !cache_read_only {
                std::fs::create_dir_all(&cache_dir)
                    .with_context(|| format!("Failed to create {}", cache_dir.display()))?;
                let cache = CachedResponse {
                    body: body.clone(),
                    etag,
                    last_modified,
                };
                std::fs::write(&cache_file, serde_json::to_vec(&cache)?)
                    .with_context(|| format!("Failed to write {}", cache_file.display()))?;
            }
            Ok(body)
        }
        Err(error) => {
            if let Some(cached) = cached {
                crate::warnln!(
                    "Warning: {url} could not be loaded ({error}); using cached Symfony recipe data that may be out of date"
                );
                Ok(cached.body)
            } else {
                Err(error.into())
            }
        }
    }
}

fn cache_key(url: &str) -> String {
    let without_prefix = url
        .strip_prefix("https://raw.githubusercontent.com/")
        .unwrap_or(url);
    let key = without_prefix
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '.' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    if key.len() <= 140 {
        key
    } else {
        format!("{:x}", Md5::digest(url.as_bytes()))
    }
}

fn recipe_url(
    endpoint: &Endpoint,
    package: &str,
    version: &str,
    job: RecipeJob,
    lock: &super::lock::FlexLock,
) -> Result<String> {
    if job == RecipeJob::Uninstall {
        if let (Some(template), Some(reference)) = (
            endpoint.index.links.archived_recipes_template.as_deref(),
            lock.get(package)
                .and_then(|entry| entry.pointer("/recipe/ref"))
                .and_then(Value::as_str),
        ) {
            return Ok(resolve_template(
                &endpoint.url,
                template,
                endpoint
                    .index
                    .links
                    .archived_recipes_template_relative
                    .as_deref(),
            )
            .replace("{package_dotted}", &package.replace('/', "."))
            .replace("{ref}", reference));
        }
    }
    let template = resolve_template(
        &endpoint.url,
        &endpoint.index.links.recipe_template,
        endpoint.index.links.recipe_template_relative.as_deref(),
    );
    if template.is_empty() {
        bail!("Recipe endpoint {} has no recipe template", endpoint.url);
    }
    Ok(template
        .replace("{package_dotted}", &package.replace('/', "."))
        .replace("{package}", package)
        .replace("{version}", version))
}

fn resolve_template(endpoint: &str, template: &str, relative: Option<&str>) -> String {
    if let Some(relative) = relative {
        if let Some((base, _)) = endpoint.rsplit_once('/') {
            return format!("{base}/{relative}");
        }
    }
    template.to_owned()
}

fn decode_recipe(
    endpoint: &Endpoint,
    package: Arc<Package>,
    job: RecipeJob,
    recipe_version: &str,
    body: &str,
) -> Result<Recipe> {
    let response: Value = serde_json::from_str(body)?;
    let data = response
        .get("manifests")
        .and_then(Value::as_object)
        .and_then(|manifests| manifests.get(&package.name))
        .and_then(Value::as_object)
        .with_context(|| format!("Recipe response does not contain {}", package.name))?;
    let manifest = data
        .get("manifest")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut files = IndexMap::new();
    if let Some(response_files) = data.get("files").and_then(Value::as_object) {
        for (name, file) in response_files {
            let contents = match file.get("contents") {
                Some(Value::Array(lines)) => lines
                    .iter()
                    .map(|line| line.as_str().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n"),
                Some(Value::String(encoded)) => String::from_utf8(
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .with_context(|| {
                            format!("Invalid base64 contents for recipe file {name}")
                        })?,
                )
                .with_context(|| format!("Recipe file {name} is not UTF-8"))?,
                _ => String::new(),
            };
            files.insert(
                name.clone(),
                RecipeFile {
                    contents,
                    executable: file
                        .get("executable")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                },
            );
        }
    }
    let reference = data.get("ref").and_then(Value::as_str).unwrap_or_default();
    let lock = serde_json::json!({
        "version": package_lock_version(package.pretty_version()),
        "recipe": {
            "repo": endpoint.index.links.repository,
            "branch": endpoint.index.branch,
            "version": recipe_version,
            "ref": reference,
        }
    });
    let origin = endpoint
        .index
        .links
        .origin_template
        .replace("{package}", &package.name)
        .replace("{version}", recipe_version);
    let name = package.name.clone();
    Ok(Recipe {
        package,
        name,
        job,
        manifest,
        files,
        lock,
        origin,
        is_contrib: endpoint.index.is_contrib,
    })
}

fn decode_legacy_recipe(
    package: Arc<Package>,
    job: RecipeJob,
    body: &str,
) -> Result<Option<Recipe>> {
    let response: Value = serde_json::from_str(body)?;
    let Some(data) = response
        .get("manifests")
        .and_then(Value::as_object)
        .and_then(|manifests| manifests.get(&package.name))
        .and_then(Value::as_object)
    else {
        return Ok(None);
    };
    let manifest = data
        .get("manifest")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut files = IndexMap::new();
    if let Some(response_files) = data.get("files").and_then(Value::as_object) {
        for (name, file) in response_files {
            let contents = match file.get("contents") {
                Some(Value::Array(lines)) => lines
                    .iter()
                    .map(|line| line.as_str().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n"),
                Some(Value::String(encoded)) => String::from_utf8(
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .with_context(|| {
                            format!("Invalid base64 contents for recipe file {name}")
                        })?,
                )
                .with_context(|| format!("Recipe file {name} is not UTF-8"))?,
                _ => String::new(),
            };
            files.insert(
                name.clone(),
                RecipeFile {
                    contents,
                    executable: file
                        .get("executable")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                },
            );
        }
    }
    let lock = response
        .get("locks")
        .and_then(Value::as_object)
        .and_then(|locks| locks.get(&package.name))
        .cloned()
        .unwrap_or_else(
            || serde_json::json!({"version": package_lock_version(package.pretty_version())}),
        );
    let name = package.name.clone();
    Ok(Some(Recipe {
        package,
        name,
        job,
        manifest,
        files,
        lock,
        origin: data
            .get("origin")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        is_contrib: data
            .get("is_contrib")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }))
}

fn generated_bundle_recipe(package: Arc<Package>, job: RecipeJob) -> Option<Recipe> {
    if package.package_type.as_str() != "symfony-bundle"
        && package.package_type.as_str() != "sylius-plugin"
    {
        return None;
    }
    let autoload = package.autoload.as_ref()?;
    let namespace = autoload
        .psr4
        .keys()
        .chain(autoload.psr0.keys())
        .next()?
        .trim_matches('\\');
    let suffix = namespace.rsplit('\\').next()?;
    let class_suffix = if suffix.ends_with("Bundle") || suffix.ends_with("Plugin") {
        suffix.to_owned()
    } else {
        format!("{suffix}Bundle")
    };
    let class = format!("{namespace}\\{class_suffix}");
    let environments = vec![Value::String("all".to_owned())];
    let manifest = serde_json::json!({"bundles": {class: environments}})
        .as_object()
        .cloned()
        .unwrap_or_default();
    let name = package.name.clone();
    let version = package.pretty_version().to_owned();
    Some(Recipe {
        package,
        name: name.clone(),
        job,
        manifest,
        files: IndexMap::new(),
        lock: serde_json::json!({"version": version}),
        origin: format!("{name}:{version}@auto-generated recipe"),
        is_contrib: false,
    })
}

#[cfg(test)]
fn select_recipe_version(versions: &[String], package_version: &str) -> Option<String> {
    compatible_recipe_versions(versions, package_version)
        .into_iter()
        .next()
}

fn compatible_recipe_versions(versions: &[String], package_version: &str) -> Vec<String> {
    let package = numeric_version(package_version);
    let mut versions = versions
        .iter()
        .filter(|version| numeric_version(version) <= package)
        .cloned()
        .collect::<Vec<_>>();
    versions.sort_by_key(|version| std::cmp::Reverse(numeric_version(version)));
    versions
}

fn recipe_conflicts(recipe: &Recipe, installed_versions: &HashMap<String, String>) -> bool {
    recipe
        .manifest
        .get("conflict")
        .and_then(Value::as_object)
        .is_some_and(|conflicts| {
            conflicts.iter().any(|(name, constraint)| {
                installed_versions.get(name).is_some_and(|version| {
                    constraint.as_str().is_some_and(|constraint| {
                        riff_semver::Semver::satisfies(version, constraint)
                    })
                })
            })
        })
}

fn numeric_version(version: &str) -> (u64, u64, u64) {
    let mut numbers = version
        .trim_start_matches(['v', 'V'])
        .split(|character: char| !character.is_ascii_digit())
        .filter(|piece| !piece.is_empty())
        .take(3)
        .map(|piece| piece.parse::<u64>().unwrap_or_default());
    (
        numbers.next().unwrap_or_default(),
        numbers.next().unwrap_or(9_999_999),
        numbers.next().unwrap_or_default(),
    )
}

fn package_lock_version(version: &str) -> String {
    let (major, minor, _) = numeric_version(version);
    format!("{major}.{minor}")
}

fn recipe_order(recipe: &Recipe) -> (u8, &str) {
    let priority = if recipe.name == super::PACKAGE_NAME {
        0
    } else if recipe.package.package_type.as_str() == "symfony-pack" {
        1
    } else if recipe.package.package_type.as_str() == "metapackage" {
        2
    } else if recipe.name == "symfony/framework-bundle" {
        3
    } else {
        4
    };
    (priority, &recipe.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_empty_alias_list_from_contrib_index() {
        let index: EndpointIndex =
            serde_json::from_str(r#"{"aliases":[],"recipes":{},"versions":[],"_links":{}}"#)
                .unwrap();

        assert!(index.aliases.as_array().is_some_and(Vec::is_empty));
    }

    #[test]
    fn recognizes_legacy_recipe_endpoints() {
        let manifest: RiffManifest = serde_json::from_value(serde_json::json!({
            "extra": {"symfony": {"endpoint": "https://example.test/flex"}}
        }))
        .unwrap();
        let endpoints = endpoint_configuration_with_env(&manifest, None);
        assert!(endpoints.modern.is_empty());
        assert_eq!(
            endpoints.legacy.as_deref(),
            Some("https://example.test/flex")
        );
    }

    #[test]
    fn selects_newest_compatible_recipe() {
        let versions = vec!["2.4".into(), "6.4".into(), "7.3".into(), "8.1".into()];
        assert_eq!(
            select_recipe_version(&versions, "v8.0.2"),
            Some("7.3".into())
        );
        assert_eq!(
            select_recipe_version(&versions, "8.1.0"),
            Some("8.1".into())
        );
        assert_eq!(select_recipe_version(&versions, "1.0.0"), None);
    }

    #[test]
    fn long_cache_keys_are_md5_hashed() {
        let url = format!("https://example.com/{}", "x".repeat(180));
        assert_eq!(cache_key(&url).len(), 32);
        assert!(!cache_key("https://example.com/a.json").is_empty());
    }
}
