//! Shared editing of the unified per-repo config file. Both `glyphtrail config`
//! and `glyphtrail link` write the same `glyphtrail.toml` (committed at the repo
//! root) or `.glyphtrail/glyphtrail.toml` (the gitignored personal override),
//! and both run [`migrate_legacy`] first so the older split files
//! (`.glyphtrail/config.toml`, `glyphtrail.links.toml`, `.glyphtrail/links.toml`)
//! are folded in and removed on the next edit.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use glyphtrail_core::config::{Config, RepoPaths};
use glyphtrail_core::link_hints::{HINTS_FILE, LOCAL_HINTS_FILE};

/// The committed, team-shared config file (`<repo>/glyphtrail.toml`).
pub fn committed(repo: &Path) -> PathBuf {
    RepoPaths::new(repo).committed_config_path()
}

/// The gitignored personal override (`.glyphtrail/glyphtrail.toml`).
pub fn local(repo: &Path) -> PathBuf {
    RepoPaths::new(repo).local_config_path()
}

/// The file an edit targets: committed by default, the personal override with
/// `--local`.
pub fn target(repo: &Path, local_flag: bool) -> PathBuf {
    if local_flag {
        local(repo)
    } else {
        committed(repo)
    }
}

/// Load a config file into a table, empty when it's absent.
pub fn load_table(path: &Path) -> Result<toml::Table> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text.parse::<toml::Table>()?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(e) => Err(e.into()),
    }
}

/// Serialize, validate against the config schema, then write — so an edit never
/// leaves an invalid config behind.
pub fn save(path: &Path, table: &toml::Table) -> Result<()> {
    let text = toml::to_string_pretty(table)?;
    Config::from_toml_str(&text)
        .map_err(|e| anyhow!("that edit would make the config invalid: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}

/// Fold any legacy split files into the unified files and delete them, keeping
/// each in its original scope (committed links stay committed; the personal
/// analysis config and personal links go to the personal override). Idempotent:
/// a no-op once the legacy files are gone.
pub fn migrate_legacy(repo: &Path) -> Result<()> {
    let paths = RepoPaths::new(repo);

    // Committed: glyphtrail.links.toml -> glyphtrail.toml (links union).
    let committed_links = repo.join(HINTS_FILE);
    if committed_links.exists() {
        let mut dst = load_table(&committed(repo))?;
        append_links(&mut dst, &committed_links)?;
        save(&committed(repo), &dst)?;
        std::fs::remove_file(&committed_links)?;
    }

    // Personal: .glyphtrail/config.toml (analysis) + .glyphtrail/links.toml -> the
    // personal override.
    let legacy_cfg = paths.legacy_config_path();
    let legacy_links = paths.index_dir.join(LOCAL_HINTS_FILE);
    if legacy_cfg.exists() || legacy_links.exists() {
        let mut dst = load_table(&local(repo))?;
        if legacy_cfg.exists() {
            let mut src = load_table(&legacy_cfg)?;
            let src_links = src.remove("links");
            for (key, value) in src {
                dst.insert(key, value);
            }
            if let Some(toml::Value::Array(items)) = src_links {
                extend_links(&mut dst, items);
            }
        }
        if legacy_links.exists() {
            append_links(&mut dst, &legacy_links)?;
        }
        save(&local(repo), &dst)?;
        let _ = std::fs::remove_file(&legacy_cfg);
        let _ = std::fs::remove_file(&legacy_links);
    }
    Ok(())
}

fn append_links(dst: &mut toml::Table, from_file: &Path) -> Result<()> {
    let mut src = load_table(from_file)?;
    if let Some(toml::Value::Array(items)) = src.remove("links") {
        extend_links(dst, items);
    }
    Ok(())
}

fn extend_links(dst: &mut toml::Table, items: Vec<toml::Value>) {
    if let toml::Value::Array(arr) = dst
        .entry("links")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
    {
        arr.extend(items);
    }
}
