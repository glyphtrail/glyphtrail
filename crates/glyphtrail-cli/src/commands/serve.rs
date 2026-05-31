use std::path::Path;

use anyhow::Result;
use glyphtrail_core::config::RepoPaths;

use crate::commands::backend;

pub fn run(repo: &Path, port: u16) -> Result<()> {
    let paths = RepoPaths::new(repo);
    let store = backend::open_existing(&paths)?;
    // The `/mcp` endpoint opens the repo's graph store itself, from the
    // `.glyphtrail/ladybug` index beside this anchor path (#165). Passing the
    // canonical `graph.db` path lets it locate the `.glyphtrail` dir.
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(glyphtrail_server::serve(store, paths.db_path.clone(), port))
}
