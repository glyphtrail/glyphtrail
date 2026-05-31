#![forbid(unsafe_code)]

//! Stable repository identities from git forge remotes.
//!
//! A repo's user-chosen name/folder is fragile; its forge identity is not. This
//! crate derives stable ids from a repo's git remotes — a canonical
//! `host/owner/repo` slug hashed to a UUIDv5, and (optionally, via the forge
//! API) the forge's numeric repo id, which survives owner/repo renames on the
//! forge. A repo carries a *set* of ids: an origin plus mirrors, or a prior
//! identity it migrated from.
//!
//! Standalone and dependency-light (no host-application coupling), so it can be
//! reused outside glyphtrail. Three layers:
//! - [`id`] — pure: remote URL → canonical slug → [`RepoId`] (UUIDv5). Offline.
//! - [`config`] — a host → token-env / forge-kind mapping ([`ForgeConfig`]),
//!   loaded from a TOML file the caller chooses.
//! - [`api`] — best-effort forge-API numeric ids over HTTP (needs a token).

pub mod api;
pub mod config;
pub mod error;
pub mod id;

pub use api::forge_numeric_ids;
pub use config::{ForgeConfig, ForgeHost, ForgeKind};
pub use error::ForgeIdError;
pub use id::{RepoId, canonicalize_remote, forge_numeric_repo_id, repo_ids, repo_uuid};
