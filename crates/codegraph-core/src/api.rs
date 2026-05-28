//! Canonical representation of API operations shared across protocols.
//!
//! The cross-boundary linker matches a client call site against the server
//! endpoint that answers it. Both sides are reduced to an [`OperationKey`]
//! (protocol + method + normalized path) and compared via [`OperationKey::signature`],
//! which collapses path parameters and concrete dynamic values so that a server
//! template like `/users/{id}` matches a client call to `/users/123`.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Wire protocol an API operation is exposed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Rest,
    Grpc,
    GraphQl,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Rest => "rest",
            Protocol::Grpc => "grpc",
            Protocol::GraphQl => "graphql",
        }
    }
}

/// HTTP method for REST operations. gRPC/GraphQL operations carry no method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
    Trace,
    Connect,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Head => "HEAD",
            HttpMethod::Options => "OPTIONS",
            HttpMethod::Trace => "TRACE",
            HttpMethod::Connect => "CONNECT",
        }
    }

    /// Parse a method name case-insensitively. Returns `None` for unknown verbs.
    pub fn parse(s: &str) -> Option<HttpMethod> {
        match s.trim().to_ascii_uppercase().as_str() {
            "GET" => Some(HttpMethod::Get),
            "POST" => Some(HttpMethod::Post),
            "PUT" => Some(HttpMethod::Put),
            "PATCH" => Some(HttpMethod::Patch),
            "DELETE" => Some(HttpMethod::Delete),
            "HEAD" => Some(HttpMethod::Head),
            "OPTIONS" => Some(HttpMethod::Options),
            "TRACE" => Some(HttpMethod::Trace),
            "CONNECT" => Some(HttpMethod::Connect),
            _ => None,
        }
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A canonical API operation: protocol + (REST) method + normalized path.
///
/// For gRPC, `path` holds `package.Service/Method` and `method` is `None`.
/// For GraphQL, `path` holds `OperationType.field` and `method` is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OperationKey {
    pub protocol: Protocol,
    pub method: Option<HttpMethod>,
    pub path: String,
}

impl OperationKey {
    /// Build a REST key, normalizing the raw path (`/users/:id` → `/users/{id}`).
    pub fn rest(method: HttpMethod, raw_path: &str) -> Self {
        OperationKey {
            protocol: Protocol::Rest,
            method: Some(method),
            path: normalize_path(raw_path),
        }
    }

    /// A protocol-agnostic key whose `path` is taken verbatim (already canonical).
    pub fn opaque(protocol: Protocol, path: impl Into<String>) -> Self {
        OperationKey {
            protocol,
            method: None,
            path: path.into(),
        }
    }

    /// A match signature that collapses path parameters and concrete dynamic
    /// values, so equivalent client/server operations produce the same string.
    /// e.g. `GET /users/{id}` and `GET /users/123` both yield `rest|GET|/users/{}`.
    pub fn signature(&self) -> String {
        let method = self.method.map(|m| m.as_str()).unwrap_or("*");
        format!(
            "{}|{}|{}",
            self.protocol.as_str(),
            method,
            path_signature(&self.path)
        )
    }
}

impl fmt::Display for OperationKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.method {
            Some(m) => write!(f, "{} {}", m, self.path),
            None => write!(f, "{} {}", self.protocol.as_str(), self.path),
        }
    }
}

/// Normalize a path or URL into a canonical templated path:
/// drops scheme/host, query and fragment; collapses duplicate and trailing
/// slashes; and rewrites framework route-parameter syntaxes to `{name}`.
///
/// Path case is preserved (paths are case-sensitive); only structure and
/// parameter syntax are canonicalized.
pub fn normalize_path(raw: &str) -> String {
    // Drop scheme://authority if a full URL was passed.
    let mut s = raw.trim();
    if let Some(idx) = s.find("://") {
        let after = &s[idx + 3..];
        s = match after.find('/') {
            Some(p) => &after[p..],
            None => "",
        };
    }
    // Drop query string / fragment.
    let s = s.split(['?', '#']).next().unwrap_or("");

    let mut out = String::from("/");
    let mut first = true;
    for seg in s.split('/') {
        if seg.is_empty() {
            continue;
        }
        let canon = canon_param_segment(seg).unwrap_or_else(|| seg.to_string());
        if !first {
            out.push('/');
        }
        out.push_str(&canon);
        first = false;
    }
    out
}

/// Collapse a normalized path to a match signature: every parameter or concrete
/// dynamic segment becomes `{}`, so `/users/{id}` and `/users/123` are equal.
pub fn path_signature(path: &str) -> String {
    let norm = normalize_path(path);
    let mut out = String::from("/");
    let mut first = true;
    for seg in norm.split('/') {
        if seg.is_empty() {
            continue;
        }
        if !first {
            out.push('/');
        }
        out.push_str(if is_dynamic_segment(seg) { "{}" } else { seg });
        first = false;
    }
    out
}

/// Rewrite a single path segment from a framework param syntax to `{name}`,
/// or return `None` if it is a literal segment.
fn canon_param_segment(seg: &str) -> Option<String> {
    // OpenAPI / axum 0.7+: {name} or {name:regex}
    if let Some(inner) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
        let name = inner.split(':').next().unwrap_or("");
        return Some(brace(name));
    }
    // Express / Rails / axum 0.6: :name
    if let Some(name) = seg.strip_prefix(':') {
        return Some(brace(name.trim_end_matches('?')));
    }
    // Rocket / Flask: <name>, <conv:name>, <name..>
    if let Some(inner) = seg.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
        let name = inner
            .split(':')
            .next_back()
            .unwrap_or(inner)
            .trim_end_matches("..");
        return Some(brace(name));
    }
    // Next.js: [name], [...name], [[...name]]
    if seg.starts_with('[') && seg.ends_with(']') {
        let name = seg
            .trim_matches(|c| c == '[' || c == ']')
            .trim_start_matches("...");
        return Some(brace(name));
    }
    // Wildcard tail: * or *name (axum/actix)
    if seg == "*" {
        return Some("{*}".to_string());
    }
    if let Some(name) = seg.strip_prefix('*') {
        return Some(brace(name));
    }
    None
}

/// Wrap a parameter name in braces, keeping only identifier-safe characters.
fn brace(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if cleaned.is_empty() {
        "{*}".to_string()
    } else {
        format!("{{{cleaned}}}")
    }
}

/// Whether a segment should be treated as a parameter when comparing signatures:
/// an explicit `{...}` template, a JS template interpolation, a pure-numeric id,
/// or a UUID.
fn is_dynamic_segment(seg: &str) -> bool {
    if seg.starts_with('{') && seg.ends_with('}') {
        return true;
    }
    if seg.contains("${") {
        return true;
    }
    if !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    is_uuid(seg)
}

/// Canonical 8-4-4-4-12 hex UUID check.
fn is_uuid(seg: &str) -> bool {
    let b = seg.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, &c) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if c != b'-' {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_query_fragment_and_trailing_slash() {
        assert_eq!(normalize_path("/users/?page=2#x"), "/users");
        assert_eq!(normalize_path("/users/"), "/users");
        assert_eq!(normalize_path("/users//list///"), "/users/list");
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path(""), "/");
    }

    #[test]
    fn normalize_strips_scheme_and_host() {
        assert_eq!(
            normalize_path("https://api.example.com/users/1"),
            "/users/1"
        );
        assert_eq!(normalize_path("http://localhost:8080/"), "/");
    }

    #[test]
    fn normalize_rewrites_framework_param_syntaxes() {
        assert_eq!(normalize_path("/users/:id"), "/users/{id}");
        assert_eq!(normalize_path("/users/{id}"), "/users/{id}");
        assert_eq!(normalize_path("/users/{id:[0-9]+}"), "/users/{id}");
        assert_eq!(normalize_path("/users/<int:id>"), "/users/{id}");
        assert_eq!(normalize_path("/files/<path..>"), "/files/{path}");
        assert_eq!(normalize_path("/users/[id]"), "/users/{id}");
        assert_eq!(normalize_path("/docs/[...slug]"), "/docs/{slug}");
        assert_eq!(normalize_path("/assets/*path"), "/assets/{path}");
    }

    #[test]
    fn signature_matches_template_against_concrete_values() {
        let server = OperationKey::rest(HttpMethod::Get, "/api/users/{id}");
        let client_num = OperationKey::rest(HttpMethod::Get, "/api/users/123");
        let client_tmpl = OperationKey::rest(HttpMethod::Get, "/api/users/${userId}");
        let client_uuid = OperationKey::rest(
            HttpMethod::Get,
            "/api/users/550e8400-e29b-41d4-a716-446655440000",
        );
        assert_eq!(server.signature(), client_num.signature());
        assert_eq!(server.signature(), client_tmpl.signature());
        assert_eq!(server.signature(), client_uuid.signature());
        assert_eq!(server.signature(), "rest|GET|/api/users/{}");
    }

    #[test]
    fn signature_distinguishes_method_and_shape() {
        let get = OperationKey::rest(HttpMethod::Get, "/users/{id}");
        let post = OperationKey::rest(HttpMethod::Post, "/users/{id}");
        let collection = OperationKey::rest(HttpMethod::Get, "/users");
        assert_ne!(get.signature(), post.signature());
        assert_ne!(get.signature(), collection.signature());
    }

    #[test]
    fn param_name_differences_do_not_affect_matching() {
        let a = OperationKey::rest(HttpMethod::Get, "/users/{id}/posts/{postId}");
        let b = OperationKey::rest(HttpMethod::Get, "/users/:userId/posts/:pid");
        assert_eq!(a.signature(), b.signature());
    }

    #[test]
    fn literal_numeric_prefixes_stay_literal() {
        // A version prefix is literal; only standalone numeric ids collapse.
        assert_eq!(path_signature("/v1/users/42"), "/v1/users/{}");
    }

    #[test]
    fn http_method_parse_is_case_insensitive() {
        assert_eq!(HttpMethod::parse("get"), Some(HttpMethod::Get));
        assert_eq!(HttpMethod::parse("  Post "), Some(HttpMethod::Post));
        assert_eq!(HttpMethod::parse("FETCH"), None);
    }

    #[test]
    fn display_is_human_readable() {
        let k = OperationKey::rest(HttpMethod::Delete, "/users/:id");
        assert_eq!(k.to_string(), "DELETE /users/{id}");
        let g = OperationKey::opaque(Protocol::Grpc, "users.UserService/GetUser");
        assert_eq!(g.to_string(), "grpc users.UserService/GetUser");
    }
}
