#![forbid(unsafe_code)]

pub mod api;
pub mod config;
pub mod error;
pub mod federated;
pub mod groups;
pub mod identity;
pub mod impact;
pub mod lang;
pub mod links;
pub mod manifest;
pub mod matcher;
pub mod model;
pub mod registry;
pub mod repo_id;
pub mod rewrite;
pub mod scrub;

pub use api::{
    HttpMethod, OperationKey, Protocol, normalize_path, operations_matching, path_signature,
};
pub use config::{ApiConfig, Config, DynamicLanguage, SchemaFormat, SchemaSource};
pub use error::{CoreError, Result};
pub use federated::{FederatedAdjacency, qualify, unqualify};
pub use groups::{Group, Groups, default_groups_path};
pub use identity::{
    Ecosystem, ExternalUse, IndexedPackage, META_EXTERNAL_USES, META_PACKAGES, PackageExport,
    PackageIdentity,
};
pub use impact::{
    Adjacency, ClassifiedItem, CrateLevelHit, Direction, EdgeRule, FederatedReport, ImpactClass,
    ImpactItem, ImpactPolicy, ImpactReport, ImpactSummary, RepoImpact, classify, compute_impact,
    edge_rules, is_cross_boundary_path, parse_confidence,
};
pub use lang::Language;
pub use links::{CrossRepoLink, LinkKind, RepoIdentity, imported_symbols, resolve_links};
pub use manifest::{
    CargoDependency, CargoPackage, DepKind, DepSource, parse_cargo_manifest, workspace_members,
};
pub use matcher::{ClientCall, Endpoint, Match, Matcher};
pub use model::{CodeGraph, Confidence, Edge, EdgeKind, Node, NodeId, NodeKind, PendingLink, Span};
pub use registry::{Registry, RegistryEntry, RepoHealth, default_registry_path};
pub use repo_id::{RepoId, canonicalize_remote, repo_ids, repo_uuid};
pub use rewrite::{PrefixRewrite, RewriteCandidate, RewriteEngine};
pub use scrub::scrub_secrets;
