use std::path::Path;

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use meridian_core::config::RepoPaths;
use meridian_core::operations_matching as match_operations;
use meridian_core::{
    EdgeKind, HttpMethod, Node, NodeKind, OperationKey, Protocol, Registry, RegistryEntry,
    default_registry_path,
};
use meridian_store::{GraphStore, SqliteStore};
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
    /// Reconcile code endpoints against ingested schema operations and report
    /// drift: endpoints absent from the schema, and schema ops with no handler.
    Drift,
}

#[derive(Serialize)]
struct NeighborOut {
    node: Node,
    edge: String,
    confidence: String,
}

fn resolve_one(store: &dyn GraphStore, name: &str) -> Result<Node> {
    let matches = store.find_by_name(name)?;
    match matches.into_iter().next() {
        Some(n) => Ok(n),
        None => bail!("no symbol named '{name}' in the index"),
    }
}

/// A computed query answer, decoupled from rendering. One variant per verb
/// output shape, so the same value renders as text (single repo) or as
/// repo-tagged JSON (cross-repo).
enum QueryResult {
    Nodes(Vec<Node>),
    Neighbors(Vec<NeighborOut>),
    Operations(Vec<OperationOut>),
    ApiImpact(Vec<ApiImpactOut>),
    Drift(DriftReport),
}

impl QueryResult {
    fn to_value(&self) -> serde_json::Value {
        match self {
            QueryResult::Nodes(v) => serde_json::to_value(v),
            QueryResult::Neighbors(v) => serde_json::to_value(v),
            QueryResult::Operations(v) => serde_json::to_value(v),
            QueryResult::ApiImpact(v) => serde_json::to_value(v),
            QueryResult::Drift(v) => serde_json::to_value(v),
        }
        .unwrap_or(serde_json::Value::Null)
    }

    fn print_text(&self) {
        match self {
            QueryResult::Nodes(v) => nodes_text(v),
            QueryResult::Neighbors(v) => neighbors_text(v),
            QueryResult::Operations(v) => operations_text(v),
            QueryResult::ApiImpact(v) => api_impact_text(v),
            QueryResult::Drift(v) => drift_text(v),
        }
    }
}

fn neighbor_out(items: Vec<(Node, EdgeKind, meridian_core::Confidence)>) -> Vec<NeighborOut> {
    items
        .into_iter()
        .map(|(n, e, c)| NeighborOut {
            node: n,
            edge: e.as_str().to_string(),
            confidence: c.as_str().to_string(),
        })
        .collect()
}

fn nodes_text(nodes: &[Node]) {
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
}

fn neighbors_text(items: &[NeighborOut]) {
    if items.is_empty() {
        println!("(none)");
    }
    for nb in items {
        let n = &nb.node;
        let loc = n
            .span
            .map(|s| format!("{}:{}", n.file, s.start_line))
            .unwrap_or_else(|| n.file.clone());
        println!(
            "{:>10} {} ({}) [{}]",
            nb.edge, n.qualified_name, loc, nb.confidence
        );
    }
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

#[derive(Serialize)]
struct DriftReport {
    /// Code endpoints with no matching schema operation (undocumented).
    undocumented_endpoints: Vec<OperationOut>,
    /// Schema operations with no implementing endpoint (unimplemented).
    unimplemented_schema_ops: Vec<OperationOut>,
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
fn endpoint_handler(store: &dyn GraphStore, id: &str) -> Result<Option<String>> {
    let handlers = store.neighbors(id, Some(EdgeKind::Handles), false)?;
    Ok(handlers
        .into_iter()
        .next()
        .map(|(n, _, _)| n.qualified_name))
}

fn operations_text(ops: &[OperationOut]) {
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
}

/// Build display rows for every operation of `kind`, optionally filtered by
/// protocol. `with_handler` resolves the HANDLES edge for endpoints.
fn collect_operations(
    store: &dyn GraphStore,
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

fn open_store(repo: &Path) -> Result<Box<dyn GraphStore>> {
    let paths = RepoPaths::new(repo);
    if !paths.db_path.exists() {
        bail!(
            "no index found at {} — run `meridian analyze` first",
            paths.db_path.display()
        );
    }
    Ok(Box::new(SqliteStore::open(&paths.db_path)?))
}

/// Compute a query answer against one open store. Pure of output so the same
/// arm serves single-repo text, single-repo JSON, and cross-repo aggregation.
fn execute(store: &dyn GraphStore, cmd: &QueryCmd) -> Result<QueryResult> {
    Ok(match cmd {
        QueryCmd::Def { name } => QueryResult::Nodes(store.find_by_name(name)?),
        QueryCmd::Callers { name } => {
            let n = resolve_one(store, name)?;
            QueryResult::Neighbors(neighbor_out(store.neighbors(
                &n.id.0,
                Some(EdgeKind::Calls),
                false,
            )?))
        }
        QueryCmd::Callees { name } => {
            let n = resolve_one(store, name)?;
            QueryResult::Neighbors(neighbor_out(store.neighbors(
                &n.id.0,
                Some(EdgeKind::Calls),
                true,
            )?))
        }
        QueryCmd::Neighbors { name } => {
            let n = resolve_one(store, name)?;
            let mut items = store.neighbors(&n.id.0, None, true)?;
            items.extend(store.neighbors(&n.id.0, None, false)?);
            QueryResult::Neighbors(neighbor_out(items))
        }
        QueryCmd::Search { text } => QueryResult::Nodes(store.search(text, 50)?),
        QueryCmd::Impact { name, depth } => {
            let n = resolve_one(store, name)?;
            // Callers (transitively) are what breaks if this symbol changes.
            QueryResult::Nodes(store.reachable(&n.id.0, EdgeKind::Calls, false, *depth)?)
        }
        QueryCmd::Endpoints { protocol } => {
            let proto = parse_protocol_filter(protocol.as_deref())?;
            QueryResult::Operations(collect_operations(store, NodeKind::Endpoint, proto, true)?)
        }
        QueryCmd::Clients { protocol } => {
            let proto = parse_protocol_filter(protocol.as_deref())?;
            QueryResult::Operations(collect_operations(
                store,
                NodeKind::ClientCall,
                proto,
                false,
            )?)
        }
        QueryCmd::Serves { path, method } => {
            let m = parse_method_opt(method.as_deref())?;
            let endpoints = store.operations_by_kind(NodeKind::Endpoint)?;
            let mut out = Vec::new();
            for (id, key) in match_operations(&endpoints, m, path) {
                let node = store.get_node(&id.0)?;
                let handler = endpoint_handler(store, &id.0)?;
                out.push(operation_out(&key, node.as_ref(), handler));
            }
            QueryResult::Operations(out)
        }
        QueryCmd::WhoCalls { path, method } => {
            let m = parse_method_opt(method.as_deref())?;
            let endpoints = store.operations_by_kind(NodeKind::Endpoint)?;
            let matched = match_operations(&endpoints, m, path);
            if matched.is_empty() {
                bail!("no endpoint matching '{path}' in the index");
            }
            let mut items = Vec::new();
            for (id, _) in matched {
                items.extend(store.neighbors(&id.0, Some(EdgeKind::Invokes), false)?);
            }
            QueryResult::Neighbors(neighbor_out(items))
        }
        QueryCmd::ApiImpact { path, method } => {
            let m = parse_method_opt(method.as_deref())?;
            let endpoints = store.operations_by_kind(NodeKind::Endpoint)?;
            let matched = match_operations(&endpoints, m, path);
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
            QueryResult::ApiImpact(report)
        }
        QueryCmd::Drift => {
            // EXPOSES links a code endpoint (src) to the schema op it implements
            // (dst). An endpoint with no outgoing EXPOSES is absent from the
            // schema; a schema op with no incoming EXPOSES has no handler.
            let mut undocumented_endpoints = Vec::new();
            for (id, key) in store.operations_by_kind(NodeKind::Endpoint)? {
                if store
                    .neighbors(&id.0, Some(EdgeKind::Exposes), true)?
                    .is_empty()
                {
                    let node = store.get_node(&id.0)?;
                    let handler = endpoint_handler(store, &id.0)?;
                    undocumented_endpoints.push(operation_out(&key, node.as_ref(), handler));
                }
            }
            let mut unimplemented_schema_ops = Vec::new();
            for (id, key) in store.operations_by_kind(NodeKind::SchemaOp)? {
                if store
                    .neighbors(&id.0, Some(EdgeKind::Exposes), false)?
                    .is_empty()
                {
                    let node = store.get_node(&id.0)?;
                    unimplemented_schema_ops.push(operation_out(&key, node.as_ref(), None));
                }
            }
            QueryResult::Drift(DriftReport {
                undocumented_endpoints,
                unimplemented_schema_ops,
            })
        }
    })
}

/// How to render query results.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Emit {
    Text,
    Json,
    /// YAML — compact structured output for LLMs/agents.
    Yaml,
}

impl Emit {
    /// Resolve from the `--json` / `--yaml` flags (yaml wins if both set).
    pub fn from_flags(json: bool, yaml: bool) -> Self {
        if yaml {
            Emit::Yaml
        } else if json {
            Emit::Json
        } else {
            Emit::Text
        }
    }
}

fn print_value(value: &serde_json::Value, emit: Emit) -> Result<()> {
    match emit {
        Emit::Json => println!("{}", serde_json::to_string_pretty(value)?),
        Emit::Yaml => print!("{}", serde_norway::to_string(value)?),
        Emit::Text => {} // handled by callers via print_text
    }
    Ok(())
}

pub fn run(repo: &Path, cmd: QueryCmd, emit: Emit) -> Result<()> {
    let store = open_store(repo)?;
    let result = execute(&*store, &cmd)?;
    match emit {
        Emit::Text => result.print_text(),
        _ => print_value(&result.to_value(), emit)?,
    }
    Ok(())
}

/// Run a query across registered repositories. `all` selects every repo;
/// otherwise `names` selects by registry name. Results are tagged by repo:
/// text mode prints a `== name (root) ==` header before each, JSON mode emits
/// an array of `{ "repo", "result" }`. A per-repo failure (missing index,
/// unmatched endpoint) is reported inline and does not abort the run.
pub fn run_registry(
    cmd: QueryCmd,
    emit: Emit,
    all: bool,
    names: Option<Vec<String>>,
) -> Result<()> {
    let path = default_registry_path()
        .ok_or_else(|| anyhow!("cannot locate home directory (set HOME or USERPROFILE)"))?;
    let registry = Registry::load(&path)?;
    let selected: Vec<&RegistryEntry> = match (all, &names) {
        (true, _) => registry.repos.iter().collect(),
        (false, Some(ns)) => {
            let mut out = Vec::new();
            for name in ns {
                match registry.get(name) {
                    Some(e) => out.push(e),
                    None => bail!("no repository named '{name}' in the registry"),
                }
            }
            out
        }
        (false, None) => registry.repos.iter().collect(),
    };
    if selected.is_empty() {
        println!("(no repositories registered; use `meridian repo add`)");
        return Ok(());
    }

    if emit == Emit::Text {
        for e in &selected {
            println!("== {} ({}) ==", e.name, e.root.display());
            match open_store(&e.root).and_then(|s| execute(s.as_ref(), &cmd)) {
                Ok(r) => r.print_text(),
                Err(err) => println!("  error: {err:#}"),
            }
        }
        return Ok(());
    }

    let mut arr = Vec::new();
    for e in &selected {
        let value = match open_store(&e.root).and_then(|s| execute(s.as_ref(), &cmd)) {
            Ok(r) => r.to_value(),
            Err(err) => serde_json::json!({ "error": format!("{err:#}") }),
        };
        arr.push(serde_json::json!({ "repo": e.name, "result": value }));
    }
    print_value(&serde_json::Value::Array(arr), emit)
}

fn drift_text(report: &DriftReport) {
    if report.undocumented_endpoints.is_empty() && report.unimplemented_schema_ops.is_empty() {
        println!("no drift: every endpoint and schema operation is reconciled");
        return;
    }
    if !report.undocumented_endpoints.is_empty() {
        println!("endpoints absent from the schema:");
        for o in &report.undocumented_endpoints {
            let verb = o.method.as_deref().unwrap_or(&o.protocol);
            let loc = match o.line {
                Some(l) => format!("{}:{}", o.file, l),
                None => o.file.clone(),
            };
            println!("  {verb} {} ({loc})", o.path);
        }
    }
    if !report.unimplemented_schema_ops.is_empty() {
        println!("schema operations with no handler:");
        for o in &report.unimplemented_schema_ops {
            let verb = o.method.as_deref().unwrap_or(&o.protocol);
            println!("  {verb} {} ({})", o.path, o.file);
        }
    }
}

fn api_impact_text(report: &[ApiImpactOut]) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use meridian_core::NodeId;

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
        check!(ids(&matched) == ["get"]);
    }

    #[test]
    fn method_discriminates() {
        let matched = match_operations(&ops(), Some(HttpMethod::Post), "/users/{id}");
        check!(ids(&matched) == ["post"]);
    }

    #[test]
    fn omitted_method_matches_every_verb_on_the_path() {
        let found = match_operations(&ops(), None, "/users/{id}");
        let mut matched = ids(&found);
        matched.sort_unstable();
        check!(matched == ["get", "post"]);
    }

    #[test]
    fn distinct_path_shape_does_not_match() {
        let matched = match_operations(&ops(), Some(HttpMethod::Get), "/users");
        check!(ids(&matched) == ["list"]);
    }

    #[test]
    fn protocol_filter_parses_and_rejects() {
        check!(parse_protocol_filter(None).unwrap() == None);
        check!(parse_protocol_filter(Some("grpc")).unwrap() == Some(Protocol::Grpc));
        check!(parse_protocol_filter(Some("soap")).is_err());
    }

    #[test]
    fn method_filter_parses_and_rejects() {
        check!(parse_method_opt(None).unwrap() == None);
        check!(parse_method_opt(Some("delete")).unwrap() == Some(HttpMethod::Delete));
        check!(parse_method_opt(Some("fetch")).is_err());
    }
}
