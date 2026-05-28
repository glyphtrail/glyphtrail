//! API schema ingestion: parse blessed schema artifacts into the `(method,
//! path)` operations they declare, for reconciliation against code endpoints.
//!
//! Currently supports OpenAPI (Swagger 2.0 / OpenAPI 3.x) in JSON form. YAML
//! specs and gRPC/GraphQL schemas are follow-ups.

use codegraph_core::HttpMethod;

/// Parse an OpenAPI JSON document into the REST operations it declares.
/// Unparseable input yields no operations (the caller warns and skips).
pub fn openapi_rest_operations(json: &str) -> Vec<(HttpMethod, String)> {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
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
        assert!(openapi_rest_operations("not json").is_empty());
        assert!(openapi_rest_operations("{}").is_empty());
    }
}
