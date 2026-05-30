#![forbid(unsafe_code)]

//! Repository analysis orchestration: discover source files, parse them into a
//! graph, resolve cross-file links, ingest API schemas, resolve package
//! identity, and persist the result to the graph store.
//!
//! Extracted from the CLI so both `stratograph analyze` and the MCP server can
//! analyze a repository at any path (#240), rather than the analysis pipeline
//! being reachable only from the command binary.

pub mod analyze;
pub mod backend;
pub mod schema;

pub use analyze::{AnalyzeOutcome, run};
