use std::path::Path;

use anyhow::{bail, Result};
use codegraph_core::config::RepoPaths;

pub fn run(repo: &Path, port: u16) -> Result<()> {
    let paths = RepoPaths::new(repo);
    if !paths.db_path.exists() {
        bail!("no index found at {} — run `codegraph analyze` first", paths.db_path.display());
    }
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(codegraph_server::serve(paths.db_path, port))
}
