//! `glyphtrail link` — edit the cross-repo link hints (#281) that live in the
//! unified config's `[[links]]` array, without hand-writing the nested TOML.
//! Writes the committed `glyphtrail.toml` by default, or the gitignored personal
//! `.glyphtrail/glyphtrail.toml` with `--local`; other config keys in the file
//! are preserved.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Subcommand;
use glyphtrail_core::link_hints::{LinkEnd, LinkHint};
use glyphtrail_core::{Registry, default_registry_path, resolved_link_repo};

use crate::commands::config_file;

/// The registry + the declaring repo's `(name, canonical root)`, for resolving a
/// hint's `repo` sides. `None` registry when it can't be loaded (validation is
/// then skipped). The owner name falls back to `.` for an unregistered repo.
fn link_context(repo: &Path) -> (Option<Registry>, String, std::path::PathBuf) {
    let registry = default_registry_path().and_then(|p| Registry::load(&p).ok());
    let owner_root = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let owner_name = registry
        .as_ref()
        .and_then(|r| {
            r.repos
                .iter()
                .find(|e| {
                    e.roots().any(|root| {
                        root.canonicalize().ok().as_deref() == Some(owner_root.as_path())
                    })
                })
                .map(|e| e.name.clone())
        })
        .unwrap_or_else(|| ".".to_string());
    (registry, owner_name, owner_root)
}

/// The unresolved sides of a hint (`"from 'x', to 'y'"`), or `None` when every
/// explicit-repo side resolves to a registered repo (#418). A `.`/absent side is
/// always the declaring repo, so it's never dead.
fn unresolved_sides(
    h: &LinkHint,
    owner_name: &str,
    owner_root: &Path,
    registry: &Registry,
) -> Option<String> {
    let mut dead = Vec::new();
    for (label, end) in [("from", &h.from), ("to", &h.to)] {
        if let Some(repo) = &end.repo
            && resolved_link_repo(&end.repo, owner_name, owner_root, registry).is_none()
        {
            dead.push(format!("{label} '{repo}'"));
        }
    }
    (!dead.is_empty()).then(|| dead.join(", "))
}

#[derive(Subcommand)]
pub enum LinkCmd {
    /// List the declared hints (committed and local), with their indices.
    List {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Add a hint: this repo (`from`) depends on `to_repo`'s symbol/whole repo.
    Add {
        /// The producer repo this one depends on: a registry name, or a `./`,
        /// `../`, or absolute *path* (resolved to the registered repo there — so
        /// a path is distinct from a slashed name like a GitLab `group/repo`).
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
        /// Consumer repo (defaults to this repo, i.e. `.`): a name or a `./`,
        /// `../`, or absolute path.
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
    let (registry, owner_name, owner_root) = link_context(repo);
    let mut any = false;
    let mut unresolved = 0usize;
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
            let dead = registry
                .as_ref()
                .and_then(|reg| unresolved_sides(h, &owner_name, &owner_root, reg));
            match dead {
                Some(reason) => {
                    unresolved += 1;
                    println!("  {}. {}  ✗ unresolved: {reason}", i + 1, describe(h));
                }
                None => println!("  {}. {}", i + 1, describe(h)),
            }
        }
    }
    if !any {
        println!("no link hints (use `glyphtrail repo link add <repo> [--to-symbol ..]`)");
    } else if unresolved > 0 {
        println!(
            "\n{unresolved} unresolved (a repo names no registered repo — use a registry name, \
             or a ./path)"
        );
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

    // #420: flag a side whose repo names no registered repo, so the mistake
    // (a wrong name or a path pointing nowhere) is seen now, not as a silently
    // dead hint later. The hint is still written — the target may not be
    // registered yet.
    let (registry, owner_name, owner_root) = link_context(repo);
    if let Some(reg) = &registry
        && let Some(reason) = unresolved_sides(&hint, &owner_name, &owner_root, reg)
    {
        eprintln!(
            "warning: no registered repo for {reason} — use a registry name (`glyphtrail repo \
             list`) or a ./path"
        );
    }
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
