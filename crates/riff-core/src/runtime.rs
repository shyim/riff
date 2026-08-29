use std::path::PathBuf;
use std::process::Command;

/// Executables used when Riff scripts delegate to PHP or Riff itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeContext {
    pub php_binary: PathBuf,
    pub riff_binary: PathBuf,
}

impl RuntimeContext {
    pub fn new(php_binary: PathBuf, riff_binary: PathBuf) -> Self {
        Self {
            php_binary,
            riff_binary,
        }
    }

    /// Build a Riff subprocess pinned to the caller-supplied PHP runtime.
    pub fn riff_command(&self) -> Command {
        let mut command = Command::new(&self.riff_binary);
        command.arg("--php").arg(&self.php_binary);
        command
    }

    /// Build a PHP subprocess from the caller-supplied runtime.
    pub fn php_command(&self) -> Command {
        Command::new(&self.php_binary)
    }
}

impl Default for RuntimeContext {
    fn default() -> Self {
        Self {
            php_binary: PathBuf::from("php"),
            riff_binary: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("riff")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn riff_command_pins_the_configured_php_binary() {
        let runtime =
            RuntimeContext::new(PathBuf::from("custom-php"), PathBuf::from("custom-riff"));
        let command = runtime.riff_command();

        assert_eq!(command.get_program(), OsStr::new("custom-riff"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [OsStr::new("--php"), OsStr::new("custom-php")]
        );
    }

    #[test]
    fn php_command_uses_the_configured_binary() {
        let runtime =
            RuntimeContext::new(PathBuf::from("custom-php"), PathBuf::from("custom-riff"));
        assert_eq!(
            runtime.php_command().get_program(),
            OsStr::new("custom-php")
        );
    }
}
