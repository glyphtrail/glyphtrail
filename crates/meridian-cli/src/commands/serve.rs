use std::path::Path;

use anyhow::{Result, bail};
use meridian_core::config::RepoPaths;

use super::backend::{BackendKind, open as open_backend};

pub fn run(repo: &Path, port: u16, backend: BackendKind) -> Result<()> {
    let paths = RepoPaths::new(repo);
    if !backend.exists(&paths) {
        bail!(
            "no index found at {} — run `meridian analyze` first",
            backend.location(&paths).display()
        );
    }
    let store = open_backend(&paths, backend)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(meridian_server::serve(store, repo.to_path_buf(), port))
}
