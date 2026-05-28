use std::path::Path;

use anyhow::{Result, bail};
use meridian_core::config::RepoPaths;
use meridian_store::SqliteStore;

pub fn run(repo: &Path, output: &Path, limit: usize) -> Result<()> {
    let paths = RepoPaths::new(repo);
    if !paths.db_path.exists() {
        bail!(
            "no index found at {} — run `meridian analyze` first",
            paths.db_path.display()
        );
    }
    let store = SqliteStore::open(&paths.db_path)?;
    let (nodes, edges) = store.export_graph(limit)?;
    let ops = store.all_operations()?;
    let html = meridian_viz::static_html(&nodes, &edges, &ops);
    std::fs::write(output, html)?;
    println!(
        "Wrote {} ({} nodes, {} edges)",
        output.display(),
        nodes.len(),
        edges.len()
    );
    Ok(())
}
