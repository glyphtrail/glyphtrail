//! `stratograph group` — manage named groups of registered repositories (#221).
//!
//! A group scopes a federated operation (cross-repo impact, link sync) to a
//! subset of the registry. Groups point at registry repos by name; `status`
//! reconciles those names against the registry so dangling or unindexed members
//! are visible before a federated query relies on them.

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use std::path::PathBuf;
use stratograph_core::{Groups, Registry, RepoHealth, default_groups_path, default_registry_path};

#[derive(Subcommand)]
pub enum GroupCmd {
    /// Create a group or add repositories to it (by registry name).
    Add {
        /// Group name.
        name: String,
        /// Registry repo names to include.
        #[arg(required = true)]
        repos: Vec<String>,
    },
    /// List groups and their member repositories.
    List,
    /// Remove a whole group, or a single repo from it with `--repo`.
    Remove {
        /// Group name.
        name: String,
        /// Remove only this repo from the group, leaving the group in place.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Resolve a group's members against the registry and show their health.
    Status {
        /// Group name (defaults to every group).
        name: Option<String>,
    },
}

fn groups_path() -> Result<PathBuf> {
    default_groups_path()
        .ok_or_else(|| anyhow!("cannot locate home directory (set HOME or USERPROFILE)"))
}

fn registry_path() -> Result<PathBuf> {
    default_registry_path()
        .ok_or_else(|| anyhow!("cannot locate home directory (set HOME or USERPROFILE)"))
}

pub fn run(cmd: GroupCmd) -> Result<()> {
    let path = groups_path()?;
    match cmd {
        GroupCmd::Add { name, repos } => {
            let created = Groups::mutate(&path, |g| g.add_repos(name.clone(), repos.clone()))?;
            println!(
                "{} group '{}' ({} repo{})",
                if created { "created" } else { "updated" },
                name,
                repos.len(),
                if repos.len() == 1 { "" } else { "s" }
            );
        }
        GroupCmd::List => {
            let groups = Groups::load(&path)?;
            if groups.groups.is_empty() {
                println!("(no groups defined; use `stratograph group add`)");
            }
            for g in &groups.groups {
                println!("{:<20} {}", g.name, g.repos.join(", "));
            }
        }
        GroupCmd::Remove { name, repo } => match repo {
            Some(repo) => {
                if Groups::mutate(&path, |g| g.remove_repo(&name, &repo))? {
                    println!("removed '{repo}' from group '{name}'");
                } else {
                    bail!("group '{name}' has no repo '{repo}'");
                }
            }
            None => {
                if Groups::mutate(&path, |g| g.remove(&name))? {
                    println!("removed group '{name}'");
                } else {
                    bail!("no group named '{name}'");
                }
            }
        },
        GroupCmd::Status { name } => status(&path, name.as_deref())?,
    }
    Ok(())
}

/// Print each selected group's members with their registry health.
fn status(path: &std::path::Path, only: Option<&str>) -> Result<()> {
    let groups = Groups::load(path)?;
    let registry = Registry::load(&registry_path()?)?;

    let selected: Vec<_> = match only {
        Some(n) => match groups.get(n) {
            Some(g) => vec![g],
            None => bail!("no group named '{n}'"),
        },
        None => groups.groups.iter().collect(),
    };
    if selected.is_empty() {
        println!("(no groups defined; use `stratograph group add`)");
        return Ok(());
    }

    for g in selected {
        println!("== {} ==", g.name);
        if g.repos.is_empty() {
            println!("  (no repos)");
        }
        for repo in &g.repos {
            let note = match registry.get(repo) {
                None => "unregistered".to_string(),
                Some(e) => match e.health() {
                    RepoHealth::Indexed => format!("indexed  {}", e.root.display()),
                    RepoHealth::Unindexed => format!("not indexed  {}", e.root.display()),
                    RepoHealth::Missing => format!("missing  {}", e.root.display()),
                },
            };
            println!("  {repo:<20} {note}");
        }
    }
    Ok(())
}
