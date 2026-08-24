mod add;
pub mod commands;
mod install;
pub mod platform;
mod remove;
mod update;

use std::io;
use std::path::PathBuf;

use anyhow::Result;

use platform::AppContext;

#[derive(Debug, usage_rs::Cli)]
#[usage(
    name = "composer-rs",
    bin = "composer-rs",
    version,
    about = "A fast, standalone Composer-compatible package manager",
    completion,
    disable_help_subcommand = true
)]
struct Cli {
    /// PHP executable used for platform detection and @php scripts
    #[usage(long, global, value_name = "PATH")]
    php: Option<PathBuf>,

    #[usage(subcommand)]
    command: Commands,
}

#[derive(Debug, usage_rs::Subcommands)]
enum Commands {
    Install(install::InstallArgs),
    Update(update::UpdateArgs),
    #[usage(name = "require", alias = "add")]
    Require(add::AddArgs),
    Remove(remove::RemoveArgs),
    DumpAutoload(commands::dump_autoload::DumpAutoloadArgs),
    #[usage(name = "run", alias = "run-script")]
    Run(commands::run::RunArgs),
    #[usage(name = "show", alias = "info")]
    Show(commands::show::ShowArgs),
    #[usage(name = "why", alias = "depends")]
    Why(commands::why::WhyArgs),
    #[usage(name = "why-not", alias = "prohibits")]
    WhyNot(commands::why::WhyArgs),
    Outdated(commands::outdated::OutdatedArgs),
    Audit(commands::audit::AuditArgs),
    Validate(commands::validate::ValidateArgs),
    Config(commands::config::ConfigArgs),
    Status(commands::status::StatusArgs),
    CheckPlatformReqs(commands::check_platform_reqs::CheckPlatformReqsArgs),
    Completion(CompletionArgs),
}

impl Commands {
    fn needs_parallel_runtime(&self) -> bool {
        match self {
            Self::Install(args) => !args.dry_run,
            Self::Update(args) => !args.dry_run,
            Self::Require(_) | Self::Remove(_) | Self::Outdated(_) | Self::Audit(_) => true,
            Self::Show(args) => args.available || args.latest || args.outdated,
            _ => false,
        }
    }
}

#[derive(Debug, usage_rs::Args)]
struct CompletionArgs {
    /// Shell to generate completion for
    #[usage(arg, name = "SHELL")]
    shell: String,
}

pub fn run() -> i32 {
    // Parse first so help, version, and argument errors never pay for an async runtime.
    let cli = Cli::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let mut runtime = if cli.command.needs_parallel_runtime() {
        let mut runtime = tokio::runtime::Builder::new_multi_thread();
        if std::env::var_os("TOKIO_WORKER_THREADS").is_none() {
            runtime.worker_threads(2);
        }
        runtime
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    match runtime
        .enable_all()
        .build()
        .and_then(|runtime| runtime.block_on(run_async(cli)).map_err(io::Error::other))
    {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error:#}");
            1
        }
    }
}

async fn run_async(cli: Cli) -> Result<i32> {
    let context = AppContext::from_sources(cli.php)?;

    match cli.command {
        Commands::Install(args) => install::execute(args, &context).await,
        Commands::Update(args) => update::execute(args, &context).await,
        Commands::Require(args) => add::execute(args, &context).await,
        Commands::Remove(args) => remove::execute(args, &context).await,
        Commands::DumpAutoload(args) => commands::dump_autoload::execute(args, &context).await,
        Commands::Run(args) => commands::run::execute(args, &context).await,
        Commands::Show(args) => commands::show::execute(args, &context).await,
        Commands::Why(args) => commands::why::execute(args, false).await,
        Commands::WhyNot(args) => commands::why::execute(args, true).await,
        Commands::Outdated(args) => commands::outdated::execute(args, &context).await,
        Commands::Audit(args) => commands::audit::execute(args).await,
        Commands::Validate(args) => commands::validate::execute(args).await,
        Commands::Config(args) => commands::config::execute(args).await,
        Commands::Status(args) => commands::status::execute(args, &context).await,
        Commands::CheckPlatformReqs(args) => {
            commands::check_platform_reqs::execute(args, &context).await
        }
        Commands::Completion(args) => {
            let shell = usage_rs::complete::Shell::from_name(&args.shell)
                .ok_or_else(|| anyhow::anyhow!("Unsupported shell '{}'", args.shell))?;
            print!(
                "{}",
                Cli::app()
                    .name("composer-rs")
                    .bin("composer-rs")
                    .completion_app()
                    .completion_script(shell)
            );
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(args: &[&str]) -> Commands {
        let args = args
            .iter()
            .copied()
            .map(std::ffi::OsStr::new)
            .collect::<Vec<_>>();
        Cli::parse_from(&args).unwrap().command
    }

    fn cli(args: &[&str]) -> Cli {
        let args = args
            .iter()
            .copied()
            .map(std::ffi::OsStr::new)
            .collect::<Vec<_>>();
        Cli::parse_from(&args).unwrap()
    }

    #[test]
    fn dry_run_update_uses_current_thread_runtime() {
        assert!(!command(&["update", "--dry-run"]).needs_parallel_runtime());
    }

    #[test]
    fn installing_update_keeps_parallel_runtime() {
        assert!(command(&["update"]).needs_parallel_runtime());
    }

    #[test]
    fn global_php_and_require_alias_are_preserved() {
        let cli = cli(&["add", "vendor/package:^1.0", "--php", "/opt/php"]);
        assert_eq!(cli.php, Some(PathBuf::from("/opt/php")));
        let Commands::Require(args) = cli.command else {
            panic!("add alias did not select require");
        };
        assert_eq!(args.packages, ["vendor/package:^1.0"]);
    }

    #[test]
    fn compact_verbosity_flags_are_counted() {
        let Commands::Install(args) = command(&["install", "-vvv"]) else {
            panic!("install command was not parsed");
        };
        assert_eq!(args.verbose, 3);
    }

    #[test]
    fn run_forwards_hyphenated_arguments() {
        let Commands::Run(args) = command(&["run-script", "build", "--", "--release"]) else {
            panic!("run-script alias did not select run");
        };
        assert_eq!(args.script.as_deref(), Some("build"));
        assert_eq!(args.args, ["--release"]);
    }

    #[test]
    fn required_packages_and_outdated_conflicts_are_enforced() {
        let require = [std::ffi::OsStr::new("require")];
        assert!(Cli::parse_from(&require).is_err());

        let conflict = [
            std::ffi::OsStr::new("outdated"),
            std::ffi::OsStr::new("--major-only"),
            std::ffi::OsStr::new("--minor-only"),
        ];
        assert!(Cli::parse_from(&conflict).is_err());
    }
}
