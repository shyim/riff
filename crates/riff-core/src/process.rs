//! Process execution primitives shared by Riff and its native plugins.

use crate::output::{Output, OutputLevel, OutputStream};
use regex::{Captures, Regex};
use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::LazyLock;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use thiserror::Error;

static URL_CREDENTIALS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"://(?P<user>[^:/\s@]+)(?::(?P<password>[^@\s/]+))?@")
        .expect("URL credential regex is valid")
});
static PASSWORD_ARGUMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)(?P<prefix>--password\s+)'(?:\\.|[^'])*'")
        .expect("password argument regex is valid")
});
static WINDOWS_ENV_EXPANSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"%[^%]+%|![^!]+!").expect("Windows expansion regex is valid"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Capture,
    Inherit,
}

#[derive(Debug)]
pub struct ProcessOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ProcessOutput {
    pub fn exit_code(&self) -> i32 {
        self.status.code().unwrap_or(1)
    }
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("process timed out after {0:?}")]
    Timeout(Duration),
    #[error("process output reader panicked")]
    ReaderPanicked,
}

#[derive(Debug, Clone, Copy)]
pub struct ProcessExecutor {
    timeout: Option<Duration>,
}

impl ProcessExecutor {
    pub fn new(timeout: Option<Duration>) -> Self {
        Self { timeout }
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    pub fn execute(
        &self,
        command: &mut Command,
        output_mode: OutputMode,
    ) -> Result<ProcessOutput, ProcessError> {
        match output_mode {
            OutputMode::Capture => {
                command.stdout(Stdio::piped()).stderr(Stdio::piped());
            }
            OutputMode::Inherit => {
                command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
            }
        }

        let mut child = command.spawn()?;
        let stdout = child.stdout.take().map(read_in_background);
        let stderr = child.stderr.take().map(read_in_background);
        let status = match wait_for_exit(&mut child, self.timeout) {
            Ok(status) => status,
            Err(error) => {
                join_reader(stdout)?;
                join_reader(stderr)?;
                return Err(error);
            }
        };

        Ok(ProcessOutput {
            status,
            stdout: join_reader(stdout)?,
            stderr: join_reader(stderr)?,
        })
    }

    pub fn spawn(&self, command: &mut Command) -> io::Result<RunningProcess> {
        Ok(RunningProcess {
            child: Some(command.spawn()?),
        })
    }
}

/// Executes a child process using Riff's instance-scoped output policy.
///
/// Process-rendered text output is inherited so interactive commands retain
/// terminal behavior. Library sinks, silent output, and JSON output are
/// captured and replayed as structured Riff events instead.
#[derive(Debug, Clone, Copy)]
pub struct ProcessRunner<'a> {
    output: &'a Output,
    timeout: Option<Duration>,
}

impl<'a> ProcessRunner<'a> {
    pub fn new(output: &'a Output) -> Self {
        Self {
            output,
            timeout: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Configure a Composer-compatible timeout, where zero disables it.
    pub fn with_timeout_seconds(self, timeout: u64) -> Self {
        self.with_timeout((timeout > 0).then(|| Duration::from_secs(timeout)))
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    pub fn execute(&self, command: &mut Command) -> Result<ProcessOutput, ProcessError> {
        let output_mode = if self.output.captures_process_output() {
            OutputMode::Capture
        } else {
            OutputMode::Inherit
        };
        let process_output = ProcessExecutor::new(self.timeout).execute(command, output_mode)?;
        if output_mode == OutputMode::Capture {
            replay_process_output(self.output, OutputStream::Stdout, &process_output.stdout);
            replay_process_output(self.output, OutputStream::Stderr, &process_output.stderr);
        }
        Ok(process_output)
    }
}

pub struct RunningProcess {
    child: Option<Child>,
}

impl RunningProcess {
    pub fn is_running(&mut self) -> io::Result<bool> {
        self.child.as_mut().map_or(Ok(false), |child| {
            child.try_wait().map(|status| status.is_none())
        })
    }

    pub fn cancel(&mut self) -> io::Result<()> {
        if let Some(mut child) = self.child.take() {
            if child.try_wait()?.is_none() {
                child.kill()?;
            }
            child.wait()?;
        }
        Ok(())
    }
}

impl Drop for RunningProcess {
    fn drop(&mut self) {
        let _ = self.cancel();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Unix,
    Windows,
}

pub fn escape_argument(argument: Option<&str>) -> String {
    #[cfg(unix)]
    const SHELL: Shell = Shell::Unix;
    #[cfg(windows)]
    const SHELL: Shell = Shell::Windows;
    escape_argument_for(argument, SHELL)
}

pub fn escape_argument_for(argument: Option<&str>, shell: Shell) -> String {
    let argument = argument.unwrap_or_default();
    match shell {
        Shell::Unix => format!("'{}'", argument.replace('\'', "'\\''")),
        Shell::Windows => escape_windows_argument(argument),
    }
}

pub fn redact_command(command: &str) -> String {
    let command = URL_CREDENTIALS.replace_all(command, |captures: &Captures<'_>| {
        let user = sanitize_username(&captures["user"]);
        if captures.name("password").is_some() {
            format!("://{user}:***@")
        } else {
            format!("://{user}@")
        }
    });
    PASSWORD_ARGUMENT
        .replace_all(&command, "${prefix}'***'")
        .into_owned()
}

pub fn split_lines(output: Option<&str>) -> Vec<&str> {
    let output = output.unwrap_or_default().trim();
    if output.is_empty() {
        Vec::new()
    } else {
        output
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .collect()
    }
}

fn wait_for_exit(child: &mut Child, timeout: Option<Duration>) -> Result<ExitStatus, ProcessError> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
            let duration = timeout.expect("timeout was checked");
            child.kill()?;
            child.wait()?;
            return Err(ProcessError::Timeout(duration));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn read_in_background(mut stream: impl Read + Send + 'static) -> JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut output = Vec::new();
        stream.read_to_end(&mut output)?;
        Ok(output)
    })
}

fn join_reader(reader: Option<JoinHandle<io::Result<Vec<u8>>>>) -> Result<Vec<u8>, ProcessError> {
    match reader {
        Some(reader) => reader
            .join()
            .map_err(|_| ProcessError::ReaderPanicked)?
            .map_err(ProcessError::Io),
        None => Ok(Vec::new()),
    }
}

fn replay_process_output(output: &Output, stream: OutputStream, bytes: &[u8]) {
    let contents = String::from_utf8_lossy(bytes);
    let level = if stream == OutputStream::Stderr {
        OutputLevel::Error
    } else {
        OutputLevel::Info
    };
    for line in contents.split_inclusive('\n') {
        let newline = line.ends_with('\n');
        let message = line
            .strip_suffix('\n')
            .unwrap_or(line)
            .strip_suffix('\r')
            .unwrap_or_else(|| line.strip_suffix('\n').unwrap_or(line));
        if newline {
            output.emit(level, stream, format_args!("{message}"));
        } else if !message.is_empty() {
            output.write(level, stream, format_args!("{message}"));
        }
    }
}

fn sanitize_username(username: &str) -> String {
    if username.len() >= 12 {
        format!("{}***", username.chars().take(3).collect::<String>())
    } else {
        username.to_string()
    }
}

fn escape_windows_argument(argument: &str) -> String {
    if argument.is_empty() {
        return "\"\"".to_string();
    }

    let argument = argument
        .replace('\n', " ")
        .replace(
            ['\u{ff02}', '\u{02ba}', '\u{301d}', '\u{301e}', '\u{030e}'],
            "\"",
        )
        .replace(['\u{ff1a}', '\u{0589}', '\u{2236}'], ":")
        .replace(['\u{ff0f}', '\u{2044}', '\u{2215}', '\u{00b4}'], "/");
    let mut quote = argument.contains([' ', '\t', ',']);
    let (mut argument, contains_double_quote) = escape_windows_double_quotes(&argument);
    let meta = contains_double_quote || WINDOWS_ENV_EXPANSION.is_match(&argument);

    if !meta && !quote {
        quote = argument.contains(['^', '&', '|', '<', '>', '(', ')']);
    }

    if quote {
        let trailing_backslashes = argument
            .chars()
            .rev()
            .take_while(|char| *char == '\\')
            .count();
        argument.push_str(&"\\".repeat(trailing_backslashes));
        argument = format!("\"{argument}\"");
    }

    if meta {
        let mut escaped = String::new();
        for character in argument.chars() {
            if matches!(
                character,
                '"' | '^' | '&' | '|' | '<' | '>' | '(' | ')' | '%'
            ) {
                escaped.push('^');
            } else if character == '!' {
                escaped.push_str("^^");
            }
            escaped.push(character);
        }
        argument = escaped;
    }

    argument
}

fn escape_windows_double_quotes(argument: &str) -> (String, bool) {
    let mut escaped = String::new();
    let mut backslashes = 0;
    let mut contains_double_quote = false;

    for character in argument.chars() {
        if character == '\\' {
            backslashes += 1;
            continue;
        }
        if character == '"' {
            escaped.push_str(&"\\".repeat(backslashes * 2 + 1));
            escaped.push('"');
            contains_double_quote = true;
        } else {
            escaped.push_str(&"\\".repeat(backslashes));
            escaped.push(character);
        }
        backslashes = 0;
    }
    escaped.push_str(&"\\".repeat(backslashes));
    (escaped, contains_double_quote)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::output::{OutputEvent, OutputSink};

    use super::*;

    #[derive(Default)]
    struct Collector(Mutex<Vec<OutputEvent>>);

    impl OutputSink for Collector {
        fn emit(&self, event: OutputEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[cfg(unix)]
    #[test]
    fn composer_process_executor_captures_stdout() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'foo\\n'"]);
        let output = ProcessExecutor::new(None)
            .execute(&mut command, OutputMode::Capture)
            .unwrap();
        assert_eq!(output.exit_code(), 0);
        assert_eq!(output.stdout, b"foo\n");
    }

    #[cfg(unix)]
    #[test]
    fn composer_process_executor_captures_stderr() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'missing file' >&2; exit 1"]);
        let output = ProcessExecutor::new(None)
            .execute(&mut command, OutputMode::Capture)
            .unwrap();
        assert_eq!(output.exit_code(), 1);
        assert_eq!(output.stderr, b"missing file");
    }

    #[cfg(unix)]
    #[test]
    fn process_runner_replays_captured_output_to_the_instance_sink() {
        let collector = Arc::new(Collector::default());
        let output = Output::from_sink(collector.clone());
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'out\\n'; printf 'err' >&2; exit 7"]);

        let process_output = ProcessRunner::new(&output).execute(&mut command).unwrap();

        assert_eq!(process_output.exit_code(), 7);
        assert_eq!(process_output.stdout, b"out\n");
        assert_eq!(process_output.stderr, b"err");
        assert_eq!(
            *collector.0.lock().unwrap(),
            [
                OutputEvent {
                    level: OutputLevel::Info,
                    stream: OutputStream::Stdout,
                    message: "out".to_owned(),
                    newline: true,
                },
                OutputEvent {
                    level: OutputLevel::Error,
                    stream: OutputStream::Stderr,
                    message: "err".to_owned(),
                    newline: false,
                },
            ]
        );
    }

    #[test]
    fn process_runner_uses_composer_timeout_semantics() {
        let output = Output::silent();
        assert_eq!(
            ProcessRunner::new(&output)
                .with_timeout_seconds(0)
                .timeout(),
            None
        );
        assert_eq!(
            ProcessRunner::new(&output)
                .with_timeout_seconds(5)
                .timeout(),
            Some(Duration::from_secs(5))
        );
    }

    #[cfg(unix)]
    #[test]
    fn composer_process_executor_enforces_timeout() {
        let mut command = Command::new("sleep");
        command.arg("2");
        let timeout = Duration::from_millis(20);
        let executor = ProcessExecutor::new(Some(timeout));
        assert_eq!(executor.timeout(), Some(timeout));
        let error = executor
            .execute(&mut command, OutputMode::Capture)
            .unwrap_err();
        assert!(matches!(error, ProcessError::Timeout(value) if value == timeout));
    }

    #[test]
    fn composer_process_executor_redacts_credentials_data_provider() {
        let cases = [
            ("echo https://foo:bar@example.org/", "echo https://foo:***@example.org/"),
            ("echo http://foo@example.org", "echo http://foo@example.org"),
            ("echo http://abcdef1234567890234578:x-oauth-token@github.com/", "echo http://abc***:***@github.com/"),
            ("echo http://github_pat_1234567890abcdefghijkl_1234567890abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVW:x-oauth-token@github.com/", "echo http://git***:***@github.com/"),
            ("echo http://ghp_1234567890abcdefghijklmnopqrstuvwxyzAB@github.com/", "echo http://ghp***@github.com/"),
            ("echo http://abcdef1234567890234578@github.com/", "echo http://abc***@github.com/"),
            ("svn ls --verbose --non-interactive  --username 'foo' --password 'bar'  'https://foo.example.org/svn/'", "svn ls --verbose --non-interactive  --username 'foo' --password '***'  'https://foo.example.org/svn/'"),
            ("svn ls --verbose --non-interactive  --username 'foo' --password 'bar \\'bar'  'https://foo.example.org/svn/'", "svn ls --verbose --non-interactive  --username 'foo' --password '***'  'https://foo.example.org/svn/'"),
        ];
        for (command, expected) in cases {
            assert_eq!(redact_command(command), expected, "command: {command}");
        }
    }

    #[test]
    fn composer_process_executor_preserves_url_ports() {
        assert_eq!(
            redact_command("echo https://localhost:1234/"),
            "echo https://localhost:1234/"
        );
    }

    #[test]
    fn composer_process_executor_splits_lines() {
        assert_eq!(split_lines(Some("")), Vec::<&str>::new());
        assert_eq!(split_lines(None), Vec::<&str>::new());
        assert_eq!(split_lines(Some("foo")), ["foo"]);
        assert_eq!(split_lines(Some("foo\nbar")), ["foo", "bar"]);
        assert_eq!(split_lines(Some("foo\r\nbar")), ["foo", "bar"]);
        assert_eq!(split_lines(Some("foo\r\nbar\n")), ["foo", "bar"]);
    }

    #[cfg(unix)]
    #[test]
    fn composer_process_executor_cancels_async_process() {
        let mut command = Command::new("sleep");
        command.arg("2").stdout(Stdio::null()).stderr(Stdio::null());
        let started = Instant::now();
        let mut process = ProcessExecutor::new(None).spawn(&mut command).unwrap();
        assert!(process.is_running().unwrap());
        process.cancel().unwrap();
        assert!(!process.is_running().unwrap());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn composer_process_executor_escapes_arguments_data_provider() {
        let cases = [
            ("", "\"\"", "''"),
            ("a'bc", "a'bc", "'a'\\''bc'"),
            ("a\nb\nc", "\"a b c\"", "'a\nb\nc'"),
            ("a b c", "\"a b c\"", "'a b c'"),
            ("a\tb\tc", "\"a\tb\tc\"", "'a\tb\tc'"),
            ("abc", "abc", "'abc'"),
            ("a,bc", "\"a,bc\"", "'a,bc'"),
            ("a\"bc", "a\\^\"bc", "'a\"bc'"),
            ("a\\\"bc", "a\\\\\\^\"bc", "'a\\\"bc'"),
            ("ab\\\\c\\", "ab\\\\c\\", "'ab\\\\c\\'"),
            ("a b c\\", "\"a b c\\\\\"", "'a b c\\'"),
            ("a \"b\" c", "^\"a \\^\"b\\^\" c^\"", "'a \"b\" c'"),
            ("%path%", "^%path^%", "'%path%'"),
            ("%path", "%path", "'%path'"),
            ("%%path", "%%path", "'%%path'"),
            ("!path!", "^^!path^^!", "'!path!'"),
            ("!path", "!path", "'!path'"),
            ("!!path", "!!path", "'!!path'"),
            ("<>\"&|()^", "^<^>\\^\"^&^|^(^)^^", "'<>\"&|()^'"),
            ("<> &| ()^", "\"<> &| ()^\"", "'<> &| ()^'"),
            ("<>&|()^", "\"<>&|()^\"", "'<>&|()^'"),
        ];
        assert_eq!(escape_argument_for(None, Shell::Windows), "\"\"");
        assert_eq!(escape_argument_for(None, Shell::Unix), "''");
        for (argument, windows, unix) in cases {
            assert_eq!(
                escape_argument_for(Some(argument), Shell::Windows),
                windows,
                "Windows argument: {argument:?}"
            );
            assert_eq!(
                escape_argument_for(Some(argument), Shell::Unix),
                unix,
                "Unix argument: {argument:?}"
            );
        }
    }
}
