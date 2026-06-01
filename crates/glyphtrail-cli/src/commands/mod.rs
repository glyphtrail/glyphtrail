// Analysis orchestration lives in the `glyphtrail-analyze` library so the MCP
// server can analyze repos too (#240). Re-exported here so existing call sites
// (`commands::analyze::run`, `commands::backend::open_existing`) are unchanged.
pub use glyphtrail_analyze::{analyze, backend};

use glyphtrail_store::GraphStore;
use std::path::Path;

/// Print a staleness advisory to stderr when the index lags the repo (#313).
/// On stderr so it never corrupts JSON/YAML emitted on stdout, yet still reaches
/// a human at the terminal and a shell that forwards stderr.
pub fn note_staleness(repo: &Path, store: &dyn GraphStore) {
    if let Some(hint) = glyphtrail_analyze::index_staleness(repo, store).hint() {
        eprintln!("{hint}");
    }
}

pub mod config;
pub mod config_file;
pub mod cypher;
pub mod drift;
pub mod group;
pub mod impact;
pub mod link;
pub mod llm;
pub mod outline;
pub mod query;
pub mod remote;
pub mod repo;
pub mod serve;
pub mod setup;
pub mod status;
pub mod story;
pub mod viz;
pub mod wiki;
