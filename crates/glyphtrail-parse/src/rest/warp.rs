//! warp route extraction (best-effort).
//!
//! warp composes routes from filter combinators, so a route's `(method, path,
//! handler)` is spread across a chain rather than declared at one call. This
//! extractor handles the common per-binding shape:
//!
//! ```ignore
//! let hello = warp::path!("hello" / String)   // path segments (types -> {param})
//!     .and(warp::get())                        // method filter
//!     .and_then(get_hello);                    // handler
//! ```
//!
//! Within each `let` binding (or top-level filter statement) it gathers the
//! path segments (from `path!(...)` and `warp::path("seg")`), the HTTP method
//! filters (`warp::get()` / `get()` / …), and the handler (`.and_then(fn)` /
//! `.map(fn)`). Combinator graphs split across bindings, dynamic segments, and
//! closure handlers are out of scope — precision is intentionally limited.

use glyphtrail_core::{HttpMethod, Language};
use tree_sitter::{Node, Parser};

use super::ts::{last_segment, span_of, text, walk};
use super::{RawEndpoint, RawMount};
use crate::registry::grammar;

const VERBS: [&str; 7] = ["get", "post", "put", "delete", "patch", "head", "options"];

/// Extract warp endpoints from Rust `source`. Returns empty on parse failure.
pub fn extract_warp(source: &str) -> Vec<RawEndpoint> {
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
    let mut out = Vec::new();
    // Each `let route = <filter>;` binding is one route definition. Top-level
    // filter expressions (returned directly) are covered by `let` in practice;
    // bindings keep unrelated filters from bleeding into one another.
    walk(tree.root_node(), &mut |n| {
        if n.kind() == "let_declaration"
            && let Some(value) = n.child_by_field_name("value")
            && mentions_warp_filter(value, src)
        {
            emit_binding(value, src, &mut out);
        }
    });
    out
}

/// warp has no function-builder mounts; router composition is via `.or(...)`,
/// which does not change a route's `(method, path)`.
pub fn extract_warp_mounts(_source: &str) -> Vec<RawMount> {
    Vec::new()
}

/// Whether `expr` references a warp path filter, gating the more expensive scan.
fn mentions_warp_filter(expr: Node, src: &[u8]) -> bool {
    let mut found = false;
    walk(expr, &mut |n| {
        if found {
            return;
        }
        let seg = match n.kind() {
            "macro_invocation" => macro_last_segment(n, src),
            "call_expression" => callee_last_segment(n, src),
            _ => None,
        };
        if seg.as_deref() == Some("path") {
            found = true;
        }
    });
    found
}

/// Build endpoint(s) for one filter binding: join its path segments, find its
/// method filter(s) and handler.
fn emit_binding(expr: Node, src: &[u8], out: &mut Vec<RawEndpoint>) {
    let mut segments: Vec<(usize, String)> = Vec::new();
    let mut methods: Vec<HttpMethod> = Vec::new();
    let mut handler = String::new();

    walk(expr, &mut |n| match n.kind() {
        "macro_invocation" if macro_last_segment(n, src).as_deref() == Some("path") => {
            path_macro_segments(n, src, &mut segments);
        }
        "call_expression" => {
            let Some(callee) = callee_last_segment(n, src) else {
                return;
            };
            let args = n.child_by_field_name("arguments");
            if callee == "path"
                && let Some(seg) = first_string_arg(args, src)
            {
                segments.push((n.start_byte(), seg));
            } else if VERBS.contains(&callee.as_str())
                && let Some(m) = HttpMethod::parse(&callee.to_uppercase())
                && !methods.contains(&m)
            {
                methods.push(m);
            } else if matches!(callee.as_str(), "and_then" | "map" | "then" | "map_async")
                && handler.is_empty()
                && let Some(h) = first_ident_arg(args, src)
            {
                handler = h;
            }
        }
        _ => {}
    });

    if segments.is_empty() {
        return; // no recoverable path (e.g. only `warp::any()`); skip.
    }
    segments.sort_by_key(|(b, _)| *b);
    let path = format!(
        "/{}",
        segments
            .iter()
            .map(|(_, s)| s.as_str())
            .collect::<Vec<_>>()
            .join("/")
    );
    if methods.is_empty() {
        methods.push(HttpMethod::Get); // a filter with no method matches any; report GET.
    }
    for method in methods {
        out.push(RawEndpoint {
            method,
            path: path.clone(),
            handler: handler.clone(),
            span: span_of(expr),
        });
    }
}

/// Path segments from a `path!("a" / Type / "b")` macro, in source order: a
/// string literal is a literal segment; any other token (a type) is `{param}`.
fn path_macro_segments(mac: Node, src: &[u8], out: &mut Vec<(usize, String)>) {
    let mut cursor = mac.walk();
    let Some(tt) = mac.children(&mut cursor).find(|c| c.kind() == "token_tree") else {
        return;
    };
    let mut tc = tt.walk();
    for c in tt.named_children(&mut tc) {
        let seg = if c.kind() == "string_literal" {
            string_inner(c, src)
        } else {
            "{param}".to_string()
        };
        if !seg.is_empty() {
            out.push((c.start_byte(), seg));
        }
    }
}

/// The last `::` segment of a macro invocation's name (`warp::path` -> `path`).
fn macro_last_segment(mac: Node, src: &[u8]) -> Option<String> {
    let m = mac.child_by_field_name("macro")?;
    Some(last_segment(&text(m, src)).to_string())
}

/// The last `::`/`.` segment of a call's callee (`warp::get` -> `get`,
/// `x.and_then` -> `and_then`).
fn callee_last_segment(call: Node, src: &[u8]) -> Option<String> {
    let func = call.child_by_field_name("function")?;
    match func.kind() {
        "field_expression" => func.child_by_field_name("field").map(|f| text(f, src)),
        _ => Some(last_segment(&text(func, src)).to_string()),
    }
}

/// First positional argument as a string-literal's inner text.
fn first_string_arg(args: Option<Node>, src: &[u8]) -> Option<String> {
    let args = args?;
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .find(|a| a.kind() == "string_literal")
        .map(|a| string_inner(a, src))
}

/// First positional argument that is a (possibly scoped) identifier handler.
fn first_ident_arg(args: Option<Node>, src: &[u8]) -> Option<String> {
    let args = args?;
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .find(|a| matches!(a.kind(), "identifier" | "scoped_identifier"))
        .map(|a| last_segment(&text(a, src)).to_string())
}

/// Inner text of a `string_literal` (its `string_content`), else empty.
fn string_inner(lit: Node, src: &[u8]) -> String {
    let mut cursor = lit.walk();
    lit.named_children(&mut cursor)
        .find(|c| c.kind() == "string_content")
        .map(|c| text(c, src))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn ep<'a>(eps: &'a [RawEndpoint], method: HttpMethod, path: &str) -> Option<&'a RawEndpoint> {
        eps.iter().find(|e| e.method == method && e.path == path)
    }

    #[test]
    fn path_macro_with_method_and_handler() {
        let src = r#"
fn routes() {
    let hello = warp::path!("hello" / String)
        .and(warp::get())
        .and_then(get_hello);
}
"#;
        let eps = extract_warp(src);
        let e = ep(&eps, HttpMethod::Get, "/hello/{param}").expect("hello route");
        check!(e.handler == "get_hello");
    }

    #[test]
    fn method_first_then_path_segment_calls() {
        let src = r#"
fn routes() {
    let create = warp::post()
        .and(warp::path("users"))
        .and_then(create_user);
}
"#;
        let eps = extract_warp(src);
        let e = ep(&eps, HttpMethod::Post, "/users").expect("users route");
        check!(e.handler == "create_user");
    }

    #[test]
    fn no_method_defaults_to_get() {
        let src = r#"
fn routes() {
    let r = warp::path!("health").map(health);
}
"#;
        let eps = extract_warp(src);
        check!(ep(&eps, HttpMethod::Get, "/health").map(|e| e.handler.as_str()) == Some("health"));
    }

    #[test]
    fn bindings_do_not_bleed_into_each_other() {
        let src = r#"
fn routes() {
    let a = warp::path!("a").and(warp::get()).and_then(ha);
    let b = warp::path!("b").and(warp::post()).and_then(hb);
}
"#;
        let eps = extract_warp(src);
        check!(ep(&eps, HttpMethod::Get, "/a").unwrap().handler == "ha");
        check!(ep(&eps, HttpMethod::Post, "/b").unwrap().handler == "hb");
        // No cross-contamination: /a is not POST, /b is not GET.
        check!(ep(&eps, HttpMethod::Post, "/a").is_none());
        check!(ep(&eps, HttpMethod::Get, "/b").is_none());
    }
}
