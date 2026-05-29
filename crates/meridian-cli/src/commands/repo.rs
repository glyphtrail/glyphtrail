use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use meridian_core::{Registry, RepoHealth, default_registry_path};

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
}

fn registry_path() -> Result<PathBuf> {
    default_registry_path()
        .ok_or_else(|| anyhow!("cannot locate home directory (set HOME or USERPROFILE)"))
}

/// Re-analyze every registered repo (`analyze --all`). Per-repo failures are
/// reported and don't abort the run.
pub fn analyze_all(update: bool, backend: super::backend::BackendKind) -> Result<()> {
    each_repo("analyze", |root| super::analyze::run(root, update, backend))
}

/// Show index stats for every registered repo (`status --all`).
pub fn status_all() -> Result<()> {
    each_repo("status", super::status::run)
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
        println!("(no repositories registered; use `meridian repo add`)");
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
            let name = name.unwrap_or_else(|| {
                root.file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "repo".into())
            });
            let added = Registry::mutate(&path, |reg| reg.add(name.clone(), root.clone()))?;
            println!(
                "{} '{}' -> {}",
                if added { "registered" } else { "updated" },
                name,
                root.display()
            );
        }
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
    }
    Ok(())
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
