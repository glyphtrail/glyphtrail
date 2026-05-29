use std::path::Path;

use anyhow::{Result, bail};
use meridian_core::config::RepoPaths;

use crate::commands::backend::{self, BackendKind};

pub fn run(repo: &Path, port: u16, backend: BackendKind) -> Result<()> {
    let paths = RepoPaths::new(repo);
    if !backend.exists(&paths) {
        bail!(
            "no index found at {} — run `meridian analyze` first",
            backend.location(&paths).display()
        );
    }
    let store = backend::open(&paths, backend)?;
    // The `/mcp` endpoint opens the repo's graph store itself, auto-detecting the
    // backend beside this path, so it works for either backend (#165). Passing
    // the canonical `graph.db` path lets it locate the `.meridian` dir.
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(meridian_server::serve(
        store,
        Some(paths.db_path.clone()),
        port,
    ))
}
