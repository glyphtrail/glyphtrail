//! REST server-side route extraction (per-framework).

pub mod axum;

pub use axum::{extract_axum, RawEndpoint};
