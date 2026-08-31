mod add;
mod application;
pub mod commands;
mod context;
mod create_project;
mod env;
mod install;
pub mod platform;
mod remove;
mod startup;
mod update;

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

use anyhow::Result;

pub use context::CommandContext;
use platform::PhpPlatformDetector;
use riff_core::{AnsiMode, Output, OutputLevel, OutputOptions, OutputStream, Platform};

#[derive(Debug, usage_rs::Cli)]
#[usage(
    name = "riff",
    bin = "riff",
    version,
    about = "A fast, standalone Composer-compatible package manager",
    completion,
    disable_help_subcommand = true
)]
struct Cli {
    /// PHP executable used for platform detection and @php scripts
    #[usage(long, global, value_name = "PATH")]
    php: Option<PathBuf>,

    /// Output format: text (default) or newline-delimited JSON
    #[usage(long, global, default = "text")]
    output: String,

    /// Do not output informational or progress messages
    #[usage(short = 'q', long, global)]
    quiet: bool,

    /// Disable interactive progress output
    #[usage(long, global)]
    no_progress: bool,

    /// Force ANSI output
    #[usage(long, global)]
    ansi: bool,

    /// Disable ANSI output
    #[usage(long, global)]
    no_ansi: bool,

    #[usage(subcommand)]
    command: Commands,
}

#[derive(Debug, usage_rs::Subcommands)]
enum Commands {
    /// Shows a short description of Riff
    About(commands::about::AboutArgs),
    /// Creates an archive of this Composer package
    Archive(commands::archive::ArchiveArgs),
    /// Raises dependency lower bounds to installed versions
    Bump(commands::bump::BumpArgs),
    /// Clears Riff's internal package cache
    #[usage(name = "clear-cache", alias = "clearcache", alias = "cc")]
    ClearCache(commands::clear_cache::ClearCacheArgs),
    Install(install::InstallArgs),
    /// Uninstall and install matching packages again
    Reinstall(commands::reinstall::ReinstallArgs),
    Update(update::UpdateArgs),
    #[usage(name = "require", alias = "add")]
    Require(add::AddArgs),
    CreateProject(create_project::CreateProjectArgs),
    Remove(remove::RemoveArgs),
    /// Extract an installed dependency into an editable patch workspace
    Patch(commands::patch::PatchArgs),
    /// Generate and install a patch from an edit workspace
    PatchCommit(commands::patch::PatchCommitArgs),
    /// Remove native package patches and restore package contents
    PatchRemove(commands::patch::PatchRemoveArgs),
    /// Regenerate native and Composer-compatible patch locks
    #[usage(alias = "prl")]
    PatchesRelock(commands::patch::PatchesRelockArgs),
    /// Reinstall packages and reapply their current patches
    #[usage(alias = "prp")]
    PatchesRepatch(commands::patch::PatchesRepatchArgs),
    /// Validate patch declarations, locks, files, and installed state
    #[usage(alias = "pd")]
    PatchesDoctor(commands::patch::PatchesDoctorArgs),
    DumpAutoload(commands::dump_autoload::DumpAutoloadArgs),
    /// Executes a vendored binary or script
    Exec(commands::exec::ExecArgs),
    /// Discover how to help fund the maintenance of dependencies
    Fund(commands::fund::FundArgs),
    /// Show information about installed Symfony recipes
    #[usage(name = "recipes", alias = "symfony:recipes")]
    Recipes(commands::flex::RecipesArgs),
    /// Install or reinstall recipes for installed packages
    #[usage(
        name = "recipes:install",
        alias = "symfony:recipes:install",
        alias = "sync-recipes",
        alias = "symfony:sync-recipes",
        alias = "fix-recipes"
    )]
    RecipesInstall(commands::flex::RecipesInstallArgs),
    /// Update an installed Symfony recipe
    #[usage(name = "recipes:update", alias = "symfony:recipes:update")]
    RecipesUpdate(commands::flex::RecipesUpdateArgs),
    /// Compile dotenv files into .env.local.php
    #[usage(name = "dump-env", alias = "symfony:dump-env")]
    DumpEnv(commands::flex::DumpEnvArgs),
    /// Runs a command in Composer's global home directory
    Global(commands::global::GlobalArgs),
    /// Creates a basic composer.json file in the current directory
    Init(commands::init::InitArgs),
    /// Open or show a package repository URL or homepage
    #[usage(name = "browse", alias = "home")]
    Browse(commands::home::HomeArgs),
    #[usage(name = "run", alias = "run-script")]
    Run(commands::run::RunArgs),
    /// Searches for packages
    Search(commands::search::SearchArgs),
    #[usage(name = "show", alias = "info")]
    Show(commands::show::ShowArgs),
    /// Shows information about licenses of dependencies
    #[usage(name = "licenses", alias = "license")]
    Licenses(commands::licenses::LicensesArgs),
    /// Shows package suggestions
    #[usage(name = "suggests", alias = "suggest")]
    Suggests(commands::suggests::SuggestsArgs),
    #[usage(name = "why", alias = "depends")]
    Why(commands::why::WhyArgs),
    #[usage(name = "why-not", alias = "prohibits")]
    WhyNot(commands::why::WhyNotArgs),
    Outdated(commands::outdated::OutdatedArgs),
    Audit(commands::audit::AuditArgs),
    /// Checks platform settings, project files, and network connectivity
    Diagnose(commands::diagnose::DiagnoseArgs),
    Validate(commands::validate::ValidateArgs),
    Config(commands::config::ConfigArgs),
    /// Manage package repositories
    #[usage(name = "repository", alias = "repo")]
    Repository(commands::repository::RepositoryArgs),
    /// Manage custom dependency policies and their sources
    Policy(commands::policy::PolicyArgs),
    Status(commands::status::StatusArgs),
    CheckPlatformReqs(commands::check_platform_reqs::CheckPlatformReqsArgs),
    Completion(CompletionArgs),
}

impl Commands {
    fn needs_parallel_runtime(&self) -> bool {
        match self {
            Self::Install(args) => !args.dry_run,
            Self::Update(args) => !args.dry_run,
            Self::Reinstall(_)
            | Self::Require(_)
            | Self::CreateProject(_)
            | Self::Remove(_)
            | Self::Patch(_)
            | Self::PatchCommit(_)
            | Self::PatchRemove(_)
            | Self::PatchesRelock(_)
            | Self::PatchesRepatch(_)
            | Self::PatchesDoctor(_)
            | Self::Recipes(_)
            | Self::RecipesInstall(_)
            | Self::RecipesUpdate(_)
            | Self::Outdated(_)
            | Self::Audit(_) => true,
            Self::Show(args) => args.available || args.latest || args.outdated,
            _ => false,
        }
    }

    fn needs_platform(&self) -> bool {
        matches!(
            self,
            Self::Install(_)
                | Self::Update(_)
                | Self::Require(_)
                | Self::CreateProject(_)
                | Self::Remove(_)
                | Self::Patch(_)
                | Self::PatchCommit(_)
                | Self::PatchRemove(_)
                | Self::PatchesRelock(_)
                | Self::PatchesRepatch(_)
                | Self::PatchesDoctor(_)
                | Self::Search(_)
                | Self::Show(_)
                | Self::Outdated(_)
                | Self::CheckPlatformReqs(_)
        )
    }
}

#[derive(Debug, usage_rs::Args)]
struct CompletionArgs {
    /// Shell to generate completion for
    #[usage(arg, name = "SHELL")]
    shell: String,
}

pub fn run() -> i32 {
    let mut output = Output::process(OutputOptions::default());
    // Parse first so help, version, and argument errors never pay for an async runtime.
    let raw_arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    let (cli, invocation) = match parse_arguments(raw_arguments, &output) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    if let Some(warning) = application::configured_development_warning(&invocation) {
        riff_core::errln!(output, "{warning}");
    }
    let options = match configure_presentation(&cli, &output) {
        Ok(options) => options,
        Err(code) => return code,
    };
    output = output.with_options(options);
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    startup::raise_nofile_limit();
    if let Some(command_name) = invocation.telemetry_command_name() {
        log::debug!(target: "riff::telemetry", "command={command_name}");
    }
    let mut runtime = if cli.command.needs_parallel_runtime() {
        let mut runtime = tokio::runtime::Builder::new_multi_thread();
        if std::env::var_os("TOKIO_WORKER_THREADS").is_none() {
            runtime.worker_threads(2);
        }
        runtime
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    match runtime.enable_all().build().and_then(|runtime| {
        runtime
            .block_on(run_with_detected_platform(cli, output.clone()))
            .map_err(io::Error::other)
    }) {
        Ok(code) => code,
        Err(error) => {
            riff_core::errln!(output, "Error: {error:#}");
            1
        }
    }
}

/// Execute Riff command-line arguments with caller-supplied runtime and
/// platform information.
///
/// Arguments exclude the executable name. This function never probes PHP,
/// reads PHP-selection environment variables, creates an async runtime, or
/// initializes logging.
pub async fn run_with_args<I, T>(arguments: I, mut context: CommandContext) -> Result<i32>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let raw_arguments = arguments.into_iter().map(Into::into).collect();
    let (mut cli, invocation) = match parse_arguments(raw_arguments, context.output()) {
        Ok(parsed) => parsed,
        Err(code) => return Ok(code),
    };
    let options = match configure_presentation(&cli, context.output()) {
        Ok(options) => options,
        Err(code) => return Ok(code),
    };
    let configured_output = context.output().clone().with_options(options);
    context = context.with_output(configured_output);
    if let Some(php_binary) = cli.php.take() {
        context = context.with_php_binary(php_binary);
    }
    if let Some(command_name) = invocation.telemetry_command_name() {
        log::debug!(target: "riff::telemetry", "command={command_name}");
    }
    execute_cli(cli, context).await
}

fn parse_arguments(
    raw_arguments: Vec<OsString>,
    output: &Output,
) -> std::result::Result<(Cli, application::ApplicationInvocation), i32> {
    if let Some(answer) = Cli::completion_request(&raw_arguments) {
        output.write(
            OutputLevel::Info,
            OutputStream::Stdout,
            format_args!(
                "{}",
                commands::completion::supplement_completion(&raw_arguments, answer)
            ),
        );
        return Err(0);
    }
    let invocation = application::ApplicationInvocation::resolve(
        raw_arguments,
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        Cli::command(),
    );
    let argument_refs = invocation
        .arguments
        .iter()
        .map(OsString::as_os_str)
        .collect::<Vec<_>>();
    match usage_rs::embedded::outcome(Cli::spec(), Cli::command(), &argument_refs, Cli::parse_from)
    {
        usage_rs::embedded::Outcome::Parsed(cli) => Ok((cli, invocation)),
        usage_rs::embedded::Outcome::Exit(exit) => {
            if exit.stderr {
                output.write(
                    OutputLevel::Error,
                    OutputStream::Stderr,
                    format_args!("{}", exit.text),
                );
            } else {
                output.write(
                    OutputLevel::Info,
                    OutputStream::Stdout,
                    format_args!("{}", exit.text),
                );
            }
            Err(exit.code)
        }
    }
}

fn configure_presentation(cli: &Cli, output: &Output) -> std::result::Result<OutputOptions, i32> {
    let Some(output_mode) = riff_core::OutputMode::parse(&cli.output) else {
        riff_core::errln!(
            output,
            "Error: Unsupported output format '{}'. Use text or json.",
            cli.output
        );
        return Err(2);
    };
    Ok(OutputOptions {
        mode: output_mode,
        quiet: cli.quiet,
        progress: !cli.no_progress,
        ansi: if cli.ansi {
            AnsiMode::Always
        } else if cli.no_ansi || output_mode == riff_core::OutputMode::Json {
            AnsiMode::Never
        } else {
            AnsiMode::Auto
        },
    })
}

async fn run_with_detected_platform(cli: Cli, output: Output) -> Result<i32> {
    let detector = PhpPlatformDetector::from_sources(cli.php.clone())?;
    let platform = if cli.command.needs_platform() {
        detector.detect()?
    } else {
        Platform::empty()
    };
    let context = CommandContext::new(detector.runtime().clone(), platform).with_output(output);
    execute_cli(cli, context).await
}

async fn execute_cli(cli: Cli, context: CommandContext) -> Result<i32> {
    let command = match cli.command {
        Commands::About(args) => return commands::about::execute(args, &context),
        Commands::ClearCache(args) => return commands::clear_cache::execute(args, &context),
        Commands::Suggests(args) => return commands::suggests::execute(args, &context).await,
        Commands::Fund(args) => return commands::fund::execute(args, &context).await,
        Commands::Recipes(args) => return commands::flex::recipes(args, &context).await,
        Commands::DumpEnv(args) => return commands::flex::dump_env(args, &context),
        Commands::Global(args) => return commands::global::execute(args, &context),
        Commands::Init(args) => return commands::init::execute(args, &context),
        Commands::Browse(args) => return commands::home::execute(args, &context).await,
        Commands::Exec(args) => return commands::exec::execute(args, &context),
        Commands::Bump(args) => return commands::bump::execute(args, &context),
        command => command,
    };
    match command {
        Commands::About(_) => unreachable!("about command returned before runtime setup"),
        Commands::Archive(args) => commands::archive::execute(args, &context),
        Commands::Bump(_) => unreachable!("bump returned before runtime setup"),
        Commands::ClearCache(_) => unreachable!("clear-cache returned before runtime setup"),
        Commands::Suggests(_) => unreachable!("suggests returned before runtime setup"),
        Commands::Fund(_) => unreachable!("fund returned before runtime setup"),
        Commands::Recipes(_) => unreachable!("recipes returned before runtime setup"),
        Commands::DumpEnv(_) => unreachable!("dump-env returned before runtime setup"),
        Commands::Global(_) => unreachable!("global returned before runtime setup"),
        Commands::Init(_) => unreachable!("init returned before runtime setup"),
        Commands::Browse(_) => unreachable!("browse returned before runtime setup"),
        Commands::Exec(_) => unreachable!("exec returned before runtime setup"),
        Commands::Install(args) => install::execute(args, &context).await,
        Commands::Reinstall(args) => commands::reinstall::execute(args, &context).await,
        Commands::Update(args) => update::execute(args, &context).await,
        Commands::Require(args) => add::execute(args, &context).await,
        Commands::CreateProject(args) => create_project::execute(args, &context).await,
        Commands::Remove(args) => remove::execute(args, &context).await,
        Commands::Patch(args) => commands::patch::execute_patch(args, &context).await,
        Commands::PatchCommit(args) => commands::patch::execute_patch_commit(args, &context).await,
        Commands::PatchRemove(args) => commands::patch::execute_patch_remove(args, &context).await,
        Commands::PatchesRelock(args) => {
            commands::patch::execute_patches_relock(args, &context).await
        }
        Commands::PatchesRepatch(args) => {
            commands::patch::execute_patches_repatch(args, &context).await
        }
        Commands::PatchesDoctor(args) => {
            commands::patch::execute_patches_doctor(args, &context).await
        }
        Commands::RecipesInstall(args) => commands::flex::install_recipes(args, &context).await,
        Commands::RecipesUpdate(args) => commands::flex::update_recipe(args, &context).await,
        Commands::DumpAutoload(args) => commands::dump_autoload::execute(args, &context).await,
        Commands::Run(args) => commands::run::execute(args, &context).await,
        Commands::Search(args) => commands::search::execute(args, &context).await,
        Commands::Show(args) => commands::show::execute(args, &context).await,
        Commands::Licenses(args) => commands::licenses::execute(args, &context),
        Commands::Why(args) => commands::why::execute(args, false, &context).await,
        Commands::WhyNot(args) => commands::why::execute_why_not(args, &context).await,
        Commands::Outdated(args) => commands::outdated::execute(args, &context).await,
        Commands::Audit(args) => commands::audit::execute(args, &context).await,
        Commands::Diagnose(args) => commands::diagnose::execute(args, &context).await,
        Commands::Validate(args) => commands::validate::execute(args, &context).await,
        Commands::Config(args) => commands::config::execute(args, &context).await,
        Commands::Repository(args) => commands::repository::execute(args, &context).await,
        Commands::Policy(args) => commands::policy::execute(args, &context).await,
        Commands::Status(args) => commands::status::execute(args, &context).await,
        Commands::CheckPlatformReqs(args) => {
            commands::check_platform_reqs::execute(args, &context).await
        }
        Commands::Completion(args) => {
            let shell = usage_rs::complete::Shell::from_name(&args.shell)
                .ok_or_else(|| anyhow::anyhow!("Unsupported shell '{}'", args.shell))?;
            context.output().write(
                OutputLevel::Info,
                OutputStream::Stdout,
                format_args!(
                    "{}",
                    Cli::app()
                        .name("riff")
                        .bin("riff")
                        .completion_app()
                        .completion_script(shell)
                ),
            );
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use riff_core::{OutputEvent, OutputSink, PlatformSnapshot, RuntimeContext};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Collector(Mutex<Vec<OutputEvent>>);

    impl OutputSink for Collector {
        fn emit(&self, event: OutputEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn complete(line: &str) -> Vec<String> {
        let argv = [
            std::ffi::OsString::from("__complete_word__"),
            std::ffi::OsString::from("--shell"),
            std::ffi::OsString::from("bash"),
            std::ffi::OsString::from("--line"),
            std::ffi::OsString::from(line),
        ];
        let answer = Cli::completion_request(&argv).expect("completion request should be handled");
        commands::completion::supplement_completion(&argv, answer)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test]
    async fn supplied_platform_runner_does_not_probe_php() {
        let project = tempfile::TempDir::new().unwrap();
        std::fs::write(
            project.path().join("composer.json"),
            r#"{"name":"root/project"}"#,
        )
        .unwrap();
        let platform = Platform::from_snapshot(PlatformSnapshot {
            php_version: "7.1.33".to_string(),
            php_version_id: 70133,
            int_size: 8,
            zts: false,
            debug: false,
            ipv6: true,
            extensions: BTreeMap::new(),
            libraries: BTreeMap::new(),
        });
        let missing_php = project.path().join("php-does-not-exist");
        let context = CommandContext::new(
            RuntimeContext::new(missing_php.clone(), PathBuf::from("riff")),
            platform,
        );
        let arguments = vec![
            OsString::from("show"),
            OsString::from("--platform"),
            OsString::from("--working-dir"),
            project.path().as_os_str().to_owned(),
            OsString::from("--php"),
            missing_php.into_os_string(),
        ];

        assert_eq!(run_with_args(arguments, context).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn embedded_output_is_instance_scoped_and_quiet_is_local() {
        let visible_events = Arc::new(Collector::default());
        let quiet_events = Arc::new(Collector::default());
        let runtime = RuntimeContext::new(PathBuf::from("php"), PathBuf::from("riff"));
        let visible_context = CommandContext::new(runtime.clone(), Platform::empty())
            .with_output(Output::from_sink(visible_events.clone()));
        let quiet_context = CommandContext::new(runtime, Platform::empty())
            .with_output(Output::from_sink(quiet_events.clone()));

        let (visible, quiet) = tokio::join!(
            run_with_args(["about"], visible_context),
            run_with_args(["--quiet", "about"], quiet_context),
        );

        assert_eq!(visible.unwrap(), 0);
        assert_eq!(quiet.unwrap(), 0);
        assert!(!visible_events.0.lock().unwrap().is_empty());
        assert!(quiet_events.0.lock().unwrap().is_empty());
    }

    fn assert_contains(actual: &[String], expected: &[&str]) {
        for expected in expected {
            assert!(
                actual.iter().any(|candidate| candidate == expected),
                "completion candidates {actual:?} did not contain {expected:?}"
            );
        }
    }

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
    fn install_compatibility_flags_are_parsed() {
        let Commands::Install(args) = command(&[
            "install",
            "--prefer-install=auto",
            "--download-only",
            "--classmap-authoritative",
            "--apcu-autoloader-prefix=fixture",
            "--ignore-platform-req=php+",
            "--no-security-blocking",
            "--no-blocking",
        ]) else {
            panic!("install command was not parsed");
        };
        assert_eq!(args.prefer_install.as_deref(), Some("auto"));
        assert!(args.download_only);
        assert!(args.classmap_authoritative);
        assert_eq!(args.apcu_autoloader_prefix.as_deref(), Some("fixture"));
        assert_eq!(args.ignore_platform_req, ["php+"]);
        assert!(args.no_security_blocking);
        assert!(args.no_blocking);
    }

    #[test]
    fn update_compatibility_flags_are_parsed() {
        let Commands::Update(args) = command(&[
            "update",
            "--with",
            "vendor/package:^2.0",
            "--minimal-changes",
            "--no-install",
            "--root-reqs",
            "--bump-after-update=dev",
            "--no-security-blocking",
            "--no-blocking",
        ]) else {
            panic!("update command was not parsed");
        };
        assert_eq!(args.with_constraints, ["vendor/package:^2.0"]);
        assert!(args.minimal_changes);
        assert!(args.no_install);
        assert!(args.root_reqs);
        assert_eq!(args.bump_after_update, Some(Some("dev".to_string())));
        assert!(args.no_security_blocking);
        assert!(args.no_blocking);

        let Commands::Update(args) = command(&["update", "--bump-after-update"]) else {
            panic!("update command was not parsed");
        };
        assert_eq!(args.bump_after_update, Some(None));
    }

    #[test]
    fn dependency_policy_compatibility_flags_cover_all_composer_commands() {
        let Commands::Require(require) = command(&[
            "require",
            "vendor/package",
            "--no-security-blocking",
            "--no-blocking",
        ]) else {
            panic!("require command was not parsed");
        };
        assert!(require.no_security_blocking && require.no_blocking);

        let Commands::Remove(remove) = command(&[
            "remove",
            "vendor/package",
            "--no-security-blocking",
            "--no-blocking",
        ]) else {
            panic!("remove command was not parsed");
        };
        assert!(remove.no_security_blocking && remove.no_blocking);

        let Commands::CreateProject(create) = command(&[
            "create-project",
            "vendor/project",
            "--no-security-blocking",
            "--no-blocking",
        ]) else {
            panic!("create-project command was not parsed");
        };
        assert!(create.no_security_blocking && create.no_blocking);

        let Commands::Audit(audit) =
            command(&["audit", "--ignore-severity=high", "--ignore-unreachable"])
        else {
            panic!("audit command was not parsed");
        };
        assert_eq!(audit.ignore_severity, ["high"]);
        assert!(audit.ignore_unreachable);
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
    fn global_forwards_nested_command_options() {
        let Commands::Global(args) = command(&["global", "show", "--name-only", "vendor/package"])
        else {
            panic!("global command was not parsed");
        };
        assert_eq!(args.command_name, "show");
        assert_eq!(args.args, ["--name-only", "vendor/package"]);
    }

    #[test]
    fn patch_commands_and_short_aliases_are_parsed() {
        let Commands::Patch(args) = command(&["patch", "Vendor/Package@1.2.3"]) else {
            panic!("patch command was not parsed");
        };
        assert_eq!(args.package, "Vendor/Package@1.2.3");

        assert!(matches!(command(&["prl"]), Commands::PatchesRelock(_)));
        assert!(matches!(command(&["prp"]), Commands::PatchesRepatch(_)));
        assert!(matches!(command(&["pd"]), Commands::PatchesDoctor(_)));
    }

    #[test]
    fn symfony_flex_command_aliases_are_parsed() {
        assert!(matches!(
            command(&["symfony:recipes"]),
            Commands::Recipes(_)
        ));
        assert!(matches!(
            command(&["sync-recipes", "--force"]),
            Commands::RecipesInstall(args) if args.force
        ));
        assert!(matches!(
            command(&["symfony:recipes:update", "symfony/framework-bundle"]),
            Commands::RecipesUpdate(_)
        ));
        assert!(matches!(
            command(&["symfony:dump-env", "prod"]),
            Commands::DumpEnv(_)
        ));
    }

    #[test]
    fn policy_add_source_arguments_are_parsed() {
        let Commands::Policy(args) = command(&[
            "policy",
            "add-source",
            "company-policy",
            "url",
            "https://example.org/policy.json",
            "--file",
            "alt.composer.json",
        ]) else {
            panic!("policy command was not parsed");
        };
        assert_eq!(args.action, "add-source");
        assert_eq!(args.name.as_deref(), Some("company-policy"));
        assert_eq!(args.arg1.as_deref(), Some("url"));
        assert_eq!(
            args.arg2.as_deref(),
            Some("https://example.org/policy.json")
        );
        assert_eq!(args.file, Some(PathBuf::from("alt.composer.json")));
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

    #[test]
    fn composer_completion_contract_covers_dynamic_arguments_and_values() {
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join("vendor/bin")).unwrap();
        for binary in ["composer", "phpstan", "phpstan.phar"] {
            std::fs::write(project.path().join("vendor/bin").join(binary), "").unwrap();
        }
        std::fs::write(
            project.path().join("composer.json"),
            r#"{
                "name":"root/project",
                "require":{"composer/semver":"^3","psr/log":"^3"},
                "scripts":{"compile":"true","test":"true","phpstan":"true"},
                "extra":{"branch-alias":{"dev-main":"1.x-dev"}},
                "suggest":{"ext-zip":"Needed for archives"},
                "repositories":{
                    "packagist.org":false,
                    "fixture":{"type":"package","packages":[
                        {"name":"a/package","version":"1.0.0"},
                        {"name":"symfony/http-kernel","version":"1.0.0"},
                        {"name":"symfony/http-foundation","version":"1.0.0"}
                    ]}
                }
            }"#,
        )
        .unwrap();
        std::fs::write(
            project.path().join("composer.lock"),
            r#"{"packages":[{"name":"composer/semver","version":"3.0.0"},{"name":"psr/log","version":"3.0.0"}],"packages-dev":[]}"#,
        )
        .unwrap();
        // Completion lines are parsed as shell input. Forward slashes keep the
        // temporary Windows path from being interpreted as backslash escapes.
        let path = project.path().to_string_lossy().replace('\\', "/");
        for command in [
            "why",
            "depends",
            "browse",
            "outdated",
            "reinstall",
            "suggests",
            "update",
        ] {
            let line = format!("riff {command} -d {path} ");
            assert_contains(&complete(&line), &["composer/semver", "psr/log"]);
        }
        assert_contains(
            &complete(&format!("riff remove -d {path} ")),
            &["composer/semver", "psr/log"],
        );
        assert_contains(
            &complete(&format!("riff exec -d {path} ")),
            &["composer", "phpstan", "phpstan.phar"],
        );
        assert_contains(
            &complete(&format!("riff run-script -d {path} ")),
            &["compile", "test", "phpstan"],
        );
        for command in ["archive", "require"] {
            assert_contains(
                &complete(&format!("riff {command} -d {path} symfony/http-")),
                &["symfony/http-kernel", "symfony/http-foundation"],
            );
        }
        for command in ["install", "update", "require", "reinstall"] {
            assert_contains(
                &complete(&format!("riff {command} -d {path} --prefer-install ")),
                &["dist", "source", "auto"],
            );
        }
        assert_contains(
            &complete(&format!("riff archive -d {path} --format ")),
            &["tar", "zip"],
        );
        for command in ["search", "show"] {
            assert_contains(
                &complete(&format!("riff {command} -d {path} --format ")),
                &["text", "json"],
            );
        }
        assert_contains(
            &complete(&format!("riff config -d {path} extra.")),
            &["extra.branch-alias", "extra.branch-alias.dev-main"],
        );
        assert_contains(
            &complete(&format!("riff config -d {path} suggest.")),
            &["suggest.ext-zip"],
        );
        assert_contains(
            &complete(&format!("riff config -d {path} repositories.")),
            &["repositories.packagist.org"],
        );
    }
}
