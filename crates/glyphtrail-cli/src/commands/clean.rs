//! `glyphtrail clean` — remove a repo's local index, so it leaves no trace (#402).
//!
//! By default it removes just the index database (the generated data), keeping any
//! personal config / link hints under `.glyphtrail/`; `--all` removes the whole
//! `.glyphtrail/` directory. `--deregister` also drops the repo from the global
//! registry. CLI-only (not an MCP tool) — it deletes on disk.

use std::path::Path;

use anyhow::{Context, Result};
use glyphtrail_core::config::RepoPaths;
use glyphtrail_core::{Registry, default_registry_path};

pub fn run(path: &Path, all: bool, deregister: bool) -> Result<()> {
    let paths = RepoPaths::new(path);
    let index_dir = &paths.index_dir;

    if !index_dir.exists() {
        println!("no index at {} (nothing to clean)", index_dir.display());
    } else if all {
        std::fs::remove_dir_all(index_dir)
            .with_context(|| format!("removing {}", index_dir.display()))?;
        println!("removed {}", index_dir.display());
    } else {
        // Remove the generated database (the ladybug store + the graph.db anchor),
        // leaving any config/links the user authored under `.glyphtrail/`.
        let mut removed = Vec::new();
        for target in [index_dir.join("ladybug"), paths.db_path.clone()] {
            if target.is_dir() {
                std::fs::remove_dir_all(&target)
                    .with_context(|| format!("removing {}", target.display()))?;
                removed.push(target);
            } else if target.is_file() {
                std::fs::remove_file(&target)
                    .with_context(|| format!("removing {}", target.display()))?;
                removed.push(target);
            }
        }
        // Drop the index dir if nothing (no config) is left in it.
        let now_empty = std::fs::read_dir(index_dir)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if now_empty {
            let _ = std::fs::remove_dir(index_dir);
            println!("removed {}", index_dir.display());
        } else if removed.is_empty() {
            println!(
                "no index database under {} (config kept)",
                index_dir.display()
            );
        } else {
            println!(
                "removed the index database under {} (config/links kept); use --all to remove the rest",
                index_dir.display(),
            );
        }
    }

    if deregister {
        deregister_repo(path)?;
    }
    Ok(())
}

/// Remove the repo at `path` from the global registry, by its canonical root.
fn deregister_repo(path: &Path) -> Result<()> {
    let Some(reg_path) = default_registry_path() else {
        println!("no registry to deregister from");
        return Ok(());
    };
    let root = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    // Resolve the name and remove it in one locked `mutate`, so a load error
    // surfaces (not swallowed) and we only report success when the repo was found
    // and removed under the lock (no unlocked-lookup-then-mutate race).
    let removed = Registry::mutate(&reg_path, |r| {
        let name = r.name_at_root(&root).map(str::to_string);
        if let Some(name) = &name {
            r.remove(name);
        }
        name
    })?;
    match removed {
        Some(name) => println!("deregistered '{name}'"),
        None => println!("repo not in the registry (nothing to deregister)"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn temp_repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gt-clean-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let index = dir.join(".glyphtrail");
        std::fs::create_dir_all(index.join("ladybug")).unwrap();
        std::fs::write(index.join("ladybug/data"), b"db").unwrap();
        std::fs::write(index.join("glyphtrail.toml"), b"[impact]\n").unwrap();
        dir
    }

    #[test]
    fn default_removes_db_but_keeps_config() {
        let dir = temp_repo();
        run(&dir, false, false).unwrap();
        let index = dir.join(".glyphtrail");
        check!(!index.join("ladybug").exists()); // db removed
        check!(index.join("glyphtrail.toml").exists()); // config kept
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn all_removes_the_whole_index_dir() {
        let dir = temp_repo();
        run(&dir, true, false).unwrap();
        check!(!dir.join(".glyphtrail").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
