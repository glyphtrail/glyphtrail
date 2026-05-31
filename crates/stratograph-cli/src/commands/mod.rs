// Analysis orchestration lives in the `stratograph-analyze` library so the MCP
// server can analyze repos too (#240). Re-exported here so existing call sites
// (`commands::analyze::run`, `commands::backend::open_existing`) are unchanged.
pub use stratograph_analyze::{analyze, backend};

pub mod cypher;
pub mod drift;
pub mod forge;
pub mod group;
pub mod impact;
pub mod llm;
pub mod query;
pub mod repo;
pub mod serve;
pub mod status;
pub mod story;
pub mod viz;
pub mod wiki;
