//! Diagnose project metadata and the package-manager runtime environment.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::HeaderMap;
use riff_core::json::{validate_composer_manifest, ManifestValidationOptions, RiffLockfile};

const PACKAGIST_HTTP_URL: &str = "http://repo.packagist.org/packages.json";
const PACKAGIST_HTTPS_URL: &str = "https://repo.packagist.org/packages.json";
const GITHUB_RATE_LIMIT_URL: &str = "https://api.github.com/rate_limit";

#[derive(Debug, usage_rs::Args)]
pub struct DiagnoseArgs {
    /// Skip checks which make network requests
    #[usage(long)]
    pub no_network: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CheckStatus {
    Ok(Option<String>),
    Warning(Vec<String>),
    Error(Vec<String>),
    Skipped(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiagnosticCheck {
    label: &'static str,
    status: CheckStatus,
}

impl DiagnosticCheck {
    fn ok(label: &'static str) -> Self {
        Self {
            label,
            status: CheckStatus::Ok(None),
        }
    }

    fn warning(label: &'static str, messages: Vec<String>) -> Self {
        Self {
            label,
            status: CheckStatus::Warning(messages),
        }
    }

    fn error(label: &'static str, message: impl Into<String>) -> Self {
        Self {
            label,
            status: CheckStatus::Error(vec![message.into()]),
        }
    }

    fn skipped(label: &'static str, reason: impl Into<String>) -> Self {
        Self {
            label,
            status: CheckStatus::Skipped(reason.into()),
        }
    }

    fn is_problem(&self) -> bool {
        matches!(self.status, CheckStatus::Warning(_) | CheckStatus::Error(_))
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DiagnosticReport {
    checks: Vec<DiagnosticCheck>,
}

impl DiagnosticReport {
    fn exit_code(&self) -> i32 {
        i32::from(self.checks.iter().any(DiagnosticCheck::is_problem))
    }

    fn render(&self, context: &crate::CommandContext) {
        for check in &self.checks {
            match &check.status {
                CheckStatus::Ok(detail) => {
                    let suffix = detail
                        .as_deref()
                        .map(|detail| format!(" ({detail})"))
                        .unwrap_or_default();
                    riff_core::outln!(context.output(), "Checking {}: OK{}", check.label, suffix);
                }
                CheckStatus::Warning(messages) => {
                    riff_core::outln!(context.output(), "Checking {}: WARNING", check.label);
                    for message in messages {
                        riff_core::warnln!(context.output(), "{message}");
                    }
                }
                CheckStatus::Error(messages) => {
                    riff_core::outln!(context.output(), "Checking {}: FAIL", check.label);
                    for message in messages {
                        riff_core::errln!(context.output(), "{message}");
                    }
                }
                CheckStatus::Skipped(reason) => {
                    riff_core::outln!(
                        context.output(),
                        "Checking {}: SKIP ({reason})",
                        check.label
                    );
                }
            }
        }
    }
}

pub async fn execute(args: DiagnoseArgs, context: &crate::CommandContext) -> Result<i32> {
    let working_dir = args
        .working_dir
        .canonicalize()
        .context("Failed to resolve working directory")?;
    let mut report = DiagnosticReport {
        checks: project_checks(&working_dir),
    };
    report.checks.extend(network_checks(args.no_network).await);
    report.render(context);
    Ok(report.exit_code())
}

fn project_checks(working_dir: &Path) -> Vec<DiagnosticCheck> {
    let manifest_path = working_dir.join("composer.json");
    let manifest_content = match fs::read_to_string(&manifest_path) {
        Ok(content) => content,
        Err(error) => {
            return vec![DiagnosticCheck::error(
                "composer.json",
                format!("Failed to read {}: {error}", manifest_path.display()),
            )];
        }
    };
    let validation = validate_composer_manifest(
        &manifest_content,
        "composer.json",
        ManifestValidationOptions::default(),
    );
    let manifest_check = if !validation.errors.is_empty() {
        DiagnosticCheck {
            label: "composer.json",
            status: CheckStatus::Error(validation.errors),
        }
    } else {
        let warnings = validation
            .warnings
            .into_iter()
            .chain(validation.publish_errors)
            .collect::<Vec<_>>();
        if warnings.is_empty() {
            DiagnosticCheck::ok("composer.json")
        } else {
            DiagnosticCheck::warning("composer.json", warnings)
        }
    };

    let lock_path = working_dir.join("composer.lock");
    let lock_check = if !lock_path.exists() {
        DiagnosticCheck::skipped("composer.lock", "not found")
    } else {
        match fs::read_to_string(&lock_path) {
            Err(error) => DiagnosticCheck::error(
                "composer.lock",
                format!("Failed to read {}: {error}", lock_path.display()),
            ),
            Ok(content) => match serde_json::from_str::<RiffLockfile>(&content) {
                Err(error) => DiagnosticCheck::error(
                    "composer.lock",
                    format!("composer.lock does not contain valid JSON: {error}"),
                ),
                Ok(lock) if !lock.is_fresh(&manifest_content) => DiagnosticCheck::warning(
                    "composer.lock",
                    vec!["The lock file is not up to date with composer.json.".to_owned()],
                ),
                Ok(_) => DiagnosticCheck::ok("composer.lock"),
            },
        }
    };

    vec![manifest_check, lock_check]
}

async fn network_checks(skip: bool) -> Vec<DiagnosticCheck> {
    const LABELS: [(&str, &str); 3] = [
        (
            "http connectivity to packagist",
            "RIFF_DIAGNOSE_PACKAGIST_HTTP_URL",
        ),
        (
            "https connectivity to packagist",
            "RIFF_DIAGNOSE_PACKAGIST_HTTPS_URL",
        ),
        (
            "github.com rate limit",
            "RIFF_DIAGNOSE_GITHUB_RATE_LIMIT_URL",
        ),
    ];
    if skip {
        return LABELS
            .into_iter()
            .map(|(label, _)| DiagnosticCheck::skipped(label, "network checks disabled"))
            .collect();
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .user_agent("Riff diagnose")
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return LABELS
                .into_iter()
                .map(|(label, _)| DiagnosticCheck::error(label, error.to_string()))
                .collect();
        }
    };

    let http_url = diagnostic_url("RIFF_DIAGNOSE_PACKAGIST_HTTP_URL", PACKAGIST_HTTP_URL);
    let https_url = diagnostic_url("RIFF_DIAGNOSE_PACKAGIST_HTTPS_URL", PACKAGIST_HTTPS_URL);
    let github_url = diagnostic_url("RIFF_DIAGNOSE_GITHUB_RATE_LIMIT_URL", GITHUB_RATE_LIMIT_URL);
    let (http, https, github) = tokio::join!(
        probe_url(&client, "http connectivity to packagist", &http_url, false),
        probe_url(
            &client,
            "https connectivity to packagist",
            &https_url,
            false
        ),
        probe_url(&client, "github.com rate limit", &github_url, true),
    );
    vec![http, https, github]
}

fn diagnostic_url(variable: &str, default: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| default.to_owned())
}

async fn probe_url(
    client: &reqwest::Client,
    label: &'static str,
    url: &str,
    include_rate_limit: bool,
) -> DiagnosticCheck {
    match client.get(url).send().await {
        Ok(response) if response.status().is_success() => DiagnosticCheck {
            label,
            status: CheckStatus::Ok(
                include_rate_limit.then(|| github_rate_limit(response.headers())),
            ),
        },
        Ok(response) => DiagnosticCheck::error(
            label,
            format!(
                "{} returned HTTP {}",
                riff_core::url_utils::sanitize_url(url),
                response.status()
            ),
        ),
        Err(error) => DiagnosticCheck::error(
            label,
            format!(
                "{}: {}",
                riff_core::url_utils::sanitize_url(url),
                request_error_reason(&error)
            ),
        ),
    }
}

fn request_error_reason(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "request timed out"
    } else if error.is_connect() {
        "connection failed"
    } else if error.is_redirect() {
        "redirect failed"
    } else {
        "request failed"
    }
}

fn github_rate_limit(headers: &HeaderMap) -> String {
    headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .map(|remaining| format!("{remaining} requests remaining"))
        .unwrap_or_else(|| "rate limit available".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_checks_distinguish_warning_success_and_invalid_json() {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("composer.json"),
            r#"{"name":"foo/bar","description":"test pkg"}"#,
        )
        .unwrap();
        let warning = project_checks(project.path());
        assert!(matches!(warning[0].status, CheckStatus::Warning(_)));
        assert!(DiagnosticReport { checks: warning }.exit_code() == 1);

        fs::write(
            project.path().join("composer.json"),
            r#"{"name":"foo/bar","description":"test pkg","license":"MIT"}"#,
        )
        .unwrap();
        let success = project_checks(project.path());
        assert_eq!(success[0], DiagnosticCheck::ok("composer.json"));
        assert_eq!(DiagnosticReport { checks: success }.exit_code(), 0);

        fs::write(project.path().join("composer.json"), "{").unwrap();
        assert!(matches!(
            project_checks(project.path())[0].status,
            CheckStatus::Error(_)
        ));
    }
}
