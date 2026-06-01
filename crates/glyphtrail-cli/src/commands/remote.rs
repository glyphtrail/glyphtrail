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
use glyphtrail_core::{canonicalize_remote, default_registry_path};

/// Whether `arg` is a git remote URL rather than a local path. A path that
/// exists on disk always wins, so a local checkout is never mistaken for a URL.
pub fn is_remote_arg(arg: &str) -> bool {
    if Path::new(arg).exists() {
        return false;
    }
    arg.contains("://") || arg.ends_with(".git") || (arg.contains('@') && arg.contains(':'))
}

/// `~/.glyphtrail/remote` — where managed clones of remote repos live.
fn remote_dir() -> Result<PathBuf> {
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
