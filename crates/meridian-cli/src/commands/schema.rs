//! API schema ingestion: parse blessed schema artifacts into the operations
//! they declare, for reconciliation against code endpoints.
//!
//! Supports OpenAPI (Swagger 2.0 / OpenAPI 3.x) in JSON or YAML form and gRPC
//! `.proto` service definitions. GraphQL SDL is a follow-up.

use meridian_core::HttpMethod;
use serde_json::Value;

/// Parse an OpenAPI document (JSON or YAML) into the REST operations it
/// declares. Unparseable input yields no operations (the caller warns and
/// skips). JSON is attempted first, then YAML — every JSON document is also
/// valid YAML, but the JSON parser is faster and gives better errors.
pub fn openapi_rest_operations(text: &str) -> Vec<(HttpMethod, String)> {
    let doc = serde_json::from_str::<Value>(text)
        .ok()
        .or_else(|| serde_norway::from_str::<Value>(text).ok());
    let Some(doc) = doc else {
        return Vec::new();
    };
    operations_from_doc(&doc)
}

/// Extract `(method, path)` pairs from a parsed OpenAPI document's `paths`.
fn operations_from_doc(doc: &Value) -> Vec<(HttpMethod, String)> {
    let Some(paths) = doc.get("paths").and_then(|p| p.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        // A path item maps HTTP-method keys to operations; non-method keys
        // (parameters, $ref, servers, summary, …) are not operations.
        let before = out.len();
        for method_key in item.keys() {
            if let Some(method) = operation_method(method_key) {
                out.push((method, path.clone()));
            }
        }
        // A path item expressed only via `$ref` is not resolved (yet); surface
        // it so missing operations are explainable rather than silent.
        if out.len() == before && item.contains_key("$ref") {
            tracing::warn!(
                "schema path {path:?} is a $ref path item; reference resolution is unsupported, no operations extracted"
            );
        }
    }
    out
}

/// Parse a gRPC `.proto` file into the operations it declares, as canonical
/// `package.Service/Method` paths (the gRPC wire convention). Best-effort
/// line scanner: tracks the `package`, the enclosing `service`, and each `rpc`.
/// Service bodies hold only rpcs/options, and `message`/`enum`/`service` start
/// new top-level blocks, so a brace-free scan is sufficient for normal protos.
pub fn proto_grpc_operations(text: &str) -> Vec<String> {
    let mut package = String::new();
    let mut service: Option<String> = None;
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if let Some(rest) = line.strip_prefix("package ") {
            package = rest.trim_end_matches(';').trim().to_string();
        } else if let Some(rest) = line.strip_prefix("service ") {
            service = first_ident(rest);
        } else if line.starts_with("message ") || line.starts_with("enum ") {
            service = None; // left the service body
        } else if let Some(rest) = line.strip_prefix("rpc ")
            && let Some(svc) = &service
            && let Some(method) = first_ident(rest)
        {
            let prefix = if package.is_empty() {
                svc.clone()
            } else {
                format!("{package}.{svc}")
            };
            out.push(format!("{prefix}/{method}"));
        }
    }
    out
}

/// The leading identifier of `s` (up to the first whitespace, `{`, or `(`).
fn first_ident(s: &str) -> Option<String> {
    let name: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The HTTP method for an OpenAPI path-item key, or `None` for non-operation
/// keys. Limited to the verbs OpenAPI defines as operations; matched
/// case-insensitively so non-canonical (upper/mixed-case) keys still parse.
fn operation_method(key: &str) -> Option<HttpMethod> {
    let key = key.trim().to_ascii_lowercase();
    matches!(
        key.as_str(),
        "get" | "post" | "put" | "patch" | "delete" | "head" | "options" | "trace"
    )
    .then(|| HttpMethod::parse(&key))
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(ops: &[(HttpMethod, String)], m: HttpMethod, p: &str) -> bool {
        ops.iter().any(|(om, op)| *om == m && op == p)
    }

    #[test]
    fn extracts_paths_and_methods() {
        let json = r#"{
            "openapi": "3.0.0",
            "paths": {
                "/users": { "get": {}, "post": {} },
                "/users/{id}": {
                    "parameters": [],
                    "get": {},
                    "delete": {}
                }
            }
        }"#;
        let ops = openapi_rest_operations(json);
        assert_eq!(ops.len(), 4);
        assert!(has(&ops, HttpMethod::Get, "/users"));
        assert!(has(&ops, HttpMethod::Post, "/users"));
        assert!(has(&ops, HttpMethod::Get, "/users/{id}"));
        assert!(has(&ops, HttpMethod::Delete, "/users/{id}"));
    }

    #[test]
    fn ignores_non_operation_keys() {
        let json = r#"{ "paths": { "/x": { "summary": "s", "$ref": "y", "get": {} } } }"#;
        let ops = openapi_rest_operations(json);
        assert_eq!(ops, vec![(HttpMethod::Get, "/x".to_string())]);
    }

    #[test]
    fn method_keys_are_case_insensitive() {
        let json = r#"{ "paths": { "/x": { "GET": {}, "Post": {} } } }"#;
        let ops = openapi_rest_operations(json);
        assert!(has(&ops, HttpMethod::Get, "/x"));
        assert!(has(&ops, HttpMethod::Post, "/x"));
    }

    #[test]
    fn ref_only_path_item_yields_no_ops() {
        let json = r##"{ "paths": { "/x": { "$ref": "#/components/pathItems/X" } } }"##;
        assert!(openapi_rest_operations(json).is_empty());
    }

    #[test]
    fn invalid_or_empty_yields_nothing() {
        assert!(openapi_rest_operations(": : :").is_empty());
        assert!(openapi_rest_operations("{}").is_empty());
    }

    #[test]
    fn proto_operations_use_package_service_method() {
        let proto = r#"
syntax = "proto3";
package users.v1;

// User management.
service UserService {
    rpc GetUser(GetUserRequest) returns (User);
    rpc CreateUser(CreateUserRequest) returns (User); // inline comment
}

message GetUserRequest { string id = 1; }

service AdminService {
    rpc Ban(BanRequest) returns (BanReply);
}
"#;
        let ops = proto_grpc_operations(proto);
        assert!(ops.contains(&"users.v1.UserService/GetUser".to_string()));
        assert!(ops.contains(&"users.v1.UserService/CreateUser".to_string()));
        assert!(ops.contains(&"users.v1.AdminService/Ban".to_string()));
        // The `message` block's contents are not rpcs.
        assert_eq!(ops.len(), 3);
    }

    #[test]
    fn proto_without_package_uses_bare_service() {
        let proto = "service S {\n  rpc M(A) returns (B);\n}\n";
        assert_eq!(proto_grpc_operations(proto), vec!["S/M"]);
    }

    #[test]
    fn parses_yaml_specs() {
        let yaml = r#"
openapi: 3.0.0
paths:
  /users:
    get: {}
    post: {}
  /users/{id}:
    parameters: []
    get: {}
    delete: {}
"#;
        let ops = openapi_rest_operations(yaml);
        assert_eq!(ops.len(), 4);
        assert!(has(&ops, HttpMethod::Get, "/users"));
        assert!(has(&ops, HttpMethod::Post, "/users"));
        assert!(has(&ops, HttpMethod::Get, "/users/{id}"));
        assert!(has(&ops, HttpMethod::Delete, "/users/{id}"));
    }

    #[test]
    fn json_and_yaml_agree() {
        let json = r#"{ "paths": { "/x": { "get": {}, "put": {} } } }"#;
        let yaml = "paths:\n  /x:\n    get: {}\n    put: {}\n";
        let key = |ops: Vec<(HttpMethod, String)>| {
            let mut v: Vec<String> = ops.iter().map(|(m, p)| format!("{m} {p}")).collect();
            v.sort();
            v
        };
        assert_eq!(
            key(openapi_rest_operations(json)),
            key(openapi_rest_operations(yaml))
        );
    }
}
