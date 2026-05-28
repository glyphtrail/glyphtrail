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

use codegraph_core::{normalize_path, HttpMethod, Language, Span};
use tree_sitter::{Node, Parser};

use crate::registry::grammar;

/// A server endpoint extracted from axum router code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEndpoint {
    pub method: HttpMethod,
    /// Accumulated, normalized route path (e.g. `/api/users/{id}`).
    pub path: String,
    /// Handler symbol name (last path segment), empty for closures.
    pub handler: String,
    /// Span of the `.route(...)` call.
    pub span: Span,
}

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
    let builders = collect_builders(root, src);
    let builder_ids: HashSet<usize> = builders.values().map(|n| n.id()).collect();
    let referenced = collect_referenced_builders(root, src, &builders);

    let mut out = Vec::new();

    // Inline entry routers: a `Router::new()` chain that is a statement / binding
    // (not nested as an argument of another chain, and not a builder body).
    for cr in router_chain_roots(root, src) {
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
                        if let (Some(path), Some(mr)) =
                            (named_arg(args, 0, src), named_arg(args, 1, src))
                        {
                            let p = join(prefix, &string_text(path, src));
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
                    "nest" | "nest_service" => {
                        if let (Some(pre), Some(inner)) =
                            (named_arg(args, 0, src), named_arg(args, 1, src))
                        {
                            let p = join(prefix, &string_text(pre, src));
                            expand(inner, &p, src, builders, visited, out);
                        }
                    }
                    "merge" => {
                        if let Some(inner) = named_arg(args, 0, src) {
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

/// Collect `(method, handler)` pairs from a MethodRouter expression such as
/// `get(list)` or `get(list).post(create)`.
fn method_router(node: Node, src: &[u8]) -> Vec<(HttpMethod, String)> {
    let mut out = Vec::new();
    collect_method_router(node, src, &mut out);
    out
}

fn collect_method_router(node: Node, src: &[u8], out: &mut Vec<(HttpMethod, String)>) {
    if node.kind() != "call_expression" {
        return;
    }
    let Some(func) = node.child_by_field_name("function") else {
        return;
    };
    let args = node.child_by_field_name("arguments");
    if func.kind() == "field_expression" {
        if let Some(recv) = func.child_by_field_name("value") {
            collect_method_router(recv, src, out);
        }
        let verb = func
            .child_by_field_name("field")
            .map(|n| text(n, src))
            .unwrap_or_default();
        if let Some(m) = HttpMethod::parse(&verb) {
            out.push((m, handler_name(args, src)));
        }
    } else {
        let verb = func_name(func, src);
        if let Some(m) = HttpMethod::parse(last_segment(&verb)) {
            out.push((m, handler_name(args, src)));
        }
    }
}

/// Index `fn NAME(...) -> Router { ... }` builders to their router chain root.
fn collect_builders<'a>(root: Node<'a>, src: &[u8]) -> HashMap<String, Node<'a>> {
    let mut builders = HashMap::new();
    walk(root, &mut |n| {
        if n.kind() != "function_item" {
            return;
        }
        let returns_router = n
            .child_by_field_name("return_type")
            .map(|rt| text(rt, src).contains("Router"))
            .unwrap_or(false);
        if !returns_router {
            return;
        }
        let (Some(name), Some(body)) = (
            n.child_by_field_name("name").map(|x| text(x, src)),
            n.child_by_field_name("body"),
        ) else {
            return;
        };
        // The returned router is the body's top-level chain (not one nested as
        // an argument of another chain); take the last such in source order.
        let chain = router_chain_roots(body, src)
            .into_iter()
            .rfind(|c| c.parent().map(|p| p.kind()) != Some("arguments"));
        if let Some(chain) = chain {
            builders.insert(name, chain);
        }
    });
    builders
}

/// Names of builder fns referenced by a `.nest`/`.merge` argument.
fn collect_referenced_builders(
    root: Node,
    src: &[u8],
    builders: &HashMap<String, Node>,
) -> HashSet<String> {
    let mut referenced = HashSet::new();
    walk(root, &mut |n| {
        if n.kind() != "call_expression" {
            return;
        }
        let Some(func) = n.child_by_field_name("function") else {
            return;
        };
        if func.kind() != "field_expression" {
            return;
        }
        let method = func
            .child_by_field_name("field")
            .map(|x| text(x, src))
            .unwrap_or_default();
        if method != "nest" && method != "merge" && method != "nest_service" {
            return;
        }
        let args = n.child_by_field_name("arguments");
        for i in 0..2 {
            if let Some(a) = named_arg(args, i, src) {
                let name = match a.kind() {
                    "call_expression" => a
                        .child_by_field_name("function")
                        .map(|f| func_name(f, src))
                        .unwrap_or_default(),
                    "identifier" | "scoped_identifier" => text(a, src),
                    _ => String::new(),
                };
                let short = last_segment(&name);
                if builders.contains_key(short) {
                    referenced.insert(short.to_string());
                }
            }
        }
    });
    referenced
}

/// The topmost `call_expression` of every `Router::new()` method chain.
fn router_chain_roots<'a>(root: Node<'a>, src: &[u8]) -> Vec<Node<'a>> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    walk(root, &mut |n| {
        if n.kind() != "call_expression" {
            return;
        }
        let is_router_new = n
            .child_by_field_name("function")
            .map(|f| f.kind() == "scoped_identifier" && text(f, src) == "Router::new")
            .unwrap_or(false);
        if !is_router_new {
            return;
        }
        let cr = chain_root(n);
        if seen.insert(cr.id()) {
            roots.push(cr);
        }
    });
    roots
}

/// Ascend a node through `receiver.method(...)` chains to the outermost call.
fn chain_root(mut n: Node) -> Node {
    while let Some(p) = n.parent() {
        if p.kind() == "field_expression"
            && p.child_by_field_name("value").map(|v| v.id()) == Some(n.id())
        {
            if let Some(gp) = p.parent() {
                if gp.kind() == "call_expression"
                    && gp.child_by_field_name("function").map(|f| f.id()) == Some(p.id())
                {
                    n = gp;
                    continue;
                }
            }
        }
        break;
    }
    n
}

fn walk<'a>(node: Node<'a>, f: &mut dyn FnMut(Node<'a>)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, f);
    }
}

fn named_arg<'a>(args: Option<Node<'a>>, i: usize, _src: &[u8]) -> Option<Node<'a>> {
    let args = args?;
    let mut cursor = args.walk();
    let nth = args.named_children(&mut cursor).nth(i);
    nth
}

/// The handler name from a call's arguments (first arg's last path segment).
fn handler_name(args: Option<Node>, src: &[u8]) -> String {
    match named_arg(args, 0, src) {
        Some(n) if matches!(n.kind(), "identifier" | "scoped_identifier") => {
            last_segment(&text(n, src)).to_string()
        }
        _ => String::new(),
    }
}

fn func_name(func: Node, src: &[u8]) -> String {
    text(func, src)
}

fn last_segment(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

fn text(node: Node, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
}

/// Text of a string literal with surrounding quotes removed.
fn string_text(node: Node, src: &[u8]) -> String {
    text(node, src).trim().trim_matches('"').to_string()
}

/// Join an accumulated prefix with a path segment and normalize.
fn join(prefix: &str, path: &str) -> String {
    let combined = format!(
        "{}/{}",
        prefix.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    normalize_path(&combined)
}

fn span_of(node: Node) -> Span {
    Span {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
