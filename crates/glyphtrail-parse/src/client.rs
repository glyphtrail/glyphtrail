//! Client-side HTTP call extraction, so outgoing calls can be linked to the
//! server endpoints that answer them (the `INVOKES` edge).
//!
//! JS / TS / TSX (fetch / axios):
//! - `fetch(url, { method })` — the method defaults to GET;
//! - `axios.get(url)` / `axios.post(url, ...)` — verb taken from the call;
//! - `axios(config)` / `axios.request(config)` — `url`/`method` from the config
//!   object (method defaults to GET); and
//! - instance clients created in the same file via `const api = axios.create(…)`
//!   — `api.get(url)`, `api(config)`, `api.request(config)` are treated like
//!   their `axios` equivalents.
//!
//! Rust (reqwest):
//! - `reqwest::get(url)` and builder verbs `client.get(url)` / `.post(...)` /
//!   `.put` / `.delete` / `.patch` / `.head`, where `url` is a string literal
//!   that looks like a URL or path. Type inference is out of scope, so the
//!   URL-shape check is what separates a request builder from an unrelated
//!   `.get(...)`.
//!
//! Go (net/http):
//! - package verbs `http.Get(url)` / `http.Post` / `http.PostForm`, client
//!   verbs `client.Get(url)` / `.Post(...)`, and
//!   `http.NewRequest[WithContext](method, url, …)`. Same URL-shape guard.
//!
//! Python (requests / httpx / aiohttp):
//! - attribute verbs `requests.get(url)` / `client.post(url, …)` and
//!   `requests.request(method, url)`. The URL-shape guard makes this
//!   receiver-agnostic, so module calls (`requests.get`), client/session
//!   instances (`httpx.Client().get`, `aiohttp` `session.post`, a `requests`
//!   `Session`) all match without per-instance tracking. f-string URLs keep
//!   their interpolation (`f"/users/{id}"` -> `/users/{id}`).
//!
//! Only string-literal and template-literal URLs are extracted; fully dynamic
//! URLs (bare variables, concatenations) are out of scope. Template
//! interpolations (`/users/${id}`) are preserved verbatim so they collapse to a
//! dynamic segment in the operation signature.

use glyphtrail_core::{HttpMethod, Language, Span};
use tree_sitter::{Node, Parser};

use crate::registry::grammar;

/// A client-side HTTP call extracted from source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawClientCall {
    pub method: HttpMethod,
    /// URL as written (literal or template); canonicalized later via `OperationKey`.
    pub path: String,
    /// Span of the call expression.
    pub span: Span,
}

/// Extract client HTTP calls from `source`, dispatching by language. Returns
/// empty on parse failure or for languages with no client extractor.
pub fn extract_client_calls(source: &str, lang: &Language) -> Vec<RawClientCall> {
    match lang {
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            js_client_calls(source, lang)
        }
        Language::Rust => rust_client_calls(source),
        Language::Go => go_client_calls(source),
        Language::Python => python_client_calls(source),
        Language::Java | Language::Kotlin => retrofit_client_calls(source, lang),
        _ => Vec::new(),
    }
}

/// Retrofit HTTP-client call sites (JVM): method-level annotations
/// `@GET("path")` / `@POST(...)` / … on interface methods become `(method,
/// path)` client calls. Works for both Java and Kotlin (the annotation shape
/// differs but both nest the verb identifier and a path string literal).
fn retrofit_client_calls(source: &str, lang: &Language) -> Vec<RawClientCall> {
    const VERBS: [&str; 7] = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];
    let mut parser = Parser::new();
    if parser
        .set_language(&grammar(lang).expect("built-in grammar"))
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    walk(tree.root_node(), &mut |n| {
        if n.kind() != "annotation" {
            return;
        }
        // Annotation name: first identifier descendant (Java `@GET`, Kotlin
        // `@GET(...)` via constructor_invocation > user_type > identifier).
        let Some(verb) = first_descendant(n, "identifier").map(|d| text(d, src)) else {
            return;
        };
        let Some(method) = VERBS
            .contains(&verb.as_str())
            .then(|| HttpMethod::parse(&verb))
            .flatten()
        else {
            return;
        };
        // Path: the annotation's string argument (Java string_fragment, Kotlin
        // string_content).
        let path = first_descendant(n, "string_fragment")
            .or_else(|| first_descendant(n, "string_content"))
            .map(|d| text(d, src));
        if let Some(p) = path {
            let path = if p.starts_with('/') {
                p
            } else {
                format!("/{p}")
            };
            out.push(RawClientCall {
                method,
                path,
                span: span_of(n),
            });
        }
    });
    out
}

/// First descendant of `node` with the given kind, in pre-order.
fn first_descendant<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut found = None;
    walk(node, &mut |n| {
        if found.is_none() && n.kind() == kind {
            found = Some(n);
        }
    });
    found
}

/// fetch / axios calls in JS/TS/TSX source.
fn js_client_calls(source: &str, lang: &Language) -> Vec<RawClientCall> {
    let mut parser = Parser::new();
    if parser
        .set_language(&grammar(lang).expect("built-in grammar"))
        .is_err()
    {
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
        if n.kind() == "call_expression"
            && let Some(call) = client_call(n, src, &clients)
        {
            out.push(call);
        }
    });
    out
}

/// reqwest calls in Rust source.
fn rust_client_calls(source: &str) -> Vec<RawClientCall> {
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
    walk(tree.root_node(), &mut |n| {
        if n.kind() == "call_expression"
            && let Some(call) = reqwest_call(n, src)
        {
            out.push(call);
        }
    });
    out
}

/// Classify a Rust `call_expression` as a reqwest request and pull its
/// `(method, url)`. The HTTP verb is the called method/function name; the URL
/// must be a string-literal argument that looks like a URL or path.
fn reqwest_call(call: Node, src: &[u8]) -> Option<RawClientCall> {
    let func = call.child_by_field_name("function")?;
    let verb = match func.kind() {
        // `client.get(url)` / `Client::new().post(url)`
        "field_expression" => text(func.child_by_field_name("field")?, src),
        // `reqwest::get(url)`
        "scoped_identifier" => text(func.child_by_field_name("name")?, src),
        _ => return None,
    };
    let method = HttpMethod::parse(&verb)?;
    let arg = named_arg(call.child_by_field_name("arguments"), 0)?;
    let url = rust_string(arg, src)?;
    if !is_url_like(&url) {
        return None;
    }
    Some(RawClientCall {
        method,
        path: url,
        span: span_of(call),
    })
}

/// Inner text of a Rust `string_literal`, or `None` for non-literals.
fn rust_string(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "string_literal" {
        return None;
    }
    let mut cursor = node.walk();
    if let Some(content) = node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "string_content")
    {
        return Some(text(content, src));
    }
    // Empty string literal: no `string_content` child.
    Some(String::new())
}

/// Whether a literal looks like an HTTP URL or absolute path worth linking.
/// Guards the type-free verb heuristic against unrelated getters/setters.
fn is_url_like(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://") || s.starts_with('/')
}

/// net/http calls in Go source.
fn go_client_calls(source: &str) -> Vec<RawClientCall> {
    let mut parser = Parser::new();
    if parser
        .set_language(&grammar(&Language::Go).expect("built-in grammar"))
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    walk(tree.root_node(), &mut |n| {
        if n.kind() == "call_expression"
            && let Some(call) = go_call(n, src)
        {
            out.push(call);
        }
    });
    out
}

/// Classify a Go `call_expression` as an net/http request: package verbs
/// (`http.Get`, `http.PostForm`), client verbs (`client.Post`), or
/// `http.NewRequest[WithContext](method, url, …)`. A URL-shaped string literal
/// argument is required, since the receiver type isn't inferred.
fn go_call(call: Node, src: &[u8]) -> Option<RawClientCall> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "selector_expression" {
        return None;
    }
    let field = text(func.child_by_field_name("field")?, src);
    let args = call.child_by_field_name("arguments");
    let (method, path) = match field.as_str() {
        "Get" | "Post" | "Put" | "Delete" | "Patch" | "Head" | "PostForm" => {
            (go_verb(&field)?, first_url_arg(args, src)?)
        }
        "NewRequest" | "NewRequestWithContext" => {
            let strings = go_string_args(args, src);
            let method = strings.iter().find_map(|s| HttpMethod::parse(s))?;
            let url = strings.into_iter().find(|s| is_url_like(s))?;
            (method, url)
        }
        _ => return None,
    };
    Some(RawClientCall {
        method,
        path,
        span: span_of(call),
    })
}

/// First URL-shaped string-literal argument, if any.
fn first_url_arg(args: Option<Node>, src: &[u8]) -> Option<String> {
    go_string_args(args, src)
        .into_iter()
        .find(|s| is_url_like(s))
}

/// Text of every string-literal argument (interpreted or raw), in order.
fn go_string_args(args: Option<Node>, src: &[u8]) -> Vec<String> {
    let Some(args) = args else {
        return Vec::new();
    };
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .filter_map(|a| go_string(a, src))
        .collect()
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

/// Map a net/http convenience-function name to its HTTP method.
fn go_verb(field: &str) -> Option<HttpMethod> {
    match field {
        "Get" => Some(HttpMethod::Get),
        "Post" | "PostForm" => Some(HttpMethod::Post),
        "Put" => Some(HttpMethod::Put),
        "Delete" => Some(HttpMethod::Delete),
        "Patch" => Some(HttpMethod::Patch),
        "Head" => Some(HttpMethod::Head),
        _ => None,
    }
}

/// requests / httpx calls in Python source.
fn python_client_calls(source: &str) -> Vec<RawClientCall> {
    let mut parser = Parser::new();
    if parser
        .set_language(&grammar(&Language::Python).expect("built-in grammar"))
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    walk(tree.root_node(), &mut |n| {
        if n.kind() == "call"
            && let Some(call) = python_call(n, src)
        {
            out.push(call);
        }
    });
    out
}

/// Classify a Python `call` as a requests/httpx request: attribute verbs
/// (`requests.get`, `client.post`) or `requests.request(method, url)`. A
/// URL-shaped string argument is required, since the receiver isn't typed.
fn python_call(call: Node, src: &[u8]) -> Option<RawClientCall> {
    // A decorator call like `@app.get("/users")` is a route declaration, not an
    // outgoing client request — leave it to the Flask/FastAPI server extractor.
    if call.parent().map(|p| p.kind()) == Some("decorator") {
        return None;
    }
    let func = call.child_by_field_name("function")?;
    if func.kind() != "attribute" {
        return None;
    }
    let attr = text(func.child_by_field_name("attribute")?, src);
    let args = call.child_by_field_name("arguments");
    let (method, path) = match attr.as_str() {
        "get" | "post" | "put" | "delete" | "patch" | "head" => {
            (HttpMethod::parse(&attr)?, first_py_url(args, src)?)
        }
        "request" => {
            let strings = py_string_args(args, src);
            let method = strings.iter().find_map(|s| HttpMethod::parse(s))?;
            let url = strings.into_iter().find(|s| is_url_like(s))?;
            (method, url)
        }
        _ => return None,
    };
    Some(RawClientCall {
        method,
        path,
        span: span_of(call),
    })
}

/// First URL-shaped positional string argument, if any.
fn first_py_url(args: Option<Node>, src: &[u8]) -> Option<String> {
    py_string_args(args, src)
        .into_iter()
        .find(|s| is_url_like(s))
}

/// Text of every positional string-literal argument (keyword args skipped).
fn py_string_args(args: Option<Node>, src: &[u8]) -> Vec<String> {
    let Some(args) = args else {
        return Vec::new();
    };
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .filter_map(|a| py_string(a, src))
        .collect()
}

/// Inner text of a Python `string` literal, else `None`. f-strings are
/// reconstructed with their interpolations kept verbatim (`f"/users/{id}"` ->
/// `/users/{id}`), mirroring the JS template-literal handling, so the dynamic
/// segment collapses during `OperationKey` signature normalization.
fn py_string(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut cursor = node.walk();
    let mut s = String::new();
    for c in node.named_children(&mut cursor) {
        match c.kind() {
            "string_content" | "interpolation" => s.push_str(&text(c, src)),
            _ => {}
        }
    }
    Some(s)
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

/// Pre-order walk of the whole subtree, visiting each node left-to-right.
/// Iterative (explicit stack) rather than recursive so a deeply nested AST —
/// generated code, minified blobs — can't overflow the thread stack on a large
/// repo. Visit order matches the previous recursive version.
fn walk<'a>(root: Node<'a>, f: &mut dyn FnMut(Node<'a>)) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        f(node);
        let mut cursor = node.walk();
        let start = stack.len();
        stack.extend(node.children(&mut cursor));
        // Children were appended in order; reverse just the new tail so they pop
        // (and are visited) left-to-right.
        stack[start..].reverse();
    }
}

fn named_arg<'a>(args: Option<Node<'a>>, i: usize) -> Option<Node<'a>> {
    let args = args?;
    let mut cursor = args.walk();

    args.named_children(&mut cursor).nth(i)
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
    use assert2::check;

    fn call<'a>(
        calls: &'a [RawClientCall],
        method: HttpMethod,
        path: &str,
    ) -> Option<&'a RawClientCall> {
        calls.iter().find(|c| c.method == method && c.path == path)
    }

    #[test]
    fn fetch_get_string_url() {
        let calls = extract_client_calls("fetch(\"/api/users\");", &Language::JavaScript);
        check!(call(&calls, HttpMethod::Get, "/api/users").is_some());
    }

    // A deeply nested AST must not overflow the stack: `walk` is iterative, so
    // even thousands of nesting levels (generated code) are handled. The old
    // recursive walk overflowed a worker (~2MB) stack here. Still finds the call.
    #[test]
    fn deeply_nested_ast_does_not_overflow() {
        let depth = 20_000;
        let src = format!(
            "{}fetch(\"/api/deep\"){}",
            "[".repeat(depth),
            "]".repeat(depth)
        );
        let calls = extract_client_calls(&src, &Language::JavaScript);
        check!(call(&calls, HttpMethod::Get, "/api/deep").is_some());
    }

    #[test]
    fn fetch_method_from_options() {
        let src = "fetch(\"/api/users\", { method: \"POST\" });";
        let calls = extract_client_calls(src, &Language::JavaScript);
        check!(call(&calls, HttpMethod::Post, "/api/users").is_some());
    }

    #[test]
    fn fetch_template_url_keeps_interpolation() {
        let src = "const r = fetch(`/api/users/${id}`);";
        let calls = extract_client_calls(src, &Language::JavaScript);
        check!(call(&calls, HttpMethod::Get, "/api/users/${id}").is_some());
    }

    #[test]
    fn axios_verb_method() {
        let src = "axios.get(\"/api/items/42\"); axios.delete(\"/api/items/42\");";
        let calls = extract_client_calls(src, &Language::TypeScript);
        check!(call(&calls, HttpMethod::Get, "/api/items/42").is_some());
        check!(call(&calls, HttpMethod::Delete, "/api/items/42").is_some());
    }

    #[test]
    fn dynamic_url_is_skipped() {
        // A bare variable URL is out of scope (no literal to extract).
        let calls = extract_client_calls("fetch(endpoint);", &Language::JavaScript);
        check!(calls.is_empty());
    }

    #[test]
    fn non_axios_member_call_ignored() {
        // `.get` on an unknown object is not assumed to be an HTTP client.
        let calls = extract_client_calls("store.get(\"/x\");", &Language::JavaScript);
        check!(calls.is_empty());
    }

    #[test]
    fn works_in_tsx() {
        let src = "const f = () => fetch(\"/api/ping\");";
        let calls = extract_client_calls(src, &Language::Tsx);
        check!(call(&calls, HttpMethod::Get, "/api/ping").is_some());
    }

    #[test]
    fn axios_config_call() {
        let src = "axios({ url: \"/things\", method: \"PUT\" });";
        let calls = extract_client_calls(src, &Language::JavaScript);
        check!(call(&calls, HttpMethod::Put, "/things").is_some());
    }

    #[test]
    fn axios_request_config_defaults_get() {
        let src = "axios.request({ url: \"/r\" });";
        let calls = extract_client_calls(src, &Language::TypeScript);
        check!(call(&calls, HttpMethod::Get, "/r").is_some());
    }

    #[test]
    fn axios_instance_verb_and_config() {
        let src = "const api = axios.create({ baseURL: \"/api\" });\n\
                   api.get(\"/users\");\n\
                   api({ url: \"/things\", method: \"post\" });";
        let calls = extract_client_calls(src, &Language::TypeScript);
        check!(call(&calls, HttpMethod::Get, "/users").is_some());
        check!(call(&calls, HttpMethod::Post, "/things").is_some());
        // The `axios.create(...)` itself is not an HTTP call.
        check!(calls.len() == 2);
    }

    #[test]
    fn unrelated_instance_methods_ignored() {
        // A non-verb method on an axios instance is not a call.
        let src = "const api = axios.create();\napi.interceptors.use(fn);";
        let calls = extract_client_calls(src, &Language::JavaScript);
        check!(calls.is_empty());
    }

    #[test]
    fn instance_binding_is_scope_sensitive() {
        // `api` is an axios instance only inside `withClient`; the same-named
        // parameter in `other` must not be treated as an axios client.
        let src = "function withClient() { const api = axios.create(); return api.get(\"/in\"); }\n\
                   function other(api) { return api.get(\"/out\"); }";
        let calls = extract_client_calls(src, &Language::JavaScript);
        check!(call(&calls, HttpMethod::Get, "/in").is_some());
        check!(call(&calls, HttpMethod::Get, "/out").is_none());
    }

    #[test]
    fn reqwest_free_function_and_builder_verbs() {
        let src = r#"
            async fn run(client: reqwest::Client) {
                let _ = reqwest::get("https://api.example.com/users").await;
                let _ = client.post("/api/items").send().await;
                let _ = client.delete("/api/items/42").send().await;
            }
        "#;
        let calls = extract_client_calls(src, &Language::Rust);
        check!(call(&calls, HttpMethod::Get, "https://api.example.com/users").is_some());
        check!(call(&calls, HttpMethod::Post, "/api/items").is_some());
        check!(call(&calls, HttpMethod::Delete, "/api/items/42").is_some());
    }

    #[test]
    fn reqwest_skips_non_url_and_dynamic_args() {
        // `.get` with a non-URL literal or a bare variable is not a request.
        let src = r#"
            fn f(map: std::collections::HashMap<String, u32>, url: &str) {
                let _ = map.get("key");
                let _ = client.get(url);
            }
        "#;
        let calls = extract_client_calls(src, &Language::Rust);
        check!(calls.is_empty());
    }

    #[test]
    fn reqwest_ignores_client_construction() {
        // `reqwest::Client::new()` is not a request (verb `new` is not a method).
        let src = "fn f() { let _c = reqwest::Client::new(); }";
        check!(extract_client_calls(src, &Language::Rust).is_empty());
    }

    #[test]
    fn go_net_http_verbs_and_new_request() {
        let src = r#"
func f(c *http.Client) {
    http.Get("https://api.example.com/x/1")
    c.Post("/y", "application/json", nil)
    http.PostForm("/form", nil)
    req, _ := http.NewRequest("PUT", "/z", nil)
    req2, _ := http.NewRequestWithContext(ctx, "DELETE", "/w", nil)
}
"#;
        let calls = extract_client_calls(src, &Language::Go);
        check!(call(&calls, HttpMethod::Get, "https://api.example.com/x/1").is_some());
        check!(call(&calls, HttpMethod::Post, "/y").is_some());
        check!(call(&calls, HttpMethod::Post, "/form").is_some());
        check!(call(&calls, HttpMethod::Put, "/z").is_some());
        check!(call(&calls, HttpMethod::Delete, "/w").is_some());
    }

    #[test]
    fn python_requests_and_httpx_verbs_and_request() {
        let src = r#"
import requests
requests.get("https://api.example.com/x/1")
client.post("/y", json=payload)
requests.request("DELETE", "/z")
"#;
        let calls = extract_client_calls(src, &Language::Python);
        check!(call(&calls, HttpMethod::Get, "https://api.example.com/x/1").is_some());
        check!(call(&calls, HttpMethod::Post, "/y").is_some());
        check!(call(&calls, HttpMethod::Delete, "/z").is_some());
    }

    #[test]
    fn python_httpx_aiohttp_instances_and_fstrings() {
        // Receiver-agnostic: httpx client instances, aiohttp sessions, and
        // requests Sessions all match via the verb + URL-shape guard; f-string
        // URLs keep their interpolation so `{id}` collapses to a dynamic segment.
        let src = r#"
client = httpx.Client()
client.get(f"/users/{id}")
async def f(session):
    await session.post("/items")
s = requests.Session()
s.put(f"/users/{user_id}/profile")
"#;
        let calls = extract_client_calls(src, &Language::Python);
        check!(call(&calls, HttpMethod::Get, "/users/{id}").is_some());
        check!(call(&calls, HttpMethod::Post, "/items").is_some());
        check!(call(&calls, HttpMethod::Put, "/users/{user_id}/profile").is_some());
    }

    #[test]
    fn python_skips_non_url_calls() {
        // `.get` with a non-URL literal (dict access) is not a request.
        let src = "d.get(\"key\")\nfetchData(url)\n";
        check!(extract_client_calls(src, &Language::Python).is_empty());
    }

    #[test]
    fn retrofit_java_annotations_become_client_calls() {
        let src = "interface Api {\n  @GET(\"users/{id}\")\n  Call<User> getUser(@Path(\"id\") int id);\n  @POST(\"/items\")\n  Call<Item> create();\n}\n";
        let calls = extract_client_calls(src, &Language::Java);
        check!(call(&calls, HttpMethod::Get, "/users/{id}").is_some());
        check!(call(&calls, HttpMethod::Post, "/items").is_some());
        // `@Path` is a parameter annotation, not an HTTP verb.
        check!(calls.len() == 2);
    }

    #[test]
    fn retrofit_kotlin_annotations_become_client_calls() {
        let src = "interface Api {\n  @GET(\"users/{id}\")\n  suspend fun getUser(@Path(\"id\") id: Int): User\n}\n";
        let calls = extract_client_calls(src, &Language::Kotlin);
        check!(call(&calls, HttpMethod::Get, "/users/{id}").is_some());
    }

    #[test]
    fn python_decorator_routes_are_not_client_calls() {
        // `@app.get("/users")` is a FastAPI route, not an outgoing request.
        let src = "@app.get(\"/users/{id}\")\ndef get_user(id): ...\n";
        check!(extract_client_calls(src, &Language::Python).is_empty());
    }

    #[test]
    fn go_skips_non_url_and_non_http_calls() {
        // `.Get` with a non-URL literal (e.g. a map key) is not a request.
        let src = r#"
func f(m map[string]int) {
    _ = m.Get("key")
    fmt.Println("/not-a-call")
}
"#;
        check!(extract_client_calls(src, &Language::Go).is_empty());
    }
}
