//! Storage backend selection (#10): open the SQLite or LadybugDB backend
//! behind the common `GraphStore` trait, so analyze and query can target
//! either. The LadybugDB backend is only available when built with the
//! `ladybug` feature.

use std::path::PathBuf;

use anyhow::Result;
use clap::ValueEnum;
use meridian_core::config::RepoPaths;
use meridian_store::{GraphStore, SqliteStore};

#[derive(Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum BackendKind {
    /// SQLite + FTS5 (default; a single `.meridian/graph.db` file).
    #[default]
    Sqlite,
    /// LadybugDB / Cypher (a `.meridian/ladybug` directory; needs `--features ladybug`).
    Ladybug,
}

impl BackendKind {
    /// On-disk location of this backend's index for `paths`.
    pub fn location(self, paths: &RepoPaths) -> PathBuf {
        match self {
            BackendKind::Sqlite => paths.db_path.clone(),
            BackendKind::Ladybug => paths.index_dir.join("ladybug"),
        }
    }

    /// Whether an index for this backend already exists.
    pub fn exists(self, paths: &RepoPaths) -> bool {
        self.location(paths).exists()
    }
}

/// Open (creating if needed) the selected backend for `paths`.
pub fn open(paths: &RepoPaths, kind: BackendKind) -> Result<Box<dyn GraphStore>> {
    match kind {
        BackendKind::Sqlite => Ok(Box::new(SqliteStore::open(&paths.db_path)?)),
        BackendKind::Ladybug => open_ladybug(paths),
    }
}

#[cfg(feature = "ladybug")]
fn open_ladybug(paths: &RepoPaths) -> Result<Box<dyn GraphStore>> {
    Ok(Box::new(meridian_store::LadybugStore::open(
        &paths.index_dir.join("ladybug"),
    )?))
}

#[cfg(not(feature = "ladybug"))]
fn open_ladybug(_paths: &RepoPaths) -> Result<Box<dyn GraphStore>> {
    anyhow::bail!("the ladybug backend is not built; rebuild with `--features ladybug`")
}
