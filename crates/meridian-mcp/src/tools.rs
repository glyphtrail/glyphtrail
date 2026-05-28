//! MCP tool definitions and handlers over the Meridian graph store.

use std::path::Path;

use meridian_core::{
    Confidence, EdgeKind, HttpMethod, Node, NodeId, NodeKind, OperationKey, Protocol,
    operations_matching,
};
use meridian_store::SqliteStore;
use serde_json::{Value, json};

/// The advertised tool set (`tools/list`).
pub fn definitions() -> Vec<Value> {
    let name_arg = json!({ "name": { "type": "string", "description": "Symbol name." } });
    let proto_arg =
        json!({ "protocol": { "type": "string", "enum": ["rest", "grpc", "graphql"] } });
    let path_method = json!({
        "path": { "type": "string", "description": "Request path, e.g. /users/{id} or /users/123." },
        "method": { "type": "string", "description": "Optional HTTP method (GET, POST, …)." }
    });
    vec![
        tool(
            "search",
            "Full-text search over symbol names and doc comments.",
            json!({ "query": { "type": "string" }, "limit": { "type": "integer" } }),
            &["query"],
        ),
        tool(
            "definition",
            "Locate definition(s) matching a name.",
            name_arg.clone(),
            &["name"],
        ),
        tool(
            "callers",
            "Symbols that call the named symbol.",
            name_arg.clone(),
            &["name"],
        ),
        tool(
            "callees",
            "Symbols the named symbol calls.",
            name_arg.clone(),
            &["name"],
        ),
        tool(
            "neighbors",
            "Direct graph neighbours of the named symbol, any direction.",
            name_arg.clone(),
            &["name"],
        ),
        tool(
            "impact",
            "Transitive set of symbols affected if the named symbol changes.",
            json!({ "name": { "type": "string" }, "depth": { "type": "integer" } }),
            &["name"],
        ),
        tool(
            "endpoints",
            "List server-side API endpoints (routes) with their handlers.",
            proto_arg.clone(),
            &[],
        ),
        tool(
            "clients",
            "List client-side API call sites.",
            proto_arg,
            &[],
        ),
        tool(
            "serves",
            "Endpoint(s) serving a path (template- and value-aware).",
            path_method.clone(),
            &["path"],
        ),
        tool(
            "who_calls",
            "Client call sites that invoke an endpoint (cross-boundary INVOKES).",
            path_method.clone(),
            &["path"],
        ),
        tool(
            "api_impact",
            "Cross-boundary view of an endpoint: invoking clients, handler(s), schema op(s).",
            path_method,
            &["path"],
        ),
        tool(
            "status",
            "Index statistics for the repository.",
            json!({}),
            &[],
        ),
    ]
}

/// Execute a tool call, returning a `tools/call` result object. Tool-level
/// failures are reported as `isError` results rather than protocol errors, per
/// the MCP convention.
pub fn call(db: &Path, name: &str, args: &Value) -> Value {
    match dispatch(db, name, args) {
        Ok(value) => text_result(&value, false),
        Err(message) => text_result(&json!({ "error": message }), true),
    }
}

fn dispatch(db: &Path, name: &str, args: &Value) -> Result<Value, String> {
    let store = open(db)?;
    match name {
        "search" => {
            let limit = opt_usize(args, "limit").unwrap_or(30);
            let nodes = store.search(req_str(args, "query")?, limit).map_err(err)?;
            Ok(nodes_json(&nodes))
        }
        "definition" => {
            let nodes = store.find_by_name(req_str(args, "name")?).map_err(err)?;
            Ok(nodes_json(&nodes))
        }
        "callers" => neighbors_of(&store, args, EdgeKind::Calls, false),
        "callees" => neighbors_of(&store, args, EdgeKind::Calls, true),
        "neighbors" => {
            let node = resolve_one(&store, req_str(args, "name")?)?;
            let mut items = store.neighbors(&node.id.0, None, true).map_err(err)?;
            items.extend(store.neighbors(&node.id.0, None, false).map_err(err)?);
            Ok(neighbors_json(&items))
        }
        "impact" => {
            let node = resolve_one(&store, req_str(args, "name")?)?;
            let depth = opt_usize(args, "depth").unwrap_or(5);
            let nodes = store
                .reachable(&node.id.0, EdgeKind::Calls, false, depth)
                .map_err(err)?;
            Ok(nodes_json(&nodes))
        }
        "endpoints" => operations_list(&store, NodeKind::Endpoint, args, true),
        "clients" => operations_list(&store, NodeKind::ClientCall, args, false),
        "serves" => {
            let matched = matched_endpoints(&store, args)?;
            let mut out = Vec::new();
            for (id, key) in matched {
                let node = store.get_node(&id.0).map_err(err)?;
                out.push(operation_json(
                    &key,
                    node.as_ref(),
                    endpoint_handler(&store, &id)?,
                ));
            }
            Ok(Value::Array(out))
        }
        "who_calls" => {
            let mut items = Vec::new();
            for (id, _) in matched_endpoints(&store, args)? {
                items.extend(
                    store
                        .neighbors(&id.0, Some(EdgeKind::Invokes), false)
                        .map_err(err)?,
                );
            }
            Ok(neighbors_json(&items))
        }
        "api_impact" => {
            let mut report = Vec::new();
            for (id, key) in matched_endpoints(&store, args)? {
                let node = store.get_node(&id.0).map_err(err)?;
                report.push(json!({
                    "endpoint": operation_json(&key, node.as_ref(), None),
                    "handlers": neighbor_nodes(&store, &id, EdgeKind::Handles, false)?,
                    "callers": neighbor_nodes(&store, &id, EdgeKind::Invokes, false)?,
                    "schema_ops": neighbor_nodes(&store, &id, EdgeKind::Exposes, true)?,
                }));
            }
            Ok(Value::Array(report))
        }
        "status" => {
            let s = store.stats().map_err(err)?;
            Ok(json!({ "nodes": s.nodes, "edges": s.edges, "files": s.files }))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn open(db: &Path) -> Result<SqliteStore, String> {
    if !db.exists() {
        return Err(format!(
            "no index at {} — run `meridian analyze` first",
            db.display()
        ));
    }
    SqliteStore::open(db).map_err(err)
}

fn neighbors_of(
    store: &SqliteStore,
    args: &Value,
    kind: EdgeKind,
    outgoing: bool,
) -> Result<Value, String> {
    let node = resolve_one(store, req_str(args, "name")?)?;
    let items = store
        .neighbors(&node.id.0, Some(kind), outgoing)
        .map_err(err)?;
    Ok(neighbors_json(&items))
}

fn operations_list(
    store: &SqliteStore,
    kind: NodeKind,
    args: &Value,
    with_handler: bool,
) -> Result<Value, String> {
    let proto = match opt_str(args, "protocol") {
        Some(p) => Some(Protocol::parse(p).ok_or_else(|| format!("unknown protocol '{p}'"))?),
        None => None,
    };
    let mut out = Vec::new();
    for (id, key) in store.operations_by_kind(kind).map_err(err)? {
        if proto.is_some_and(|p| key.protocol != p) {
            continue;
        }
        let node = store.get_node(&id.0).map_err(err)?;
        let handler = if with_handler {
            endpoint_handler(store, &id)?
        } else {
            None
        };
        out.push(operation_json(&key, node.as_ref(), handler));
    }
    Ok(Value::Array(out))
}

/// Endpoints matching the `path` (+ optional `method`) arguments.
fn matched_endpoints(
    store: &SqliteStore,
    args: &Value,
) -> Result<Vec<(NodeId, OperationKey)>, String> {
    let path = req_str(args, "path")?;
    let method = match opt_str(args, "method") {
        Some(m) => Some(HttpMethod::parse(m).ok_or_else(|| format!("unknown HTTP method '{m}'"))?),
        None => None,
    };
    let endpoints = store.operations_by_kind(NodeKind::Endpoint).map_err(err)?;
    let matched = operations_matching(&endpoints, method, path);
    if matched.is_empty() {
        return Err(format!("no endpoint matching '{path}'"));
    }
    Ok(matched)
}

fn endpoint_handler(store: &SqliteStore, id: &NodeId) -> Result<Option<String>, String> {
    Ok(store
        .neighbors(&id.0, Some(EdgeKind::Handles), false)
        .map_err(err)?
        .into_iter()
        .next()
        .map(|(n, _, _)| n.qualified_name))
}

fn neighbor_nodes(
    store: &SqliteStore,
    id: &NodeId,
    kind: EdgeKind,
    outgoing: bool,
) -> Result<Value, String> {
    let nodes: Vec<Node> = store
        .neighbors(&id.0, Some(kind), outgoing)
        .map_err(err)?
        .into_iter()
        .map(|(n, _, _)| n)
        .collect();
    Ok(nodes_json(&nodes))
}

fn operation_json(key: &OperationKey, node: Option<&Node>, handler: Option<String>) -> Value {
    json!({
        "protocol": key.protocol.as_str(),
        "method": key.method.map(|m| m.as_str()),
        "path": key.path,
        "file": node.map(|n| n.file.clone()),
        "line": node.and_then(|n| n.span.map(|s| s.start_line)),
        "handler": handler,
    })
}

fn nodes_json(nodes: &[Node]) -> Value {
    Value::Array(nodes.iter().map(node_json).collect())
}

fn node_json(n: &Node) -> Value {
    serde_json::to_value(n).unwrap_or(Value::Null)
}

fn neighbors_json(items: &[(Node, EdgeKind, Confidence)]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|(n, e, c)| {
                json!({ "node": node_json(n), "edge": e.as_str(), "confidence": c.as_str() })
            })
            .collect(),
    )
}

fn resolve_one(store: &SqliteStore, name: &str) -> Result<Node, String> {
    store
        .find_by_name(name)
        .map_err(err)?
        .into_iter()
        .next()
        .ok_or_else(|| format!("no symbol named '{name}' in the index"))
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
        },
    })
}

fn text_result(value: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    json!({ "content": [ { "type": "text", "text": text } ], "isError": is_error })
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required string argument '{key}'"))
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn opt_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|v| v as usize)
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_core::{Confidence, Edge, Span};

    // Build a tiny graph at a temp db path and return that path.
    fn build_db(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let db = std::env::temp_dir().join(format!("meridian-mcp-{tag}-{nanos}.db"));
        let mut store = SqliteStore::open(&db).unwrap();
        let mk = |id: &str, name: &str, file: &str, kind: NodeKind| Node {
            id: NodeId(id.into()),
            kind,
            name: name.into(),
            qualified_name: name.into(),
            file: file.into(),
            language: Some("rust".into()),
            span: Some(Span {
                start_byte: 0,
                end_byte: 1,
                start_line: 3,
                end_line: 4,
            }),
            doc: None,
        };
        store
            .insert_graph(
                &[
                    mk("a", "caller", "a.rs", NodeKind::Function),
                    mk("b", "callee", "b.rs", NodeKind::Function),
                ],
                &[Edge {
                    src: NodeId("a".into()),
                    dst: NodeId("b".into()),
                    kind: EdgeKind::Calls,
                    confidence: Confidence::Extracted,
                }],
            )
            .unwrap();
        db
    }

    #[test]
    fn missing_index_is_a_tool_error() {
        let res = call(Path::new("/nonexistent/meridian.db"), "status", &json!({}));
        assert_eq!(res["isError"], json!(true));
    }

    #[test]
    fn callers_tool_returns_the_caller() {
        let db = build_db("callers");
        let res = call(&db, "callers", &json!({ "name": "callee" }));
        assert_eq!(res["isError"], json!(false));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed[0]["node"]["name"], json!("caller"));
        assert_eq!(parsed[0]["edge"], json!("calls"));
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn status_tool_reports_counts() {
        let db = build_db("status");
        let res = call(&db, "status", &json!({}));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["nodes"], json!(2));
        assert_eq!(parsed["edges"], json!(1));
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn unknown_tool_errors() {
        let db = build_db("unknown");
        let res = call(&db, "nope", &json!({}));
        assert_eq!(res["isError"], json!(true));
        std::fs::remove_file(&db).ok();
    }
}
