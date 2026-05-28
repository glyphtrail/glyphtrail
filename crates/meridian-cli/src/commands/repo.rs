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

pub fn run(cmd: RepoCmd) -> Result<()> {
    let path = default_registry_path()
        .ok_or_else(|| anyhow!("cannot locate home directory (set HOME or USERPROFILE)"))?;
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
