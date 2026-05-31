//! Global registry of indexed repositories, stored at
//! `~/.stratograph/registry.json`. It tracks repo roots by name so the analyzer
//! can target or span many repositories; each repo's `.stratograph/` index remains
//! the source of truth.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};

use crate::config::RepoPaths;
use crate::{CoreError, RepoId, Result};

/// Liveness of a registered repository on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoHealth {
    /// Root exists and contains an index.
    Indexed,
    /// Root exists but has not been analyzed yet.
    Unindexed,
    /// Root path no longer exists (moved, renamed, or deleted).
    Missing,
}

/// One registered repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub name: String,
    /// Absolute repository root (contains the `.stratograph/` index).
    pub root: PathBuf,
    /// Unix seconds when the root was first observed missing; cleared when it
    /// reappears. Drives `prune_missing` so dead entries don't collect dust,
    /// while tolerating transient glitches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_since: Option<i64>,
    /// Stable forge identities of this repo (#233), derived from its git
    /// remotes — an origin plus any mirrors. Independent of the folder/repo
    /// name, so the same repo is recognisable across renames, clones, and
    /// name collisions. Empty for a repo with no recognised remotes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<RepoId>,
}

impl RegistryEntry {
    /// Current on-disk health of this entry.
    pub fn health(&self) -> RepoHealth {
        if !self.root.exists() {
            RepoHealth::Missing
        } else if RepoPaths::new(&self.root)
            .index_dir
            .join("ladybug")
            .exists()
        {
            RepoHealth::Indexed
        } else {
            RepoHealth::Unindexed
        }
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
    /// The write is atomic: the JSON is staged in a process-unique temp file in
    /// the same directory and then renamed over `path`, so a concurrent reader
    /// or an interrupted write never sees a truncated registry.
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

    /// Serialize a load → modify → save cycle under an exclusive OS advisory
    /// lock on a sibling `registry.lock` file, so concurrent `repo` writers in
    /// separate processes can't lose an update (last-rename-wins, #129). The
    /// registry is (re)loaded *inside* the lock so each writer sees the latest
    /// state, and persisted before the lock drops. Returns the closure's value.
    ///
    /// Hold the lock only for the quick mutation; never run long work (analysis)
    /// inside `f`.
    pub fn mutate<R>(path: &Path, f: impl FnOnce(&mut Registry) -> R) -> Result<R> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock_path = path.with_extension("lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        // Blocks until the exclusive lock is acquired; released on drop too.
        FileExt::lock_exclusive(&lock)?;
        let outcome = (|| {
            let mut reg = Registry::load(path)?;
            let value = f(&mut reg);
            reg.save(path)?;
            Ok(value)
        })();
        let _ = FileExt::unlock(&lock);
        outcome
    }

    /// Register a repo, replacing any existing entry with the same name.
    /// Returns `true` if it was newly added, `false` if it replaced one.
    pub fn add(&mut self, name: String, root: PathBuf) -> bool {
        match self.repos.iter_mut().find(|e| e.name == name) {
            Some(existing) => {
                existing.root = root;
                existing.missing_since = None;
                false
            }
            None => {
                self.repos.push(RegistryEntry {
                    name,
                    root,
                    missing_since: None,
                    ids: Vec::new(),
                });
                true
            }
        }
    }

    /// Set the stable forge identities for a named entry (#233), replacing any
    /// existing set. No-op if no entry has that name.
    pub fn set_ids(&mut self, name: &str, ids: Vec<RepoId>) {
        if let Some(e) = self.repos.iter_mut().find(|e| e.name == name) {
            e.ids = ids;
        }
    }

    /// Find a registered repo by any of its stable forge ids (#233), so the same
    /// repo is recognisable across renames, clones, and name collisions.
    pub fn find_by_id(&self, id: &str) -> Option<&RegistryEntry> {
        self.repos.iter().find(|e| e.ids.iter().any(|r| r.id == id))
    }

    /// Reconcile `missing_since` with the current filesystem: stamp newly-missing
    /// roots, clear it for roots that reappeared. Returns `true` if anything
    /// changed (the caller should persist).
    pub fn refresh_health(&mut self) -> bool {
        let now = now_secs();
        let mut changed = false;
        for e in &mut self.repos {
            match (e.root.exists(), e.missing_since) {
                (false, None) => {
                    e.missing_since = Some(now);
                    changed = true;
                }
                (true, Some(_)) => {
                    e.missing_since = None;
                    changed = true;
                }
                _ => {}
            }
        }
        changed
    }

    /// Remove entries whose root has been missing for at least `max_age_secs`.
    /// Returns the names dropped. Call `refresh_health` first so stamps are
    /// current.
    pub fn prune_missing(&mut self, max_age_secs: i64) -> Vec<String> {
        let now = now_secs();
        let mut removed = Vec::new();
        self.repos.retain(|e| {
            let stale = e
                .missing_since
                .map(|t| now - t >= max_age_secs)
                .unwrap_or(false);
            if stale {
                removed.push(e.name.clone());
            }
            !stale
        });
        removed
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

/// The default registry path, `~/.stratograph/registry.json`, from `HOME` (or
/// `USERPROFILE` on Windows). `None` if neither is set.
pub fn default_registry_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".stratograph")
            .join("registry.json"),
    )
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
    fn refresh_health_stamps_and_clears_missing() {
        let mut reg = Registry::default();
        reg.add("gone".into(), PathBuf::from("/nope/does/not/exist"));
        reg.add("here".into(), std::env::temp_dir());

        check!(reg.refresh_health()); // first observation -> change
        check!(reg.get("gone").unwrap().missing_since.is_some());
        check!(reg.get("here").unwrap().missing_since == None);
        check!(!reg.refresh_health()); // stable -> no change

        // A re-add of the missing entry against an existing root clears the stamp.
        reg.add("gone".into(), std::env::temp_dir());
        check!(reg.get("gone").unwrap().missing_since == None);
    }

    #[test]
    fn prune_missing_drops_only_stale_entries() {
        let mut reg = Registry::default();
        reg.add("fresh".into(), PathBuf::from("/nope/a"));
        reg.add("stale".into(), PathBuf::from("/nope/b"));
        // Stamp both as missing now, then backdate "stale" past the threshold.
        reg.refresh_health();
        let now = now_secs();
        reg.repos
            .iter_mut()
            .find(|e| e.name == "stale")
            .unwrap()
            .missing_since = Some(now - 10_000);

        let removed = reg.prune_missing(3600); // 1h threshold
        check!(removed == vec!["stale".to_string()]);
        check!(reg.get("stale").is_none());
        check!(reg.get("fresh").is_some());
    }

    #[test]
    fn remove_reports_whether_present() {
        let mut reg = Registry::default();
        reg.add("api".into(), PathBuf::from("/a"));
        check!(reg.remove("api"));
        check!(!reg.remove("api"));
        check!(reg.repos.is_empty());
    }

    // #233: a repo is findable by any of its forge ids (origin or a mirror),
    // independent of its registry name.
    #[test]
    fn find_by_id_matches_any_forge_id() {
        use crate::RepoId;
        let mut reg = Registry::default();
        reg.add("strato".into(), PathBuf::from("/a"));
        reg.set_ids(
            "strato",
            vec![
                RepoId {
                    id: "uuid-gh".into(),
                    source: "github.com/o/r".into(),
                },
                RepoId {
                    id: "uuid-cb".into(),
                    source: "codeberg.org/o/r".into(),
                },
            ],
        );
        check!(reg.find_by_id("uuid-cb").map(|e| e.name.as_str()) == Some("strato"));
        check!(reg.find_by_id("nope").is_none());
    }

    // #129: many processes (here, threads — each opens its own fd, so the OS
    // advisory lock still serializes them) racing on `mutate` must not lose any
    // update. Without the lock the load→modify→save RMW would drop writes and
    // the final count would fall short of N.
    #[test]
    fn mutate_serializes_concurrent_writers() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("stratograph-reg-lock-{nanos}"));
        let path = dir.join("registry.json");
        const N: usize = 16;
        std::thread::scope(|s| {
            for i in 0..N {
                let path = path.clone();
                s.spawn(move || {
                    Registry::mutate(&path, |reg| {
                        reg.add(format!("r{i}"), PathBuf::from(format!("/p/{i}")));
                    })
                    .unwrap();
                });
            }
        });
        let reg = Registry::load(&path).unwrap();
        check!(
            reg.repos.len() == N,
            "expected {N} repos, got {}",
            reg.repos.len()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_then_load_roundtrips_and_missing_is_empty() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("stratograph-reg-{nanos}/registry.json"));
        check!(Registry::load(&path).unwrap() == Registry::default()); // missing -> empty
        let mut reg = Registry::default();
        reg.add("web".into(), PathBuf::from("/srv/web"));
        reg.save(&path).unwrap();
        check!(Registry::load(&path).unwrap() == reg);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
