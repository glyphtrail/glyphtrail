use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use meridian_core::config::RepoPaths;
use meridian_core::{Registry, default_registry_path};

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
    /// List registered repositories.
    List,
    /// Remove a repository from the registry by name.
    Remove { name: String },
}

fn registry_path() -> Result<PathBuf> {
    default_registry_path()
        .ok_or_else(|| anyhow!("cannot locate home directory (set HOME or USERPROFILE)"))
}

/// Re-analyze every registered repo (`analyze --all`). Per-repo failures are
/// reported and don't abort the run.
pub fn analyze_all(update: bool) -> Result<()> {
    each_repo("analyze", |root| super::analyze::run(root, update))
}

/// Show index stats for every registered repo (`status --all`).
pub fn status_all() -> Result<()> {
    each_repo("status", super::status::run)
}

/// Run `op` over every registered repo, printing a per-repo header and a final
/// success/failure summary.
fn each_repo(verb: &str, op: impl Fn(&std::path::Path) -> Result<()>) -> Result<()> {
    let registry = Registry::load(&registry_path()?)?;
    if registry.repos.is_empty() {
        println!("(no repositories registered; use `meridian repo add`)");
        return Ok(());
    }
    let (mut ok, mut failed) = (0u32, 0u32);
    for e in &registry.repos {
        println!("== {} ({}) ==", e.name, e.root.display());
        match op(&e.root) {
            Ok(()) => ok += 1,
            Err(err) => {
                failed += 1;
                eprintln!("  {}: {err:#}", e.name);
            }
        }
    }
    println!("{verb}: {ok} ok, {failed} failed");
    Ok(())
}

pub fn run(cmd: RepoCmd) -> Result<()> {
    let path = registry_path()?;
    let mut registry = Registry::load(&path)?;

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
            let added = registry.add(name.clone(), root.clone());
            registry.save(&path)?;
            println!(
                "{} '{}' -> {}",
                if added { "registered" } else { "updated" },
                name,
                root.display()
            );
        }
        RepoCmd::List => {
            if registry.repos.is_empty() {
                println!("(no repositories registered)");
            }
            for e in &registry.repos {
                let indexed = RepoPaths::new(&e.root).db_path.exists();
                println!(
                    "{:<20} {}{}",
                    e.name,
                    e.root.display(),
                    if indexed { "" } else { "  (not indexed)" }
                );
            }
        }
        RepoCmd::Remove { name } => {
            if registry.remove(&name) {
                registry.save(&path)?;
                println!("removed '{name}'");
            } else {
                bail!("no repository named '{name}' in the registry");
            }
        }
    }
    Ok(())
}
