#![forbid(unsafe_code)]

pub mod api;
pub mod config;
pub mod error;
pub mod lang;
pub mod matcher;
pub mod model;
pub mod registry;
pub mod rewrite;

pub use api::{
    HttpMethod, OperationKey, Protocol, normalize_path, operations_matching, path_signature,
};
pub use config::{ApiConfig, Config, DynamicLanguage, SchemaSource};
pub use error::{CoreError, Result};
pub use lang::Language;
pub use matcher::{ClientCall, Endpoint, Match, Matcher};
pub use model::{CodeGraph, Confidence, Edge, EdgeKind, Node, NodeId, NodeKind, PendingLink, Span};
pub use registry::{Registry, RegistryEntry, default_registry_path};
pub use rewrite::{PrefixRewrite, RewriteCandidate, RewriteEngine};
