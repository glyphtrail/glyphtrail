//! REST server-side route extraction (per-framework).

pub mod axum;
mod ts;
pub mod utoipa;

use codegraph_core::{HttpMethod, Span};

pub use axum::extract_axum;
pub use utoipa::extract_utoipa;

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
