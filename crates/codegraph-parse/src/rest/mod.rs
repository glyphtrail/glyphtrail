//! REST server-side route extraction (per-framework).

pub mod axum;
mod ts;
pub mod utoipa;

use codegraph_core::{HttpMethod, Span};

pub use axum::{extract_axum, extract_axum_mounts};
pub use utoipa::{extract_utoipa, extract_utoipa_mounts};

/// A server endpoint extracted from router code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEndpoint {
    pub method: HttpMethod,
    /// Accumulated, normalized route path (e.g. `/api/users/{id}`).
    pub path: String,
    /// Handler symbol name (last path segment), empty for closures.
    pub handler: String,
    /// Span of the route-declaring call.
    pub span: Span,
}

/// A router-composition mount: builder `parent` nests/merges builder `child`.
/// Both are same-file `fn() -> Router` builders, resolved to `Function` nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMount {
    pub parent: String,
    pub child: String,
}
