//! Client-side HTTP call extraction (fetch / axios) for JS, TS, and TSX.
//!
//! Detects the `(method, url)` of outgoing HTTP calls so they can be linked to
//! the server endpoints that answer them (the `INVOKES` edge). Recognizes:
//! - `fetch(url, { method })` — the method defaults to GET, and
//! - `axios.get(url)` / `axios.post(url, ...)` — verb taken from the call.
//!
//! Only string-literal and template-literal URLs are extracted; fully dynamic
//! URLs (bare variables, concatenations) are out of scope. Template
//! interpolations (`/users/${id}`) are preserved verbatim so they collapse to a
//! dynamic segment in the operation signature.

use codegraph_core::{HttpMethod, Language, Span};
use tree_sitter::{Node, Parser};

use crate::registry::grammar;

/// A client-side HTTP call extracted from JS/TS source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawClientCall {
    pub method: HttpMethod,
    /// URL as written (literal or template); canonicalized later via `OperationKey`.
    pub path: String,
    /// Span of the call expression.
    pub span: Span,
}

/// Extract client HTTP calls from JS/TS/TSX `source`. Returns empty on parse
/// failure or for non-JS languages.
pub fn extract_client_calls(source: &str, lang: Language) -> Vec<RawClientCall> {
    let mut parser = Parser::new();
    if parser.set_language(&grammar(lang)).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    walk(tree.root_node(), &mut |n| {
        if n.kind() == "call_expression" {
            if let Some(call) = client_call(n, src) {
                out.push(call);
            }
        }
    });
    out
}

/// Classify a `call_expression` as a `fetch`/`axios` HTTP call and pull its
/// `(method, url)`; `None` if it is neither or the URL is non-literal.
fn client_call(call: Node, src: &[u8]) -> Option<RawClientCall> {
    let func = call.child_by_field_name("function")?;
    let args = call.child_by_field_name("arguments");
    let (method, url_node) = match func.kind() {
        "identifier" if text(func, src) == "fetch" => {
            (fetch_method(args, src), named_arg(args, 0)?)
        }
        "member_expression" => {
            let obj = func.child_by_field_name("object")?;
            if text(obj, src) != "axios" {
                return None;
            }
            let verb = func.child_by_field_name("property")?;
            (HttpMethod::parse(&text(verb, src))?, named_arg(args, 0)?)
        }
        _ => return None,
    };
    let path = url_text(url_node, src)?;
    Some(RawClientCall {
        method,
        path,
        span: span_of(call),
    })
}

/// The `method` of a `fetch` call's options object, defaulting to GET.
fn fetch_method(args: Option<Node>, src: &[u8]) -> HttpMethod {
    let Some(opts) = named_arg(args, 1) else {
        return HttpMethod::Get;
    };
    if opts.kind() != "object" {
        return HttpMethod::Get;
    }
    let mut cursor = opts.walk();
    for pair in opts.named_children(&mut cursor) {
        if pair.kind() != "pair" {
            continue;
        }
        let (Some(key), Some(value)) = (
            pair.child_by_field_name("key"),
            pair.child_by_field_name("value"),
        ) else {
            continue;
        };
        if key_name(key, src).as_deref() == Some("method") {
            if let Some(m) = js_string(value, src).and_then(|s| HttpMethod::parse(&s)) {
                return m;
            }
        }
    }
    HttpMethod::Get
}

/// Property-key name, whether a bare identifier or a quoted string key.
fn key_name(key: Node, src: &[u8]) -> Option<String> {
    match key.kind() {
        "property_identifier" => Some(text(key, src)),
        "string" => js_string(key, src),
        _ => None,
    }
}

/// URL string from a literal or template argument; `None` for dynamic URLs.
fn url_text(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "string" => js_string(node, src),
        // Keep `${...}` verbatim; it canonicalizes to a dynamic segment.
        "template_string" => Some(text(node, src).trim_matches('`').to_string()),
        _ => None,
    }
}

/// Inner text of a JS string literal (concatenated `string_fragment`s).
fn js_string(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut cursor = node.walk();
    let mut s = String::new();
    for ch in node.named_children(&mut cursor) {
        if ch.kind() == "string_fragment" {
            s.push_str(&text(ch, src));
        }
    }
    Some(s)
}

fn walk<'a>(node: Node<'a>, f: &mut dyn FnMut(Node<'a>)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, f);
    }
}

fn named_arg<'a>(args: Option<Node<'a>>, i: usize) -> Option<Node<'a>> {
    let args = args?;
    let mut cursor = args.walk();
    let nth = args.named_children(&mut cursor).nth(i);
    nth
}

fn text(node: Node, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
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

    fn call<'a>(
        calls: &'a [RawClientCall],
        method: HttpMethod,
        path: &str,
    ) -> Option<&'a RawClientCall> {
        calls.iter().find(|c| c.method == method && c.path == path)
    }

    #[test]
    fn fetch_get_string_url() {
        let calls = extract_client_calls("fetch(\"/api/users\");", Language::JavaScript);
        assert!(call(&calls, HttpMethod::Get, "/api/users").is_some());
    }

    #[test]
    fn fetch_method_from_options() {
        let src = "fetch(\"/api/users\", { method: \"POST\" });";
        let calls = extract_client_calls(src, Language::JavaScript);
        assert!(call(&calls, HttpMethod::Post, "/api/users").is_some());
    }

    #[test]
    fn fetch_template_url_keeps_interpolation() {
        let src = "const r = fetch(`/api/users/${id}`);";
        let calls = extract_client_calls(src, Language::JavaScript);
        assert!(call(&calls, HttpMethod::Get, "/api/users/${id}").is_some());
    }

    #[test]
    fn axios_verb_method() {
        let src = "axios.get(\"/api/items/42\"); axios.delete(\"/api/items/42\");";
        let calls = extract_client_calls(src, Language::TypeScript);
        assert!(call(&calls, HttpMethod::Get, "/api/items/42").is_some());
        assert!(call(&calls, HttpMethod::Delete, "/api/items/42").is_some());
    }

    #[test]
    fn dynamic_url_is_skipped() {
        // A bare variable URL is out of scope (no literal to extract).
        let calls = extract_client_calls("fetch(endpoint);", Language::JavaScript);
        assert!(calls.is_empty());
    }

    #[test]
    fn non_axios_member_call_ignored() {
        // `.get` on an unknown object is not assumed to be an HTTP client.
        let calls = extract_client_calls("store.get(\"/x\");", Language::JavaScript);
        assert!(calls.is_empty());
    }

    #[test]
    fn works_in_tsx() {
        let src = "const f = () => fetch(\"/api/ping\");";
        let calls = extract_client_calls(src, Language::Tsx);
        assert!(call(&calls, HttpMethod::Get, "/api/ping").is_some());
    }
}
