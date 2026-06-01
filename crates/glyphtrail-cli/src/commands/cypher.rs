//! `glyphtrail cypher` — run a raw Cypher query against the graph (#9).
//!
//! An escape hatch for ad-hoc queries straight against the LadybugDB store.

use std::path::Path;

use anyhow::{Result, bail};
use glyphtrail_core::config::RepoPaths;
use glyphtrail_store::LadybugStore;

pub fn run(repo: &Path, query: &str) -> Result<()> {
    let dir = RepoPaths::new(repo).index_dir.join("ladybug");
    if !dir.exists() {
        bail!(
            "no index at {} — run `glyphtrail analyze` first",
            dir.display()
        );
    }
    let store = LadybugStore::open(&dir)?;
    crate::commands::note_staleness(repo, &store);
    print!("{}", store.cypher(query)?);
    Ok(())
}
