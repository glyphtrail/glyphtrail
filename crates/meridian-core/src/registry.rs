//! Global registry of indexed repositories, stored at
//! `~/.meridian/registry.json`. It tracks repo roots by name so the analyzer
//! can target or span many repositories; each repo's `.meridian/graph.db`
//! remains the source of truth.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{CoreError, Result};

/// One registered repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub name: String,
    /// Absolute repository root (contains `.meridian/graph.db`).
    pub root: PathBuf,
}

/// The set of registered repositories.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub repos: Vec<RegistryEntry>,
}

impl Registry {
    /// Load the registry from `path`, returning an empty registry when the file
    /// doesn't exist yet.
    pub fn load(path: &Path) -> Result<Registry> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|source| CoreError::RegistryParse {
                path: path.to_path_buf(),
                source,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
            Err(e) => Err(CoreError::Io(e)),
        }
    }

    /// Write the registry to `path`, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json =
            serde_json::to_string_pretty(self).map_err(|source| CoreError::RegistryParse {
                path: path.to_path_buf(),
                source,
            })?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Register a repo, replacing any existing entry with the same name.
    /// Returns `true` if it was newly added, `false` if it replaced one.
    pub fn add(&mut self, name: String, root: PathBuf) -> bool {
        match self.repos.iter_mut().find(|e| e.name == name) {
            Some(existing) => {
                existing.root = root;
                false
            }
            None => {
                self.repos.push(RegistryEntry { name, root });
                true
            }
        }
    }

    /// Remove the repo with the given name; returns whether one was removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.repos.len();
        self.repos.retain(|e| e.name != name);
        self.repos.len() != before
    }

    /// Look up a registered repo by name.
    pub fn get(&self, name: &str) -> Option<&RegistryEntry> {
        self.repos.iter().find(|e| e.name == name)
    }
}

/// The default registry path, `~/.meridian/registry.json`, from `HOME` (or
/// `USERPROFILE` on Windows). `None` if neither is set.
pub fn default_registry_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".meridian").join("registry.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn add_is_upsert_by_name() {
        let mut reg = Registry::default();
        check!(reg.add("api".into(), PathBuf::from("/a")));
        check!(!reg.add("api".into(), PathBuf::from("/b"))); // replaces
        check!(reg.repos.len() == 1);
        check!(reg.get("api").unwrap().root == PathBuf::from("/b"));
    }

    #[test]
    fn remove_reports_whether_present() {
        let mut reg = Registry::default();
        reg.add("api".into(), PathBuf::from("/a"));
        check!(reg.remove("api"));
        check!(!reg.remove("api"));
        check!(reg.repos.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips_and_missing_is_empty() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("meridian-reg-{nanos}/registry.json"));
        check!(Registry::load(&path).unwrap() == Registry::default()); // missing -> empty
        let mut reg = Registry::default();
        reg.add("web".into(), PathBuf::from("/srv/web"));
        reg.save(&path).unwrap();
        check!(Registry::load(&path).unwrap() == reg);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
