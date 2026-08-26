use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value};

#[derive(Debug, Clone)]
pub(crate) struct FlexLock {
    path: PathBuf,
    entries: Map<String, Value>,
    changed: bool,
}

impl FlexLock {
    pub(crate) fn load(working_dir: &Path) -> Result<Self> {
        let path = std::env::var_os("SYMFONY_LOCKFILE")
            .map(PathBuf::from)
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    working_dir.join(path)
                }
            })
            .unwrap_or_else(|| working_dir.join("symfony.lock"));
        let entries = match std::fs::read(&path) {
            Ok(contents) => serde_json::from_slice::<Value>(&contents)
                .with_context(|| format!("Failed to parse {}", path.display()))?
                .as_object()
                .cloned()
                .with_context(|| format!("{} must contain a JSON object", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to read {}", path.display()))
            }
        };
        Ok(Self {
            path,
            entries,
            changed: false,
        })
    }

    pub(crate) fn has(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Value> {
        self.entries.get(name)
    }

    pub(crate) fn set(&mut self, name: impl Into<String>, value: Value) {
        let name = name.into();
        if self.entries.get(&name) != Some(&value) {
            self.entries.insert(name, value);
            self.changed = true;
        }
    }

    pub(crate) fn add_files(&mut self, name: &str, files: Vec<String>) {
        let entry = self
            .entries
            .entry(name.to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(entry) = entry.as_object_mut() else {
            return;
        };
        let value = Value::Array(files.into_iter().map(Value::String).collect());
        if entry.get("files") != Some(&value) {
            entry.insert("files".to_owned(), value);
            self.changed = true;
        }
    }

    pub(crate) fn remove(&mut self, name: &str) {
        if self.entries.remove(name).is_some() {
            self.changed = true;
        }
    }

    pub(crate) fn all(&self) -> &Map<String, Value> {
        &self.entries
    }

    pub(crate) fn clear_for_lookup(&mut self) {
        self.entries.clear();
        self.changed = false;
    }

    pub(crate) fn write(&mut self) -> Result<()> {
        if !self.changed {
            return Ok(());
        }
        if self.entries.is_empty() {
            match std::fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to remove {}", self.path.display()))
                }
            }
            self.changed = false;
            return Ok(());
        }

        let mut entries = std::mem::take(&mut self.entries)
            .into_iter()
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        self.entries.extend(entries);
        let contents = crate::json::encode_pretty_json(&self.entries, b"    ")?;
        std::fs::write(&self.path, contents)
            .with_context(|| format!("Failed to write {}", self.path.display()))?;
        self.changed = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_sorted_lock_and_removes_empty_lock() {
        let project = tempfile::tempdir().unwrap();
        let mut lock = FlexLock::load(project.path()).unwrap();
        lock.set("z/package", serde_json::json!({"version": "1.0"}));
        lock.set("a/package", serde_json::json!({"version": "2.0"}));
        lock.write().unwrap();
        let contents = std::fs::read_to_string(project.path().join("symfony.lock")).unwrap();
        assert!(contents.starts_with("{\n    \"a/package\""));
        assert!(contents.find("a/package").unwrap() < contents.find("z/package").unwrap());

        lock.remove("a/package");
        lock.remove("z/package");
        lock.write().unwrap();
        assert!(!project.path().join("symfony.lock").exists());
    }
}
