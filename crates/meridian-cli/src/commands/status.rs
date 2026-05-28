use std::path::Path;

use anyhow::{bail, Result};
use meridian_core::config::RepoPaths;
use meridian_store::SqliteStore;

pub fn run(repo: &Path) -> Result<()> {
    let paths = RepoPaths::new(repo);
    if !paths.db_path.exists() {
        bail!("no index found at {} — run `meridian analyze` first", paths.db_path.display());
    }
    let store = SqliteStore::open(&paths.db_path)?;
    let s = store.stats()?;
    println!("index:  {}", paths.db_path.display());
    println!("files:  {}", s.files);
    println!("nodes:  {}", s.nodes);
    println!("edges:  {}", s.edges);
    Ok(())
}
