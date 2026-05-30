//! Named groups of registered repositories (#221), stored at
//! `~/.stratograph/groups.json`. A group scopes a federated operation
//! (cross-repo impact, link sync) to a subset of the registry. The whole
//! registry auto-links by default, so a group is an optional filter, not a
//! precondition for linking; it just narrows which repos a query spans.
//!
//! Like the [`crate::registry`], groups point at registry repos *by name* and
//! the per-repo `.stratograph/` index stays the source of truth. The file is
//! mutated under the same advisory lock so concurrent writers can't clobber.

use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};

use crate::{CoreError, Result};

/// One named group: a set of registry repo names that federate together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    /// Registry repo names belonging to this group, in insertion order.
    #[serde(default)]
    pub repos: Vec<String>,
}

/// The set of defined groups.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Groups {
    #[serde(default)]
    pub groups: Vec<Group>,
}

impl Groups {
    /// Load groups from `path`, returning an empty set when the file is absent.
    pub fn load(path: &Path) -> Result<Groups> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|source| CoreError::RegistryParse {
                path: path.to_path_buf(),
                source,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Groups::default()),
            Err(e) => Err(CoreError::Io(e)),
        }
    }

    /// Write groups to `path`, creating parent directories as needed. The write
    /// is atomic: staged in a process-unique temp file and renamed over `path`.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json =
            serde_json::to_string_pretty(self).map_err(|source| CoreError::RegistryParse {
                path: path.to_path_buf(),
                source,
            })?;
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Serialize a load → modify → save cycle under an exclusive advisory lock on
    /// a sibling `groups.lock`, so concurrent writers can't lose an update
    /// (mirrors [`crate::Registry::mutate`], #129). Returns the closure's value.
    pub fn mutate<R>(path: &Path, f: impl FnOnce(&mut Groups) -> R) -> Result<R> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock_path = path.with_extension("lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        FileExt::lock_exclusive(&lock)?;
        let outcome = (|| {
            let mut groups = Groups::load(path)?;
            let value = f(&mut groups);
            groups.save(path)?;
            Ok(value)
        })();
        let _ = FileExt::unlock(&lock);
        outcome
    }

    /// Add `repos` to the group named `name`, creating it if absent. Repo names
    /// already present are not duplicated. Returns `true` if the group was newly
    /// created.
    pub fn add_repos(&mut self, name: String, repos: Vec<String>) -> bool {
        let (group, created) = match self.groups.iter_mut().find(|g| g.name == name) {
            Some(g) => (g, false),
            None => {
                self.groups.push(Group {
                    name,
                    repos: Vec::new(),
                });
                (self.groups.last_mut().expect("just pushed"), true)
            }
        };
        for repo in repos {
            if !group.repos.contains(&repo) {
                group.repos.push(repo);
            }
        }
        created
    }

    /// Remove a repo from a group; returns whether one was removed.
    pub fn remove_repo(&mut self, name: &str, repo: &str) -> bool {
        match self.groups.iter_mut().find(|g| g.name == name) {
            Some(g) => {
                let before = g.repos.len();
                g.repos.retain(|r| r != repo);
                g.repos.len() != before
            }
            None => false,
        }
    }

    /// Remove the group with the given name; returns whether one was removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.groups.len();
        self.groups.retain(|g| g.name != name);
        self.groups.len() != before
    }

    /// Look up a group by name.
    pub fn get(&self, name: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.name == name)
    }
}

/// The default groups path, `~/.stratograph/groups.json`, from `HOME` (or
/// `USERPROFILE` on Windows). `None` if neither is set.
pub fn default_groups_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".stratograph").join("groups.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn add_repos_creates_then_extends_without_duplicates() {
        let mut groups = Groups::default();
        check!(groups.add_repos("svc".into(), vec!["api".into(), "core".into()]));
        check!(!groups.add_repos("svc".into(), vec!["core".into(), "web".into()]));
        check!(groups.get("svc").unwrap().repos == vec!["api", "core", "web"]);
    }

    #[test]
    fn remove_repo_and_group() {
        let mut groups = Groups::default();
        groups.add_repos("svc".into(), vec!["api".into(), "core".into()]);
        check!(groups.remove_repo("svc", "api"));
        check!(!groups.remove_repo("svc", "api")); // already gone
        check!(groups.get("svc").unwrap().repos == vec!["core"]);
        check!(groups.remove("svc"));
        check!(!groups.remove("svc"));
        check!(groups.groups.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("stratograph-groups-{nanos}/groups.json"));
        check!(Groups::load(&path).unwrap() == Groups::default()); // missing -> empty
        let mut groups = Groups::default();
        groups.add_repos("svc".into(), vec!["api".into()]);
        groups.save(&path).unwrap();
        check!(Groups::load(&path).unwrap() == groups);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    // #129-style: concurrent `mutate` writers must not lose updates.
    #[test]
    fn mutate_serializes_concurrent_writers() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("stratograph-groups-lock-{nanos}"));
        let path = dir.join("groups.json");
        const N: usize = 16;
        std::thread::scope(|s| {
            for i in 0..N {
                let path = path.clone();
                s.spawn(move || {
                    Groups::mutate(&path, |g| {
                        g.add_repos(format!("g{i}"), vec!["r".into()]);
                    })
                    .unwrap();
                });
            }
        });
        check!(Groups::load(&path).unwrap().groups.len() == N);
        std::fs::remove_dir_all(&dir).ok();
    }
}
