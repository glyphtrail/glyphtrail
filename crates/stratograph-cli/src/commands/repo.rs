use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use stratograph_core::{
    RecordOutcome, Registry, RegistryEntry, RepoHealth, default_registry_path, filelock, lock_path,
    repo_ids,
};
use stratograph_forge_id::{ForgeConfig, forge_numeric_ids};

#[derive(Subcommand)]
pub enum RepoCmd {
    /// Register a repository in the global registry.
    Add {
        /// Repository root (defaults to the current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Name for the repo (defaults to the directory name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Recursively find repositories under a directory and register them all.
    ///
    /// Walks the tree for version-control roots (`.git`, `.svn`, `.bzr`, `.hg`)
    /// and registers each one — handy for pointing at a whole workspace at once.
    /// Registration always runs, even for an already-indexed repo, so a repo
    /// whose earlier registration failed gets picked up.
    Scan {
        /// Directory to scan (defaults to the current directory).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Also analyze each repo after registering it.
        #[arg(long)]
        analyze: bool,
        /// With --analyze, only reparse files changed since the last index.
        #[arg(long)]
        update: bool,
        /// Descend into repositories to also register nested repos (submodules,
        /// vendored checkouts). By default a repo root is a boundary.
        #[arg(long)]
        nested: bool,
    },
    /// List registered repositories and their health.
    List,
    /// Remove a repository from the registry by name.
    Remove { name: String },
    /// Drop registry entries whose root has been missing for too long.
    Prune {
        /// Minimum age (in days) a missing entry must reach before removal.
        #[arg(long, default_value_t = 30)]
        older_than_days: u64,
    },
    /// Force-release a stuck registry lock (escape hatch for a lock left by a
    /// dead writer on a network/FUSE filesystem). Safe: only removes the lock
    /// file; the registry self-heals stale locks automatically, so this is
    /// rarely needed.
    Unlock,
}

fn registry_path() -> Result<PathBuf> {
    default_registry_path()
        .ok_or_else(|| anyhow!("cannot locate home directory (set HOME or USERPROFILE)"))
}

/// Re-analyze every registered repo (`analyze --all`). Per-repo failures are
/// reported and don't abort the run.
pub fn analyze_all(update: bool) -> Result<()> {
    each_repo("analyze", |root| {
        let outcome = super::analyze::run(root, update)?;
        if outcome.up_to_date {
            println!("  up to date ({} files)", outcome.files);
        } else {
            println!(
                "  {} files, {} nodes, {} edges",
                outcome.files, outcome.nodes, outcome.edges
            );
        }
        Ok(())
    })
}

/// Show index stats for every registered repo (`status --all`).
pub fn status_all() -> Result<()> {
    each_repo("status", |root| {
        super::status::run(root, super::query::Emit::Text)
    })
}

/// Run `op` over every registered repo, printing a per-repo header and a final
/// summary. Entries whose root is missing are skipped (not failed) so a single
/// moved/deleted repo doesn't drown the run in errors; their `missing_since`
/// stamp is refreshed and persisted for later `repo prune`.
fn each_repo(verb: &str, op: impl Fn(&std::path::Path) -> Result<()>) -> Result<()> {
    let path = registry_path()?;
    // Refresh + persist health under the lock, then release it before the long
    // per-repo work below (analysis must never run while holding the lock).
    let registry = Registry::mutate(&path, |reg| {
        reg.refresh_health();
        reg.clone()
    })?;
    if registry.repos.is_empty() {
        println!("(no repositories registered; use `stratograph repo add`)");
        return Ok(());
    }
    let (mut ok, mut failed, mut skipped) = (0u32, 0u32, 0u32);
    for e in &registry.repos {
        println!("== {} ({}) ==", e.name, e.root.display());
        if e.health() == RepoHealth::Missing {
            skipped += 1;
            println!("  skipped: root is missing");
            continue;
        }
        match op(&e.root) {
            Ok(()) => ok += 1,
            Err(err) => {
                failed += 1;
                eprintln!("  {}: {err:#}", e.name);
            }
        }
    }
    println!("{verb}: {ok} ok, {failed} failed, {skipped} skipped");
    Ok(())
}

pub fn run(cmd: RepoCmd) -> Result<()> {
    let path = registry_path()?;

    // Every arm that mutates the registry runs its load → modify → save under
    // an exclusive advisory lock (#129), so concurrent `repo` processes can't
    // clobber each other's update.
    match cmd {
        RepoCmd::Add { path: repo, name } => {
            let root = repo
                .canonicalize()
                .with_context(|| format!("cannot resolve path {}", repo.display()))?;
            let entry = entry_for(&path, &root, name);
            let ids = entry.ids.clone();
            let name = entry.name.clone();
            // Loss-proof write: applies under the lock, or defers to a spillover
            // file when the lock is busy (a later run merges it in) instead of
            // failing — important when indexing many repos on a slow/contended
            // network filesystem.
            match Registry::record(&path, vec![entry])? {
                RecordOutcome::Applied => {
                    println!("registered '{}' -> {}", name, root.display());
                }
                RecordOutcome::Spilled => {
                    println!(
                        "registry busy; queued '{}' -> {} (merged on the next run)",
                        name,
                        root.display()
                    );
                }
            }
            for id in &ids {
                println!("  id {} ({})", id.id, id.source);
            }
        }
        RepoCmd::Scan {
            dir,
            analyze,
            update,
            nested,
        } => scan(&path, &dir, analyze, update, nested)?,
        RepoCmd::List => {
            let registry = Registry::mutate(&path, |reg| {
                reg.refresh_health();
                reg.clone()
            })?;
            if registry.repos.is_empty() {
                println!("(no repositories registered)");
            }
            for e in &registry.repos {
                let note = match e.health() {
                    RepoHealth::Indexed => String::new(),
                    RepoHealth::Unindexed => "  (not indexed)".into(),
                    RepoHealth::Missing => {
                        format!("  (missing{})", missing_for(e.missing_since))
                    }
                };
                println!("{:<20} {}{}", e.name, e.root.display(), note);
                for id in &e.ids {
                    println!("{:<20} ↳ {}", "", id.source);
                }
            }
        }
        RepoCmd::Remove { name } => {
            if Registry::mutate(&path, |reg| reg.remove(&name))? {
                println!("removed '{name}'");
            } else {
                bail!("no repository named '{name}' in the registry");
            }
        }
        RepoCmd::Prune { older_than_days } => {
            let removed = Registry::mutate(&path, |reg| {
                reg.refresh_health();
                reg.prune_missing(older_than_days as i64 * 86_400)
            })?;
            if removed.is_empty() {
                println!("nothing to prune (no entry missing for >= {older_than_days}d)");
            } else {
                for name in &removed {
                    println!("pruned '{name}'");
                }
            }
        }
        RepoCmd::Unlock => match filelock::force_unlock(&lock_path(&path))? {
            Some(desc) => println!("released registry lock ({desc})"),
            None => println!("no registry lock held"),
        },
    }
    Ok(())
}

/// Build a [`RegistryEntry`] for `root`: its name (the given override, else the
/// directory name) plus its stable forge identities (#233) derived from its git
/// remotes — slug ids always, forge-API numeric ids when a token is configured
/// (via `~/.stratograph/forge.toml`, the sibling of `registry_path`).
fn entry_for(registry_path: &Path, root: &Path, name: Option<String>) -> RegistryEntry {
    let name = name.unwrap_or_else(|| {
        root.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".into())
    });
    let remotes = git_remote_urls(root);
    let mut ids = repo_ids(&remotes);
    let forge_config = ForgeConfig::load_or_default(&registry_path.with_file_name("forge.toml"));
    for numeric in forge_numeric_ids(&remotes, &forge_config) {
        if !ids.iter().any(|i| i.id == numeric.id) {
            ids.push(numeric);
        }
    }
    RegistryEntry {
        name,
        root: root.to_path_buf(),
        missing_since: None,
        ids,
    }
}

/// Version-control markers that mark a directory as a repository root.
const VCS_MARKERS: [&str; 4] = [".git", ".svn", ".bzr", ".hg"];

/// Whether `dir` is a repository root (holds a VCS marker). `.git` may be a file
/// (worktrees, submodules) or a directory, so existence is enough.
fn is_repo_root(dir: &Path) -> bool {
    VCS_MARKERS.iter().any(|m| dir.join(m).exists())
}

/// Collect repository roots under `start` (depth-first). A repo root is a
/// boundary by default (its contents aren't scanned for nested repos) unless
/// `nested` is set. Symlinked directories are not followed, so cycles can't
/// trap the walk.
fn find_repo_roots(start: &Path, nested: bool, out: &mut Vec<PathBuf>) {
    if is_repo_root(start) {
        out.push(start.to_path_buf());
        if !nested {
            return; // boundary: don't descend into the repo
        }
    }
    let Ok(entries) = std::fs::read_dir(start) else {
        return; // unreadable dir (permissions) — skip, don't abort the scan
    };
    for entry in entries.flatten() {
        // Only descend into real directories; skip symlinks to avoid cycles.
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => {}
            _ => continue,
        }
        find_repo_roots(&entry.path(), nested, out);
    }
}

/// Recursively register every repository under `dir` (#263). Registration runs
/// for every repo found, independent of `--analyze`: an already-indexed repo is
/// still (re)registered, so one whose earlier registration failed (e.g. a stuck
/// lock) is recovered. With `analyze`, each repo is indexed after it's recorded.
fn scan(registry_path: &Path, dir: &Path, analyze: bool, update: bool, nested: bool) -> Result<()> {
    let root = dir
        .canonicalize()
        .with_context(|| format!("cannot resolve path {}", dir.display()))?;
    let mut roots = Vec::new();
    find_repo_roots(&root, nested, &mut roots);
    if roots.is_empty() {
        println!("no repositories found under {}", root.display());
        return Ok(());
    }

    let (mut registered, mut queued, mut analyzed, mut failed) = (0u32, 0u32, 0u32, 0u32);
    for repo in &roots {
        let entry = entry_for(registry_path, repo, None);
        let name = entry.name.clone();
        // Always register first, regardless of index state, so a repo whose
        // prior registration was lost is recovered even when it's up to date.
        match Registry::record(registry_path, vec![entry])? {
            RecordOutcome::Applied => {
                registered += 1;
                println!("registered '{}' -> {}", name, repo.display());
            }
            RecordOutcome::Spilled => {
                queued += 1;
                println!(
                    "registry busy; queued '{}' -> {} (merged on the next run)",
                    name,
                    repo.display()
                );
            }
        }
        if analyze {
            match super::analyze::run(repo, update) {
                Ok(outcome) if outcome.up_to_date => {
                    println!("  index up to date ({} files)", outcome.files);
                }
                Ok(outcome) => {
                    analyzed += 1;
                    println!(
                        "  indexed {} files: {} nodes, {} edges",
                        outcome.files, outcome.nodes, outcome.edges
                    );
                }
                Err(err) => {
                    failed += 1;
                    eprintln!("  analyze failed: {err:#}");
                }
            }
        }
    }
    print!("scan: {registered} registered");
    if queued > 0 {
        print!(", {queued} queued");
    }
    if analyze {
        print!(", {analyzed} analyzed, {failed} failed");
    }
    println!();
    Ok(())
}

/// The URLs of a repo's git remotes, de-duplicated. Best-effort: a non-git
/// directory or any git failure yields no URLs (the repo just gets no stable
/// ids — its name stays the handle).
fn git_remote_urls(root: &Path) -> Vec<String> {
    let remotes = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("remote")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut urls = Vec::new();
    for name in remotes {
        if let Some(out) = Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("remote")
            .arg("get-url")
            .arg(&name)
            .output()
            .ok()
            .filter(|o| o.status.success())
        {
            let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !url.is_empty() && !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    urls
}

/// Render how long an entry has been missing, e.g. ` for 5d`. Empty when the
/// stamp is absent (just observed missing this run).
fn missing_for(missing_since: Option<i64>) -> String {
    let Some(since) = missing_since else {
        return String::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(since);
    let days = (now - since).max(0) / 86_400;
    format!(" for {days}d")
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn scratch() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("stratograph-scan-{nanos}"));
        // alpha is a git repo; sub/beta an svn repo; alpha/vendor/gamma a nested
        // git repo; notrepo holds no VCS marker.
        for sub in [
            "alpha/.git",
            "sub/beta/.svn",
            "alpha/vendor/gamma/.git",
            "notrepo/src",
        ] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        dir
    }

    #[test]
    fn scan_stops_at_repo_boundaries_by_default() {
        let dir = scratch();
        let mut roots = Vec::new();
        find_repo_roots(&dir, false, &mut roots);
        let names: Vec<_> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        check!(roots.len() == 2); // alpha + beta; gamma is inside alpha (a boundary)
        check!(names.contains(&"alpha".to_string()));
        check!(names.contains(&"beta".to_string()));
        check!(!names.contains(&"gamma".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_nested_descends_into_repos() {
        let dir = scratch();
        let mut roots = Vec::new();
        find_repo_roots(&dir, true, &mut roots);
        let names: Vec<_> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        check!(names.contains(&"gamma".to_string())); // submodule now found
        check!(roots.len() == 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_repo_root_recognizes_markers() {
        let dir = scratch();
        check!(is_repo_root(&dir.join("alpha")));
        check!(is_repo_root(&dir.join("sub/beta")));
        check!(!is_repo_root(&dir.join("notrepo")));
        std::fs::remove_dir_all(&dir).ok();
    }
}
