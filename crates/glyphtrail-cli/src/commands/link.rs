//! `glyphtrail link` — edit manual cross-repo link hints (#281) without
//! hand-writing the nested TOML. Operates on one overlay at a time: the
//! committed, team-shared `glyphtrail.links.toml` at the repo root by default,
//! or the gitignored personal `.glyphtrail/links.toml` with `--local`.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Subcommand;
use glyphtrail_core::config::RepoPaths;
use glyphtrail_core::link_hints::{HINTS_FILE, LOCAL_HINTS_FILE, LinkEnd, LinkHint, LinkHints};

#[derive(Subcommand)]
pub enum LinkCmd {
    /// List the declared hints (committed and local), with their indices.
    List {
        /// Repository root.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Add a hint: this repo (`from`) depends on `to_repo`'s symbol/whole repo.
    Add {
        /// The producer repo this one depends on (the other repo).
        to_repo: String,
        /// Producer symbol; omit for a coarse whole-repo link.
        #[arg(long)]
        to_symbol: Option<String>,
        /// Consumer symbol in this repo; omit for a coarse link.
        #[arg(long)]
        from_symbol: Option<String>,
        /// Consumer repo (defaults to this repo, i.e. `.`).
        #[arg(long)]
        from_repo: Option<String>,
        /// Write to the gitignored personal override instead of the committed file.
        #[arg(long)]
        local: bool,
        /// Repository root.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Remove a hint by its 1-based index (from `link list`) in the chosen file.
    #[command(alias = "rm")]
    Remove {
        /// 1-based index within the committed file (or the local file with --local).
        index: usize,
        /// Operate on the gitignored personal override.
        #[arg(long)]
        local: bool,
        /// Repository root.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
}

pub fn run(cmd: LinkCmd) -> Result<()> {
    match cmd {
        LinkCmd::List { repo } => list(&repo),
        LinkCmd::Add {
            to_repo,
            to_symbol,
            from_symbol,
            from_repo,
            local,
            repo,
        } => add(&repo, local, to_repo, to_symbol, from_repo, from_symbol),
        LinkCmd::Remove { index, local, repo } => remove(&repo, local, index),
    }
}

/// The committed root file and the gitignored local override for `repo`.
fn committed_file(repo: &Path) -> PathBuf {
    repo.join(HINTS_FILE)
}
fn local_file(repo: &Path) -> PathBuf {
    RepoPaths::new(repo).index_dir.join(LOCAL_HINTS_FILE)
}

/// One line describing a hint, e.g. `fetchUser -> user-svc:get_user`.
fn describe(h: &LinkHint) -> String {
    let end = |e: &LinkEnd, default_repo: &str| match (&e.repo, &e.symbol) {
        (Some(r), Some(s)) => format!("{r}:{s}"),
        (Some(r), None) => r.clone(),
        (None, Some(s)) => format!("{default_repo}:{s}"),
        (None, None) => default_repo.to_string(),
    };
    format!("{} -> {}", end(&h.from, "."), end(&h.to, "."))
}

fn list(repo: &Path) -> Result<()> {
    let mut any = false;
    for (label, path) in [
        ("shared", committed_file(repo)),
        ("local", local_file(repo)),
    ] {
        let hints = LinkHints::load_file(&path);
        if hints.links.is_empty() {
            continue;
        }
        any = true;
        println!("{label} ({}):", path.display());
        for (i, h) in hints.links.iter().enumerate() {
            println!("  {}. {}", i + 1, describe(h));
        }
    }
    if !any {
        println!("no link hints (use `glyphtrail link add <repo> [--to-symbol ..]`)");
    }
    Ok(())
}

fn add(
    repo: &Path,
    local: bool,
    to_repo: String,
    to_symbol: Option<String>,
    from_repo: Option<String>,
    from_symbol: Option<String>,
) -> Result<()> {
    let path = if local {
        local_file(repo)
    } else {
        committed_file(repo)
    };
    let mut hints = LinkHints::load_file(&path);
    let hint = LinkHint {
        from: LinkEnd {
            // `.`/None means "this repo"; only record a real other repo.
            repo: from_repo.filter(|r| r != "."),
            symbol: from_symbol,
        },
        to: LinkEnd {
            repo: Some(to_repo),
            symbol: to_symbol,
        },
    };
    println!("added: {}  -> {}", describe(&hint), path.display());
    hints.links.push(hint);
    hints.save_file(&path)?;
    Ok(())
}

fn remove(repo: &Path, local: bool, index: usize) -> Result<()> {
    let path = if local {
        local_file(repo)
    } else {
        committed_file(repo)
    };
    let mut hints = LinkHints::load_file(&path);
    if index == 0 || index > hints.links.len() {
        bail!(
            "no hint at index {index} in {} ({} hint(s); see `glyphtrail link list`)",
            path.display(),
            hints.links.len()
        );
    }
    let removed = hints.links.remove(index - 1);
    hints.save_file(&path)?;
    println!("removed: {}  ({})", describe(&removed), path.display());
    Ok(())
}
