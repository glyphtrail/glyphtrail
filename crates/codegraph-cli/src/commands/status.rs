use std::path::Path;

use anyhow::{bail, Result};
use codegraph_core::config::RepoPaths;
use codegraph_store::SqliteStore;

pub fn run(repo: &Path) -> Result<()> {
    let paths = RepoPaths::new(repo);
    if !paths.db_path.exists() {
        bail!("no index found at {} — run `codegraph analyze` first", paths.db_path.display());
    }
    let store = SqliteStore::open(&paths.db_path)?;
    let s = store.stats()?;
    println!("index:  {}", paths.db_path.display());
    println!("files:  {}", s.files);
    println!("nodes:  {}", s.nodes);
    println!("edges:  {}", s.edges);
    Ok(())
}
