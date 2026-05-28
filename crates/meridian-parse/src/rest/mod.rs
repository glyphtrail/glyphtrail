//! REST server-side route extraction (per-framework).

pub mod axum;
pub mod gin;
pub mod spring;
mod ts;
pub mod utoipa;

use meridian_core::{HttpMethod, Language, Span};

pub use axum::{extract_axum, extract_axum_mounts};
pub use gin::{extract_gin, extract_gin_mounts};
pub use spring::{extract_spring, extract_spring_mounts};
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

/// A per-framework REST server-route extractor. Each implementation is a
/// self-contained module that turns source for one framework into endpoints
/// and router-composition mounts. Implementations share the prefix-accumulation
/// helper (`ts::join`) so nested-router prefixes are handled uniformly.
pub trait RestServerExtractor {
    /// Short framework identifier (e.g. "axum").
    fn name(&self) -> &'static str;
    /// Source language this extractor applies to.
    fn language(&self) -> Language;
    /// Endpoints declared in `source`.
    fn endpoints(&self, source: &str) -> Vec<RawEndpoint>;
    /// Router-composition mounts (`parent` nests/merges builder `child`).
    fn mounts(&self, source: &str) -> Vec<RawMount>;
}

struct AxumExtractor;
impl RestServerExtractor for AxumExtractor {
    fn name(&self) -> &'static str {
        "axum"
    }
    fn language(&self) -> Language {
        Language::Rust
    }
    fn endpoints(&self, source: &str) -> Vec<RawEndpoint> {
        extract_axum(source)
    }
    fn mounts(&self, source: &str) -> Vec<RawMount> {
        extract_axum_mounts(source)
    }
}

struct UtoipaExtractor;
impl RestServerExtractor for UtoipaExtractor {
    fn name(&self) -> &'static str {
        "utoipa-axum"
    }
    fn language(&self) -> Language {
        Language::Rust
    }
    fn endpoints(&self, source: &str) -> Vec<RawEndpoint> {
        extract_utoipa(source)
    }
    fn mounts(&self, source: &str) -> Vec<RawMount> {
        extract_utoipa_mounts(source)
    }
}

struct SpringExtractor;
impl RestServerExtractor for SpringExtractor {
    fn name(&self) -> &'static str {
        "spring"
    }
    fn language(&self) -> Language {
        Language::Java
    }
    fn endpoints(&self, source: &str) -> Vec<RawEndpoint> {
        extract_spring(source)
    }
    fn mounts(&self, source: &str) -> Vec<RawMount> {
        extract_spring_mounts(source)
    }
}

struct GinExtractor;
impl RestServerExtractor for GinExtractor {
    fn name(&self) -> &'static str {
        "gin"
    }
    fn language(&self) -> Language {
        Language::Go
    }
    fn endpoints(&self, source: &str) -> Vec<RawEndpoint> {
        extract_gin(source)
    }
    fn mounts(&self, source: &str) -> Vec<RawMount> {
        extract_gin_mounts(source)
    }
}

/// All registered REST server extractors. New frameworks slot in here as
/// additional self-contained modules implementing [`RestServerExtractor`].
pub fn registry() -> Vec<Box<dyn RestServerExtractor>> {
    vec![
        Box::new(AxumExtractor),
        Box::new(UtoipaExtractor),
        Box::new(SpringExtractor),
        Box::new(GinExtractor),
    ]
}

/// The registered extractors that apply to `lang`.
pub fn extractors_for(lang: Language) -> Vec<Box<dyn RestServerExtractor>> {
    registry()
        .into_iter()
        .filter(|e| e.language() == lang)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_filters_by_language() {
        assert_eq!(extractors_for(Language::Rust).len(), 2);
        assert!(extractors_for(Language::Python).is_empty());
        let rust: Vec<_> = extractors_for(Language::Rust)
            .iter()
            .map(|e| e.name())
            .collect();
        assert!(rust.contains(&"axum"));
        assert!(rust.contains(&"utoipa-axum"));
        let java: Vec<_> = extractors_for(Language::Java)
            .iter()
            .map(|e| e.name())
            .collect();
        assert_eq!(java, ["spring"]);
        let go: Vec<_> = extractors_for(Language::Go)
            .iter()
            .map(|e| e.name())
            .collect();
        assert_eq!(go, ["gin"]);
    }

    #[test]
    fn axum_extractor_trait_yields_endpoints() {
        let src = "fn app() -> Router { Router::new().route(\"/health\", get(health)) }";
        let eps = AxumExtractor.endpoints(src);
        assert!(eps.iter().any(|e| e.path == "/health"));
    }
}
