use std::path::{Path, PathBuf};

/// Per-repo layout. The index lives in `<repo>/.codegraph/`.
pub const INDEX_DIR: &str = ".codegraph";
pub const DB_FILE: &str = "graph.db";
pub const IGNORE_FILE: &str = ".codegraphignore";

#[derive(Debug, Clone)]
pub struct RepoPaths {
    pub root: PathBuf,
    pub index_dir: PathBuf,
    pub db_path: PathBuf,
}

impl RepoPaths {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let index_dir = root.join(INDEX_DIR);
        let db_path = index_dir.join(DB_FILE);
        Self {
            root,
            index_dir,
            db_path,
        }
    }

    pub fn ensure_index_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.index_dir)
    }
}
