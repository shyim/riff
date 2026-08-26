//! Shared presentation layer for Riff's human and machine-readable output.

use std::fmt;
use std::io::IsTerminal;
use std::sync::{OnceLock, RwLock};

use serde::Serialize;

/// The renderer selected for command output.
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

/// Process-wide presentation controls configured by the CLI before work starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputOptions {
    pub mode: OutputMode,
    pub quiet: bool,
    pub progress: bool,
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self {
            mode: OutputMode::Text,
            quiet: false,
            progress: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputLevel {
    Info,
    Success,
    Warning,
    Error,
    Progress,
}

#[derive(Debug, Clone, Copy)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Serialize)]
struct OutputEvent<'a> {
    level: OutputLevel,
    message: &'a str,
}

fn options() -> &'static RwLock<OutputOptions> {
    static OPTIONS: OnceLock<RwLock<OutputOptions>> = OnceLock::new();
    OPTIONS.get_or_init(|| RwLock::new(OutputOptions::default()))
}

/// Configure the renderer for the current process.
pub fn configure_output(next: OutputOptions) {
    *options()
        .write()
        .expect("output configuration lock poisoned") = next;
}

/// Returns whether interactive progress may be rendered.
pub fn progress_enabled() -> bool {
    let options = *options()
        .read()
        .expect("output configuration lock poisoned");
    can_render_progress(options, std::io::stderr().is_terminal())
}

fn can_render_progress(options: OutputOptions, terminal: bool) -> bool {
    options.progress && !options.quiet && options.mode == OutputMode::Text && terminal
}

/// Emit one complete output line through the selected renderer.
pub fn emit(level: OutputLevel, stream: OutputStream, arguments: fmt::Arguments<'_>) {
    let options = *options()
        .read()
        .expect("output configuration lock poisoned");
    if options.quiet
        && matches!(
            level,
            OutputLevel::Info | OutputLevel::Success | OutputLevel::Progress
        )
    {
        return;
    }

    let message = arguments.to_string();
    if options.mode == OutputMode::Json {
        let event = OutputEvent {
            level,
            message: &message,
        };
        if let Ok(event) = serde_json::to_string(&event) {
            println!("{event}");
        }
        return;
    }

    match stream {
        OutputStream::Stdout => println!("{message}"),
        OutputStream::Stderr => eprintln!("{message}"),
    }
}

#[macro_export]
macro_rules! outln {
    () => {
        $crate::output::emit(
            $crate::output::OutputLevel::Info,
            $crate::output::OutputStream::Stdout,
            format_args!(""),
        )
    };
    ($($arg:tt)*) => {
        $crate::output::emit(
            $crate::output::OutputLevel::Info,
            $crate::output::OutputStream::Stdout,
            format_args!($($arg)*),
        )
    };
}

#[macro_export]
macro_rules! successln {
    () => {
        $crate::output::emit(
            $crate::output::OutputLevel::Success,
            $crate::output::OutputStream::Stdout,
            format_args!(""),
        )
    };
    ($($arg:tt)*) => {
        $crate::output::emit(
            $crate::output::OutputLevel::Success,
            $crate::output::OutputStream::Stdout,
            format_args!($($arg)*),
        )
    };
}

#[macro_export]
macro_rules! errln {
    () => {
        $crate::output::emit(
            $crate::output::OutputLevel::Error,
            $crate::output::OutputStream::Stderr,
            format_args!(""),
        )
    };
    ($($arg:tt)*) => {
        $crate::output::emit(
            $crate::output::OutputLevel::Error,
            $crate::output::OutputStream::Stderr,
            format_args!($($arg)*),
        )
    };
}

#[macro_export]
macro_rules! warnln {
    () => {
        $crate::output::emit(
            $crate::output::OutputLevel::Warning,
            $crate::output::OutputStream::Stderr,
            format_args!(""),
        )
    };
    ($($arg:tt)*) => {
        $crate::output::emit(
            $crate::output::OutputLevel::Warning,
            $crate::output::OutputStream::Stderr,
            format_args!($($arg)*),
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_supported_output_modes() {
        assert_eq!(OutputMode::parse("text"), Some(OutputMode::Text));
        assert_eq!(OutputMode::parse("json"), Some(OutputMode::Json));
        assert_eq!(OutputMode::parse("ndjson"), Some(OutputMode::Json));
        assert_eq!(OutputMode::parse("yaml"), None);
    }

    #[test]
    fn progress_requires_text_output_and_a_terminal() {
        let options = OutputOptions::default();
        assert!(can_render_progress(options, true));
        assert!(!can_render_progress(options, false));
        assert!(!can_render_progress(
            OutputOptions {
                mode: OutputMode::Json,
                ..options
            },
            true
        ));
        assert!(!can_render_progress(
            OutputOptions {
                quiet: true,
                ..options
            },
            true
        ));
        assert!(!can_render_progress(
            OutputOptions {
                progress: false,
                ..options
            },
            true
        ));
    }
}
