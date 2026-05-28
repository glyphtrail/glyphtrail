#![forbid(unsafe_code)]

pub mod api;
pub mod config;
pub mod error;
pub mod lang;
pub mod matcher;
pub mod model;
pub mod rewrite;

pub use api::{HttpMethod, OperationKey, Protocol, normalize_path, path_signature};
pub use config::{ApiConfig, Config, SchemaSource};
pub use error::{CoreError, Result};
pub use lang::Language;
pub use matcher::{ClientCall, Endpoint, Match, Matcher};
pub use model::{CodeGraph, Confidence, Edge, EdgeKind, Node, NodeId, NodeKind, Span};
pub use rewrite::{PrefixRewrite, RewriteCandidate, RewriteEngine};
