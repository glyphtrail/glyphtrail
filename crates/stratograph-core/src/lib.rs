#![forbid(unsafe_code)]

pub mod api;
pub mod config;
pub mod error;
pub mod groups;
pub mod identity;
pub mod impact;
pub mod lang;
pub mod manifest;
pub mod matcher;
pub mod model;
pub mod registry;
pub mod rewrite;
pub mod scrub;

pub use api::{
    HttpMethod, OperationKey, Protocol, normalize_path, operations_matching, path_signature,
};
pub use config::{ApiConfig, Config, DynamicLanguage, SchemaFormat, SchemaSource};
pub use error::{CoreError, Result};
pub use groups::{Group, Groups, default_groups_path};
pub use identity::{
    ExternalUse, IndexedPackage, META_EXTERNAL_USES, META_PACKAGES, PackageExport, PackageIdentity,
};
pub use impact::{
    Adjacency, ClassifiedItem, Direction, EdgeRule, ImpactClass, ImpactItem, ImpactPolicy,
    ImpactReport, ImpactSummary, classify, compute_impact, edge_rules, is_cross_boundary_path,
    parse_confidence,
};
pub use lang::Language;
pub use manifest::{
    CargoDependency, CargoPackage, DepKind, DepSource, parse_cargo_manifest, workspace_members,
};
pub use matcher::{ClientCall, Endpoint, Match, Matcher};
pub use model::{CodeGraph, Confidence, Edge, EdgeKind, Node, NodeId, NodeKind, PendingLink, Span};
pub use registry::{Registry, RegistryEntry, RepoHealth, default_registry_path};
pub use rewrite::{PrefixRewrite, RewriteCandidate, RewriteEngine};
pub use scrub::scrub_secrets;
