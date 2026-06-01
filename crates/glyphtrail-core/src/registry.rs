//! Global registry of indexed repositories, stored at
//! `~/.glyphtrail/registry.json`. It tracks repo roots by name so the analyzer
//! can target or span many repositories; each repo's `.glyphtrail/` index remains
//! the source of truth.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::RepoPaths;
use crate::{CoreError, PackageIdentity, RepoId, Result};

/// Sibling lock file for a registry path (`registry.lock`).
pub fn lock_path(registry_path: &Path) -> PathBuf {
    registry_path.with_extension("lock")
}

/// Prefix of spillover files (`registry.spill.<pid>.<nanos>.json`): deferred
/// adds written when the lock can't be acquired, merged in by the next
/// lock-holding run. See [`Registry::record`].
const SPILL_PREFIX: &str = "registry.spill.";

/// Result of [`Registry::record`]: the adds were applied to the registry (with
/// a per-entry [`Resolution`], in input order), or deferred to a spillover file
/// because the lock was busy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    Applied(Vec<Resolution>),
    Spilled,
}

/// How [`Registry::upsert_repo`] resolved one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// A brand-new repository entry.
    Added,
    /// An existing same-named entry was refreshed in place.
    Updated,
    /// Recognised as the same repo (shared forge id) as an existing entry (#272),
    /// named by `into`. `new_path` is true when this registration contributed a
    /// location not already known for that repo.
    SameRepo { into: String, new_path: bool },
}

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
    /// Primary absolute repository root (contains the `.glyphtrail/` index).
    pub root: PathBuf,
    /// Additional known locations of the *same* repo — a symlink, a second
    /// clone, a backup copy (#272). The same repo found at another path is
    /// recognised by its forge id and folded in here instead of creating a
    /// duplicate entry (which would double-count it in federated impact).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alt_roots: Vec<PathBuf>,
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
    /// Top git authors by commit count (#265), recorded at registration so
    /// "which repos did I work on?" is answerable across the whole registry
    /// without touching each repo's git history. Capped; empty for a non-git
    /// repo.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributors: Vec<Contributor>,
    /// Cross-repo package identity (#292): the packages this repo publishes (with
    /// their exports) and the external packages its files use. Cached here at
    /// analyze time so federated impact can resolve cross-repo links from the
    /// loaded registry — and open only the link-connected member stores — instead
    /// of opening every member just to read its identity. `None` for an entry
    /// indexed before this cache existed; federation backfills it on demand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<PackageIdentity>,
}

/// A git author and how many commits they have in a repo (#265).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contributor {
    pub name: String,
    pub email: String,
    pub commits: u32,
}

impl RegistryEntry {
    /// Whether `needle` matches a contributor's email or name, case-insensitively
    /// (substring). Drives `repo list --author` / `--mine` (#265).
    pub fn has_contributor(&self, needle: &str) -> bool {
        let needle = needle.to_lowercase();
        self.contributors.iter().any(|c| {
            c.email.to_lowercase().contains(&needle) || c.name.to_lowercase().contains(&needle)
        })
    }
}

impl RegistryEntry {
    /// All known locations of the repo: the primary root first, then any
    /// alternates (#272).
    pub fn roots(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.root).chain(self.alt_roots.iter())
    }

    /// The location to operate on: the first root that exists on disk, falling
    /// back to the primary root when none do. Lets a repo's index be found at a
    /// backup/symlink path when the primary is offline (#272).
    pub fn active_root(&self) -> &PathBuf {
        self.roots().find(|r| r.exists()).unwrap_or(&self.root)
    }

    /// Whether `path` (as given) is already one of this entry's roots.
    fn has_root(&self, path: &Path) -> bool {
        self.roots().any(|r| r == path)
    }

    /// Record an additional location of this repo, unless already known (#272).
    fn add_path(&mut self, path: PathBuf) {
        if !self.has_root(&path) {
            self.alt_roots.push(path);
        }
    }

    /// Current on-disk health, taken as the best across all known roots: indexed
    /// if any location has an index, missing only if every location is gone.
    pub fn health(&self) -> RepoHealth {
        let mut any_exists = false;
        for root in self.roots() {
            if root.exists() {
                any_exists = true;
                if RepoPaths::new(root).index_dir.join("ladybug").exists() {
                    return RepoHealth::Indexed;
                }
            }
        }
        if any_exists {
            RepoHealth::Unindexed
        } else {
            RepoHealth::Missing
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

    /// Serialize a load → modify → save cycle under the portable file lock on a
    /// sibling `registry.lock`, so concurrent `repo` writers can't lose an update
    /// (last-write-wins, #129). The registry is (re)loaded *inside* the lock so
    /// each writer sees the latest state, and persisted before the lock drops.
    /// The lock works on network/FUSE filesystems and self-heals a stale lock
    /// (see [`crate::filelock`]). Returns the closure's value.
    ///
    /// Hold the lock only for the quick mutation; never run long work (analysis)
    /// inside `f`.
    pub fn mutate<R>(path: &Path, f: impl FnOnce(&mut Registry) -> R) -> Result<R> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::filelock::with_lock(&lock_path(path), || {
            let mut reg = Registry::load(path)?;
            ingest_spillovers(path, &mut reg); // string in any deferred adds first
            let value = f(&mut reg);
            // Self-heal same-root rename duplicates on every write, not just via
            // `record` — `analyze`'s identity pingback reaches the registry through
            // here, so this is where its duplicates would otherwise persist (#350).
            reg.dedup_by_root();
            reg.save(path)?;
            Ok(value)
        })
    }

    /// Upsert `entries` into the registry, deferring to a spillover file when the
    /// lock is busy rather than failing (#129). When the lock is obtained, any
    /// pending spillovers are merged first; otherwise the entries are written to
    /// `registry.spill.<pid>.<nanos>.json` (atomically) and a later
    /// lock-holding run strings them in. Returns whether the write applied
    /// directly or spilled.
    ///
    /// This is the loss-proof path for `repo add`/scan on slow or contended
    /// filesystems; arbitrary mutations still go through [`Self::mutate`], which
    /// errors rather than spilling (its change can't be replayed generically).
    pub fn record(path: &Path, entries: Vec<RegistryEntry>) -> Result<RecordOutcome> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let applied = crate::filelock::with_lock(&lock_path(path), || {
            let mut reg = Registry::load(path)?;
            ingest_spillovers(path, &mut reg);
            let resolutions: Vec<Resolution> =
                entries.iter().map(|e| reg.upsert_repo(e.clone())).collect();
            // Self-heal any same-root duplicate left by an earlier rename (#350).
            reg.dedup_by_root();
            reg.save(path)?;
            Ok(resolutions)
        });
        match applied {
            Ok(resolutions) => Ok(RecordOutcome::Applied(resolutions)),
            Err(CoreError::Lock { .. }) => {
                write_spillover(path, &entries)?;
                Ok(RecordOutcome::Spilled)
            }
            Err(e) => Err(e),
        }
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
                    alt_roots: Vec::new(),
                    missing_since: None,
                    ids: Vec::new(),
                    contributors: Vec::new(),
                    identity: None,
                });
                true
            }
        }
    }

    /// Insert or fold in `entry`, the loss-proof registration primitive (used by
    /// [`Self::record`]). Resolution order (#272):
    ///
    /// 1. **Same repo by forge id** — an existing entry shares any of `entry`'s
    ///    ids: treat them as the same repo and fold `entry`'s path(s) in as
    ///    additional locations (the first-registered entry keeps primacy), union
    ///    the ids, and clear `missing_since`. This is what stops a working copy
    ///    and its backup/symlink becoming two entries (and double-counting in
    ///    federated impact).
    /// 2. **Same name** — refresh that entry in place (the name-keyed behaviour
    ///    that predates ids; a repo with no remotes has no id to match on).
    /// 3. Otherwise a brand-new entry.
    pub fn upsert_repo(&mut self, entry: RegistryEntry) -> Resolution {
        // 1. Same repo (shared forge id) -> fold the path(s) in.
        if !entry.ids.is_empty()
            && let Some(existing) = self.repos.iter_mut().find(|e| {
                e.ids
                    .iter()
                    .any(|id| entry.ids.iter().any(|nid| nid.id == id.id))
            })
        {
            let before = existing.alt_roots.len();
            let new_roots: Vec<PathBuf> = entry.roots().cloned().collect();
            for r in new_roots {
                existing.add_path(r);
            }
            for id in entry.ids {
                if !existing.ids.iter().any(|e| e.id == id.id) {
                    existing.ids.push(id);
                }
            }
            if !entry.contributors.is_empty() {
                existing.contributors = entry.contributors; // refresh from this scan
            }
            existing.missing_since = None;
            return Resolution::SameRepo {
                into: existing.name.clone(),
                new_path: existing.alt_roots.len() > before,
            };
        }
        // 2. Same name -> refresh in place.
        if let Some(existing) = self.repos.iter_mut().find(|e| e.name == entry.name) {
            existing.root = entry.root;
            existing.alt_roots = entry.alt_roots;
            existing.missing_since = None;
            existing.ids = entry.ids;
            existing.contributors = entry.contributors;
            return Resolution::Updated;
        }
        // 3. New repo.
        self.repos.push(entry);
        Resolution::Added
    }

    /// Collapse entries that point at the same physical repository — they share a
    /// root, compared canonically when it resolves on disk and by the path as
    /// written otherwise — into a single entry. The most recently registered name
    /// wins (a rename's new name); ids and alternate locations are unioned, and
    /// contributors are refreshed to the most recent non-empty scan (the same
    /// semantics as [`Self::upsert_repo`]). This repairs duplicates left when a
    /// repo is re-registered under a new name before its forge ids line up —
    /// which `upsert_repo`'s id/name matching alone does not catch (#350).
    /// Returns whether anything merged.
    pub fn dedup_by_root(&mut self) -> bool {
        // One pass folds each entry into the *first* kept entry it overlaps, so an
        // entry whose roots bridge two previously-distinct entries needs a further
        // pass to collapse them. Repeat to a fixpoint (each merging pass drops at
        // least one entry, so it terminates).
        let mut merged_any = false;
        while self.dedup_pass() {
            merged_any = true;
        }
        merged_any
    }

    /// A single [`Self::dedup_by_root`] pass. Returns whether anything merged.
    fn dedup_pass(&mut self) -> bool {
        fn key(p: &Path) -> PathBuf {
            p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
        }
        // Carry each kept entry's root keys so the O(n^2) overlap scan never
        // re-canonicalizes (one stat pass per entry, not per comparison).
        let mut kept: Vec<(RegistryEntry, Vec<PathBuf>)> = Vec::with_capacity(self.repos.len());
        let mut merged = false;
        for entry in std::mem::take(&mut self.repos) {
            let entry_roots: Vec<PathBuf> = entry.roots().cloned().collect();
            let entry_keys: Vec<PathBuf> = entry_roots.iter().map(|r| key(r)).collect();
            match kept
                .iter()
                .position(|(_, keys)| keys.iter().any(|k| entry_keys.contains(k)))
            {
                Some(i) => {
                    merged = true;
                    let (target, keys) = &mut kept[i];
                    // `entry` was registered later, so its name is the current one.
                    target.name = entry.name;
                    for r in entry_roots {
                        target.add_path(r);
                    }
                    for id in entry.ids {
                        if !target.ids.iter().any(|e| e.id == id.id) {
                            target.ids.push(id);
                        }
                    }
                    if !entry.contributors.is_empty() {
                        target.contributors = entry.contributors;
                    }
                    if target.identity.is_none() {
                        target.identity = entry.identity;
                    }
                    // Missing only if *both* were missing; then keep the earliest
                    // stamp so `prune_missing` isn't delayed.
                    target.missing_since = match (target.missing_since, entry.missing_since) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        _ => None,
                    };
                    for k in entry_keys {
                        if !keys.contains(&k) {
                            keys.push(k);
                        }
                    }
                }
                None => kept.push((entry, entry_keys)),
            }
        }
        self.repos = kept.into_iter().map(|(e, _)| e).collect();
        merged
    }

    /// Set the stable forge identities for a named entry (#233), replacing any
    /// existing set. No-op if no entry has that name.
    pub fn set_ids(&mut self, name: &str, ids: Vec<RepoId>) {
        if let Some(e) = self.repos.iter_mut().find(|e| e.name == name) {
            e.ids = ids;
        }
    }

    /// Cache a repo's cross-repo [`PackageIdentity`] on its entry, matched by
    /// canonical root (#292). No-op when no registered repo owns `root` (an
    /// unregistered repo doesn't federate). Returns whether an entry was updated.
    pub fn set_identity_by_root(&mut self, root: &Path, identity: PackageIdentity) -> bool {
        let canon = root.canonicalize().ok();
        let matches =
            |r: &PathBuf| r == root || (canon.is_some() && r.canonicalize().ok() == canon);
        if let Some(e) = self.repos.iter_mut().find(|e| e.roots().any(&matches)) {
            e.identity = Some(identity);
            true
        } else {
            false
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
            // Missing only when *every* known location is gone (#272).
            let exists = e.roots().any(|r| r.exists());
            match (exists, e.missing_since) {
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

/// Merge any spillover files (`registry.spill.*.json`) beside `registry_path`
/// into `reg`, then delete them. Must be called while holding the lock. Each
/// spillover is a `Vec<RegistryEntry>` of deferred adds; entries are upserted
/// (last-write-wins, same as a direct add). Tolerant: a malformed or empty
/// spillover is skipped *and removed* (it can only be leftover garbage — a
/// spillover being written is still a `.tmp`, see [`write_spillover`]).
fn ingest_spillovers(registry_path: &Path, reg: &mut Registry) {
    let Some(dir) = registry_path.parent() else {
        return;
    };
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with(SPILL_PREFIX) && name.ends_with(".json")) {
            continue; // skips the `.tmp` of an in-flight spillover
        }
        let p = entry.path();
        if let Ok(text) = std::fs::read_to_string(&p)
            && let Ok(spilled) = serde_json::from_str::<Vec<RegistryEntry>>(&text)
        {
            for e in spilled {
                reg.upsert_repo(e); // id-merges, like a direct add (#272)
            }
        }
        let _ = std::fs::remove_file(&p); // consumed (or undecodable garbage)
    }
}

/// Write `entries` to a uniquely-named spillover file beside `registry_path`,
/// for a later lock-holding run to [`ingest_spillovers`]. Written to a `.tmp`
/// then atomically renamed into place, so a reader never observes a partially
/// written spillover.
fn write_spillover(registry_path: &Path, entries: &[RegistryEntry]) -> Result<PathBuf> {
    let dir = registry_path.parent().unwrap_or_else(|| Path::new("."));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stem = format!("{SPILL_PREFIX}{}.{nanos}.json", std::process::id());
    let final_path = dir.join(&stem);
    let tmp = dir.join(format!("{stem}.tmp"));
    std::fs::write(
        &tmp,
        serde_json::to_vec(entries).expect("entries serialize"),
    )?;
    std::fs::rename(&tmp, &final_path)?; // atomic publish
    Ok(final_path)
}

/// The default registry path, `~/.glyphtrail/registry.json`, from `HOME` (or
/// `USERPROFILE` on Windows). `None` if neither is set.
pub fn default_registry_path() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?);
    migrate_legacy_home(&home);
    Some(home.join(".glyphtrail").join("registry.json"))
}

/// Silently upgrade the pre-rename home state to the current names (#293, the
/// project was `stratograph`): move `~/.stratograph` -> `~/.glyphtrail` (registry,
/// groups, forge.toml) and `~/.stratographignore` -> `~/.glyphtrailignore`, each
/// only when the old exists and the new one doesn't. Best-effort; called from
/// the path resolvers so it runs before the registry/groups are read.
pub fn migrate_legacy_home(home: &Path) {
    for (legacy, current) in [
        (".stratograph", ".glyphtrail"),
        (".stratographignore", ".glyphtrailignore"),
    ] {
        let (old, new) = (home.join(legacy), home.join(current));
        if old.exists() && !new.exists() {
            let _ = std::fs::rename(&old, &new);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    // #293: the pre-rename home dir + user-wide ignore are moved on first use.
    #[test]
    fn migrate_legacy_home_moves_dir_and_ignore_file() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = std::env::temp_dir().join(format!("glyphtrail-home-{nanos}"));
        std::fs::create_dir_all(home.join(".stratograph")).unwrap();
        std::fs::write(home.join(".stratograph").join("registry.json"), b"{}").unwrap();
        std::fs::write(home.join(".stratographignore"), b"pattern\n").unwrap();

        migrate_legacy_home(&home);
        check!(home.join(".glyphtrail").join("registry.json").exists());
        check!(home.join(".glyphtrailignore").exists());
        check!(!home.join(".stratograph").exists());
        check!(!home.join(".stratographignore").exists());
        std::fs::remove_dir_all(&home).ok();
    }

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

    // #129: many writers racing on `mutate` must not lose any update. The
    // portable file lock serializes the load→modify→save RMW; without it writes
    // would be dropped and the final count would fall short of N.
    #[test]
    fn mutate_serializes_concurrent_writers() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("glyphtrail-reg-lock-{nanos}"));
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
        let path = std::env::temp_dir().join(format!("glyphtrail-reg-{nanos}/registry.json"));
        check!(Registry::load(&path).unwrap() == Registry::default()); // missing -> empty
        let mut reg = Registry::default();
        reg.add("web".into(), PathBuf::from("/srv/web"));
        reg.save(&path).unwrap();
        check!(Registry::load(&path).unwrap() == reg);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    // #292: cross-repo identity rides on the entry, set by canonical root, and an
    // old registry without the field still loads (identity reads as None).
    #[test]
    fn set_identity_by_root_and_back_compat() {
        let mut reg = Registry::default();
        reg.add("a".into(), PathBuf::from("/x/a"));
        check!(reg.repos[0].identity.is_none());

        let id = PackageIdentity {
            packages: vec![crate::IndexedPackage {
                ecosystem: crate::Ecosystem::Cargo,
                name: "a".into(),
                version: None,
                dir: String::new(),
                exports: Vec::new(),
            }],
            external_uses: Vec::new(),
        };
        check!(reg.set_identity_by_root(Path::new("/x/a"), id));
        check!(reg.repos[0].identity.is_some());
        // Unregistered root is a no-op.
        check!(!reg.set_identity_by_root(Path::new("/nope"), PackageIdentity::default()));

        // Round-trips through JSON…
        let s = serde_json::to_string(&reg).unwrap();
        let back: Registry = serde_json::from_str(&s).unwrap();
        check!(back.repos[0].identity.is_some());

        // …and an older registry.json without `identity` loads as None.
        let old: Registry =
            serde_json::from_str(r#"{"repos":[{"name":"a","root":"/x/a"}]}"#).unwrap();
        check!(old.repos[0].identity.is_none());
    }

    fn reg_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("glyphtrail-reg-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(name: &str, root: &str) -> RegistryEntry {
        RegistryEntry {
            name: name.into(),
            root: PathBuf::from(root),
            alt_roots: Vec::new(),
            missing_since: None,
            ids: Vec::new(),
            contributors: Vec::new(),
            identity: None,
        }
    }

    fn entry_with_id(name: &str, root: &str, id: &str) -> RegistryEntry {
        RegistryEntry {
            ids: vec![RepoId {
                id: id.into(),
                source: format!("forge/{id}"),
            }],
            ..entry(name, root)
        }
    }

    // #272: the same repo (shared forge id) discovered at a second path folds
    // in as an alternate location instead of creating a duplicate entry.
    #[test]
    fn upsert_merges_same_repo_by_id_into_one_entry() {
        let mut reg = Registry::default();
        check!(reg.upsert_repo(entry_with_id("proj", "/work/proj", "uuid-1")) == Resolution::Added);
        let res = reg.upsert_repo(entry_with_id("proj-backup", "/backup/proj", "uuid-1"));
        check!(
            res == Resolution::SameRepo {
                into: "proj".into(),
                new_path: true
            }
        );
        check!(reg.repos.len() == 1); // one repo, two paths
        let e = &reg.repos[0];
        check!(e.root == PathBuf::from("/work/proj")); // first registered keeps primacy
        check!(e.alt_roots == vec![PathBuf::from("/backup/proj")]);
        // Re-adding a known path folds in nothing new.
        check!(
            reg.upsert_repo(entry_with_id("proj", "/work/proj", "uuid-1"))
                == Resolution::SameRepo {
                    into: "proj".into(),
                    new_path: false
                }
        );
        check!(reg.repos[0].alt_roots.len() == 1);
    }

    // #265: contributor match drives `repo list --author/--mine`.
    #[test]
    fn has_contributor_matches_name_or_email_case_insensitively() {
        let mut e = entry("proj", "/p");
        e.contributors = vec![Contributor {
            name: "Jane Doe".into(),
            email: "jane@example.com".into(),
            commits: 42,
        }];
        check!(e.has_contributor("JANE@example.com"));
        check!(e.has_contributor("jane doe"));
        check!(e.has_contributor("example.com"));
        check!(!e.has_contributor("bob"));
    }

    // A repo with no forge id can't be id-merged; it stays name-keyed.
    #[test]
    fn upsert_without_ids_is_name_keyed() {
        let mut reg = Registry::default();
        check!(reg.upsert_repo(entry("a", "/a")) == Resolution::Added);
        check!(reg.upsert_repo(entry("a", "/a2")) == Resolution::Updated);
        check!(reg.repos.len() == 1);
        check!(reg.repos[0].root == PathBuf::from("/a2"));
    }

    // #350: a rename re-registers the same root under a new name. Even without an
    // id match, the same-root entries collapse into one and the new name wins.
    #[test]
    fn dedup_by_root_folds_a_renamed_duplicate() {
        let mut reg = Registry::default();
        reg.repos.push(entry("oldname", "/work/proj"));
        reg.repos.push(entry("newname", "/work/proj"));
        check!(reg.dedup_by_root());
        check!(reg.repos.len() == 1);
        check!(reg.repos[0].name == "newname"); // most recently registered wins
    }

    // Distinct repos at distinct roots are left untouched.
    #[test]
    fn dedup_by_root_keeps_distinct_repos() {
        let mut reg = Registry::default();
        reg.repos.push(entry("a", "/work/a"));
        reg.repos.push(entry("b", "/work/b"));
        check!(!reg.dedup_by_root());
        check!(reg.repos.len() == 2);
    }

    // Folding a same-root duplicate unions forge ids so neither identity is lost.
    #[test]
    fn dedup_by_root_unions_ids() {
        let mut reg = Registry::default();
        reg.repos.push(entry_with_id("old", "/work/proj", "x"));
        reg.repos.push(entry_with_id("new", "/work/proj", "y"));
        check!(reg.dedup_by_root());
        check!(reg.repos.len() == 1);
        check!(reg.repos[0].ids.iter().any(|r| r.id == "x"));
        check!(reg.repos[0].ids.iter().any(|r| r.id == "y"));
    }

    // `mutate` (the path `analyze`'s identity pingback uses) self-heals same-root
    // duplicates, so a rename leftover doesn't survive the next analyze (#350).
    #[test]
    fn mutate_dedups_same_root_entries() {
        let dir = reg_dir("mutate-dedup");
        let path = dir.join("registry.json");
        let mut reg = Registry::default();
        reg.repos.push(entry("oldname", "/work/proj"));
        reg.repos.push(entry("newname", "/work/proj"));
        reg.save(&path).unwrap();
        Registry::mutate(&path, |_| {}).unwrap();
        let reloaded = Registry::load(&path).unwrap();
        check!(reloaded.repos.len() == 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    // A later entry whose roots bridge two previously-distinct entries collapses
    // all three — the fixpoint loop, not just a single first-match pass.
    #[test]
    fn dedup_by_root_collapses_transitive_bridge() {
        let mut reg = Registry::default();
        reg.repos.push(entry("a", "/x"));
        reg.repos.push(entry("b", "/y"));
        let mut bridge = entry("c", "/x");
        bridge.alt_roots.push(PathBuf::from("/y"));
        reg.repos.push(bridge);
        check!(reg.dedup_by_root());
        check!(reg.repos.len() == 1);
    }

    // `record` applies under the lock and reports it.
    #[test]
    fn record_applies_when_lock_is_free() {
        let dir = reg_dir("rec");
        let path = dir.join("registry.json");
        check!(matches!(
            Registry::record(&path, vec![entry("api", "/a")]).unwrap(),
            RecordOutcome::Applied(_)
        ));
        check!(Registry::load(&path).unwrap().get("api").unwrap().root == PathBuf::from("/a"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // A spillover file left by a deferred add is merged in (and deleted) by the
    // next lock-holding write, so no add is lost on a contended lock.
    #[test]
    fn ingest_merges_and_removes_spillovers() {
        let dir = reg_dir("spill");
        let path = dir.join("registry.json");
        // Seed an existing registry, then drop a spillover beside it by hand.
        Registry::record(&path, vec![entry("existing", "/old")]).unwrap();
        let spill = write_spillover(&path, &[entry("queued", "/new")]).unwrap();
        check!(spill.exists());

        // Any lock-holding op ingests it.
        Registry::mutate(&path, |_| {}).unwrap();
        let reg = Registry::load(&path).unwrap();
        check!(reg.get("queued").unwrap().root == PathBuf::from("/new"));
        check!(reg.get("existing").is_some());
        check!(!spill.exists()); // consumed
        std::fs::remove_dir_all(&dir).ok();
    }

    // An in-flight spillover (still a `.tmp`) and undecodable garbage must not
    // corrupt the registry: the `.tmp` is left alone, garbage `.json` is dropped.
    #[test]
    fn ingest_skips_tmp_and_drops_garbage() {
        let dir = reg_dir("spill-safe");
        let path = dir.join("registry.json");
        Registry::record(&path, vec![entry("base", "/b")]).unwrap();
        let tmp = dir.join(format!("{SPILL_PREFIX}999.123.json.tmp"));
        std::fs::write(&tmp, b"{ partial").unwrap(); // mid-write, not yet renamed
        let garbage = dir.join(format!("{SPILL_PREFIX}999.456.json"));
        std::fs::write(&garbage, b"not json at all").unwrap();

        Registry::mutate(&path, |_| {}).unwrap();
        let reg = Registry::load(&path).unwrap();
        check!(reg.repos.len() == 1); // only "base"; nothing garbage merged
        check!(tmp.exists()); // in-flight tmp untouched
        check!(!garbage.exists()); // decoded-as-garbage .json dropped
        std::fs::remove_dir_all(&dir).ok();
    }
}
