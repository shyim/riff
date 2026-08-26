use std::path::PathBuf;

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
}

impl Default for RuntimeContext {
    fn default() -> Self {
        Self {
            php_binary: PathBuf::from("php"),
            riff_binary: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("riff")),
        }
    }
}
