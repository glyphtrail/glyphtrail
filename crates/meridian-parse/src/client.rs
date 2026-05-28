//! Client-side HTTP call extraction (fetch / axios) for JS, TS, and TSX.
//!
//! Detects the `(method, url)` of outgoing HTTP calls so they can be linked to
//! the server endpoints that answer them (the `INVOKES` edge). Recognizes:
//! - `fetch(url, { method })` — the method defaults to GET;
//! - `axios.get(url)` / `axios.post(url, ...)` — verb taken from the call;
//! - `axios(config)` / `axios.request(config)` — `url`/`method` from the config
//!   object (method defaults to GET); and
//! - instance clients created in the same file via `const api = axios.create(…)`
//!   — `api.get(url)`, `api(config)`, `api.request(config)` are treated like
//!   their `axios` equivalents.
//!
//! Only string-literal and template-literal URLs are extracted; fully dynamic
//! URLs (bare variables, concatenations) are out of scope. Template
//! interpolations (`/users/${id}`) are preserved verbatim so they collapse to a
//! dynamic segment in the operation signature.

use meridian_core::{HttpMethod, Language, Span};
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
    let root = tree.root_node();
    let clients = axios_clients(root, src);
    let mut out = Vec::new();
    walk(root, &mut |n| {
        if n.kind() == "call_expression" {
            if let Some(call) = client_call(n, src, &clients) {
                out.push(call);
            }
        }
    });
    out
}

/// A same-file `axios.create(...)` instance binding and the byte range of the
/// scope it is visible in (its enclosing block / the module), used to keep
/// instance detection scope-sensitive rather than promoting the name globally.
struct AxiosBinding {
    name: String,
    scope: std::ops::Range<usize>,
}

/// Axios client bindings in `root`: same-file `const NAME = axios.create(...)`
/// declarations, each scoped to its enclosing block. `axios` itself is always a
/// client (handled separately, in every scope).
fn axios_clients(root: Node, src: &[u8]) -> Vec<AxiosBinding> {
    let mut bindings = Vec::new();
    walk(root, &mut |n| {
        if n.kind() != "variable_declarator" {
            return;
        }
        let (Some(name), Some(value)) = (
            n.child_by_field_name("name"),
            n.child_by_field_name("value"),
        ) else {
            return;
        };
        if name.kind() == "identifier" && is_axios_create(value, src) {
            let scope = enclosing_scope(n);
            bindings.push(AxiosBinding {
                name: text(name, src),
                scope: scope.start_byte()..scope.end_byte(),
            });
        }
    });
    bindings
}

/// The nearest enclosing lexical scope of `node`: its containing block, or the
/// whole program for a top-level declaration.
fn enclosing_scope(node: Node) -> Node {
    let mut n = node;
    while let Some(p) = n.parent() {
        if matches!(p.kind(), "statement_block" | "program") {
            return p;
        }
        n = p;
    }
    n
}

/// Whether `name` used at byte offset `at` refers to an axios client: either
/// the global `axios`, or an instance binding whose scope covers `at`.
fn is_axios_client(clients: &[AxiosBinding], name: &str, at: usize) -> bool {
    name == "axios"
        || clients
            .iter()
            .any(|b| b.name == name && b.scope.contains(&at))
}

/// Whether `node` is an `axios.create(...)` call.
fn is_axios_create(node: Node, src: &[u8]) -> bool {
    if node.kind() != "call_expression" {
        return false;
    }
    let Some(func) = node.child_by_field_name("function") else {
        return false;
    };
    func.kind() == "member_expression"
        && func
            .child_by_field_name("object")
            .map(|o| text(o, src))
            .as_deref()
            == Some("axios")
        && func
            .child_by_field_name("property")
            .map(|p| text(p, src))
            .as_deref()
            == Some("create")
}

/// Classify a `call_expression` as a `fetch`/`axios` HTTP call and pull its
/// `(method, url)`; `None` if it is neither or the URL is non-literal.
fn client_call(call: Node, src: &[u8], clients: &[AxiosBinding]) -> Option<RawClientCall> {
    let func = call.child_by_field_name("function")?;
    let args = call.child_by_field_name("arguments");
    let (method, path) = match func.kind() {
        "identifier" => {
            let name = text(func, src);
            if name == "fetch" {
                let path = url_text(named_arg(args, 0)?, src)?;
                let method = named_arg(args, 1)
                    .map(|o| object_method(o, src))
                    .unwrap_or(HttpMethod::Get);
                (method, path)
            } else if is_axios_client(clients, &name, func.start_byte()) {
                // `axios(config)` / `instance(config)`.
                config_call(named_arg(args, 0)?, src)?
            } else {
                return None;
            }
        }
        "member_expression" => {
            let obj = func.child_by_field_name("object")?;
            if !is_axios_client(clients, &text(obj, src), obj.start_byte()) {
                return None;
            }
            let prop = text(func.child_by_field_name("property")?, src);
            if prop == "request" {
                // `axios.request(config)` / `instance.request(config)`.
                config_call(named_arg(args, 0)?, src)?
            } else {
                // `axios.get(url)` / `instance.post(url, ...)`.
                let method = HttpMethod::parse(&prop)?;
                (method, url_text(named_arg(args, 0)?, src)?)
            }
        }
        _ => return None,
    };
    Some(RawClientCall {
        method,
        path,
        span: span_of(call),
    })
}

/// `(method, url)` from an axios config object: `url` is required (and must be
/// a literal), `method` defaults to GET.
fn config_call(config: Node, src: &[u8]) -> Option<(HttpMethod, String)> {
    let url = url_text(object_field(config, "url", src)?, src)?;
    Some((object_method(config, src), url))
}

/// The `method` field of an options/config object, defaulting to GET.
fn object_method(obj: Node, src: &[u8]) -> HttpMethod {
    object_field(obj, "method", src)
        .and_then(|v| js_string(v, src))
        .and_then(|s| HttpMethod::parse(&s))
        .unwrap_or(HttpMethod::Get)
}

/// Value node of `key` in an object literal, if present.
fn object_field<'a>(obj: Node<'a>, key: &str, src: &[u8]) -> Option<Node<'a>> {
    if obj.kind() != "object" {
        return None;
    }
    let mut cursor = obj.walk();
    for pair in obj.named_children(&mut cursor) {
        if pair.kind() != "pair" {
            continue;
        }
        let (Some(k), Some(v)) = (
            pair.child_by_field_name("key"),
            pair.child_by_field_name("value"),
        ) else {
            continue;
        };
        if key_name(k, src).as_deref() == Some(key) {
            return Some(v);
        }
    }
    None
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

    #[test]
    fn axios_config_call() {
        let src = "axios({ url: \"/things\", method: \"PUT\" });";
        let calls = extract_client_calls(src, Language::JavaScript);
        assert!(call(&calls, HttpMethod::Put, "/things").is_some());
    }

    #[test]
    fn axios_request_config_defaults_get() {
        let src = "axios.request({ url: \"/r\" });";
        let calls = extract_client_calls(src, Language::TypeScript);
        assert!(call(&calls, HttpMethod::Get, "/r").is_some());
    }

    #[test]
    fn axios_instance_verb_and_config() {
        let src = "const api = axios.create({ baseURL: \"/api\" });\n\
                   api.get(\"/users\");\n\
                   api({ url: \"/things\", method: \"post\" });";
        let calls = extract_client_calls(src, Language::TypeScript);
        assert!(call(&calls, HttpMethod::Get, "/users").is_some());
        assert!(call(&calls, HttpMethod::Post, "/things").is_some());
        // The `axios.create(...)` itself is not an HTTP call.
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn unrelated_instance_methods_ignored() {
        // A non-verb method on an axios instance is not a call.
        let src = "const api = axios.create();\napi.interceptors.use(fn);";
        let calls = extract_client_calls(src, Language::JavaScript);
        assert!(calls.is_empty());
    }

    #[test]
    fn instance_binding_is_scope_sensitive() {
        // `api` is an axios instance only inside `withClient`; the same-named
        // parameter in `other` must not be treated as an axios client.
        let src =
            "function withClient() { const api = axios.create(); return api.get(\"/in\"); }\n\
                   function other(api) { return api.get(\"/out\"); }";
        let calls = extract_client_calls(src, Language::JavaScript);
        assert!(call(&calls, HttpMethod::Get, "/in").is_some());
        assert!(call(&calls, HttpMethod::Get, "/out").is_none());
    }
}
