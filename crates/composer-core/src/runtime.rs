use std::path::PathBuf;

/// Executables used when Composer scripts delegate to PHP or Composer itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeContext {
    pub php_binary: PathBuf,
    pub composer_binary: PathBuf,
}

impl RuntimeContext {
    pub fn new(php_binary: PathBuf, composer_binary: PathBuf) -> Self {
        Self {
            php_binary,
            composer_binary,
        }
    }
}

impl Default for RuntimeContext {
    fn default() -> Self {
        Self {
            php_binary: PathBuf::from("php"),
            composer_binary: std::env::current_exe()
                .unwrap_or_else(|_| PathBuf::from("composer-rs")),
        }
    }
}
