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
//!   their `axios` equivalents; and
//! - Angular `HttpClient`: a field injected/declared as `HttpClient` —
//!   `constructor(private http: HttpClient)`, a `http: HttpClient` field, or
//!   `http = inject(HttpClient)` — makes `this.http.get(url)` / `.post` / … and
//!   `this.http.request(method, url, …)` (generated services) client calls.
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
//! String-literal, template-literal, and `+`-concatenation URLs are extracted;
//! same-file `const NAME = "literal"` bases are folded in (`` `${BASE}/x` `` /
//! `BASE + "/x"`). A fully dynamic URL (a bare non-constant variable) is out of
//! scope. Unresolved template interpolations (`/users/${id}`) are preserved
//! verbatim so they collapse to a dynamic segment in the operation signature.

use std::collections::HashMap;

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

/// Module-scope constants in a JS/TS/TSX file, for resolving client URLs built
/// from an *imported* constant base at the analyze layer (#405). Keys are a bare
/// `NAME` or a flattened object property `OBJ.PROP`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ModuleConsts {
    /// `const NAME = "lit"` and object string properties (`OBJ.PROP -> "lit"`).
    pub strings: Vec<(String, String)>,
    /// References resolved later: `const NAME = OTHER` / `const NAME = obj.prop`
    /// and object properties whose value is an identifier or member access
    /// (`KEY -> referenced NAME or OBJ.PROP`). Drives the Angular `environment`
    /// idiom (`const API_URL = environment.API_URL`).
    pub refs: Vec<(String, String)>,
}

/// Extract [`ModuleConsts`] from a JS/TS/TSX file; empty for other languages and
/// on parse failure.
pub fn module_constants(source: &str, lang: &Language) -> ModuleConsts {
    let mut out = ModuleConsts::default();
    if !matches!(
        lang,
        Language::JavaScript | Language::TypeScript | Language::Tsx
    ) {
        return out;
    }
    let mut parser = Parser::new();
    if parser
        .set_language(&grammar(lang).expect("built-in grammar"))
        .is_err()
    {
        return out;
    }
    let Some(tree) = parser.parse(source, None) else {
        return out;
    };
    let src = source.as_bytes();
    walk(tree.root_node(), &mut |n| {
        if n.kind() != "variable_declarator" || enclosing_scope(n).kind() != "program" {
            return;
        }
        let (Some(name), Some(value)) = (
            n.child_by_field_name("name"),
            n.child_by_field_name("value"),
        ) else {
            return;
        };
        if name.kind() != "identifier" {
            return;
        }
        let name = text(name, src);
        match value.kind() {
            "string" => {
                if let Some(s) = js_string(value, src) {
                    out.strings.push((name, s));
                }
            }
            "identifier" => out.refs.push((name, text(value, src))),
            "member_expression" => {
                if let Some(m) = member_path(value, src) {
                    out.refs.push((name, m));
                }
            }
            "object" => collect_object_consts(&name, value, src, &mut out),
            _ => {}
        }
    });
    out
}

/// `obj.prop` for a `member_expression` of `identifier.property_identifier`, else
/// `None` (nested/computed accesses are out of scope).
fn member_path(node: Node, src: &[u8]) -> Option<String> {
    let obj = node.child_by_field_name("object")?;
    let prop = node.child_by_field_name("property")?;
    (obj.kind() == "identifier" && prop.kind() == "property_identifier")
        .then(|| format!("{}.{}", text(obj, src), text(prop, src)))
}

/// Flatten an object literal's string / identifier / member-access properties
/// into `OBJ.PROP` entries (`export const environment = { API_URL: "…" }`).
fn collect_object_consts(obj_name: &str, obj: Node, src: &[u8], out: &mut ModuleConsts) {
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
        let Some(key) = key_name(k, src) else {
            continue;
        };
        let full = format!("{obj_name}.{key}");
        match v.kind() {
            "string" => {
                if let Some(s) = js_string(v, src) {
                    out.strings.push((full, s));
                }
            }
            "identifier" => out.refs.push((full, text(v, src))),
            "member_expression" => {
                if let Some(m) = member_path(v, src) {
                    out.refs.push((full, m));
                }
            }
            _ => {}
        }
    }
}

/// Deprecated alias for the string-constant half of [`module_constants`], kept
/// as a transition shim (#405).
#[deprecated(note = "use module_constants(source, lang).strings")]
pub fn module_string_constants(source: &str, lang: &Language) -> Vec<(String, String)> {
    module_constants(source, lang).strings
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
    let http_fields = http_client_fields(root, src);
    let consts = string_constants(root, src);
    // Local variable bindings resolved to URL strings (#443), kept *separate* from
    // `consts`: a binding is only followed when the variable is the whole URL
    // argument, never folded into a `${}` substitution (which stays module-scoped).
    let bindings = local_string_bindings(root, src, &consts);
    let mut out = Vec::new();
    walk(root, &mut |n| {
        if n.kind() == "call_expression"
            && let Some(call) = client_call(n, src, &clients, &http_fields, &consts, &bindings)
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

/// Classify a `call_expression` as a `fetch`/`axios`/Angular-`HttpClient` HTTP
/// call and pull its `(method, url)`; `None` if it is none of those or the URL
/// is non-literal.
fn client_call(
    call: Node,
    src: &[u8],
    clients: &[AxiosBinding],
    http_fields: &[HttpClientField],
    consts: &HashMap<String, String>,
    bindings: &HashMap<String, String>,
) -> Option<RawClientCall> {
    let func = call.child_by_field_name("function")?;
    let args = call.child_by_field_name("arguments");
    let (method, path) = match func.kind() {
        "identifier" => {
            let name = text(func, src);
            if name == "fetch" {
                let path = resolve_url(named_arg(args, 0)?, src, consts, bindings)?;
                let method = named_arg(args, 1)
                    .map(|o| object_method(o, src))
                    .unwrap_or(HttpMethod::Get);
                (method, path)
            } else if is_axios_client(clients, &name, func.start_byte()) {
                // `axios(config)` / `instance(config)`.
                config_call(named_arg(args, 0)?, src, consts, bindings)?
            } else {
                return None;
            }
        }
        "member_expression" => {
            let obj = func.child_by_field_name("object")?;
            let prop = text(func.child_by_field_name("property")?, src);
            // Angular `HttpClient`: `this.<field>.<verb>(url)` where `<field>` is
            // a field typed `HttpClient`. The verb must be a real HTTP method and
            // the URL a literal, so non-HTTP `this.x.get(...)` doesn't match.
            if let Some(field) = http_client_field(obj, src)
                && http_fields
                    .iter()
                    .any(|f| f.name == field && f.scope.contains(&call.start_byte()))
            {
                if prop == "request" {
                    // `HttpClient.request(method, url, options)` — the overload
                    // generated/OpenAPI Angular services use. Method is the first
                    // string arg, URL the second.
                    let method =
                        js_string(named_arg(args, 0)?, src).and_then(|s| HttpMethod::parse(&s))?;
                    (
                        method,
                        resolve_url(named_arg(args, 1)?, src, consts, bindings)?,
                    )
                } else {
                    let method = HttpMethod::parse(&prop)?;
                    (
                        method,
                        resolve_url(named_arg(args, 0)?, src, consts, bindings)?,
                    )
                }
            } else if is_axios_client(clients, &text(obj, src), obj.start_byte()) {
                if prop == "request" {
                    // `axios.request(config)` / `instance.request(config)`.
                    config_call(named_arg(args, 0)?, src, consts, bindings)?
                } else {
                    // `axios.get(url)` / `instance.post(url, ...)`.
                    let method = HttpMethod::parse(&prop)?;
                    (
                        method,
                        resolve_url(named_arg(args, 0)?, src, consts, bindings)?,
                    )
                }
            } else {
                return None;
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

/// The field name of a `this.<field>` receiver (a `member_expression` whose
/// object is `this`), e.g. `http` for `this.http`. `None` otherwise.
fn http_client_field(obj: Node, src: &[u8]) -> Option<String> {
    if obj.kind() != "member_expression" {
        return None;
    }
    if text(obj.child_by_field_name("object")?, src) != "this" {
        return None;
    }
    Some(text(obj.child_by_field_name("property")?, src))
}

/// An `HttpClient`-typed field and the byte range of the class it belongs to, so
/// `this.<name>` resolves per-class — two classes can each name a field `http`
/// with different types without cross-matching.
struct HttpClientField {
    name: String,
    scope: std::ops::Range<usize>,
}

/// Fields typed `HttpClient` in `root`, each scoped to its class — constructor
/// parameter-properties (`constructor(private http: HttpClient)`) or class
/// fields (`http: HttpClient`) — so `this.<name>.<verb>(url)` is recognised as
/// an Angular client call only within the declaring class.
fn http_client_fields(root: Node, src: &[u8]) -> Vec<HttpClientField> {
    let mut fields = Vec::new();
    walk(root, &mut |n| {
        let name = match n.kind() {
            // A constructor parameter-property must carry an accessibility or
            // `readonly` modifier; an ordinary parameter doesn't become a field.
            "required_parameter" | "optional_parameter" => {
                if !is_parameter_property(n) {
                    return;
                }
                n.child_by_field_name("pattern")
            }
            "public_field_definition" => n.child_by_field_name("name"),
            _ => return,
        };
        // Either a `: HttpClient` type annotation, or an `= inject(HttpClient)`
        // initialiser (Angular's functional injection, which has no type).
        let typed = n
            .child_by_field_name("type")
            .is_some_and(|ty| text(ty, src).trim_start_matches(':').trim() == "HttpClient");
        let injected = n
            .child_by_field_name("value")
            .is_some_and(|v| is_inject_httpclient(v, src));
        if !typed && !injected {
            return;
        }
        let Some(name) = name.filter(|x| matches!(x.kind(), "identifier" | "property_identifier"))
        else {
            return;
        };
        let scope = enclosing_class(n).unwrap_or(root);
        fields.push(HttpClientField {
            name: text(name, src),
            scope: scope.start_byte()..scope.end_byte(),
        });
    });
    fields
}

/// Whether `node` is an `inject(HttpClient)` call (Angular functional injection).
fn is_inject_httpclient(node: Node, src: &[u8]) -> bool {
    node.kind() == "call_expression"
        && node
            .child_by_field_name("function")
            .map(|f| text(f, src))
            .as_deref()
            == Some("inject")
        && node
            .child_by_field_name("arguments")
            .and_then(|a| named_arg(Some(a), 0))
            .map(|a| text(a, src))
            .as_deref()
            == Some("HttpClient")
}

/// Whether a parameter is a TypeScript parameter-property (carries an
/// accessibility modifier — `public`/`private`/`protected` — or `readonly`),
/// which is what turns a constructor parameter into a `this.<name>` field.
fn is_parameter_property(param: Node) -> bool {
    let mut cursor = param.walk();
    param
        .children(&mut cursor)
        .any(|c| matches!(c.kind(), "accessibility_modifier" | "readonly"))
}

/// The nearest enclosing class declaration/expression of `node`, if any.
fn enclosing_class(node: Node) -> Option<Node> {
    let mut n = node;
    while let Some(p) = n.parent() {
        if matches!(p.kind(), "class_declaration" | "class") {
            return Some(p);
        }
        n = p;
    }
    None
}

/// `(method, url)` from an axios config object: `url` is required, `method`
/// defaults to GET. The URL folds known same-file constants.
fn config_call(
    config: Node,
    src: &[u8],
    consts: &HashMap<String, String>,
    bindings: &HashMap<String, String>,
) -> Option<(HttpMethod, String)> {
    let url = resolve_url(object_field(config, "url", src)?, src, consts, bindings)?;
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

/// Same-file `const NAME = "literal"` string constants, so a URL built from a
/// base constant (`` `${BASE}/x` `` or `BASE + "/x"`) folds to a concrete path
/// that can match a server route. Imported constants are resolved later, at the
/// analyze layer.
fn string_constants(root: Node, src: &[u8]) -> HashMap<String, String> {
    let mut consts = HashMap::new();
    walk(root, &mut |n| {
        if n.kind() != "variable_declarator" {
            return;
        }
        // Only module-scope declarations, so a name shadowed/redeclared inside a
        // block can't contaminate calls elsewhere (base-URL consts live at module
        // scope). `enclosing_scope` returns `program` for a top-level decl.
        if enclosing_scope(n).kind() != "program" {
            return;
        }
        let (Some(name), Some(value)) = (
            n.child_by_field_name("name"),
            n.child_by_field_name("value"),
        ) else {
            return;
        };
        if name.kind() == "identifier"
            && value.kind() == "string"
            && let Some(s) = js_string(value, src)
        {
            consts.insert(text(name, src), s);
        }
    });
    consts
}

/// URL string from a literal, template, `+`-concatenation, or bare constant,
/// folding known same-file string constants; `None` for a fully dynamic URL.
fn resolve_url(
    node: Node,
    src: &[u8],
    consts: &HashMap<String, String>,
    bindings: &HashMap<String, String>,
) -> Option<String> {
    match node.kind() {
        "string" => js_string(node, src),
        "template_string" => Some(resolve_template(node, src, consts)),
        "binary_expression" => resolve_concat(node, src, consts),
        // A bare identifier used directly as the whole URL: a module const, or a
        // local variable bound to a URL (#443). `${}` folding stays module-scoped.
        "identifier" => {
            let name = text(node, src);
            consts.get(&name).or_else(|| bindings.get(&name)).cloned()
        }
        _ => None,
    }
}

/// Reconstruct a template literal, folding `${NAME}` for a known const and
/// keeping any other `${...}` verbatim (so it collapses to a dynamic segment).
fn resolve_template(node: Node, src: &[u8], consts: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "string_fragment" | "escape_sequence" => out.push_str(&text(child, src)),
            "template_substitution" => {
                let folded = child
                    .named_child(0)
                    .filter(|e| e.kind() == "identifier")
                    .and_then(|e| consts.get(&text(e, src)).cloned());
                match folded {
                    Some(v) => out.push_str(&v),
                    None => out.push_str(&text(child, src)), // `${...}` verbatim
                }
            }
            _ => {}
        }
    }
    out
}

/// Resolve a `+` string concatenation, folding constants; an unresolvable
/// operand becomes a `${dyn}` dynamic segment. `None` when nothing concrete
/// resolves (a fully dynamic expression), so a bare `a + b` is still skipped.
fn resolve_concat(node: Node, src: &[u8], consts: &HashMap<String, String>) -> Option<String> {
    let mut parts = Vec::new();
    let mut any_concrete = false;
    flatten_concat(node, src, consts, &mut parts, &mut any_concrete);
    any_concrete.then(|| parts.concat())
}

/// Every local `const`/`let`/`var X = <init>` binding whose initialiser resolves
/// to a (possibly-dynamic) URL string (#443), so a bare identifier used as a whole
/// URL argument can be followed. Walked in source order, resolving each init
/// against the module consts plus the bindings collected so far (so a binding can
/// reference a module const or an earlier local). Kept separate from the module
/// consts that feed `${}` folding. File-wide (last-wins on a name collision).
fn local_string_bindings(
    root: Node,
    src: &[u8],
    module_consts: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut bindings: HashMap<String, String> = HashMap::new();
    walk(root, &mut |n| {
        if n.kind() != "variable_declarator" {
            return;
        }
        if let Some(name) = n.child_by_field_name("name")
            && name.kind() == "identifier"
            && let Some(value) = n.child_by_field_name("value")
            && let Some(resolved) = resolve_url(value, src, module_consts, &bindings)
        {
            bindings.insert(text(name, src), resolved);
        }
    });
    bindings
}

fn flatten_concat(
    node: Node,
    src: &[u8],
    consts: &HashMap<String, String>,
    parts: &mut Vec<String>,
    any_concrete: &mut bool,
) {
    let is_plus = node.kind() == "binary_expression"
        && node
            .child_by_field_name("operator")
            .map(|o| text(o, src))
            .as_deref()
            == Some("+");
    if !is_plus {
        match resolve_operand(node, src, consts) {
            Some(v) => {
                parts.push(v);
                *any_concrete = true;
            }
            None => parts.push("${dyn}".to_string()),
        }
        return;
    }
    for side in [
        node.child_by_field_name("left"),
        node.child_by_field_name("right"),
    ]
    .into_iter()
    .flatten()
    {
        flatten_concat(side, src, consts, parts, any_concrete);
    }
}

/// Resolve a single concatenation operand to a string, folding a known const.
fn resolve_operand(node: Node, src: &[u8], consts: &HashMap<String, String>) -> Option<String> {
    match node.kind() {
        "string" => js_string(node, src),
        "template_string" => Some(resolve_template(node, src, consts)),
        "identifier" => consts.get(&text(node, src)).cloned(),
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

    // #398: Angular `HttpClient` injected via the constructor — `this.http.<verb>`
    // calls are detected (verb from the method, URL literal or template).
    #[test]
    fn angular_http_client_verbs() {
        let src = "import { HttpClient } from '@angular/common/http';\n\
                   class ApiService {\n\
                     constructor(private http: HttpClient) {}\n\
                     getUser(id: string) { return this.http.get(`/api/users/${id}`); }\n\
                     createUser(b: unknown) { return this.http.post('/api/users', b); }\n\
                   }";
        let calls = extract_client_calls(src, &Language::TypeScript);
        check!(call(&calls, HttpMethod::Get, "/api/users/${id}").is_some());
        check!(call(&calls, HttpMethod::Post, "/api/users").is_some());
    }

    // A field declared `HttpClient` (not constructor-injected) works too.
    #[test]
    fn angular_http_client_field_declaration() {
        let src = "class S {\n  http: HttpClient;\n  f() { return this.http.delete('/api/x'); }\n}";
        let calls = extract_client_calls(src, &Language::TypeScript);
        check!(call(&calls, HttpMethod::Delete, "/api/x").is_some());
    }

    // A `this.<field>.<verb>()` on a non-HttpClient field is not a client call.
    #[test]
    fn non_http_client_field_is_not_a_call() {
        let src = "class S {\n  constructor(private repo: UserRepo) {}\n  \
                   f(id: string) { return this.repo.get(id); }\n}";
        check!(extract_client_calls(src, &Language::TypeScript).is_empty());
    }

    // HttpClient field detection is class-scoped: a same-named `http` field of a
    // different type in another class must not be treated as a client call.
    #[test]
    fn http_client_field_is_class_scoped() {
        let src = "class A {\n  constructor(private http: HttpClient) {}\n  \
                   a() { return this.http.get('/api/a'); }\n}\n\
                   class B {\n  constructor(private http: Other) {}\n  \
                   b() { return this.http.get('/api/b'); }\n}";
        let calls = extract_client_calls(src, &Language::TypeScript);
        check!(call(&calls, HttpMethod::Get, "/api/a").is_some()); // A.http is HttpClient
        check!(call(&calls, HttpMethod::Get, "/api/b").is_none()); // B.http is Other
        check!(calls.len() == 1);
    }

    // #404: `inject(HttpClient)` functional injection (no type annotation) is a
    // recognised HttpClient field.
    #[test]
    fn angular_inject_function_field() {
        let src = "import { inject } from '@angular/core';\n\
                   class S {\n  private http = inject(HttpClient);\n  \
                   f() { return this.http.get('/api/x'); } }";
        let calls = extract_client_calls(src, &Language::TypeScript);
        check!(call(&calls, HttpMethod::Get, "/api/x").is_some());
    }

    // #404: `HttpClient.request(method, url, …)` — the overload generated Angular
    // services use. Method from arg 0, URL from arg 1.
    #[test]
    fn angular_http_client_request_overload() {
        let src = "class S {\n  constructor(private http: HttpClient) {}\n  \
                   f() { return this.http.request('POST', '/api/x', { body: {} }); } }";
        let calls = extract_client_calls(src, &Language::TypeScript);
        check!(call(&calls, HttpMethod::Post, "/api/x").is_some());
    }

    // #405: a same-file `const` base folds into a template-literal URL so the
    // path is concrete enough to match a server route.
    #[test]
    fn client_url_folds_same_file_const_in_template() {
        let src = "const BASE = '/api';\n\
                   class S {\n  constructor(private http: HttpClient) {}\n  \
                   f(id: string) { return this.http.get(`${BASE}/users/${id}`); } }";
        let calls = extract_client_calls(src, &Language::TypeScript);
        // BASE folded; the unknown `${id}` stays dynamic.
        check!(call(&calls, HttpMethod::Get, "/api/users/${id}").is_some());
    }

    // #405: the same base folds across `+` string concatenation.
    #[test]
    fn client_url_folds_same_file_const_in_concat() {
        let src = "const BASE = '/api';\n\
                   class S {\n  constructor(private http: HttpClient) {}\n  \
                   f() { return this.http.get(BASE + '/users'); } }";
        let calls = extract_client_calls(src, &Language::TypeScript);
        check!(call(&calls, HttpMethod::Get, "/api/users").is_some());
    }

    // #405: only module-scope consts fold; a same-named const inside a block
    // must not contaminate — `${BASE}` stays verbatim (dynamic) when BASE is local.
    #[test]
    fn inner_block_const_does_not_fold() {
        let src = "class S {\n  constructor(private http: HttpClient) {}\n  \
                   f() { const BASE = '/local'; return this.http.get(`${BASE}/x`); } }";
        let calls = extract_client_calls(src, &Language::TypeScript);
        check!(call(&calls, HttpMethod::Get, "${BASE}/x").is_some());
    }

    // #443: when the whole URL argument is a local variable, its `const`/`let`
    // binding is followed (and the existing folding then runs on the initialiser).
    #[test]
    fn local_variable_url_binding_is_followed() {
        let src = "const API = '/api/';\n\
                   class S {\n  constructor(private http: HttpClient) {}\n  \
                   inline() { return this.http.get(API + 'things'); }\n  \
                   viaVar() { const url = API + 'widgets'; return this.http.get(url); } }";
        let calls = extract_client_calls(src, &Language::TypeScript);
        // The inline case already worked; the variable-bound case now does too.
        check!(call(&calls, HttpMethod::Get, "/api/things").is_some());
        check!(call(&calls, HttpMethod::Get, "/api/widgets").is_some());
    }

    // #405: module_constants collects string consts, object string properties
    // (flattened as OBJ.PROP), and references (aliases / member access / props
    // naming an identifier) for cross-file resolution.
    #[test]
    fn module_constants_collects_strings_objects_and_refs() {
        let src = "import { LOGIN } from './model';\n\
                   const BASE = '/api';\n\
                   const API_URL = environment.API_URL;\n\
                   export const environment = { API_URL: 'https://h', LOGIN: LOGIN };";
        let c = module_constants(src, &Language::TypeScript);
        check!(c.strings.contains(&("BASE".into(), "/api".into())));
        check!(
            c.strings
                .contains(&("environment.API_URL".into(), "https://h".into()))
        );
        // Member-access alias and a property naming an identifier are references.
        check!(
            c.refs
                .contains(&("API_URL".into(), "environment.API_URL".into()))
        );
        check!(
            c.refs
                .contains(&("environment.LOGIN".into(), "LOGIN".into()))
        );
    }

    // A plain (non-parameter-property) constructor param does not become a
    // `this.` field, so its name must not be picked up.
    #[test]
    fn plain_constructor_param_is_not_a_field() {
        let src = "class S {\n  constructor(http: HttpClient) {}\n  \
                   f() { return this.http.get('/api/x'); }\n}";
        check!(extract_client_calls(src, &Language::TypeScript).is_empty());
    }
}
