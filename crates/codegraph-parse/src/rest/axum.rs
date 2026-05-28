//! axum route extraction.
//!
//! Walks a Rust file's syntax tree and resolves the `(method, path)` each
//! handler serves, following router composition so prefixes accumulate:
//! `Router::new().route("/users", get(list)).nest("/api", admin())` yields the
//! handler `list` at `GET /api/users` when `admin()` mounts it, etc.
//!
//! Resolution handles, statically:
//! - chained `.route(path, method_router)` (incl. `get(h).post(h2)` MethodRouter
//!   chains),
//! - `.nest(prefix, inner)` / `.merge(inner)` with inline routers, and
//! - `.nest(prefix, builder())` where `fn builder() -> Router { ... }` is defined
//!   in the same file (expanded with the accumulated prefix, cycle-guarded).
//!
//! Dynamically-built paths and cross-file routers are out of scope here and are
//! left for the resolver/confidence layer.

use std::collections::{HashMap, HashSet};

use codegraph_core::Language;
use tree_sitter::{Node, Parser};

use super::ts::{
    collect_builders, collect_referenced_builders, func_name, join, last_segment, method_router,
    named_arg, router_chain_roots, span_of, string_literal_text, text,
};
use super::RawEndpoint;
use crate::registry::grammar;

const ROUTER: &str = "Router";

/// Extract axum endpoints from Rust `source`. Returns empty on parse failure.
pub fn extract_axum(source: &str) -> Vec<RawEndpoint> {
    let mut parser = Parser::new();
    if parser.set_language(&grammar(Language::Rust)).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let root = tree.root_node();

    // `fn name() -> Router { ... }` builders, keyed by name -> their router chain.
    let builders = collect_builders(root, src, ROUTER);
    let builder_ids: HashSet<usize> = builders.values().map(|n| n.id()).collect();
    let referenced = collect_referenced_builders(root, src, &builders);

    let mut out = Vec::new();

    // Inline entry routers: a `Router::new()` chain that is a statement / binding
    // (not nested as an argument of another chain, and not a builder body).
    for cr in router_chain_roots(root, src, ROUTER) {
        if builder_ids.contains(&cr.id()) {
            continue;
        }
        if cr.parent().map(|p| p.kind()) == Some("arguments") {
            continue; // nested inside another chain; expanded via its parent
        }
        expand(cr, "", src, &builders, &mut HashSet::new(), &mut out);
    }

    // Builder routers that nothing nests are entry points in their own right.
    for (name, cr) in &builders {
        if !referenced.contains(name) {
            let mut visited = HashSet::new();
            visited.insert(name.clone());
            expand(*cr, "", src, &builders, &mut visited, &mut out);
        }
    }

    out
}

/// Recursively expand a router expression, accumulating `prefix`.
fn expand<'a>(
    expr: Node<'a>,
    prefix: &str,
    src: &[u8],
    builders: &HashMap<String, Node<'a>>,
    visited: &mut HashSet<String>,
    out: &mut Vec<RawEndpoint>,
) {
    match expr.kind() {
        "call_expression" => {
            let Some(func) = expr.child_by_field_name("function") else {
                return;
            };
            if func.kind() == "field_expression" {
                // `receiver.method(args)`
                let receiver = func.child_by_field_name("value");
                if let Some(r) = receiver {
                    expand(r, prefix, src, builders, visited, out);
                }
                let method = func
                    .child_by_field_name("field")
                    .map(|n| text(n, src))
                    .unwrap_or_default();
                let args = expr.child_by_field_name("arguments");
                match method.as_str() {
                    "route" | "route_service" => {
                        if let (Some(path), Some(mr)) = (named_arg(args, 0), named_arg(args, 1)) {
                            // Dynamic (non-literal) paths are out of scope.
                            if let Some(path) = string_literal_text(path, src) {
                                let p = join(prefix, &path);
                                for (m, h) in method_router(mr, src) {
                                    out.push(RawEndpoint {
                                        method: m,
                                        path: p.clone(),
                                        handler: h,
                                        span: span_of(expr),
                                    });
                                }
                            }
                        }
                    }
                    "nest" | "nest_service" => {
                        if let (Some(pre), Some(inner)) = (named_arg(args, 0), named_arg(args, 1)) {
                            // Dynamic (non-literal) prefixes are out of scope.
                            if let Some(pre) = string_literal_text(pre, src) {
                                let p = join(prefix, &pre);
                                expand(inner, &p, src, builders, visited, out);
                            }
                        }
                    }
                    "merge" => {
                        if let Some(inner) = named_arg(args, 0) {
                            expand(inner, prefix, src, builders, visited, out);
                        }
                    }
                    // .layer/.with_state/.fallback/... : receiver already handled.
                    _ => {}
                }
            } else {
                // Base of a chain: `Router::new()` (no routes) or `builder()`.
                expand_ref(&func_name(func, src), prefix, src, builders, visited, out);
            }
        }
        // A bare builder reference, e.g. `.nest("/x", users_router)`.
        "identifier" | "scoped_identifier" => {
            expand_ref(&text(expr, src), prefix, src, builders, visited, out)
        }
        _ => {}
    }
}

/// Expand a named router reference (builder fn) if known and not yet visited.
fn expand_ref<'a>(
    name: &str,
    prefix: &str,
    src: &[u8],
    builders: &HashMap<String, Node<'a>>,
    visited: &mut HashSet<String>,
    out: &mut Vec<RawEndpoint>,
) {
    let short = last_segment(name);
    if short == "new" {
        return; // Router::new()
    }
    if let Some(chain) = builders.get(short) {
        if visited.insert(short.to_string()) {
            expand(*chain, prefix, src, builders, visited, out);
            visited.remove(short);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_core::HttpMethod;

    fn ep<'a>(eps: &'a [RawEndpoint], method: HttpMethod, path: &str) -> Option<&'a RawEndpoint> {
        eps.iter().find(|e| e.method == method && e.path == path)
    }

    #[test]
    fn flat_routes_and_method_router_chain() {
        let src = r#"
fn build() -> Router {
    Router::new()
        .route("/users", get(list).post(create))
        .route("/users/:id", get(show))
}
"#;
        let eps = extract_axum(src);
        assert_eq!(ep(&eps, HttpMethod::Get, "/users").unwrap().handler, "list");
        assert_eq!(
            ep(&eps, HttpMethod::Post, "/users").unwrap().handler,
            "create"
        );
        // `:id` is normalized to `{id}`.
        assert_eq!(
            ep(&eps, HttpMethod::Get, "/users/{id}").unwrap().handler,
            "show"
        );
    }

    #[test]
    fn inline_nesting_accumulates_prefix() {
        let src = r#"
fn app() -> Router {
    Router::new().nest("/api", Router::new().route("/users/:id", get(show)))
}
"#;
        let eps = extract_axum(src);
        assert!(ep(&eps, HttpMethod::Get, "/api/users/{id}").is_some());
        // The un-prefixed form is not emitted.
        assert!(ep(&eps, HttpMethod::Get, "/users/{id}").is_none());
    }

    #[test]
    fn nesting_resolves_builder_function() {
        let src = r#"
fn users_router() -> Router {
    Router::new().route("/", get(list)).route("/:id", get(show))
}

fn app() -> Router {
    Router::new().nest("/api/users", users_router())
}
"#;
        let eps = extract_axum(src);
        assert!(ep(&eps, HttpMethod::Get, "/api/users").is_some());
        assert_eq!(
            ep(&eps, HttpMethod::Get, "/api/users/{id}")
                .unwrap()
                .handler,
            "show"
        );
        // users_router is referenced via nest, so it is not also emitted unprefixed.
        assert!(ep(&eps, HttpMethod::Get, "/").is_none());
    }

    #[test]
    fn merge_keeps_prefix() {
        let src = r#"
fn app() -> Router {
    Router::new().nest("/api", Router::new().merge(Router::new().route("/health", get(health))))
}
"#;
        let eps = extract_axum(src);
        assert!(ep(&eps, HttpMethod::Get, "/api/health").is_some());
    }

    #[test]
    fn ignores_non_route_chain_methods() {
        let src = r#"
fn app() -> Router {
    Router::new()
        .route("/x", get(handle))
        .with_state(state)
        .layer(mw)
}
"#;
        let eps = extract_axum(src);
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].path, "/x");
    }

    #[test]
    fn handles_scoped_handler_paths() {
        let src = r#"
fn app() -> Router {
    Router::new().route("/u", get(handlers::users::list))
}
"#;
        let eps = extract_axum(src);
        assert_eq!(ep(&eps, HttpMethod::Get, "/u").unwrap().handler, "list");
    }

    #[test]
    fn raw_string_paths_are_unwrapped() {
        let src = r##"
fn app() -> Router {
    Router::new()
        .nest(r"/api", Router::new().route(r#"/users/:id"#, get(show)))
}
"##;
        let eps = extract_axum(src);
        assert_eq!(
            ep(&eps, HttpMethod::Get, "/api/users/{id}")
                .unwrap()
                .handler,
            "show"
        );
    }

    #[test]
    fn turbofish_constructor_is_a_root() {
        let src = r#"
fn app() -> Router {
    Router::<AppState>::new().route("/x", get(h))
}
"#;
        let eps = extract_axum(src);
        assert_eq!(ep(&eps, HttpMethod::Get, "/x").unwrap().handler, "h");
    }

    #[test]
    fn non_literal_path_and_prefix_are_skipped() {
        // Dynamically-built paths/prefixes (consts, idents) are out of scope:
        // they must be skipped, not emitted as literal `"/PATH"` routes.
        let src = r#"
const PATH: &str = "/users";
fn app() -> Router {
    Router::new()
        .route(PATH, get(list))
        .nest(PREFIX, Router::new().route("/inner", get(inner)))
}
"#;
        let eps = extract_axum(src);
        assert!(eps.iter().all(|e| e.path != "/PATH"));
        assert!(ep(&eps, HttpMethod::Get, "/users").is_none());
        // The route under the dynamic nest prefix is not emitted either.
        assert!(eps.iter().all(|e| e.handler != "inner"));
    }

    #[test]
    fn layered_method_router_is_captured() {
        // A `.layer(..)` (or other combinator) applied to a MethodRouter must not
        // hide the verbs: collect_method_router recurses into the receiver.
        let src = r#"
fn app() -> Router {
    Router::new()
        .route("/a", get(show).layer(mw))
        .route("/b", get(list).layer(mw).post(create))
}
"#;
        let eps = extract_axum(src);
        assert_eq!(ep(&eps, HttpMethod::Get, "/a").unwrap().handler, "show");
        assert_eq!(ep(&eps, HttpMethod::Get, "/b").unwrap().handler, "list");
        assert_eq!(ep(&eps, HttpMethod::Post, "/b").unwrap().handler, "create");
    }
}
