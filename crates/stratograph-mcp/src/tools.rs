//! MCP tool definitions and handlers over the Stratograph graph store.

use std::path::Path;

use serde_json::{Value, json};
use stratograph_core::{
    Confidence, EdgeKind, Groups, HttpMethod, ImpactPolicy, ImpactReport, Node, NodeId, NodeKind,
    OperationKey, Protocol, Registry, RepoHealth, default_groups_path, default_registry_path,
    operations_matching,
};
use stratograph_store::{
    ChangeSpec, FederationScope, GraphStore, LadybugStore, SeedSpec, changed_files,
    federated_impact, seed_nodes,
};

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
            "impact",
            "Blast radius of a change: seed from a symbol, a file, or a git \
             change set, and report classified, confidence-aware impacted nodes \
             (tests, API surface, cross-boundary consumers, internal).",
            json!({
                "name": { "type": "string", "description": "Seed symbol name." },
                "file": { "type": "string", "description": "Seed every symbol in this repo-relative file." },
                "files": { "type": "array", "items": { "type": "string" }, "description": "Seed every symbol in these files." },
                "since": { "type": "string", "description": "Seed symbols changed since a git rev/range (e.g. main..HEAD)." },
                "staged": { "type": "boolean", "description": "Seed from staged changes." },
                "diff": { "type": "boolean", "description": "Seed from unstaged working-tree changes." },
                "edges": { "type": "array", "items": { "type": "string" }, "description": "Edge sets: calls, imports, impl, api." },
                "depth": { "type": "integer", "description": "Max hops from a seed (default 5)." },
                "min_confidence": { "type": "string", "enum": ["extracted", "inferred"] },
                "cross_boundary": { "type": "boolean", "description": "Include HANDLES/INVOKES/EXPOSES/MOUNTS consumers." },
                "downstream": { "type": "boolean", "description": "Extend the blast radius into OTHER indexed repos that depend on this one (federate over the registry). Reports which repos break and where." },
                "group": { "type": "string", "description": "Like downstream, but scope the federation to a named group." }
            }),
            &[],
        ),
        tool(
            "status",
            "Index statistics for the repository.",
            json!({}),
            &[],
        ),
        tool(
            "list_repos",
            "List repositories indexed in the global registry, each with its \
             on-disk health (indexed/unindexed/missing) and the groups it \
             belongs to. Use this to discover what is indexed before a \
             cross-repo query — no shell required.",
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
    // Registry-level tools span repos and need no per-repo store.
    if name == "list_repos" {
        return list_repos();
    }
    // Cross-repo impact opens its own member stores (incl. the current repo), so
    // it must not be given the pre-opened per-repo store (no double-open).
    if name == "impact" && is_federated(args) {
        return federated_impact_tool(db, args);
    }
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
        "callers" => neighbors_of(&*store, args, EdgeKind::Calls, false),
        "callees" => neighbors_of(&*store, args, EdgeKind::Calls, true),
        "neighbors" => {
            let node = resolve_one(&*store, req_str(args, "name")?)?;
            let mut items = store.neighbors(&node.id.0, None, true).map_err(err)?;
            items.extend(store.neighbors(&node.id.0, None, false).map_err(err)?);
            Ok(neighbors_json(&items))
        }
        "endpoints" => operations_list(&*store, NodeKind::Endpoint, args, true),
        "clients" => operations_list(&*store, NodeKind::ClientCall, args, false),
        "serves" => {
            let matched = matched_endpoints(&*store, args)?;
            let mut out = Vec::new();
            for (id, key) in matched {
                let node = store.get_node(&id.0).map_err(err)?;
                out.push(operation_json(
                    &key,
                    node.as_ref(),
                    endpoint_handler(&*store, &id)?,
                ));
            }
            Ok(Value::Array(out))
        }
        "who_calls" => {
            let mut items = Vec::new();
            for (id, _) in matched_endpoints(&*store, args)? {
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
            for (id, key) in matched_endpoints(&*store, args)? {
                let node = store.get_node(&id.0).map_err(err)?;
                report.push(json!({
                    "endpoint": operation_json(&key, node.as_ref(), None),
                    "handlers": neighbor_nodes(&*store, &id, EdgeKind::Handles, false)?,
                    "callers": neighbor_nodes(&*store, &id, EdgeKind::Invokes, false)?,
                    "schema_ops": neighbor_nodes(&*store, &id, EdgeKind::Exposes, true)?,
                }));
            }
            Ok(Value::Array(report))
        }
        "impact" => impact_tool(db, &*store, args),
        "status" => {
            let s = store.stats().map_err(err)?;
            let languages: serde_json::Map<String, Value> = s
                .languages
                .into_iter()
                .map(|(lang, n)| (lang, json!(n)))
                .collect();
            Ok(
                json!({ "nodes": s.nodes, "edges": s.edges, "files": s.files, "languages": languages }),
            )
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Enumerate the global registry: every indexed repo with its on-disk health
/// and group membership. Reads the registry and groups files directly (no
/// per-repo store), so an agent can discover what is indexed without a shell.
fn list_repos() -> Result<Value, String> {
    let registry = match default_registry_path() {
        Some(p) => Registry::load(&p).map_err(err)?,
        None => Registry::default(),
    };
    let groups = match default_groups_path() {
        Some(p) => Groups::load(&p).map_err(err)?,
        None => Groups::default(),
    };
    let repos: Vec<Value> = registry
        .repos
        .iter()
        .map(|e| {
            let health = match e.health() {
                RepoHealth::Indexed => "indexed",
                RepoHealth::Unindexed => "unindexed",
                RepoHealth::Missing => "missing",
            };
            let member_of: Vec<&str> = groups
                .groups
                .iter()
                .filter(|g| g.repos.iter().any(|r| r == &e.name))
                .map(|g| g.name.as_str())
                .collect();
            json!({
                "name": e.name,
                "root": e.root,
                "health": health,
                "groups": member_of,
            })
        })
        .collect();
    Ok(Value::Array(repos))
}

/// Open the repo's LadybugDB graph store. `db` is the index anchor path
/// (`.stratograph/graph.db`); the LadybugDB index lives beside it at
/// `.stratograph/ladybug`. Returns a trait object so every tool stays decoupled
/// from the concrete store type.
fn open(db: &Path) -> Result<Box<dyn GraphStore>, String> {
    let dir = db
        .parent()
        .ok_or_else(|| format!("invalid index path {}", db.display()))?;
    let ladybug = dir.join("ladybug");
    if !ladybug.exists() {
        return Err(format!(
            "no index at {} — run `stratograph analyze` first",
            ladybug.display()
        ));
    }
    Ok(Box::new(LadybugStore::open(&ladybug).map_err(err)?))
}

fn neighbors_of(
    store: &dyn GraphStore,
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
    store: &dyn GraphStore,
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

/// Impact-analysis tool: resolve seeds (symbol / file / change set), run the
/// shared engine via `store.classify_impact`, and return the `ImpactReport`
/// JSON — the same report the `stratograph impact` CLI emits.
fn impact_tool(db: &Path, store: &dyn GraphStore, args: &Value) -> Result<Value, String> {
    // The DB lives at <repo>/.stratograph/graph.db; git seed modes need the repo root.
    let repo = db
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));

    let (seeds, removed, unresolved) = if let Some(name) = opt_str(args, "name") {
        let ids: Vec<NodeId> = store
            .find_by_name(name)
            .map_err(err)?
            .into_iter()
            .map(|n| n.id)
            .collect();
        if ids.is_empty() {
            return Err(format!("no symbol named '{name}' in the index"));
        }
        (ids, Vec::new(), Vec::new())
    } else {
        let spec = if let Some(f) = opt_str(args, "file") {
            ChangeSpec::Files(vec![f.to_string()])
        } else if let Some(fs) = str_array(args, "files") {
            ChangeSpec::Files(fs)
        } else if let Some(rev) = opt_str(args, "since") {
            ChangeSpec::Since(rev.to_string())
        } else if args.get("staged").and_then(Value::as_bool).unwrap_or(false) {
            ChangeSpec::Staged
        } else if args.get("diff").and_then(Value::as_bool).unwrap_or(false) {
            ChangeSpec::WorkingTree
        } else {
            return Err("provide 'name' or one of file/files/since/staged/diff".into());
        };
        let files = changed_files(repo, &spec).map_err(err)?;
        let set = seed_nodes(store, &files).map_err(err)?;
        (set.seeds, set.removed_files, set.unresolved_files)
    };

    let policy = policy_from_args(args)?;
    let items = store.classify_impact(&seeds, &policy).map_err(err)?;
    let report = ImpactReport::new(items, removed, unresolved);
    serde_json::to_value(&report).map_err(err)
}

/// Whether the `impact` arguments request a cross-repo (federated) blast radius.
fn is_federated(args: &Value) -> bool {
    args.get("downstream")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || opt_str(args, "group").is_some()
}

/// Build the traversal policy from the shared impact arguments.
fn policy_from_args(args: &Value) -> Result<ImpactPolicy, String> {
    let depth = opt_usize(args, "depth").unwrap_or(5);
    let cross_boundary = args
        .get("cross_boundary")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut policy = if cross_boundary {
        ImpactPolicy::cross_boundary(depth)
    } else {
        ImpactPolicy::in_process(depth)
    };
    if let Some(tokens) = str_array(args, "edges") {
        let refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
        policy.edges = stratograph_core::edge_rules(&refs)?;
    }
    if let Some(mc) = opt_str(args, "min_confidence") {
        policy.min_confidence = stratograph_core::parse_confidence(mc)?;
    }
    Ok(policy)
}

/// Cross-repo impact (#222/#223): seed in the current repo and traverse into
/// downstream repos across the package boundary, returning the per-repo
/// `FederatedReport`. Opens its own member stores via the registry.
fn federated_impact_tool(db: &Path, args: &Value) -> Result<Value, String> {
    let repo = db
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));
    let scope = match opt_str(args, "group") {
        Some(g) => FederationScope::Group(g.to_string()),
        None => FederationScope::Registry,
    };
    let seeds = seed_spec_from_args(args)?;
    let policy = policy_from_args(args)?;
    let report = federated_impact(repo, &scope, seeds, &policy).map_err(err)?;
    serde_json::to_value(&report).map_err(err)
}

/// Build the seed spec from the impact arguments (a symbol name, else a git
/// change set), matching the single-repo tool's precedence.
fn seed_spec_from_args(args: &Value) -> Result<SeedSpec, String> {
    if let Some(name) = opt_str(args, "name") {
        return Ok(SeedSpec::Name(name.to_string()));
    }
    let spec = if let Some(f) = opt_str(args, "file") {
        ChangeSpec::Files(vec![f.to_string()])
    } else if let Some(fs) = str_array(args, "files") {
        ChangeSpec::Files(fs)
    } else if let Some(rev) = opt_str(args, "since") {
        ChangeSpec::Since(rev.to_string())
    } else if args.get("staged").and_then(Value::as_bool).unwrap_or(false) {
        ChangeSpec::Staged
    } else if args.get("diff").and_then(Value::as_bool).unwrap_or(false) {
        ChangeSpec::WorkingTree
    } else {
        return Err("provide 'name' or one of file/files/since/staged/diff".into());
    };
    Ok(SeedSpec::Change(spec))
}

/// Endpoints matching the `path` (+ optional `method`) arguments.
fn matched_endpoints(
    store: &dyn GraphStore,
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

fn endpoint_handler(store: &dyn GraphStore, id: &NodeId) -> Result<Option<String>, String> {
    Ok(store
        .neighbors(&id.0, Some(EdgeKind::Handles), false)
        .map_err(err)?
        .into_iter()
        .next()
        .map(|(n, _, _)| n.qualified_name))
}

fn neighbor_nodes(
    store: &dyn GraphStore,
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

fn resolve_one(store: &dyn GraphStore, name: &str) -> Result<Node, String> {
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

/// A non-empty array-of-strings argument, e.g. `files` or `edges`.
fn str_array(args: &Value, key: &str) -> Option<Vec<String>> {
    let arr = args.get(key)?.as_array()?;
    let out: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    (!out.is_empty()).then_some(out)
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use stratograph_core::{Confidence, Edge, Span};

    // Build a tiny graph in a temp index dir; return the `graph.db` anchor path
    // (the LadybugDB index sits beside it at `<dir>/ladybug`).
    fn build_db(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("stratograph-mcp-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = LadybugStore::open(&dir.join("ladybug")).unwrap();
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
        dir.join("graph.db")
    }

    #[test]
    fn missing_index_is_a_tool_error() {
        let res = call(
            Path::new("/nonexistent/stratograph.db"),
            "status",
            &json!({}),
        );
        check!(res["isError"] == json!(true));
    }

    // #165: the MCP tools open the LadybugDB index that sits beside the
    // `graph.db` anchor path.
    #[test]
    fn opens_ladybug_index_beside_graph_db() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let index_dir =
            std::env::temp_dir().join(format!("stratograph-mcp-lb-{nanos}/.stratograph"));
        std::fs::create_dir_all(&index_dir).unwrap();
        {
            let mut store = LadybugStore::open(&index_dir.join("ladybug")).unwrap();
            store
                .insert_graph(
                    &[Node {
                        id: NodeId("a".into()),
                        kind: NodeKind::Function,
                        name: "lonely".into(),
                        qualified_name: "lonely".into(),
                        file: "a.rs".into(),
                        language: Some("rust".into()),
                        span: None,
                        doc: None,
                    }],
                    &[],
                )
                .unwrap();
        }
        // graph.db itself does not exist; open() must fall through to ladybug.
        let res = call(
            &index_dir.join("graph.db"),
            "definition",
            &json!({ "name": "lonely" }),
        );
        check!(res["isError"] == json!(false));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        check!(parsed[0]["name"] == json!("lonely"));
        std::fs::remove_dir_all(index_dir.parent().unwrap()).ok();
    }

    #[test]
    fn callers_tool_returns_the_caller() {
        let db = build_db("callers");
        let res = call(&db, "callers", &json!({ "name": "callee" }));
        check!(res["isError"] == json!(false));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        check!(parsed[0]["node"]["name"] == json!("caller"));
        check!(parsed[0]["edge"] == json!("calls"));
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
    }

    #[test]
    fn status_tool_reports_counts() {
        let db = build_db("status");
        let res = call(&db, "status", &json!({}));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        check!(parsed["nodes"] == json!(2));
        check!(parsed["edges"] == json!(1));
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
    }

    // #223: a registry-level tool needs no per-repo index. It reads the global
    // registry (empty in CI) and returns an array, even with a bogus db path.
    #[test]
    fn list_repos_needs_no_store_and_returns_an_array() {
        let res = call(Path::new("/nonexistent/graph.db"), "list_repos", &json!({}));
        check!(res["isError"] == json!(false));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        check!(parsed.is_array());
    }

    #[test]
    fn unknown_tool_errors() {
        let db = build_db("unknown");
        let res = call(&db, "nope", &json!({}));
        check!(res["isError"] == json!(true));
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
    }
}
