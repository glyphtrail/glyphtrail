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
    // The `/mcp` endpoint dispatches against a SQLite path; it is only wired
    // when serving the sqlite backend (the MCP layer is sqlite-only for now).
    let mcp_db = match backend {
        BackendKind::Sqlite => Some(paths.db_path.clone()),
        _ => None,
    };
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(meridian_server::serve(store, mcp_db, port))
}
