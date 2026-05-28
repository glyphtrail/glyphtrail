//! Gin (Go) REST route extraction.
//!
//! Routes are registered as verb methods on a router/group value:
//! `r.GET("/users/:id", handler)`, `r.POST("/users", handler)`, etc. The HTTP
//! verb is the (upper-case) method name — which also distinguishes a gin route
//! from a net/http *client* call (`http.Get`, mixed-case) — the path is the
//! first string argument, and the handler is the symbol passed after it.
//!
//! Group prefixes (`v1 := r.Group("/api"); v1.GET(...)`) are not yet
//! accumulated; flat routes are extracted. Inline `func(c *gin.Context){…}`
//! handlers yield an endpoint with no handler symbol.

use tree_sitter::{Node, Parser, Tree};

use meridian_core::{HttpMethod, Language};

use super::ts::{span_of, text};
use super::{RawEndpoint, RawMount};
use crate::registry;

const VERBS: [&str; 7] = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];

/// Extract gin routes. Returns empty on parse failure.
pub fn extract_gin(source: &str) -> Vec<RawEndpoint> {
    let Some(tree) = parse(source) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    super::ts::walk(tree.root_node(), &mut |n| {
        if n.kind() == "call_expression"
            && let Some(ep) = route(n, src)
        {
            out.push(ep);
        }
    });
    out
}

/// Gin has no builder-function router composition like axum; group prefixes are
/// a follow-up. No mounts are emitted.
pub fn extract_gin_mounts(_source: &str) -> Vec<RawMount> {
    Vec::new()
}

fn parse(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&registry::grammar(Language::Go)).ok()?;
    parser.parse(source, None)
}

/// A `router.VERB("/path", handler)` call as a `RawEndpoint`, else `None`.
fn route(call: Node, src: &[u8]) -> Option<RawEndpoint> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "selector_expression" {
        return None;
    }
    let verb = text(func.child_by_field_name("field")?, src);
    if !VERBS.contains(&verb.as_str()) {
        return None;
    }
    let method = HttpMethod::parse(&verb)?;
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let mut named = args.named_children(&mut cursor);
    let path = go_string(named.next()?, src)?;
    let handler = named
        .next()
        .map(|h| handler_name(h, src))
        .unwrap_or_default();
    Some(RawEndpoint {
        method,
        path,
        handler,
        span: span_of(call),
    })
}

/// Handler symbol name from the argument after the path: a bare identifier, the
/// field of a `pkg.Handler` selector, or empty for an inline function literal.
fn handler_name(node: Node, src: &[u8]) -> String {
    match node.kind() {
        "identifier" => text(node, src),
        "selector_expression" => node
            .child_by_field_name("field")
            .map(|f| text(f, src))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Inner text of a Go string literal (interpreted or raw), else `None`.
fn go_string(node: Node, src: &[u8]) -> Option<String> {
    let content_kind = match node.kind() {
        "interpreted_string_literal" => "interpreted_string_literal_content",
        "raw_string_literal" => "raw_string_literal_content",
        _ => return None,
    };
    let mut cursor = node.walk();
    Some(
        node.named_children(&mut cursor)
            .find(|c| c.kind() == content_kind)
            .map(|c| text(c, src))
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn ep<'a>(eps: &'a [RawEndpoint], method: HttpMethod, path: &str) -> Option<&'a RawEndpoint> {
        eps.iter().find(|e| e.method == method && e.path == path)
    }

    const ROUTER: &str = r#"
package main
func setup(r *gin.Engine) {
    r.GET("/users/:id", getUser)
    r.POST("/users", handlers.CreateUser)
    r.DELETE("/users/:id", func(c *gin.Context) {})
    r.Use(logger)
}
"#;

    #[test]
    fn extracts_verb_path_and_handler() {
        let eps = extract_gin(ROUTER);
        check!(
            ep(&eps, HttpMethod::Get, "/users/:id").map(|e| e.handler.as_str()) == Some("getUser")
        );
        // Selector handler keeps the function name.
        check!(
            ep(&eps, HttpMethod::Post, "/users").map(|e| e.handler.as_str()) == Some("CreateUser")
        );
        // Inline handler -> no symbol.
        check!(ep(&eps, HttpMethod::Delete, "/users/:id").map(|e| e.handler.as_str()) == Some(""));
    }

    #[test]
    fn ignores_non_route_methods() {
        // `r.Use(...)` is middleware, not a route.
        let eps = extract_gin(ROUTER);
        check!(eps.len() == 3);
    }

    #[test]
    fn no_mounts_and_handles_parse_failure() {
        check!(extract_gin_mounts(ROUTER).is_empty());
        check!(extract_gin("???not go").is_empty());
    }
}
