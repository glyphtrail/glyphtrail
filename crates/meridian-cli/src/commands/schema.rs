//! API schema ingestion: parse blessed schema artifacts into the operations
//! they declare, for reconciliation against code endpoints.
//!
//! Supports OpenAPI (Swagger 2.0 / OpenAPI 3.x) in JSON or YAML form, gRPC
//! `.proto` service definitions, and GraphQL SDL root operations.

use meridian_core::{HttpMethod, OperationKey, Protocol};
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

/// Parse a GraphQL SDL document into the root operations it declares, as
/// `Query.field` / `Mutation.field` / `Subscription.field`. Best-effort line
/// scanner: enters a root operation type (`type Query { … }`, optionally
/// `extend`ed) and takes each field's leading identifier. Object-type fields
/// don't open braces (args use `()`), so the type's own `}` ends the block.
pub fn graphql_operations(text: &str) -> Vec<String> {
    const ROOTS: [&str; 3] = ["Query", "Mutation", "Subscription"];
    let mut out = Vec::new();
    let mut current: Option<&'static str> = None;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        match current {
            None => {
                let after = line.strip_prefix("extend ").unwrap_or(line);
                if let Some(rest) = after.strip_prefix("type ")
                    && let Some(name) = first_ident(rest)
                    && line.contains('{')
                    && let Some(root) = ROOTS.iter().find(|r| **r == name)
                {
                    current = Some(root);
                }
            }
            Some(root) => {
                if line.starts_with('}') {
                    current = None;
                } else if let Some(field) = first_ident(line) {
                    out.push(format!("{root}.{field}"));
                }
            }
        }
    }
    out
}

/// Parse Hasura metadata (JSON or YAML) into the operations it exposes:
/// per tracked table the auto-generated GraphQL CRUD fields (keyed
/// `OpType.field`, matching SDL ops), RESTified endpoints as REST operations,
/// actions as a GraphQL op plus a POST handler-URL REST op, and remote schemas
/// keyed by name. Tables are read from `sources[].tables[]` (v2/v3 export) and a
/// top-level `tables:` list (split-file `tables.yaml`).
pub fn hasura_operations(text: &str) -> Vec<OperationKey> {
    let doc = serde_json::from_str::<Value>(text)
        .ok()
        .or_else(|| serde_norway::from_str::<Value>(text).ok());
    let Some(doc) = doc else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for table in hasura_tables(&doc) {
        for (op, field) in [
            ("Query", table.clone()),
            ("Query", format!("{table}_by_pk")),
            ("Query", format!("{table}_aggregate")),
            ("Mutation", format!("insert_{table}")),
            ("Mutation", format!("update_{table}")),
            ("Mutation", format!("delete_{table}")),
            ("Subscription", table.clone()),
        ] {
            out.push(OperationKey::opaque(
                Protocol::GraphQl,
                format!("{op}.{field}"),
            ));
        }
    }

    if let Some(eps) = doc.get("rest_endpoints").and_then(Value::as_array) {
        for ep in eps {
            let Some(url) = ep.get("url").and_then(Value::as_str) else {
                continue;
            };
            let path = format!("/api/rest/{}", url.trim_start_matches('/'));
            let methods = ep.get("methods").and_then(Value::as_array);
            match methods {
                Some(ms) => {
                    for m in ms.iter().filter_map(Value::as_str) {
                        if let Some(method) = HttpMethod::parse(m) {
                            out.push(OperationKey::rest(method, &path));
                        }
                    }
                }
                None => out.push(OperationKey::rest(HttpMethod::Get, &path)),
            }
        }
    }

    // Actions: a custom GraphQL query/mutation backed by a webhook handler URL.
    // Emit the GraphQL operation (client gql ops link to it via INVOKES) and the
    // handler's path as a POST REST op (a code endpoint serving the webhook links
    // via EXPOSES). Hasura POSTs the action payload to the handler.
    for action in hasura_list(&doc, "actions") {
        let Some(name) = action.get("name").and_then(Value::as_str) else {
            continue;
        };
        let def = action.get("definition");
        let root = match def.and_then(|d| d.get("type")).and_then(Value::as_str) {
            Some("query") => "Query",
            _ => "Mutation",
        };
        out.push(OperationKey::opaque(
            Protocol::GraphQl,
            format!("{root}.{name}"),
        ));
        if let Some(path) = def
            .and_then(|d| d.get("handler"))
            .and_then(Value::as_str)
            .and_then(webhook_path)
        {
            out.push(OperationKey::rest(HttpMethod::Post, &path));
        }
    }

    // Remote schemas: an external GraphQL service stitched into the API. Its
    // fields aren't in the metadata, so key the schema itself by name as an
    // opaque GraphQL op (marks the external boundary; client ops can't be
    // field-matched without an introspection import — tracked as a follow-up).
    for remote in hasura_list(&doc, "remote_schemas") {
        if let Some(name) = remote.get("name").and_then(Value::as_str) {
            out.push(OperationKey::opaque(
                Protocol::GraphQl,
                format!("RemoteSchema.{name}"),
            ));
        }
    }
    out
}

/// A top-level Hasura metadata list (`actions`, `remote_schemas`, …), read from
/// either the unified `metadata.json` object or a split-file document whose
/// whole body is the list.
fn hasura_list<'a>(doc: &'a Value, key: &str) -> Vec<&'a Value> {
    if let Some(arr) = doc.get(key).and_then(Value::as_array) {
        return arr.iter().collect();
    }
    Vec::new()
}

/// The path component of a Hasura action handler URL. Handles absolute URLs
/// (`http://host:8080/x` → `/x`), env-templated bases
/// (`{{ACTION_BASE_URL}}/x` → `/x`) and already-relative handlers (`/x`).
fn webhook_path(handler: &str) -> Option<String> {
    let h = handler.trim();
    if let Some((_, rest)) = h.split_once("://") {
        let idx = rest.find('/')?;
        return Some(rest[idx..].to_string());
    }
    let idx = h.find('/')?;
    Some(h[idx..].to_string())
}

/// Tracked table names from Hasura metadata, across the layouts Hasura emits.
fn hasura_tables(doc: &Value) -> Vec<String> {
    let mut names = Vec::new();
    let mut collect = |tables: &Value| {
        if let Some(arr) = tables.as_array() {
            for t in arr {
                // `table` is either a bare name or `{ name, schema }`.
                let name = match t.get("table") {
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(obj) => obj.get("name").and_then(Value::as_str).map(str::to_string),
                    None => None,
                };
                if let Some(n) = name {
                    names.push(n);
                }
            }
        }
    };
    if let Some(sources) = doc.get("sources").and_then(Value::as_array) {
        for src in sources {
            if let Some(tables) = src.get("tables") {
                collect(tables);
            }
        }
    }
    if let Some(tables) = doc.get("tables") {
        collect(tables);
    }
    // Split-file `tables.yaml` is a bare top-level list of table entries.
    if doc.is_array() {
        collect(doc);
    }
    names
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
    use assert2::check;

    fn has(ops: &[(HttpMethod, String)], m: HttpMethod, p: &str) -> bool {
        ops.iter().any(|(om, op)| *om == m && op == p)
    }

    #[test]
    fn hasura_tables_yield_crud_ops_and_rest_endpoints() {
        let yaml = r#"
version: 3
sources:
  - name: default
    tables:
      - table:
          name: users
          schema: public
rest_endpoints:
  - url: get_user
    methods: [GET, POST]
"#;
        let ops = hasura_operations(yaml);
        let paths: Vec<&str> = ops.iter().map(|k| k.path.as_str()).collect();
        check!(paths.contains(&"Query.users"));
        check!(paths.contains(&"Query.users_by_pk"));
        check!(paths.contains(&"Mutation.insert_users"));
        check!(paths.contains(&"Mutation.delete_users"));
        check!(paths.contains(&"Subscription.users"));
        // RESTified endpoint, both methods, under /api/rest/.
        check!(
            ops.iter()
                .any(|k| k.method == Some(HttpMethod::Get) && k.path == "/api/rest/get_user")
        );
        check!(
            ops.iter()
                .any(|k| k.method == Some(HttpMethod::Post) && k.path == "/api/rest/get_user")
        );
    }

    #[test]
    fn hasura_actions_yield_graphql_op_and_handler_endpoint() {
        let yaml = r#"
actions:
  - name: createUser
    definition:
      kind: synchronous
      type: mutation
      handler: http://handler:3000/create_user
  - name: currentTime
    definition:
      type: query
      handler: "{{ACTION_BASE_URL}}/now"
remote_schemas:
  - name: payments
    definition:
      url: https://pay.example.com/graphql
"#;
        let ops = hasura_operations(yaml);
        let paths: Vec<&str> = ops.iter().map(|k| k.path.as_str()).collect();
        // GraphQL op per action, op type from `definition.type`.
        check!(paths.contains(&"Mutation.createUser"));
        check!(paths.contains(&"Query.currentTime"));
        // Handler URL path becomes a POST REST op (absolute + env-templated).
        check!(
            ops.iter()
                .any(|k| k.method == Some(HttpMethod::Post) && k.path == "/create_user")
        );
        check!(
            ops.iter()
                .any(|k| k.method == Some(HttpMethod::Post) && k.path == "/now")
        );
        // Remote schema keyed by name.
        check!(paths.contains(&"RemoteSchema.payments"));
    }

    #[test]
    fn webhook_path_forms() {
        check!(webhook_path("http://h:3000/x/y").as_deref() == Some("/x/y"));
        check!(webhook_path("{{BASE}}/create").as_deref() == Some("/create"));
        check!(webhook_path("/already").as_deref() == Some("/already"));
        check!(webhook_path("noslash").is_none());
    }

    #[test]
    fn hasura_top_level_tables_list_and_bare_names() {
        // Split-file `tables.yaml`: a top-level list; `table` may be a bare name.
        let yaml = "- table: posts\n- table: { name: tags, schema: public }\n";
        let ops = hasura_operations(yaml);
        let paths: Vec<&str> = ops.iter().map(|k| k.path.as_str()).collect();
        check!(paths.contains(&"Query.posts"));
        check!(paths.contains(&"Query.tags"));
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
        check!(ops.len() == 4);
        check!(has(&ops, HttpMethod::Get, "/users"));
        check!(has(&ops, HttpMethod::Post, "/users"));
        check!(has(&ops, HttpMethod::Get, "/users/{id}"));
        check!(has(&ops, HttpMethod::Delete, "/users/{id}"));
    }

    #[test]
    fn ignores_non_operation_keys() {
        let json = r#"{ "paths": { "/x": { "summary": "s", "$ref": "y", "get": {} } } }"#;
        let ops = openapi_rest_operations(json);
        check!(ops == vec![(HttpMethod::Get, "/x".to_string())]);
    }

    #[test]
    fn method_keys_are_case_insensitive() {
        let json = r#"{ "paths": { "/x": { "GET": {}, "Post": {} } } }"#;
        let ops = openapi_rest_operations(json);
        check!(has(&ops, HttpMethod::Get, "/x"));
        check!(has(&ops, HttpMethod::Post, "/x"));
    }

    #[test]
    fn ref_only_path_item_yields_no_ops() {
        let json = r##"{ "paths": { "/x": { "$ref": "#/components/pathItems/X" } } }"##;
        check!(openapi_rest_operations(json).is_empty());
    }

    #[test]
    fn invalid_or_empty_yields_nothing() {
        check!(openapi_rest_operations(": : :").is_empty());
        check!(openapi_rest_operations("{}").is_empty());
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
        check!(ops.contains(&"users.v1.UserService/GetUser".to_string()));
        check!(ops.contains(&"users.v1.UserService/CreateUser".to_string()));
        check!(ops.contains(&"users.v1.AdminService/Ban".to_string()));
        // The `message` block's contents are not rpcs.
        check!(ops.len() == 3);
    }

    #[test]
    fn proto_without_package_uses_bare_service() {
        let proto = "service S {\n  rpc M(A) returns (B);\n}\n";
        check!(proto_grpc_operations(proto) == vec!["S/M"]);
    }

    #[test]
    fn graphql_root_fields_become_operations() {
        let sdl = r#"
type User { id: ID! name: String }

type Query {
  # the current user
  me: User
  user(id: ID!): User
}

type Mutation {
  createUser(name: String!): User
}
"#;
        let ops = graphql_operations(sdl);
        check!(ops.contains(&"Query.me".to_string()));
        check!(ops.contains(&"Query.user".to_string()));
        check!(ops.contains(&"Mutation.createUser".to_string()));
        // The non-root `User` type's fields are not operations.
        check!(
            !ops.iter()
                .any(|o| o.ends_with(".id") || o.ends_with(".name"))
        );
        check!(ops.len() == 3);
    }

    #[test]
    fn graphql_extend_query_is_recognized() {
        let sdl = "extend type Query {\n  health: Boolean\n}\n";
        check!(graphql_operations(sdl) == vec!["Query.health"]);
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
        check!(ops.len() == 4);
        check!(has(&ops, HttpMethod::Get, "/users"));
        check!(has(&ops, HttpMethod::Post, "/users"));
        check!(has(&ops, HttpMethod::Get, "/users/{id}"));
        check!(has(&ops, HttpMethod::Delete, "/users/{id}"));
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
        check!(key(openapi_rest_operations(json)) == key(openapi_rest_operations(yaml)));
    }
}
