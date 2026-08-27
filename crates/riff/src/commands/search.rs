use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use riff_core::cache::runtime_cache_dir;
use riff_core::config::Config;
use riff_core::json::{Repository as JsonRepository, RiffManifest};
use riff_core::repository::{
    ComposerRepository, InstalledRepository, PlatformRepository, Repository, RepositoryManager,
    SearchMode, SearchResult,
};

use crate::CommandContext;

#[derive(Debug, usage_rs::Args)]
pub struct SearchArgs {
    /// Search terms
    #[usage(value_name = "TOKENS", required)]
    pub tokens: Vec<String>,

    /// Search package names only
    #[usage(short = 'N', long)]
    pub only_name: bool,

    /// Search vendor names only
    #[usage(short = 'O', long)]
    pub only_vendor: bool,

    /// Restrict results to this package type
    #[usage(short = 't', long = "type")]
    pub package_type: Option<String>,

    /// Output format: text or json
    #[usage(
        short = 'f',
        long,
        default = "text",
        complete = crate::commands::completion::complete_output_format
    )]
    pub format: String,

    /// Increase repository search diagnostics (-v, -vv, -vvv)
    #[usage(short = 'v', long, count)]
    pub verbose: u8,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub async fn execute(args: SearchArgs, context: &CommandContext) -> Result<i32> {
    if args.only_name && args.only_vendor {
        bail!("--only-name and --only-vendor cannot be used together");
    }
    if !matches!(args.format.as_str(), "text" | "json") {
        riff_core::errln!(
            context.output(),
            "Unsupported format \"{}\". See help for supported formats.",
            args.format
        );
        return Ok(1);
    }

    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let manifest_path = working_dir.join("composer.json");
    let manifest: RiffManifest = serde_json::from_slice(
        &std::fs::read(&manifest_path)
            .with_context(|| format!("Failed to read {}", manifest_path.display()))?,
    )?;
    let config = Config::build(Some(&working_dir), true)?;
    let mode = if args.only_vendor {
        SearchMode::Vendor
    } else if args.only_name {
        SearchMode::Name
    } else {
        SearchMode::Fulltext
    };
    let query = args.tokens.join(" ");

    let installed = Arc::new(InstalledRepository::new(config.get_vendor_dir()));
    installed.load().await.map_err(anyhow::Error::msg)?;
    let platform_packages = context.packages(&config)?;
    let mut repositories: Vec<Arc<dyn Repository>> = vec![
        installed,
        Arc::new(PlatformRepository::from_packages(platform_packages)),
    ];
    let mut manager = RepositoryManager::new().with_output(context.output().clone());
    for repository in manifest.repositories.as_vec() {
        manager.add_from_json_repository_at(&repository, &working_dir);
    }
    repositories.extend(manager.repositories().iter().cloned());
    if !packagist_is_disabled(&manifest) {
        repositories.push(Arc::new(ComposerRepository::packagist_with_cache(
            runtime_cache_dir(),
        )));
    }

    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for repository in repositories {
        let found = repository
            .search_with_type(&query, mode, args.package_type.as_deref())
            .await;
        if args.verbose > 2 {
            print_repository_diagnostic(repository.as_ref(), found.len(), context).await;
        }
        for result in found {
            if seen.insert(result.name.clone()) {
                results.push(result);
            }
        }
    }

    if args.format == "json" {
        print_json(&results, context)?;
    } else {
        print_text(&results, context);
    }
    Ok(0)
}

fn packagist_is_disabled(manifest: &RiffManifest) -> bool {
    manifest.repositories.as_vec().iter().any(|repository| {
        matches!(
            repository,
            JsonRepository::NamedDisabled { name, disabled: false }
                if name == "packagist.org"
        ) || matches!(repository, JsonRepository::Disabled(false))
    })
}

async fn print_repository_diagnostic(
    repository: &dyn Repository,
    found: usize,
    context: &CommandContext,
) {
    let count = repository.count().await;
    let description = match repository.name() {
        "installed" => format!(
            "installed array repo (defining {count} package{})",
            if count == 1 { "" } else { "s" }
        ),
        "platform" => "platform repo".to_owned(),
        name if name.starts_with("package") => format!(
            "package repo (defining {count} package{})",
            if count == 1 { "" } else { "s" }
        ),
        name => name.to_owned(),
    };
    riff_core::outln!(
        context.output(),
        "Searched {}, found {} result(s)",
        description,
        found
    );
}

fn print_json(results: &[SearchResult], context: &CommandContext) -> Result<()> {
    let values: Vec<_> = results
        .iter()
        .map(|result| {
            serde_json::json!({
                "name": result.name,
                "description": result.description,
            })
        })
        .collect();
    riff_core::outln!(
        context.output(),
        "{}",
        serde_json::to_string_pretty(&values)?
    );
    Ok(())
}

fn print_text(results: &[SearchResult], context: &CommandContext) {
    let width = results
        .iter()
        .map(|result| result.name.chars().count())
        .max()
        .unwrap_or_default()
        + 1;
    for result in results {
        let description = match (&result.abandoned, &result.description) {
            (Some(replacement), description) if replacement.is_empty() => format!(
                "! Abandoned !{}",
                description
                    .as_deref()
                    .map(|description| format!(" {description}"))
                    .unwrap_or_default()
            ),
            (Some(replacement), description) => format!(
                "! Abandoned: Use {replacement} instead !{}",
                description
                    .as_deref()
                    .map(|description| format!(" {description}"))
                    .unwrap_or_default()
            ),
            (None, Some(description)) => description.clone(),
            (None, None) => String::new(),
        };
        if description.is_empty() {
            riff_core::outln!(context.output(), "{}", result.name);
        } else {
            riff_core::outln!(context.output(), "{:<width$}{}", result.name, description);
        }
    }
}
