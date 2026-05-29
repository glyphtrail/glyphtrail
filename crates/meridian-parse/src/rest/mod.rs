//! REST server-side route extraction (per-framework).

pub mod aspnet;
pub mod axum;
pub mod express;
pub mod flask;
pub mod gin;
pub mod spring;
mod ts;
pub mod utoipa;
pub mod warp;

use meridian_core::{HttpMethod, Language, Span};

pub use aspnet::{extract_aspnet, extract_aspnet_mounts};
pub use axum::{extract_axum, extract_axum_mounts};
pub use express::{extract_express, extract_express_mounts, extract_express_router_mounts};
pub use flask::{extract_flask, extract_flask_mounts, extract_flask_router_mounts};
pub use gin::{extract_gin, extract_gin_mounts};
pub use spring::{extract_spring, extract_spring_mounts};
pub use utoipa::{extract_utoipa, extract_utoipa_mounts};
pub use warp::{extract_warp, extract_warp_mounts};

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

/// A router-*variable* mount: host router `host` mounts router `mounted` under
/// `prefix` (FastAPI `app.include_router(r, prefix=…)`, Express
/// `app.use("/x", r)`). Both ends are variables (not function builders), so they
/// resolve to synthetic `Router` nodes rather than `Function` nodes (#128).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRouterMount {
    pub host: String,
    pub mounted: String,
    pub prefix: String,
    pub span: Span,
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
    /// Router-variable mounts (`host` mounts `mounted` under a prefix). Empty for
    /// frameworks that compose via function builders rather than variables.
    fn router_mounts(&self, _source: &str) -> Vec<RawRouterMount> {
        Vec::new()
    }
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

struct WarpExtractor;
impl RestServerExtractor for WarpExtractor {
    fn name(&self) -> &'static str {
        "warp"
    }
    fn language(&self) -> Language {
        Language::Rust
    }
    fn endpoints(&self, source: &str) -> Vec<RawEndpoint> {
        extract_warp(source)
    }
    fn mounts(&self, source: &str) -> Vec<RawMount> {
        extract_warp_mounts(source)
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

struct FlaskExtractor;
impl RestServerExtractor for FlaskExtractor {
    fn name(&self) -> &'static str {
        "flask"
    }
    fn language(&self) -> Language {
        Language::Python
    }
    fn endpoints(&self, source: &str) -> Vec<RawEndpoint> {
        extract_flask(source)
    }
    fn mounts(&self, source: &str) -> Vec<RawMount> {
        extract_flask_mounts(source)
    }
    fn router_mounts(&self, source: &str) -> Vec<RawRouterMount> {
        extract_flask_router_mounts(source)
    }
}

struct AspNetExtractor;
impl RestServerExtractor for AspNetExtractor {
    fn name(&self) -> &'static str {
        "aspnet"
    }
    fn language(&self) -> Language {
        Language::CSharp
    }
    fn endpoints(&self, source: &str) -> Vec<RawEndpoint> {
        extract_aspnet(source)
    }
    fn mounts(&self, source: &str) -> Vec<RawMount> {
        extract_aspnet_mounts(source)
    }
}

/// Express applies to JS, TS and TSX; the bound language selects the grammar.
struct ExpressExtractor(Language);
impl RestServerExtractor for ExpressExtractor {
    fn name(&self) -> &'static str {
        "express"
    }
    fn language(&self) -> Language {
        self.0.clone()
    }
    fn endpoints(&self, source: &str) -> Vec<RawEndpoint> {
        extract_express(source, &self.0)
    }
    fn mounts(&self, source: &str) -> Vec<RawMount> {
        extract_express_mounts(source, &self.0)
    }
    fn router_mounts(&self, source: &str) -> Vec<RawRouterMount> {
        extract_express_router_mounts(source, &self.0)
    }
}

/// All registered REST server extractors. New frameworks slot in here as
/// additional self-contained modules implementing [`RestServerExtractor`].
pub fn registry() -> Vec<Box<dyn RestServerExtractor>> {
    vec![
        Box::new(AxumExtractor),
        Box::new(UtoipaExtractor),
        Box::new(WarpExtractor),
        Box::new(SpringExtractor),
        Box::new(GinExtractor),
        Box::new(FlaskExtractor),
        Box::new(AspNetExtractor),
        Box::new(ExpressExtractor(Language::JavaScript)),
        Box::new(ExpressExtractor(Language::TypeScript)),
        Box::new(ExpressExtractor(Language::Tsx)),
    ]
}

/// The registered extractors that apply to `lang`.
pub fn extractors_for(lang: &Language) -> Vec<Box<dyn RestServerExtractor>> {
    registry()
        .into_iter()
        .filter(|e| &e.language() == lang)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn registry_filters_by_language() {
        check!(extractors_for(&Language::Rust).len() == 3);
        check!(
            extractors_for(&Language::Python)
                .iter()
                .map(|e| e.name())
                .collect::<Vec<_>>()
                == ["flask"]
        );
        let rust: Vec<_> = extractors_for(&Language::Rust)
            .iter()
            .map(|e| e.name())
            .collect();
        check!(rust.contains(&"axum"));
        check!(rust.contains(&"utoipa-axum"));
        let java: Vec<_> = extractors_for(&Language::Java)
            .iter()
            .map(|e| e.name())
            .collect();
        check!(java == ["spring"]);
        let go: Vec<_> = extractors_for(&Language::Go)
            .iter()
            .map(|e| e.name())
            .collect();
        check!(go == ["gin"]);
        let cs: Vec<_> = extractors_for(&Language::CSharp)
            .iter()
            .map(|e| e.name())
            .collect();
        check!(cs == ["aspnet"]);
        for js in [Language::JavaScript, Language::TypeScript, Language::Tsx] {
            let names: Vec<_> = extractors_for(&js).iter().map(|e| e.name()).collect();
            check!(names == ["express"]);
        }
    }

    #[test]
    fn axum_extractor_trait_yields_endpoints() {
        let src = "fn app() -> Router { Router::new().route(\"/health\", get(health)) }";
        let eps = AxumExtractor.endpoints(src);
        check!(eps.iter().any(|e| e.path == "/health"));
    }
}
