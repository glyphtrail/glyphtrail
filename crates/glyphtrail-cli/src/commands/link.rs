//! `glyphtrail link` — edit the cross-repo link hints (#281) that live in the
//! unified config's `[[links]]` array, without hand-writing the nested TOML.
//! Writes the committed `glyphtrail.toml` by default, or the gitignored personal
//! `.glyphtrail/glyphtrail.toml` with `--local`; other config keys in the file
//! are preserved.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Subcommand;
use glyphtrail_core::link_hints::{LinkEnd, LinkHint};

use crate::commands::config_file;

#[derive(Subcommand)]
pub enum LinkCmd {
    /// List the declared hints (committed and local), with their indices.
    List {
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
        /// Producer REST endpoint (e.g. "POST /signin", or "/signin" for any
        /// method) — pins the call to the route by signature, not symbol name.
        #[arg(long)]
        to_endpoint: Option<String>,
        /// Consumer symbol in this repo; omit for a coarse link.
        #[arg(long)]
        from_symbol: Option<String>,
        /// Consumer REST endpoint in this repo (same format as --to-endpoint).
        #[arg(long)]
        from_endpoint: Option<String>,
        /// Consumer repo (defaults to this repo, i.e. `.`).
        #[arg(long)]
        from_repo: Option<String>,
        /// Write to the personal override instead of the committed file.
        #[arg(long)]
        local: bool,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Remove a hint by its 1-based index (from `link list`) in the chosen file.
    #[command(alias = "rm")]
    Remove {
        /// 1-based index within the committed file (or the local file with --local).
        index: usize,
        #[arg(long)]
        local: bool,
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
            to_endpoint,
            from_symbol,
            from_endpoint,
            from_repo,
            local,
            repo,
        } => add(
            &repo,
            local,
            to_repo,
            to_symbol,
            to_endpoint,
            from_repo,
            from_symbol,
            from_endpoint,
        ),
        LinkCmd::Remove { index, local, repo } => remove(&repo, local, index),
    }
}

/// One line describing a hint, e.g. `.:fetchUser -> user-svc:get_user` or
/// `.:login -> backend:[POST /signin]`.
fn describe(h: &LinkHint) -> String {
    let end = |e: &LinkEnd| {
        let repo = e.repo.as_deref().unwrap_or(".");
        // A side may carry a symbol, an endpoint, or both (resolved as a union).
        let mut parts: Vec<String> = Vec::new();
        if let Some(s) = &e.symbol {
            parts.push(s.clone());
        }
        if let Some(ep) = &e.endpoint {
            parts.push(format!("[{ep}]"));
        }
        if parts.is_empty() {
            repo.to_string()
        } else {
            format!("{repo}:{}", parts.join("+"))
        }
    };
    format!("{} -> {}", end(&h.from), end(&h.to))
}

/// The `[[links]]` entries of a config file, as parsed hints (index-aligned).
fn links_of(path: &Path) -> Result<Vec<LinkHint>> {
    let table = config_file::load_table(path)?;
    let Some(toml::Value::Array(items)) = table.get("links") else {
        return Ok(Vec::new());
    };
    Ok(items
        .iter()
        .filter_map(|v| v.clone().try_into().ok())
        .collect())
}

fn list(repo: &Path) -> Result<()> {
    let mut any = false;
    for (label, path) in [
        ("committed", config_file::committed(repo)),
        ("local", config_file::local(repo)),
    ] {
        let links = links_of(&path)?;
        if links.is_empty() {
            continue;
        }
        any = true;
        println!("{label} ({}):", path.display());
        for (i, h) in links.iter().enumerate() {
            println!("  {}. {}", i + 1, describe(h));
        }
    }
    if !any {
        println!("no link hints (use `glyphtrail repo link add <repo> [--to-symbol ..]`)");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add(
    repo: &Path,
    local: bool,
    to_repo: String,
    to_symbol: Option<String>,
    to_endpoint: Option<String>,
    from_repo: Option<String>,
    from_symbol: Option<String>,
    from_endpoint: Option<String>,
) -> Result<()> {
    config_file::migrate_legacy(repo)?;
    let path = config_file::target(repo, local);
    let hint = LinkHint {
        from: LinkEnd {
            // `.`/None means "this repo"; only record a real other repo.
            repo: from_repo.filter(|r| r != "."),
            symbol: from_symbol,
            endpoint: from_endpoint,
        },
        to: LinkEnd {
            repo: Some(to_repo),
            symbol: to_symbol,
            endpoint: to_endpoint,
        },
    };
    let value = toml::Value::try_from(&hint)?;

    let mut table = config_file::load_table(&path)?;
    match table
        .entry("links")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
    {
        toml::Value::Array(arr) => arr.push(value),
        _ => bail!("`links` in {} is not an array", path.display()),
    }
    config_file::save(&path, &table)?;
    println!("added: {}  ({})", describe(&hint), path.display());
    Ok(())
}

fn remove(repo: &Path, local: bool, index: usize) -> Result<()> {
    config_file::migrate_legacy(repo)?;
    let path = config_file::target(repo, local);
    let mut table = config_file::load_table(&path)?;
    let arr = match table.get_mut("links") {
        Some(toml::Value::Array(arr)) => arr,
        _ => bail!("no link hints in {}", path.display()),
    };
    if index == 0 || index > arr.len() {
        bail!(
            "no hint at index {index} in {} ({} hint(s); see `glyphtrail repo link list`)",
            path.display(),
            arr.len()
        );
    }
    let removed = arr.remove(index - 1);
    config_file::save(&path, &table)?;
    let desc = removed
        .try_into()
        .map(|h: LinkHint| describe(&h))
        .unwrap_or_else(|_| "hint".to_string());
    println!("removed: {desc}  ({})", path.display());
    Ok(())
}
