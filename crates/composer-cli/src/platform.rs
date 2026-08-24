use std::ffi::OsString;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use sonata_core::{config::Config, Package, PlatformSnapshot, RuntimeContext};
use wait_timeout::ChildExt;

const PLATFORM_PROBE: &str = include_str!("platform_probe.php");
const MINIMUM_PHP_VERSION_ID: u64 = 70205;
const PLATFORM_CACHE_VERSION: u8 = 1;

#[derive(Debug, Deserialize)]
struct ProbeOutput {
    #[serde(flatten)]
    snapshot: PlatformSnapshot,
    #[serde(default, rename = "_composer_rs_tracked_paths")]
    tracked_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
struct MetadataState {
    len: u64,
    modified_ns: u64,
    is_dir: bool,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
struct FileState {
    requested: PathBuf,
    resolved: PathBuf,
    metadata: Option<MetadataState>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
struct ProbeIdentity {
    php: FileState,
    current_dir: PathBuf,
    environment: Vec<(String, String)>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedPlatform {
    version: u8,
    identity: ProbeIdentity,
    tracked_paths: Vec<FileState>,
    snapshot: PlatformSnapshot,
}

#[derive(Debug)]
pub struct AppContext {
    runtime: RuntimeContext,
    platform: OnceCell<Result<PlatformSnapshot, String>>,
}

impl AppContext {
    pub fn from_sources(cli_php: Option<PathBuf>) -> Result<Self> {
        let php_binary = select_php_binary(
            cli_php,
            std::env::var_os("COMPOSER_RS_PHP"),
            std::env::var_os("PHP_BINARY"),
        );
        let composer_binary = std::env::current_exe().context("Failed to locate sonata")?;
        Ok(Self {
            runtime: RuntimeContext::new(php_binary, composer_binary),
            platform: OnceCell::new(),
        })
    }

    pub fn runtime(&self) -> &RuntimeContext {
        &self.runtime
    }

    pub fn snapshot(&self) -> Result<&PlatformSnapshot> {
        self.platform
            .get_or_init(|| probe(&self.runtime.php_binary).map_err(|error| format!("{error:#}")))
            .as_ref()
            .map_err(|message| anyhow::anyhow!(message.clone()))
    }

    pub fn packages(&self, config: &Config) -> Result<Vec<Package>> {
        self.snapshot()?.to_packages(&config.platform)
    }
}

fn select_php_binary(
    cli: Option<PathBuf>,
    composer_rs_php: Option<OsString>,
    php_binary: Option<OsString>,
) -> PathBuf {
    cli.or_else(|| composer_rs_php.map(PathBuf::from))
        .or_else(|| php_binary.map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("php"))
}

fn probe(php_binary: &Path) -> Result<PlatformSnapshot> {
    let cache_dir = platform_cache_dir();
    probe_with_cache(php_binary, Duration::from_secs(10), cache_dir.as_deref())
}

#[cfg(test)]
fn probe_with_timeout(php_binary: &Path, timeout: Duration) -> Result<PlatformSnapshot> {
    Ok(run_probe(php_binary, timeout)?.snapshot)
}

fn probe_with_cache(
    php_binary: &Path,
    timeout: Duration,
    cache_dir: Option<&Path>,
) -> Result<PlatformSnapshot> {
    let identity = probe_identity(php_binary);
    let cache_path = cache_dir.map(|directory| cache_path(directory, &identity));

    if let Some(path) = &cache_path {
        if let Some(snapshot) = read_cached_platform(path, &identity) {
            return Ok(snapshot);
        }
    }

    let output = run_probe(php_binary, timeout)?;
    if let Some(path) = cache_path {
        let mut tracked_paths: Vec<_> = output
            .tracked_paths
            .iter()
            .map(|path| file_state(path))
            .collect();
        tracked_paths.sort_by(|left, right| left.requested.cmp(&right.requested));
        tracked_paths.dedup_by(|left, right| left.requested == right.requested);
        let cached = CachedPlatform {
            version: PLATFORM_CACHE_VERSION,
            identity,
            tracked_paths,
            snapshot: output.snapshot.clone(),
        };
        if let Err(error) = write_cached_platform(&path, &cached) {
            log::debug!(
                "Failed to write platform cache {}: {error:#}",
                path.display()
            );
        }
    }

    Ok(output.snapshot)
}

fn run_probe(php_binary: &Path, timeout: Duration) -> Result<ProbeOutput> {
    let mut child = Command::new(php_binary)
        .arg("-d")
        .arg("display_errors=stderr")
        .arg("-d")
        .arg("html_errors=0")
        .arg("-r")
        .arg(PLATFORM_PROBE)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to execute PHP at {}", php_binary.display()))?;

    if child.wait_timeout(timeout)?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        bail!(
            "PHP platform probe timed out after {} seconds using {}",
            timeout.as_secs_f64(),
            php_binary.display()
        );
    }
    let output = child.wait_with_output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "PHP platform probe failed using {} (exit {}): {}",
            php_binary.display(),
            output.status,
            stderr.trim()
        );
    }

    let output: ProbeOutput = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "PHP at {} returned invalid platform data",
            php_binary.display()
        )
    })?;
    if output.snapshot.php_version_id < MINIMUM_PHP_VERSION_ID {
        bail!(
            "sonata requires PHP >= 7.2.5; {} reports {}",
            php_binary.display(),
            output.snapshot.php_version
        );
    }
    Ok(output)
}

fn platform_cache_dir() -> Option<PathBuf> {
    if std::env::var("COMPOSER_RS_NO_PLATFORM_CACHE").is_ok_and(|value| value != "0") {
        return None;
    }
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })
        .map(|directory| directory.join("sonata/platform"))
}

fn probe_identity(php_binary: &Path) -> ProbeIdentity {
    let resolved_php = resolve_executable(php_binary);
    let mut environment: Vec<_> = std::env::vars()
        .filter(|(name, _)| {
            name == "PATH"
                || name.starts_with("PHP")
                || name.starts_with("COMPOSER_RS_PHP")
                || name == "LD_LIBRARY_PATH"
                || name == "LD_PRELOAD"
                || name.starts_with("DYLD_")
        })
        .collect();
    environment.sort();

    ProbeIdentity {
        php: file_state(&resolved_php),
        current_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        environment,
    }
}

fn resolve_executable(binary: &Path) -> PathBuf {
    if binary.is_absolute() || binary.components().count() > 1 {
        return binary.to_path_buf();
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(binary))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| binary.to_path_buf())
}

fn file_state(path: &Path) -> FileState {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let metadata = std::fs::metadata(path).ok().map(|metadata| MetadataState {
        len: metadata.len(),
        modified_ns: metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
            .unwrap_or_default(),
        is_dir: metadata.is_dir(),
    });
    FileState {
        requested: path.to_path_buf(),
        resolved,
        metadata,
    }
}

fn cache_path(directory: &Path, identity: &ProbeIdentity) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    identity.hash(&mut hasher);
    directory.join(format!(
        "v{PLATFORM_CACHE_VERSION}-{:016x}.json",
        hasher.finish()
    ))
}

fn read_cached_platform(path: &Path, identity: &ProbeIdentity) -> Option<PlatformSnapshot> {
    let content = std::fs::read(path).ok()?;
    let cached: CachedPlatform = serde_json::from_slice(&content).ok()?;
    if cached.version != PLATFORM_CACHE_VERSION || cached.identity != *identity {
        return None;
    }
    if cached
        .tracked_paths
        .iter()
        .any(|state| file_state(&state.requested) != *state)
    {
        return None;
    }
    Some(cached.snapshot)
}

fn write_cached_platform(path: &Path, cached: &CachedPlatform) -> Result<()> {
    let directory = path
        .parent()
        .context("Platform cache path has no parent directory")?;
    std::fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(temporary.as_file_mut(), cached)?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("Failed to persist platform cache")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn executable(path: &Path, content: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, content).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn php_path_precedence_is_explicit_and_stable() {
        assert_eq!(
            select_php_binary(
                Some(PathBuf::from("cli")),
                Some("sonata-env".into()),
                Some("php-env".into())
            ),
            PathBuf::from("cli")
        );
        assert_eq!(
            select_php_binary(None, Some("sonata-env".into()), Some("php-env".into())),
            PathBuf::from("sonata-env")
        );
        assert_eq!(
            select_php_binary(None, None, Some("php-env".into())),
            PathBuf::from("php-env")
        );
        assert_eq!(select_php_binary(None, None, None), PathBuf::from("php"));
    }

    #[test]
    fn parses_probe_json() {
        let value = br#"{"php_version":"8.3.1","php_version_id":80301,"int_size":8,"zts":false,"debug":false,"ipv6":true,"extensions":{"json":"8.3.1"},"libraries":{"openssl":"3.0.0"}}"#;
        let snapshot: PlatformSnapshot = serde_json::from_slice(value).unwrap();
        assert_eq!(snapshot.php_version_id, 80301);
        assert_eq!(snapshot.extensions["json"], "8.3.1");
    }

    #[cfg(unix)]
    #[test]
    fn platform_probe_runs_once_per_context() {
        let directory = tempfile::tempdir().unwrap();
        let php = directory.path().join("php");
        let count = directory.path().join("count");
        executable(
            &php,
            &format!(
                "#!/bin/sh\nprintf x >> '{}'\nprintf '%s' '{{\"php_version\":\"8.3.1\",\"php_version_id\":80301,\"int_size\":8,\"zts\":false,\"debug\":false,\"ipv6\":true,\"extensions\":{{}},\"libraries\":{{}}}}'\n",
                count.display()
            ),
        );
        let context = AppContext::from_sources(Some(php)).unwrap();
        context.snapshot().unwrap();
        context.snapshot().unwrap();
        assert_eq!(std::fs::read_to_string(count).unwrap(), "x");
    }

    #[cfg(unix)]
    #[test]
    fn platform_cache_reuses_unchanged_probe() {
        let directory = tempfile::tempdir().unwrap();
        let php = directory.path().join("php");
        let count = directory.path().join("count");
        executable(
            &php,
            &format!(
                "#!/bin/sh\nprintf x >> '{}'\nprintf '%s' '{{\"php_version\":\"8.3.1\",\"php_version_id\":80301,\"int_size\":8,\"zts\":false,\"debug\":false,\"ipv6\":true,\"extensions\":{{}},\"libraries\":{{}}}}'\n",
                count.display()
            ),
        );
        let cache = directory.path().join("cache");
        probe_with_cache(&php, Duration::from_secs(1), Some(&cache)).unwrap();
        probe_with_cache(&php, Duration::from_secs(1), Some(&cache)).unwrap();
        assert_eq!(std::fs::read_to_string(count).unwrap(), "x");
    }

    #[cfg(unix)]
    #[test]
    fn platform_cache_invalidates_changed_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let php = directory.path().join("php");
        let count = directory.path().join("count");
        let configuration = directory.path().join("php.ini");
        std::fs::write(&configuration, "first").unwrap();
        executable(
            &php,
            &format!(
                "#!/bin/sh\nprintf x >> '{}'\nprintf '%s' '{{\"php_version\":\"8.3.1\",\"php_version_id\":80301,\"int_size\":8,\"zts\":false,\"debug\":false,\"ipv6\":true,\"extensions\":{{}},\"libraries\":{{}},\"_composer_rs_tracked_paths\":[\"{}\"]}}'\n",
                count.display(),
                configuration.display()
            ),
        );
        let cache = directory.path().join("cache");
        probe_with_cache(&php, Duration::from_secs(1), Some(&cache)).unwrap();
        std::fs::write(&configuration, "second and different").unwrap();
        probe_with_cache(&php, Duration::from_secs(1), Some(&cache)).unwrap();
        assert_eq!(std::fs::read_to_string(count).unwrap(), "xx");
    }

    #[cfg(unix)]
    #[test]
    fn probe_reports_invalid_output_and_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let invalid = directory.path().join("invalid-php");
        executable(&invalid, "#!/bin/sh\nprintf not-json\n");
        assert!(probe_with_timeout(&invalid, Duration::from_secs(1))
            .unwrap_err()
            .to_string()
            .contains("invalid platform data"));

        let slow = directory.path().join("slow-php");
        executable(&slow, "#!/bin/sh\nsleep 1\n");
        assert!(probe_with_timeout(&slow, Duration::from_millis(20))
            .unwrap_err()
            .to_string()
            .contains("timed out"));
    }
}
