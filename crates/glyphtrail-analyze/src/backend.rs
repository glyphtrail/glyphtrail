//! Storage backend access (#10): open the LadybugDB graph store behind the
//! common `GraphStore` trait. LadybugDB is the sole backend; the trait keeps
//! command code decoupled from the concrete store type.

use std::path::PathBuf;

use anyhow::{Result, bail};
use glyphtrail_core::config::RepoPaths;
use glyphtrail_store::{GraphStore, LadybugStore};

/// On-disk location of the index for `paths`.
pub fn location(paths: &RepoPaths) -> PathBuf {
    paths.index_dir.join("ladybug")
}

/// Whether an index already exists for `paths`.
pub fn exists(paths: &RepoPaths) -> bool {
    location(paths).exists()
}

/// Open (creating if needed) the store for `paths`. Used by the write path
/// (`analyze`). The store is `Send` so it can back the HTTP server's state.
pub fn open(paths: &RepoPaths) -> Result<Box<dyn GraphStore + Send>> {
    Ok(Box::new(LadybugStore::open(&location(paths))?))
}

/// Open an existing index for `paths`, erroring if none has been built yet.
/// Used by read-only commands (status, query, impact, viz) so they fail with a
/// clear "run analyze first" instead of silently creating an empty index.
pub fn open_existing(paths: &RepoPaths) -> Result<Box<dyn GraphStore + Send>> {
    if !exists(paths) {
        bail!(
            "no index found at {} — run `glyphtrail analyze` first",
            location(paths).display()
        );
    }
    open(paths)
}
