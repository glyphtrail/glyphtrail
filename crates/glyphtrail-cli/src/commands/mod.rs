// Analysis orchestration lives in the `glyphtrail-analyze` library so the MCP
// server can analyze repos too (#240). Re-exported here so existing call sites
// (`commands::analyze::run`, `commands::backend::open_existing`) are unchanged.
pub use glyphtrail_analyze::{analyze, backend};

pub mod cypher;
pub mod drift;
pub mod group;
pub mod impact;
pub mod llm;
pub mod outline;
pub mod query;
pub mod repo;
pub mod serve;
pub mod setup;
pub mod status;
pub mod story;
pub mod viz;
pub mod wiki;
