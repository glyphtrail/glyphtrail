use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use stratograph_core::{Registry, RepoHealth, default_registry_path, repo_ids};
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
            let name = name.unwrap_or_else(|| {
                root.file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "repo".into())
            });
            // Stable forge identities from the repo's git remotes (#233), so the
            // same repo is recognisable across renames, clones, and name
            // collisions, and mirrors all resolve to one repo. Slug ids always;
            // forge-API numeric ids (rename-proof) added when a token is present.
            let remotes = git_remote_urls(&root);
            let mut ids = repo_ids(&remotes);
            // Forge-API numeric ids (rename-proof), using the forge token map at
            // ~/.stratograph/forge.toml (sibling of the registry) when present.
            let forge_config = ForgeConfig::load_or_default(&path.with_file_name("forge.toml"));
            for numeric in forge_numeric_ids(&remotes, &forge_config) {
                if !ids.iter().any(|i| i.id == numeric.id) {
                    ids.push(numeric);
                }
            }
            let added = Registry::mutate(&path, |reg| {
                let added = reg.add(name.clone(), root.clone());
                reg.set_ids(&name, ids.clone());
                added
            })?;
            println!(
                "{} '{}' -> {}",
                if added { "registered" } else { "updated" },
                name,
                root.display()
            );
            for id in &ids {
                println!("  id {} ({})", id.id, id.source);
            }
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
    }
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
