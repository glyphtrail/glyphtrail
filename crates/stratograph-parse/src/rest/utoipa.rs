//! utoipa-axum route extraction.
//!
//! `utoipa_axum::router::OpenApiRouter` wraps an axum `Router`, but the route's
//! `(method, path)` is declared on the handler via `#[utoipa::path(get, path =
//! "/users/{id}")]` rather than at the mount site. Handlers are registered with
//! `.routes(routes!(handler, ...))`; routers compose with the same
//! `.nest`/`.merge`/`.route` surface as axum.
//!
//! Resolution, statically:
//! - index `#[utoipa::path(method, path = "…")]` attributes per handler fn,
//! - follow `OpenApiRouter` composition (`.routes` / `.route` / `.nest` /
//!   `.merge`, incl. same-file `fn() -> OpenApiRouter` builders) to accumulate
//!   prefixes onto each handler's declared path.
//!
//! Dynamically-built paths and cross-file routers are out of scope here.

use std::collections::{HashMap, HashSet};

use stratograph_core::{HttpMethod, Language};
use tree_sitter::{Node, Parser};

use super::ts::{
    collect_builders, collect_mounts, collect_referenced_builders, func_name, join, last_segment,
    method_router, named_arg, router_chain_roots, span_of, string_literal_text, string_text, text,
    walk,
};
use super::{RawEndpoint, RawMount};
use crate::registry::grammar;

const ROUTER: &str = "OpenApiRouter";

/// Handler fn name -> its declared `(method, path)` pairs from `#[utoipa::path]`.
type AttrMap = HashMap<String, Vec<(HttpMethod, String)>>;

/// Extract utoipa-axum endpoints from Rust `source`. Returns empty on parse
/// failure.
pub fn extract_utoipa(source: &str) -> Vec<RawEndpoint> {
    let mut parser = Parser::new();
    if parser
        .set_language(&grammar(&Language::Rust).expect("built-in grammar"))
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let root = tree.root_node();

    let attrs = collect_path_attrs(root, src);
    let builders = collect_builders(root, src, ROUTER);
    let builder_ids: HashSet<usize> = builders.values().map(|n| n.id()).collect();
    let referenced = collect_referenced_builders(root, src, &builders);

    let mut out = Vec::new();

    // Inline entry routers: an `OpenApiRouter::new()` chain that is a statement /
    // binding (not nested as an argument of another chain, not a builder body).
    for cr in router_chain_roots(root, src, ROUTER) {
        if builder_ids.contains(&cr.id()) {
            continue;
        }
        if cr.parent().map(|p| p.kind()) == Some("arguments") {
            continue;
        }
        expand(
            cr,
            "",
            src,
            &builders,
            &attrs,
            &mut HashSet::new(),
            &mut out,
        );
    }

    // Builder routers that nothing nests are entry points in their own right.
    for (name, cr) in &builders {
        if !referenced.contains(name) {
            let mut visited = HashSet::new();
            visited.insert(name.clone());
            expand(*cr, "", src, &builders, &attrs, &mut visited, &mut out);
        }
    }

    out
}

/// Extract router-composition mounts (`fn parent` nests/merges builder `child`)
/// from utoipa `OpenApiRouter` code. Returns empty on parse failure.
pub fn extract_utoipa_mounts(source: &str) -> Vec<RawMount> {
    let mut parser = Parser::new();
    if parser
        .set_language(&grammar(&Language::Rust).expect("built-in grammar"))
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let root = tree.root_node();
    let builders = collect_builders(root, src, ROUTER);
    collect_mounts(src, &builders)
        .into_iter()
        .map(|(parent, child)| RawMount { parent, child })
        .collect()
}

/// Recursively expand an `OpenApiRouter` expression, accumulating `prefix`.
fn expand<'a>(
    expr: Node<'a>,
    prefix: &str,
    src: &[u8],
    builders: &HashMap<String, Node<'a>>,
    attrs: &AttrMap,
    visited: &mut HashSet<String>,
    out: &mut Vec<RawEndpoint>,
) {
    match expr.kind() {
        "call_expression" => {
            let Some(func) = expr.child_by_field_name("function") else {
                return;
            };
            if func.kind() == "field_expression" {
                if let Some(r) = func.child_by_field_name("value") {
                    expand(r, prefix, src, builders, attrs, visited, out);
                }
                let method = func
                    .child_by_field_name("field")
                    .map(|n| text(n, src))
                    .unwrap_or_default();
                let args = expr.child_by_field_name("arguments");
                match method.as_str() {
                    "routes" => {
                        if let Some(mi) = named_arg(args, 0) {
                            for h in routes_macro_handlers(mi, src) {
                                let Some(specs) = attrs.get(&h) else {
                                    continue;
                                };
                                for (m, hpath) in specs {
                                    out.push(RawEndpoint {
                                        method: *m,
                                        path: join(prefix, hpath),
                                        handler: h.clone(),
                                        span: span_of(expr),
                                    });
                                }
                            }
                        }
                    }
                    // Undocumented routes still use the axum `.route` surface.
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
                                expand(inner, &p, src, builders, attrs, visited, out);
                            }
                        }
                    }
                    "merge" => {
                        if let Some(inner) = named_arg(args, 0) {
                            expand(inner, prefix, src, builders, attrs, visited, out);
                        }
                    }
                    _ => {}
                }
            } else {
                // Base of a chain: `OpenApiRouter::new()` or a `builder()`.
                expand_ref(
                    &func_name(func, src),
                    prefix,
                    src,
                    builders,
                    attrs,
                    visited,
                    out,
                );
            }
        }
        "identifier" | "scoped_identifier" => {
            expand_ref(&text(expr, src), prefix, src, builders, attrs, visited, out)
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
    attrs: &AttrMap,
    visited: &mut HashSet<String>,
    out: &mut Vec<RawEndpoint>,
) {
    let short = last_segment(name);
    if short == "new" {
        return; // OpenApiRouter::new()
    }
    if let Some(chain) = builders.get(short)
        && visited.insert(short.to_string())
    {
        expand(*chain, prefix, src, builders, attrs, visited, out);
        visited.remove(short);
    }
}

/// Handler names listed in a `routes!(a, b, ...)` macro invocation.
fn routes_macro_handlers(node: Node, src: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    if node.kind() != "macro_invocation" {
        return out;
    }
    let mut cursor = node.walk();
    let Some(tt) = node
        .children(&mut cursor)
        .find(|c| c.kind() == "token_tree")
    else {
        return out;
    };
    let mut tcursor = tt.walk();
    for c in tt.named_children(&mut tcursor) {
        if matches!(c.kind(), "identifier" | "scoped_identifier") {
            out.push(last_segment(&text(c, src)).to_string());
        }
    }
    out
}

/// Index `#[utoipa::path(method, path = "…")]` attributes by handler fn name.
fn collect_path_attrs(root: Node, src: &[u8]) -> AttrMap {
    let mut map: AttrMap = HashMap::new();
    walk(root, &mut |n| {
        if n.kind() != "attribute_item" {
            return;
        }
        let mut cursor = n.walk();
        let Some(attr) = n.children(&mut cursor).find(|c| c.kind() == "attribute") else {
            return;
        };
        if !is_utoipa_path(attr, src) {
            return;
        }
        let mut acursor = attr.walk();
        let Some(tt) = attr
            .children(&mut acursor)
            .find(|c| c.kind() == "token_tree")
        else {
            return;
        };
        let (methods, Some(path)) = parse_path_attr(tt, src) else {
            return;
        };
        if methods.is_empty() {
            return;
        }
        let Some(name) = attr_target_fn(n, src) else {
            return;
        };
        let specs = map.entry(name).or_default();
        for m in methods {
            specs.push((m, path.clone()));
        }
    });
    map
}

/// Whether an `attribute` node names `utoipa::path`.
fn is_utoipa_path(attr: Node, src: &[u8]) -> bool {
    let mut cursor = attr.walk();

    attr.named_children(&mut cursor)
        .next()
        .map(|p| text(p, src) == "utoipa::path")
        .unwrap_or(false)
}

/// Parse a `(method, path = "…", …)` attribute token tree into the HTTP
/// methods (bare verb identifiers) and the route path string, if present.
fn parse_path_attr(tt: Node, src: &[u8]) -> (Vec<HttpMethod>, Option<String>) {
    let mut cursor = tt.walk();
    let children: Vec<Node> = tt.named_children(&mut cursor).collect();
    let mut methods = Vec::new();
    let mut path = None;
    for (i, c) in children.iter().enumerate() {
        if c.kind() != "identifier" {
            continue;
        }
        let t = text(*c, src);
        if let Some(m) = HttpMethod::parse(&t) {
            methods.push(m);
        } else if t == "path"
            && let Some(next) = children.get(i + 1)
            && matches!(next.kind(), "string_literal" | "raw_string_literal")
        {
            path = Some(string_text(*next, src));
        }
    }
    (methods, path)
}

/// The name of the `function_item` an attribute is attached to (its following
/// sibling, skipping further attributes and comments).
fn attr_target_fn(attr_item: Node, src: &[u8]) -> Option<String> {
    let mut sib = attr_item.next_named_sibling();
    while let Some(s) = sib {
        match s.kind() {
            "attribute_item" | "line_comment" | "block_comment" => sib = s.next_named_sibling(),
            "function_item" => return s.child_by_field_name("name").map(|n| text(n, src)),
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn ep<'a>(eps: &'a [RawEndpoint], method: HttpMethod, path: &str) -> Option<&'a RawEndpoint> {
        eps.iter().find(|e| e.method == method && e.path == path)
    }

    #[test]
    fn routes_macro_uses_attribute_method_and_path() {
        let src = r#"
#[utoipa::path(get, path = "/users/{id}")]
async fn get_user() {}

#[utoipa::path(post, path = "/users")]
async fn create_user() {}

fn router() -> OpenApiRouter {
    OpenApiRouter::new().routes(routes!(get_user, create_user))
}
"#;
        let eps = extract_utoipa(src);
        check!(ep(&eps, HttpMethod::Get, "/users/{id}").unwrap().handler == "get_user");
        check!(ep(&eps, HttpMethod::Post, "/users").unwrap().handler == "create_user");
    }

    #[test]
    fn nest_accumulates_prefix_onto_declared_path() {
        let src = r#"
#[utoipa::path(get, path = "/{id}")]
async fn show() {}

fn users() -> OpenApiRouter {
    OpenApiRouter::new().routes(routes!(show))
}

fn app() -> OpenApiRouter {
    OpenApiRouter::new().nest("/api/users", users())
}
"#;
        let eps = extract_utoipa(src);
        check!(ep(&eps, HttpMethod::Get, "/api/users/{id}").is_some());
        // The builder is referenced via nest, so it is not also emitted bare.
        check!(ep(&eps, HttpMethod::Get, "/{id}").is_none());
    }

    #[test]
    fn merge_keeps_prefix() {
        let src = r#"
#[utoipa::path(get, path = "/health")]
async fn health() {}

fn app() -> OpenApiRouter {
    OpenApiRouter::new().nest("/api", OpenApiRouter::new().merge(OpenApiRouter::new().routes(routes!(health))))
}
"#;
        let eps = extract_utoipa(src);
        check!(ep(&eps, HttpMethod::Get, "/api/health").is_some());
    }

    #[test]
    fn plain_route_surface_still_works() {
        let src = r#"
fn app() -> OpenApiRouter {
    OpenApiRouter::new().route("/legacy", get(legacy))
}
"#;
        let eps = extract_utoipa(src);
        check!(ep(&eps, HttpMethod::Get, "/legacy").unwrap().handler == "legacy");
    }

    #[test]
    fn multiple_methods_on_one_handler() {
        let src = r#"
#[utoipa::path(get, path = "/things")]
#[utoipa::path(post, path = "/things")]
async fn things() {}

fn app() -> OpenApiRouter {
    OpenApiRouter::new().routes(routes!(things))
}
"#;
        let eps = extract_utoipa(src);
        check!(ep(&eps, HttpMethod::Get, "/things").is_some());
        check!(ep(&eps, HttpMethod::Post, "/things").is_some());
    }

    #[test]
    fn with_openapi_constructor_is_a_root() {
        // The top-level router is often seeded via `OpenApiRouter::with_openapi`
        // rather than `::new`; routes mounted on it must still be found.
        let src = r#"
#[utoipa::path(get, path = "/ping")]
async fn ping() {}

fn app() -> OpenApiRouter {
    OpenApiRouter::with_openapi(ApiDoc::openapi()).routes(routes!(ping))
}
"#;
        let eps = extract_utoipa(src);
        check!(ep(&eps, HttpMethod::Get, "/ping").unwrap().handler == "ping");
    }

    #[test]
    fn raw_string_path_in_attribute() {
        let src = r##"
#[utoipa::path(get, path = r"/raw/{id}")]
async fn raw() {}

fn app() -> OpenApiRouter {
    OpenApiRouter::new().routes(routes!(raw))
}
"##;
        let eps = extract_utoipa(src);
        check!(ep(&eps, HttpMethod::Get, "/raw/{id}").is_some());
    }

    #[test]
    fn turbofish_constructor_is_a_root() {
        let src = r#"
#[utoipa::path(get, path = "/ping")]
async fn ping() {}

fn app() -> OpenApiRouter {
    OpenApiRouter::<AppState>::new().routes(routes!(ping))
}
"#;
        let eps = extract_utoipa(src);
        check!(ep(&eps, HttpMethod::Get, "/ping").unwrap().handler == "ping");
    }

    #[test]
    fn non_literal_route_path_and_nest_prefix_are_skipped() {
        // Dynamic paths/prefixes on the plain `.route`/`.nest` surface are out
        // of scope and must be skipped rather than emitted as `"/PATH"`.
        let src = r#"
fn app() -> OpenApiRouter {
    OpenApiRouter::new()
        .route(PATH, get(legacy))
        .nest(PREFIX, OpenApiRouter::new().route("/inner", get(inner)))
}
"#;
        let eps = extract_utoipa(src);
        check!(eps.iter().all(|e| e.path != "/PATH"));
        check!(eps.iter().all(|e| e.handler != "inner"));
    }

    #[test]
    fn mounts_link_parent_builder_to_child() {
        let src = r#"
fn users() -> OpenApiRouter {
    OpenApiRouter::new().routes(routes!(show))
}
fn app() -> OpenApiRouter {
    OpenApiRouter::new().nest("/api/users", users())
}
"#;
        let mounts = extract_utoipa_mounts(src);
        check!(
            mounts
                == vec![RawMount {
                    parent: "app".into(),
                    child: "users".into()
                }]
        );
    }
}
