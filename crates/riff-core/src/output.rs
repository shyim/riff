//! Instance-scoped output for library and command-line callers.

use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;

use console::StyledObject;
use serde::Serialize;

/// The renderer selected for process output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Text,
    Json,
}

impl OutputMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "text" => Some(Self::Text),
            "json" | "ndjson" => Some(Self::Json),
            _ => None,
        }
    }
}

/// ANSI styling policy for process output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnsiMode {
    Auto,
    Always,
    Never,
}

/// Presentation controls used by the built-in process renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputOptions {
    pub mode: OutputMode,
    pub quiet: bool,
    pub progress: bool,
    pub ansi: AnsiMode,
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self {
            mode: OutputMode::Text,
            quiet: false,
            progress: true,
            ansi: AnsiMode::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// One Riff-generated output event.
///
/// `message` never contains ANSI escape sequences. A sink can use `newline` to
/// distinguish complete lines from prompts and other raw writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct OutputEvent {
    pub level: OutputLevel,
    pub stream: OutputStream,
    pub message: String,
    pub newline: bool,
}

/// Receives structured Riff output from library operations.
///
/// Sinks are observers: they cannot fail or change the result of an operation.
pub trait OutputSink: Send + Sync + 'static {
    fn emit(&self, event: OutputEvent);
}

#[derive(Clone)]
enum OutputTarget {
    Silent,
    Sink(Arc<dyn OutputSink>),
    Process,
}

/// Cloneable output handle owned by a Riff instance or command invocation.
#[derive(Clone)]
pub struct Output {
    target: OutputTarget,
    options: OutputOptions,
}

impl fmt::Debug for Output {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let target = match self.target {
            OutputTarget::Silent => "silent",
            OutputTarget::Sink(_) => "sink",
            OutputTarget::Process => "process",
        };
        formatter
            .debug_struct("Output")
            .field("target", &target)
            .field("options", &self.options)
            .finish()
    }
}

impl Output {
    /// Create output that discards all Riff-generated events.
    pub fn silent() -> Self {
        Self {
            target: OutputTarget::Silent,
            options: OutputOptions::default(),
        }
    }

    /// Create output that forwards plain structured events to `sink`.
    pub fn from_sink(sink: Arc<dyn OutputSink>) -> Self {
        Self {
            target: OutputTarget::Sink(sink),
            options: OutputOptions::default(),
        }
    }

    /// Create the process stdout/stderr renderer used by the standalone CLI.
    pub fn process(options: OutputOptions) -> Self {
        Self {
            target: OutputTarget::Process,
            options,
        }
    }

    /// Apply CLI presentation options while retaining the current target.
    pub fn with_options(mut self, options: OutputOptions) -> Self {
        self.options = options;
        self
    }

    pub fn options(&self) -> OutputOptions {
        self.options
    }

    /// Emit one complete line.
    pub fn emit(&self, level: OutputLevel, stream: OutputStream, arguments: fmt::Arguments<'_>) {
        self.emit_inner(level, stream, arguments, true);
    }

    /// Emit output without appending a newline and flush process output.
    pub fn write(&self, level: OutputLevel, stream: OutputStream, arguments: fmt::Arguments<'_>) {
        self.emit_inner(level, stream, arguments, false);
    }

    fn emit_inner(
        &self,
        level: OutputLevel,
        stream: OutputStream,
        arguments: fmt::Arguments<'_>,
        newline: bool,
    ) {
        if matches!(self.target, OutputTarget::Silent) || self.suppressed(level) {
            return;
        }

        let styled_message = arguments.to_string();
        let message = strip_terminal_codes(&styled_message);
        let event = OutputEvent {
            level,
            stream,
            message,
            newline,
        };

        match &self.target {
            OutputTarget::Silent => {}
            OutputTarget::Sink(sink) => sink.emit(event),
            OutputTarget::Process => self.emit_process(event, &styled_message),
        }
    }

    fn suppressed(&self, level: OutputLevel) -> bool {
        self.options.quiet && matches!(level, OutputLevel::Info | OutputLevel::Success)
    }

    fn emit_process(&self, event: OutputEvent, styled_message: &str) {
        if self.options.mode == OutputMode::Json {
            #[derive(Serialize)]
            struct JsonOutputEvent<'a> {
                level: OutputLevel,
                message: &'a str,
            }

            if let Ok(encoded) = serde_json::to_string(&JsonOutputEvent {
                level: event.level,
                message: &event.message,
            }) {
                let mut stdout = io::stdout();
                let _ = writeln!(stdout, "{encoded}");
            }
            return;
        }

        let message = if self.ansi_enabled(event.stream) {
            styled_message
        } else {
            &event.message
        };
        match event.stream {
            OutputStream::Stdout => write_process(io::stdout(), message, event.newline),
            OutputStream::Stderr => write_process(io::stderr(), message, event.newline),
        }
    }

    pub(crate) fn progress_enabled(&self) -> bool {
        matches!(self.target, OutputTarget::Process)
            && self.options.progress
            && !self.options.quiet
            && self.options.mode == OutputMode::Text
            && io::stderr().is_terminal()
    }

    pub(crate) fn captures_process_output(&self) -> bool {
        !matches!(self.target, OutputTarget::Process) || self.options.mode == OutputMode::Json
    }

    pub(crate) fn ansi_enabled(&self, stream: OutputStream) -> bool {
        match self.options.ansi {
            AnsiMode::Always => true,
            AnsiMode::Never => false,
            AnsiMode::Auto => match stream {
                OutputStream::Stdout => io::stdout().is_terminal(),
                OutputStream::Stderr => io::stderr().is_terminal(),
            },
        }
    }
}

impl Default for Output {
    fn default() -> Self {
        Self::silent()
    }
}

fn write_process(mut writer: impl Write, message: &str, newline: bool) {
    if newline {
        let _ = writeln!(writer, "{message}");
    } else {
        let _ = write!(writer, "{message}");
        let _ = writer.flush();
    }
}

fn strip_terminal_codes(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut plain = String::with_capacity(input.len());
    let mut cursor = 0;
    let mut segment_start = 0;

    while cursor < bytes.len() {
        if bytes[cursor] != 0x1b {
            cursor += 1;
            continue;
        }

        plain.push_str(&input[segment_start..cursor]);
        cursor += 1;
        if cursor == bytes.len() {
            segment_start = cursor;
            break;
        }

        match bytes[cursor] {
            b'[' => {
                cursor += 1;
                while cursor < bytes.len() {
                    let byte = bytes[cursor];
                    cursor += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            b']' | b'P' | b'X' | b'^' | b'_' => {
                cursor += 1;
                while cursor < bytes.len() {
                    if bytes[cursor] == 0x07 {
                        cursor += 1;
                        break;
                    }
                    if bytes[cursor] == 0x1b
                        && bytes.get(cursor + 1).is_some_and(|byte| *byte == b'\\')
                    {
                        cursor += 2;
                        break;
                    }
                    cursor += 1;
                }
            }
            byte if byte.is_ascii() => cursor += 1,
            _ => {}
        }
        segment_start = cursor;
    }

    plain.push_str(&input[segment_start..]);
    console::strip_ansi_codes(&plain).into_owned()
}

/// Create a styled value whose ANSI representation is decided by the output
/// instance instead of console's process-global color switches.
pub fn style<D>(value: D) -> StyledObject<D> {
    console::style(value).force_styling(true)
}

#[macro_export]
macro_rules! outln {
    ($output:expr) => {
        $output.emit(
            $crate::output::OutputLevel::Info,
            $crate::output::OutputStream::Stdout,
            format_args!(""),
        )
    };
    ($output:expr, $($arg:tt)*) => {
        $output.emit(
            $crate::output::OutputLevel::Info,
            $crate::output::OutputStream::Stdout,
            format_args!($($arg)*),
        )
    };
}

#[macro_export]
macro_rules! infoln {
    ($output:expr) => {
        $output.emit(
            $crate::output::OutputLevel::Info,
            $crate::output::OutputStream::Stderr,
            format_args!(""),
        )
    };
    ($output:expr, $($arg:tt)*) => {
        $output.emit(
            $crate::output::OutputLevel::Info,
            $crate::output::OutputStream::Stderr,
            format_args!($($arg)*),
        )
    };
}

#[macro_export]
macro_rules! successln {
    ($output:expr) => {
        $output.emit(
            $crate::output::OutputLevel::Success,
            $crate::output::OutputStream::Stdout,
            format_args!(""),
        )
    };
    ($output:expr, $($arg:tt)*) => {
        $output.emit(
            $crate::output::OutputLevel::Success,
            $crate::output::OutputStream::Stdout,
            format_args!($($arg)*),
        )
    };
}

#[macro_export]
macro_rules! errln {
    ($output:expr) => {
        $output.emit(
            $crate::output::OutputLevel::Error,
            $crate::output::OutputStream::Stderr,
            format_args!(""),
        )
    };
    ($output:expr, $($arg:tt)*) => {
        $output.emit(
            $crate::output::OutputLevel::Error,
            $crate::output::OutputStream::Stderr,
            format_args!($($arg)*),
        )
    };
}

#[macro_export]
macro_rules! warnln {
    ($output:expr) => {
        $output.emit(
            $crate::output::OutputLevel::Warning,
            $crate::output::OutputStream::Stderr,
            format_args!(""),
        )
    };
    ($output:expr, $($arg:tt)*) => {
        $output.emit(
            $crate::output::OutputLevel::Warning,
            $crate::output::OutputStream::Stderr,
            format_args!($($arg)*),
        )
    };
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Collector(Mutex<Vec<OutputEvent>>);

    impl OutputSink for Collector {
        fn emit(&self, event: OutputEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[test]
    fn recognizes_supported_output_modes() {
        assert_eq!(OutputMode::parse("text"), Some(OutputMode::Text));
        assert_eq!(OutputMode::parse("json"), Some(OutputMode::Json));
        assert_eq!(OutputMode::parse("ndjson"), Some(OutputMode::Json));
        assert_eq!(OutputMode::parse("yaml"), None);
    }

    #[test]
    fn custom_sink_receives_plain_structured_events() {
        let collector = Arc::new(Collector::default());
        let output = Output::from_sink(collector.clone());

        crate::warnln!(output, "{} warning", style("Styled").yellow());
        crate::outln!(
            output,
            "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\"
        );

        assert_eq!(
            *collector.0.lock().unwrap(),
            [
                OutputEvent {
                    level: OutputLevel::Warning,
                    stream: OutputStream::Stderr,
                    message: "Styled warning".to_owned(),
                    newline: true,
                },
                OutputEvent {
                    level: OutputLevel::Info,
                    stream: OutputStream::Stdout,
                    message: "link".to_owned(),
                    newline: true,
                },
            ]
        );
    }

    #[test]
    fn quiet_filter_only_suppresses_informational_levels() {
        let collector = Arc::new(Collector::default());
        let output = Output::from_sink(collector.clone()).with_options(OutputOptions {
            quiet: true,
            ..OutputOptions::default()
        });

        crate::outln!(output, "hidden");
        crate::successln!(output, "hidden");
        crate::warnln!(output, "visible warning");
        crate::errln!(output, "visible error");

        let events = collector.0.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].level, OutputLevel::Warning);
        assert_eq!(events[1].level, OutputLevel::Error);
    }

    #[test]
    fn sink_can_receive_from_a_worker_thread_while_the_caller_joins() {
        let collector = Arc::new(Collector::default());
        let output = Output::from_sink(collector.clone());
        let stderr = io::stderr();
        let _stderr_lock = stderr.lock();

        std::thread::scope(|scope| {
            let worker_output = output.clone();
            scope
                .spawn(move || crate::warnln!(worker_output, "worker message"))
                .join()
                .unwrap();
        });

        assert_eq!(collector.0.lock().unwrap()[0].message, "worker message");
    }

    #[test]
    fn custom_sinks_never_enable_process_progress() {
        let output =
            Output::from_sink(Arc::new(Collector::default())).with_options(OutputOptions {
                progress: true,
                ..OutputOptions::default()
            });

        assert!(!output.progress_enabled());
    }

    #[test]
    fn process_output_is_captured_when_raw_inheritance_would_break_the_output_contract() {
        assert!(Output::silent().captures_process_output());
        assert!(Output::from_sink(Arc::new(Collector::default())).captures_process_output());
        assert!(!Output::process(OutputOptions::default()).captures_process_output());
        assert!(Output::process(OutputOptions {
            mode: OutputMode::Json,
            ..OutputOptions::default()
        })
        .captures_process_output());
        assert!(!Output::process(OutputOptions {
            quiet: true,
            ..OutputOptions::default()
        })
        .captures_process_output());
    }
}
