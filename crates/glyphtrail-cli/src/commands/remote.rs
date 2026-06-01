//! Clone and index a remote repository (#291).
//!
//! `glyphtrail analyze <git-url>` clones the repo into a managed directory under
//! the user-config folder (`~/.glyphtrail/remote/<host>/<owner>/<repo>`), then
//! the normal analysis pipeline runs over that clone and it is registered like
//! any local repo. Re-running updates the clone in place. Cloning uses the
//! ambient git credentials (ssh-agent / credential helper), so private repos
//! work when the user's git is configured.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use glyphtrail_core::{Registry, RepoHealth, canonicalize_remote, default_registry_path};

use crate::commands::repo::registry_path;

/// Whether `arg` is a git remote URL rather than a local path. A path that
/// exists on disk always wins, so a local checkout is never mistaken for a URL.
pub fn is_remote_arg(arg: &str) -> bool {
    if Path::new(arg).exists() {
        return false;
    }
    arg.contains("://") || arg.ends_with(".git") || (arg.contains('@') && arg.contains(':'))
}

/// `~/.glyphtrail/remote` — where managed clones of remote repos live.
pub(crate) fn remote_dir() -> Result<PathBuf> {
    let registry =
        default_registry_path().ok_or_else(|| anyhow!("cannot locate home directory"))?;
    // registry is `~/.glyphtrail/registry.json`; its sibling `remote/` holds clones.
    Ok(registry.with_file_name("remote"))
}

/// Clone `url` into `~/.glyphtrail/remote/<host/owner/repo>` (or update the clone
/// already there), returning its local path. Shallow (`--depth 1`) by default;
/// `full` clones the complete history (needed for `story` and `impact --since`).
pub fn ensure_cloned(url: &str, full: bool) -> Result<PathBuf> {
    let slug = canonicalize_remote(url)
        .ok_or_else(|| anyhow!("could not parse '{url}' as a git remote URL"))?;
    let dest = remote_dir()?.join(&slug);

    if dest.join(".git").is_dir() {
        println!("updating clone {} ({})", slug, dest.display());
        update_clone(&dest, full)?;
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        println!("cloning {url} -> {}", dest.display());
        let mut cmd = Command::new("git");
        cmd.arg("clone");
        if !full {
            cmd.args(["--depth", "1"]);
        }
        cmd.arg(url).arg(&dest);
        run_git(cmd, &format!("clone {url}"))?;
    }
    Ok(dest)
}

/// Fetch and hard-reset an existing managed clone to its remote default branch,
/// so a force-push or rebase upstream is reflected exactly (the clone is ours,
/// never edited locally).
fn update_clone(dest: &Path, full: bool) -> Result<()> {
    let default_branch = Command::new("git")
        .arg("-C")
        .arg(dest)
        .args(["rev-parse", "--abbrev-ref", "origin/HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "origin/HEAD".to_string());

    let mut fetch = Command::new("git");
    fetch.arg("-C").arg(dest).arg("fetch");
    if !full {
        fetch.args(["--depth", "1"]);
    }
    fetch.arg("origin");
    run_git(fetch, "fetch")?;

    let mut reset = Command::new("git");
    reset
        .arg("-C")
        .arg(dest)
        .args(["reset", "--hard", &default_branch]);
    run_git(reset, "reset")?;
    Ok(())
}

fn run_git(mut cmd: Command, what: &str) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("running `git {what}` (is git installed?)"))?;
    if !status.success() {
        bail!("`git {what}` failed");
    }
    Ok(())
}

/// Manage repositories cloned by `analyze <url>` (#291 follow-up).
#[derive(Subcommand)]
pub enum RemoteCmd {
    /// List remote-analyzed repositories and their local clone paths.
    List,
    /// Delete a cloned remote repo (its files and index) and unregister it.
    #[command(alias = "rm")]
    Remove {
        /// Registry name of the remote repo (omit with --all).
        name: Option<String>,
        /// Remove every remote-analyzed repository.
        #[arg(long)]
        all: bool,
    },
}

pub fn run(cmd: RemoteCmd) -> Result<()> {
    match cmd {
        RemoteCmd::List => list(),
        RemoteCmd::Remove { name, all } => remove(name, all),
    }
}

/// Whether a registered repo's root lives under the managed remote clone dir,
/// i.e. it was created by `analyze <url>` rather than `repo add`.
fn is_clone(root: &Path, dir: &Path) -> bool {
    root.starts_with(dir)
}

fn health_label(h: RepoHealth) -> &'static str {
    match h {
        RepoHealth::Indexed => "indexed",
        RepoHealth::Unindexed => "unindexed",
        RepoHealth::Missing => "missing",
    }
}

fn list() -> Result<()> {
    let registry = Registry::load(&registry_path()?)?;
    let dir = remote_dir()?;
    let clones: Vec<_> = registry
        .repos
        .iter()
        .filter(|e| is_clone(&e.root, &dir))
        .collect();
    if clones.is_empty() {
        println!("no remote-analyzed repositories (use `glyphtrail analyze <git-url>`)");
        return Ok(());
    }
    for e in clones {
        println!(
            "{}  {}  [{}]",
            e.name,
            e.root.display(),
            health_label(e.health())
        );
        for id in &e.ids {
            println!("  id {} ({})", id.id, id.source);
        }
    }
    Ok(())
}

fn remove(name: Option<String>, all: bool) -> Result<()> {
    let path = registry_path()?;
    let registry = Registry::load(&path)?;
    let dir = remote_dir()?;

    // Resolve the clone(s) to drop: all of them, or one by name (which must be a
    // managed clone — refuse to touch a `repo add`ed local working copy).
    let targets: Vec<(String, PathBuf)> = match (all, name) {
        (true, _) => registry
            .repos
            .iter()
            .filter(|e| is_clone(&e.root, &dir))
            .map(|e| (e.name.clone(), e.root.clone()))
            .collect(),
        (false, Some(name)) => {
            let entry = registry
                .repos
                .iter()
                .find(|e| e.name == name)
                .ok_or_else(|| anyhow!("no repository named '{name}' in the registry"))?;
            if !is_clone(&entry.root, &dir) {
                bail!(
                    "'{name}' is not a remote-analyzed repo (root {}); use `glyphtrail repo remove`",
                    entry.root.display()
                );
            }
            vec![(entry.name.clone(), entry.root.clone())]
        }
        (false, None) => bail!("provide a repository name, or --all to remove every remote clone"),
    };
    if targets.is_empty() {
        println!("no remote-analyzed repositories to remove");
        return Ok(());
    }

    for (name, root) in &targets {
        // Safety: only ever delete inside the managed remote dir.
        if root.starts_with(&dir) && root.exists() {
            std::fs::remove_dir_all(root)
                .with_context(|| format!("removing clone {}", root.display()))?;
        }
        println!("removed clone {} ({name})", root.display());
    }
    let names: Vec<String> = targets.into_iter().map(|(n, _)| n).collect();
    Registry::mutate(&path, |reg| {
        for n in &names {
            reg.remove(n);
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn detects_remote_urls_but_not_local_paths() {
        check!(is_remote_arg("https://github.com/o/r"));
        check!(is_remote_arg("https://github.com/o/r.git"));
        check!(is_remote_arg("git@github.com:o/r.git"));
        check!(is_remote_arg("ssh://git@host/o/r.git"));
        check!(is_remote_arg("git://host/o/r"));
        // A bare relative/absolute path is local.
        check!(!is_remote_arg("some/relative/path"));
        check!(!is_remote_arg("/abs/path/to/repo"));
        // An existing local path wins even if it superficially looks URL-ish.
        check!(!is_remote_arg("."));
    }
}
