#![forbid(unsafe_code)]

pub mod api;
pub mod config;
pub mod dotnet;
pub mod error;
pub mod federated;
pub mod filelock;
pub mod groups;
pub mod identity;
pub mod impact;
pub mod lang;
pub mod links;
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
pub use dotnet::{CsprojProject, parse_csproj};
pub use error::{CoreError, Result};
pub use federated::{FederatedAdjacency, qualify, unqualify};
pub use glyphtrail_forge_id::{
    ForgeConfig, ForgeHost, ForgeKind, RepoId, canonicalize_remote, forge_numeric_repo_id,
    repo_ids, repo_uuid,
};
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
pub use registry::{
    Contributor, RecordOutcome, Registry, RegistryEntry, RepoHealth, Resolution,
    default_registry_path, lock_path,
};
pub use rewrite::{PrefixRewrite, RewriteCandidate, RewriteEngine};
pub use scrub::scrub_secrets;
