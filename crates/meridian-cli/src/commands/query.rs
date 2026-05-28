use std::path::Path;

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use meridian_core::config::RepoPaths;
use meridian_core::{
    EdgeKind, HttpMethod, Node, NodeId, NodeKind, OperationKey, Protocol, path_signature,
};
use meridian_store::SqliteStore;
use serde::Serialize;

#[derive(Subcommand)]
pub enum QueryCmd {
    /// Show definition(s) matching a name.
    Def { name: String },
    /// Who calls this symbol.
    Callers { name: String },
    /// What this symbol calls.
    Callees { name: String },
    /// Direct neighbours in any direction.
    Neighbors { name: String },
    /// Full-text search over names and doc comments.
    Search { text: String },
    /// Transitive set of symbols affected if this one changes.
    Impact {
        name: String,
        #[arg(long, default_value_t = 5)]
        depth: usize,
    },
    /// List server-side API endpoints (routes), with their handlers.
    Endpoints {
        /// Restrict to a protocol: rest, grpc, graphql.
        #[arg(long)]
        protocol: Option<String>,
    },
    /// List client-side API call sites.
    Clients {
        /// Restrict to a protocol: rest, grpc, graphql.
        #[arg(long)]
        protocol: Option<String>,
    },
    /// Find the endpoint(s) that serve a path (template- and value-aware).
    Serves {
        /// Request path, e.g. /users/{id} or /users/123.
        path: String,
        /// Restrict to an HTTP method (GET, POST, ...).
        #[arg(long)]
        method: Option<String>,
    },
    /// Client call sites that invoke an endpoint (incoming INVOKES).
    WhoCalls {
        /// Endpoint path, e.g. /users/{id}.
        path: String,
        #[arg(long)]
        method: Option<String>,
    },
    /// Cross-boundary impact of an endpoint: invoking clients, serving
    /// handler(s), and exposed schema operation(s).
    ApiImpact {
        /// Endpoint path, e.g. /users/{id}.
        path: String,
        #[arg(long)]
        method: Option<String>,
    },
}

#[derive(Serialize)]
struct NeighborOut {
    node: Node,
    edge: String,
    confidence: String,
}

fn resolve_one(store: &SqliteStore, name: &str) -> Result<Node> {
    let matches = store.find_by_name(name)?;
    match matches.into_iter().next() {
        Some(n) => Ok(n),
        None => bail!("no symbol named '{name}' in the index"),
    }
}

fn print_nodes(nodes: &[Node], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(nodes)?);
        return Ok(());
    }
    if nodes.is_empty() {
        println!("(none)");
    }
    for n in nodes {
        let loc = n
            .span
            .map(|s| format!("{}:{}", n.file, s.start_line))
            .unwrap_or_else(|| n.file.clone());
        println!("[{}] {} ({})", n.kind.as_str(), n.qualified_name, loc);
        if let Some(doc) = &n.doc {
            let first = doc.lines().next().unwrap_or("");
            println!("    {}", first);
        }
    }
    Ok(())
}

fn print_neighbors(
    items: &[(Node, EdgeKind, meridian_core::Confidence)],
    json: bool,
) -> Result<()> {
    if json {
        let out: Vec<NeighborOut> = items
            .iter()
            .map(|(n, e, c)| NeighborOut {
                node: n.clone(),
                edge: e.as_str().to_string(),
                confidence: c.as_str().to_string(),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if items.is_empty() {
        println!("(none)");
    }
    for (n, e, c) in items {
        let loc = n
            .span
            .map(|s| format!("{}:{}", n.file, s.start_line))
            .unwrap_or_else(|| n.file.clone());
        println!(
            "{:>10} {} ({}) [{}]",
            e.as_str(),
            n.qualified_name,
            loc,
            c.as_str()
        );
    }
    Ok(())
}

/// A flattened API operation for display/JSON output.
#[derive(Serialize)]
struct OperationOut {
    protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    path: String,
    file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    handler: Option<String>,
}

#[derive(Serialize)]
struct ApiImpactOut {
    endpoint: OperationOut,
    handlers: Vec<Node>,
    callers: Vec<Node>,
    schema_ops: Vec<Node>,
}

fn parse_method_opt(s: Option<&str>) -> Result<Option<HttpMethod>> {
    match s {
        None => Ok(None),
        Some(m) => HttpMethod::parse(m)
            .map(Some)
            .ok_or_else(|| anyhow!("unknown HTTP method '{m}'")),
    }
}

fn parse_protocol_filter(s: Option<&str>) -> Result<Option<Protocol>> {
    match s {
        None => Ok(None),
        Some(p) => Protocol::parse(p)
            .map(Some)
            .ok_or_else(|| anyhow!("unknown protocol '{p}' (expected rest, grpc, or graphql)")),
    }
}

/// Match operations from `ops` against a query path (and optional method),
/// comparing by path signature so templates match concrete values
/// (`/users/{id}` matches `/users/123`).
fn match_operations(
    ops: &[(NodeId, OperationKey)],
    method: Option<HttpMethod>,
    path: &str,
) -> Vec<(NodeId, OperationKey)> {
    let want = path_signature(path);
    ops.iter()
        .filter(|(_, key)| {
            path_signature(&key.path) == want && method.is_none_or(|m| key.method == Some(m))
        })
        .cloned()
        .collect()
}

fn operation_out(key: &OperationKey, node: Option<&Node>, handler: Option<String>) -> OperationOut {
    OperationOut {
        protocol: key.protocol.as_str().to_string(),
        method: key.method.map(|m| m.as_str().to_string()),
        path: key.path.clone(),
        file: node.map(|n| n.file.clone()).unwrap_or_default(),
        line: node.and_then(|n| n.span.map(|s| s.start_line)),
        handler,
    }
}

/// The handler symbol serving an endpoint (incoming HANDLES edge), if any.
fn endpoint_handler(store: &SqliteStore, id: &str) -> Result<Option<String>> {
    let handlers = store.neighbors(id, Some(EdgeKind::Handles), false)?;
    Ok(handlers
        .into_iter()
        .next()
        .map(|(n, _, _)| n.qualified_name))
}

fn print_operations(ops: &[OperationOut], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(ops)?);
        return Ok(());
    }
    if ops.is_empty() {
        println!("(none)");
    }
    for o in ops {
        let loc = match o.line {
            Some(l) => format!("{}:{}", o.file, l),
            None => o.file.clone(),
        };
        let verb = o.method.as_deref().unwrap_or(&o.protocol);
        let mut line = format!("{verb:>6} {} ({loc})", o.path);
        if let Some(h) = &o.handler {
            line.push_str(&format!(" -> {h}"));
        }
        println!("{line}");
    }
    Ok(())
}

/// Build display rows for every operation of `kind`, optionally filtered by
/// protocol. `with_handler` resolves the HANDLES edge for endpoints.
fn collect_operations(
    store: &SqliteStore,
    kind: NodeKind,
    protocol: Option<Protocol>,
    with_handler: bool,
) -> Result<Vec<OperationOut>> {
    let mut out = Vec::new();
    for (id, key) in store.operations_by_kind(kind)? {
        if let Some(p) = protocol
            && key.protocol != p
        {
            continue;
        }
        let node = store.get_node(&id.0)?;
        let handler = if with_handler {
            endpoint_handler(store, &id.0)?
        } else {
            None
        };
        out.push(operation_out(&key, node.as_ref(), handler));
    }
    Ok(out)
}

pub fn run(repo: &Path, cmd: QueryCmd, json: bool) -> Result<()> {
    let paths = RepoPaths::new(repo);
    if !paths.db_path.exists() {
        bail!(
            "no index found at {} — run `meridian analyze` first",
            paths.db_path.display()
        );
    }
    let store = SqliteStore::open(&paths.db_path)?;

    match cmd {
        QueryCmd::Def { name } => {
            let nodes = store.find_by_name(&name)?;
            print_nodes(&nodes, json)?;
        }
        QueryCmd::Callers { name } => {
            let n = resolve_one(&store, &name)?;
            let items = store.neighbors(&n.id.0, Some(EdgeKind::Calls), false)?;
            print_neighbors(&items, json)?;
        }
        QueryCmd::Callees { name } => {
            let n = resolve_one(&store, &name)?;
            let items = store.neighbors(&n.id.0, Some(EdgeKind::Calls), true)?;
            print_neighbors(&items, json)?;
        }
        QueryCmd::Neighbors { name } => {
            let n = resolve_one(&store, &name)?;
            let mut items = store.neighbors(&n.id.0, None, true)?;
            items.extend(store.neighbors(&n.id.0, None, false)?);
            print_neighbors(&items, json)?;
        }
        QueryCmd::Search { text } => {
            let nodes = store.search(&text, 50)?;
            print_nodes(&nodes, json)?;
        }
        QueryCmd::Impact { name, depth } => {
            let n = resolve_one(&store, &name)?;
            // Callers (transitively) are what breaks if this symbol changes.
            let nodes = store.reachable(&n.id.0, EdgeKind::Calls, false, depth)?;
            print_nodes(&nodes, json)?;
        }
        QueryCmd::Endpoints { protocol } => {
            let proto = parse_protocol_filter(protocol.as_deref())?;
            let ops = collect_operations(&store, NodeKind::Endpoint, proto, true)?;
            print_operations(&ops, json)?;
        }
        QueryCmd::Clients { protocol } => {
            let proto = parse_protocol_filter(protocol.as_deref())?;
            let ops = collect_operations(&store, NodeKind::ClientCall, proto, false)?;
            print_operations(&ops, json)?;
        }
        QueryCmd::Serves { path, method } => {
            let m = parse_method_opt(method.as_deref())?;
            let endpoints = store.operations_by_kind(NodeKind::Endpoint)?;
            let mut out = Vec::new();
            for (id, key) in match_operations(&endpoints, m, &path) {
                let node = store.get_node(&id.0)?;
                let handler = endpoint_handler(&store, &id.0)?;
                out.push(operation_out(&key, node.as_ref(), handler));
            }
            print_operations(&out, json)?;
        }
        QueryCmd::WhoCalls { path, method } => {
            let m = parse_method_opt(method.as_deref())?;
            let endpoints = store.operations_by_kind(NodeKind::Endpoint)?;
            let matched = match_operations(&endpoints, m, &path);
            if matched.is_empty() {
                bail!("no endpoint matching '{path}' in the index");
            }
            let mut items = Vec::new();
            for (id, _) in matched {
                items.extend(store.neighbors(&id.0, Some(EdgeKind::Invokes), false)?);
            }
            print_neighbors(&items, json)?;
        }
        QueryCmd::ApiImpact { path, method } => {
            let m = parse_method_opt(method.as_deref())?;
            let endpoints = store.operations_by_kind(NodeKind::Endpoint)?;
            let matched = match_operations(&endpoints, m, &path);
            if matched.is_empty() {
                bail!("no endpoint matching '{path}' in the index");
            }
            let mut report = Vec::new();
            for (id, key) in matched {
                let node = store.get_node(&id.0)?;
                let handlers = store
                    .neighbors(&id.0, Some(EdgeKind::Handles), false)?
                    .into_iter()
                    .map(|(n, _, _)| n)
                    .collect();
                let callers = store
                    .neighbors(&id.0, Some(EdgeKind::Invokes), false)?
                    .into_iter()
                    .map(|(n, _, _)| n)
                    .collect();
                let schema_ops = store
                    .neighbors(&id.0, Some(EdgeKind::Exposes), true)?
                    .into_iter()
                    .map(|(n, _, _)| n)
                    .collect();
                report.push(ApiImpactOut {
                    endpoint: operation_out(&key, node.as_ref(), None),
                    handlers,
                    callers,
                    schema_ops,
                });
            }
            print_api_impact(&report, json)?;
        }
    }
    Ok(())
}

fn print_api_impact(report: &[ApiImpactOut], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    if report.is_empty() {
        println!("(none)");
    }
    for r in report {
        let verb = r.endpoint.method.as_deref().unwrap_or(&r.endpoint.protocol);
        println!("{verb} {}", r.endpoint.path);
        let group = |label: &str, nodes: &[Node]| {
            if nodes.is_empty() {
                return;
            }
            println!("  {label}:");
            for n in nodes {
                let loc = n
                    .span
                    .map(|s| format!("{}:{}", n.file, s.start_line))
                    .unwrap_or_else(|| n.file.clone());
                println!("    {} ({loc})", n.qualified_name);
            }
        };
        group("handlers", &r.handlers);
        group("callers", &r.callers);
        group("schema", &r.schema_ops);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops() -> Vec<(NodeId, OperationKey)> {
        vec![
            (
                NodeId("get".into()),
                OperationKey::rest(HttpMethod::Get, "/users/{id}"),
            ),
            (
                NodeId("post".into()),
                OperationKey::rest(HttpMethod::Post, "/users/{id}"),
            ),
            (
                NodeId("list".into()),
                OperationKey::rest(HttpMethod::Get, "/users"),
            ),
        ]
    }

    fn ids(matched: &[(NodeId, OperationKey)]) -> Vec<&str> {
        matched.iter().map(|(id, _)| id.0.as_str()).collect()
    }

    #[test]
    fn template_matches_concrete_value() {
        let matched = match_operations(&ops(), Some(HttpMethod::Get), "/users/123");
        assert_eq!(ids(&matched), ["get"]);
    }

    #[test]
    fn method_discriminates() {
        let matched = match_operations(&ops(), Some(HttpMethod::Post), "/users/{id}");
        assert_eq!(ids(&matched), ["post"]);
    }

    #[test]
    fn omitted_method_matches_every_verb_on_the_path() {
        let found = match_operations(&ops(), None, "/users/{id}");
        let mut matched = ids(&found);
        matched.sort_unstable();
        assert_eq!(matched, ["get", "post"]);
    }

    #[test]
    fn distinct_path_shape_does_not_match() {
        let matched = match_operations(&ops(), Some(HttpMethod::Get), "/users");
        assert_eq!(ids(&matched), ["list"]);
    }

    #[test]
    fn protocol_filter_parses_and_rejects() {
        assert_eq!(parse_protocol_filter(None).unwrap(), None);
        assert_eq!(
            parse_protocol_filter(Some("grpc")).unwrap(),
            Some(Protocol::Grpc)
        );
        assert!(parse_protocol_filter(Some("soap")).is_err());
    }

    #[test]
    fn method_filter_parses_and_rejects() {
        assert_eq!(parse_method_opt(None).unwrap(), None);
        assert_eq!(
            parse_method_opt(Some("delete")).unwrap(),
            Some(HttpMethod::Delete)
        );
        assert!(parse_method_opt(Some("fetch")).is_err());
    }
}
