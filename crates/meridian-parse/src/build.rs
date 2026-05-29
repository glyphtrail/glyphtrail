use std::collections::HashSet;

use meridian_core::{
    CodeGraph, Confidence, Edge, EdgeKind, Language, Node, NodeId, NodeKind, OperationKey,
    Protocol, Span,
};

use crate::client::extract_client_calls;
use crate::extract::{ParsedFile, RawDef};
use crate::rest::extractors_for;

/// A symbol exported from a file for cross-file resolution.
#[derive(Debug, Clone)]
pub struct SymbolEntry {
    pub name: String,
    pub id: NodeId,
    pub kind: NodeKind,
}

/// An edge whose target could not be resolved within the file; resolved globally.
#[derive(Debug, Clone)]
pub struct PendingEdge {
    pub src: NodeId,
    pub name: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Default)]
pub struct FileGraph {
    pub graph: CodeGraph,
    pub symbols: Vec<SymbolEntry>,
    pub pending: Vec<PendingEdge>,
    /// Raw import targets (cleaned of quotes/brackets) for this file, resolved
    /// to real file/module nodes by the global pass in `analyze`.
    pub imports: Vec<String>,
}

/// Server-side API surface extracted from a router file: `Endpoint` nodes,
/// their operation keys, and `HANDLES` links (handler symbol -> endpoint).
#[derive(Debug, Default)]
pub struct RestGraph {
    pub graph: CodeGraph,
    /// `(endpoint id, operation key)` for persistence and cross-boundary matching.
    pub operations: Vec<(NodeId, OperationKey)>,
    /// `(handler name, endpoint id)` for handlers that were not a unique local
    /// definition; resolved against the global index as `handler -> endpoint`.
    pub pending_handlers: Vec<(String, NodeId)>,
}

/// Build the REST endpoint fragment for a router file in `lang`, running every
/// registered [`RestServerExtractor`](crate::rest::RestServerExtractor) for that
/// language. `symbols` are the file's own definitions (from [`build_file_graph`]),
/// used to resolve a handler to a `HANDLES` edge when it is uniquely defined in
/// the same file. Languages with no registered extractor yield an empty graph.
pub fn build_rest_graph(
    rel_path: &str,
    lang: &Language,
    symbols: &[SymbolEntry],
    source: &str,
) -> RestGraph {
    let mut rg = RestGraph::default();
    let extractors = extractors_for(lang);
    if extractors.is_empty() {
        return rg;
    }
    let mut raws = Vec::new();
    for ex in &extractors {
        raws.extend(ex.endpoints(source));
    }

    let mut seen: HashSet<NodeId> = HashSet::new();
    for ep in raws {
        let key = OperationKey::rest(ep.method, &ep.path);
        let method = ep.method.as_str();
        let ep_id = NodeId::derive(&[rel_path, "endpoint", method, &key.path]);
        // Distinct (method, path) per file; identical routes collapse to one node.
        if !seen.insert(ep_id.clone()) {
            continue;
        }
        rg.graph.add_node(Node {
            id: ep_id.clone(),
            kind: NodeKind::Endpoint,
            name: format!("{method} {}", key.path),
            qualified_name: key.path.clone(),
            file: rel_path.to_string(),
            language: Some(lang.name().to_string()),
            span: Some(ep.span),
            doc: None,
        });
        rg.operations.push((ep_id.clone(), key));

        if ep.handler.is_empty() {
            continue; // closure handler: no symbol to link.
        }
        let local: Vec<&SymbolEntry> = symbols.iter().filter(|s| s.name == ep.handler).collect();
        if local.len() == 1 {
            rg.graph.add_edge(
                local[0].id.clone(),
                ep_id,
                EdgeKind::Handles,
                Confidence::Extracted,
            );
        } else {
            rg.pending_handlers.push((ep.handler, ep_id));
        }
    }

    // Router composition: `parent` mounts sub-router `child` (both same-file
    // builder fns, so both resolve to local Function nodes).
    let resolve = |name: &str| -> Option<NodeId> {
        let mut hits = symbols.iter().filter(|s| s.name == name);
        match (hits.next(), hits.next()) {
            (Some(s), None) => Some(s.id.clone()),
            _ => None,
        }
    };
    let mut mounts: HashSet<(NodeId, NodeId)> = HashSet::new();
    let mut raw_mounts = Vec::new();
    for ex in &extractors {
        raw_mounts.extend(ex.mounts(source));
    }
    for m in raw_mounts {
        if let (Some(p), Some(c)) = (resolve(&m.parent), resolve(&m.child))
            && mounts.insert((p.clone(), c.clone()))
        {
            rg.graph
                .add_edge(p, c, EdgeKind::Mounts, Confidence::Extracted);
        }
    }

    // Router-variable composition (#128): FastAPI `include_router`, Express
    // `app.use`. Each router variable becomes a synthetic `Router` node (one per
    // file + variable name), linked host -> mounted with `MOUNTS`. Unlike the
    // builder mounts above, the ends are variables, not function symbols, so
    // they resolve to these synthetic nodes rather than local `Function` nodes.
    let mut router_ids: HashSet<NodeId> = HashSet::new();
    let mut router_edges: HashSet<(NodeId, NodeId)> = HashSet::new();
    let mut raw_router_mounts = Vec::new();
    for ex in &extractors {
        raw_router_mounts.extend(ex.router_mounts(source));
    }
    for rm in raw_router_mounts {
        let host_id = NodeId::derive(&[rel_path, "router", &rm.host]);
        let mounted_id = NodeId::derive(&[rel_path, "router", &rm.mounted]);
        for (id, name) in [(&host_id, &rm.host), (&mounted_id, &rm.mounted)] {
            if router_ids.insert(id.clone()) {
                rg.graph.add_node(Node {
                    id: id.clone(),
                    kind: NodeKind::Router,
                    name: name.clone(),
                    qualified_name: name.clone(),
                    file: rel_path.to_string(),
                    language: Some(lang.name().to_string()),
                    span: Some(rm.span),
                    doc: None,
                });
            }
        }
        if router_edges.insert((host_id.clone(), mounted_id.clone())) {
            rg.graph
                .add_edge(host_id, mounted_id, EdgeKind::Mounts, Confidence::Extracted);
        }
    }
    rg
}

/// Build the gRPC endpoint fragment for a file: one `Endpoint` per tonic
/// service method (`Service/method`), with a `HANDLES` edge to the impl method
/// when it resolves to a unique local symbol. gRPC is Rust/tonic-only for now;
/// other languages yield an empty graph. Reuses [`RestGraph`] (endpoints +
/// pending handlers).
pub fn build_grpc_graph(
    rel_path: &str,
    lang: &Language,
    symbols: &[SymbolEntry],
    source: &str,
) -> RestGraph {
    let mut rg = RestGraph::default();
    if *lang != Language::Rust {
        return rg;
    }
    let mut seen: HashSet<NodeId> = HashSet::new();
    for ep in crate::grpc::extract_grpc_endpoints(source) {
        let path = format!("{}/{}", ep.service, ep.method);
        let ep_id = NodeId::derive(&[rel_path, "endpoint", "grpc", &path]);
        if !seen.insert(ep_id.clone()) {
            continue;
        }
        rg.graph.add_node(Node {
            id: ep_id.clone(),
            kind: NodeKind::Endpoint,
            name: format!("grpc {path}"),
            qualified_name: path.clone(),
            file: rel_path.to_string(),
            language: Some(lang.name().to_string()),
            span: Some(ep.span),
            doc: None,
        });
        rg.operations
            .push((ep_id.clone(), OperationKey::opaque(Protocol::Grpc, path)));

        let local: Vec<&SymbolEntry> = symbols.iter().filter(|s| s.name == ep.handler).collect();
        if local.len() == 1 {
            rg.graph.add_edge(
                local[0].id.clone(),
                ep_id,
                EdgeKind::Handles,
                Confidence::Extracted,
            );
        } else {
            rg.pending_handlers.push((ep.handler, ep_id));
        }
    }
    rg
}

/// Build the GraphQL endpoint fragment for a file: one `Endpoint` per
/// async-graphql resolver method (`OpType.field`), `HANDLES`-linked to the
/// resolver when it resolves to a unique local symbol. Rust-only for now.
pub fn build_graphql_graph(
    rel_path: &str,
    lang: &Language,
    symbols: &[SymbolEntry],
    source: &str,
) -> RestGraph {
    let mut rg = RestGraph::default();
    if *lang != Language::Rust {
        return rg;
    }
    let mut seen: HashSet<NodeId> = HashSet::new();
    for f in crate::graphql::extract_graphql_resolvers(source) {
        let path = format!("{}.{}", f.op_type, f.field);
        let ep_id = NodeId::derive(&[rel_path, "endpoint", "graphql", &path]);
        if !seen.insert(ep_id.clone()) {
            continue;
        }
        rg.graph.add_node(Node {
            id: ep_id.clone(),
            kind: NodeKind::Endpoint,
            name: format!("graphql {path}"),
            qualified_name: path.clone(),
            file: rel_path.to_string(),
            language: Some(lang.name().to_string()),
            span: Some(f.span),
            doc: None,
        });
        rg.operations
            .push((ep_id.clone(), OperationKey::opaque(Protocol::GraphQl, path)));

        let local: Vec<&SymbolEntry> = symbols.iter().filter(|s| s.name == f.handler).collect();
        if local.len() == 1 {
            rg.graph.add_edge(
                local[0].id.clone(),
                ep_id,
                EdgeKind::Handles,
                Confidence::Extracted,
            );
        } else {
            rg.pending_handlers.push((f.handler, ep_id));
        }
    }
    rg
}

/// Client-side API call sites extracted from a JS/TS file: `ClientCall` nodes
/// and their operation keys (linked to endpoints later by the matcher).
#[derive(Debug, Default)]
pub struct ClientGraph {
    pub graph: CodeGraph,
    pub operations: Vec<(NodeId, OperationKey)>,
}

/// `Calls` edges from the function/method that encloses each client call site to
/// the `ClientCall` node (#130). This lets impact traversal continue past a
/// reached `ClientCall` to the enclosing symbol and on to its in-process callers
/// across the wire. `symbols` are the file's symbol nodes (only `Function` /
/// `Method` with a span participate); `client_calls` are the file's `ClientCall`
/// nodes. The innermost (smallest) span that contains the call wins.
pub fn enclosing_call_edges(symbols: &[Node], client_calls: &[Node]) -> Vec<Edge> {
    let fns: Vec<(&NodeId, Span)> = symbols
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method))
        .filter_map(|n| n.span.map(|s| (&n.id, s)))
        .collect();
    let mut edges = Vec::new();
    for cc in client_calls {
        let Some(cs) = cc.span else {
            continue;
        };
        let enclosing = fns
            .iter()
            .filter(|(_, s)| s.start_byte <= cs.start_byte && cs.end_byte <= s.end_byte)
            .min_by_key(|(_, s)| s.end_byte - s.start_byte);
        if let Some((id, _)) = enclosing {
            edges.push(Edge {
                src: (*id).clone(),
                dst: cc.id.clone(),
                kind: EdgeKind::Calls,
                confidence: Confidence::Extracted,
            });
        }
    }
    edges
}

/// Build the gRPC client-call fragment for a file: each tonic stub call
/// (`client.method(req)`) becomes a `ClientCall` keyed `Service/method`, linked
/// to the server endpoint / proto op by the matcher (INVOKES). Rust-only.
pub fn build_grpc_client_graph(rel_path: &str, source: &str, lang: &Language) -> ClientGraph {
    let mut cg = ClientGraph::default();
    if *lang != Language::Rust {
        return cg;
    }
    for call in crate::grpc::extract_grpc_client_calls(source) {
        let path = format!("{}/{}", call.service, call.method);
        let id = NodeId::derive(&[
            rel_path,
            "client_call",
            "grpc",
            &path,
            &call.span.start_byte.to_string(),
        ]);
        cg.graph.add_node(Node {
            id: id.clone(),
            kind: NodeKind::ClientCall,
            name: format!("grpc {path}"),
            qualified_name: path.clone(),
            file: rel_path.to_string(),
            language: Some(lang.name().to_string()),
            span: Some(call.span),
            doc: None,
        });
        cg.operations
            .push((id, OperationKey::opaque(Protocol::Grpc, path)));
    }
    cg
}

/// Build the GraphQL client-op fragment for a JS/TS file: each `gql`/`graphql`
/// document becomes a `ClientCall` keyed `OpType.field`, linked to a resolver
/// endpoint / schema op by the matcher (INVOKES) via the GraphQL signature.
pub fn build_graphql_client_graph(rel_path: &str, source: &str, lang: &Language) -> ClientGraph {
    let mut cg = ClientGraph::default();
    for op in crate::graphql::extract_graphql_client_ops(source, lang) {
        let path = format!("{}.{}", op.op_type, op.field);
        let id = NodeId::derive(&[
            rel_path,
            "client_call",
            "graphql",
            &path,
            &op.span.start_byte.to_string(),
        ]);
        cg.graph.add_node(Node {
            id: id.clone(),
            kind: NodeKind::ClientCall,
            name: format!("graphql {path}"),
            qualified_name: path.clone(),
            file: rel_path.to_string(),
            language: Some(lang.name().to_string()),
            span: Some(op.span),
            doc: None,
        });
        cg.operations
            .push((id, OperationKey::opaque(Protocol::GraphQl, path)));
    }
    cg
}

/// Build the client-call fragment for a JS/TS/TSX file. Each `fetch`/`axios`
/// call becomes a `ClientCall` node carrying its `(method, path)` operation key.
pub fn build_client_graph(rel_path: &str, source: &str, lang: &Language) -> ClientGraph {
    let mut cg = ClientGraph::default();
    for call in extract_client_calls(source, lang) {
        let key = OperationKey::rest(call.method, &call.path);
        let method = call.method.as_str();
        // Each call site is a distinct node; the byte offset disambiguates
        // repeated identical calls within a file.
        let id = NodeId::derive(&[
            rel_path,
            "client_call",
            method,
            &key.path,
            &call.span.start_byte.to_string(),
        ]);
        cg.graph.add_node(Node {
            id: id.clone(),
            kind: NodeKind::ClientCall,
            name: format!("{method} {}", key.path),
            qualified_name: key.path.clone(),
            file: rel_path.to_string(),
            language: Some(lang.name().to_string()),
            span: Some(call.span),
            doc: None,
        });
        cg.operations.push((id, key));
    }
    cg
}

const MARKERS: [&str; 5] = ["NOTE", "WHY", "HACK", "TODO", "FIXME"];

fn has_marker(text: &str) -> bool {
    let upper = text.to_uppercase();
    MARKERS.iter().any(|m| upper.contains(m))
}

fn type_like(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class | NodeKind::Struct | NodeKind::Trait | NodeKind::Interface | NodeKind::Enum
    )
}

fn contains(outer: &Span, inner: &Span) -> bool {
    outer.start_byte <= inner.start_byte
        && outer.end_byte >= inner.end_byte
        && !(outer.start_byte == inner.start_byte && outer.end_byte == inner.end_byte)
}

struct DefInfo {
    kind: NodeKind,
    name: String,
    span: Span,
    parent: Option<usize>,
    id: NodeId,
    qualified: String,
}

/// Find the index of the smallest definition whose span encloses `byte`.
fn enclosing_def(defs: &[DefInfo], byte: usize) -> Option<usize> {
    let probe = Span {
        start_byte: byte,
        end_byte: byte,
        ..Default::default()
    };
    defs.iter()
        .enumerate()
        .filter(|(_, d)| contains(&d.span, &probe))
        .min_by_key(|(_, d)| d.span.end_byte - d.span.start_byte)
        .map(|(i, _)| i)
}

/// Doc comment text: contiguous comments immediately preceding `start_line`.
fn doc_for(parsed: &ParsedFile, start_line: usize) -> Option<String> {
    let mut lines = Vec::new();
    let mut want = start_line.checked_sub(1)?;
    while let Some(c) = parsed.comments.iter().find(|c| c.end_line() == want) {
        lines.push(c.text.clone());
        if want <= 1 {
            break;
        }
        want = c.span.start_line - 1;
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n"))
}

impl crate::extract::RawComment {
    fn end_line(&self) -> usize {
        self.span.end_line
    }
}

/// Build a file-scoped graph fragment from parsed items.
pub fn build_file_graph(
    rel_path: &str,
    lang: &Language,
    file_id: &NodeId,
    parsed: &ParsedFile,
) -> FileGraph {
    let mut fg = FileGraph::default();

    // Establish parent relationships by span nesting.
    let raw = &parsed.defs;
    let mut parents: Vec<Option<usize>> = vec![None; raw.len()];
    for (i, d) in raw.iter().enumerate() {
        let mut best: Option<(usize, usize)> = None; // (idx, span_len)
        for (j, other) in raw.iter().enumerate() {
            if i == j {
                continue;
            }
            if contains(&other.span, &d.span) {
                let len = other.span.end_byte - other.span.start_byte;
                if best.is_none_or(|(_, bl)| len < bl) {
                    best = Some((j, len));
                }
            }
        }
        parents[i] = best.map(|(j, _)| j);
    }

    // Reclassify functions nested in a type as methods.
    let kinds: Vec<NodeKind> = raw
        .iter()
        .enumerate()
        .map(|(i, d)| match (d.kind, parents[i]) {
            (NodeKind::Function, Some(p)) if type_like(raw[p].kind) => NodeKind::Method,
            (k, _) => k,
        })
        .collect();

    // Compute qualified names (memoized by walking the parent chain).
    fn qualify(
        i: usize,
        raw: &[RawDef],
        parents: &[Option<usize>],
        memo: &mut [Option<String>],
    ) -> String {
        if let Some(q) = &memo[i] {
            return q.clone();
        }
        let q = match parents[i] {
            Some(p) => format!("{}::{}", qualify(p, raw, parents, memo), raw[i].name),
            None => raw[i].name.clone(),
        };
        memo[i] = Some(q.clone());
        q
    }
    let mut memo: Vec<Option<String>> = vec![None; raw.len()];
    let qualified: Vec<String> = (0..raw.len())
        .map(|i| qualify(i, raw, &parents, &mut memo))
        .collect();

    // Materialize definition nodes.
    let mut defs: Vec<DefInfo> = Vec::with_capacity(raw.len());
    for (i, d) in raw.iter().enumerate() {
        let kind = kinds[i];
        let id = NodeId::derive(&[rel_path, &qualified[i], kind.as_str()]);
        defs.push(DefInfo {
            kind,
            name: d.name.clone(),
            span: d.span,
            parent: parents[i],
            id,
            qualified: qualified[i].clone(),
        });
    }

    for (i, d) in defs.iter().enumerate() {
        let doc = doc_for(parsed, d.span.start_line);
        fg.graph.add_node(Node {
            id: d.id.clone(),
            kind: d.kind,
            name: d.name.clone(),
            qualified_name: d.qualified.clone(),
            file: rel_path.to_string(),
            language: Some(lang.name().to_string()),
            span: Some(d.span),
            doc,
        });
        fg.symbols.push(SymbolEntry {
            name: d.name.clone(),
            id: d.id.clone(),
            kind: d.kind,
        });
        // Structural containment.
        let parent_id = match d.parent {
            Some(p) => defs[p].id.clone(),
            None => file_id.clone(),
        };
        fg.graph.add_edge(
            parent_id,
            d.id.clone(),
            EdgeKind::Contains,
            Confidence::Extracted,
        );
        let _ = i;
    }

    // Calls: resolve to a unique local definition, else defer to the global pass.
    for r in &parsed.calls {
        let caller = enclosing_def(&defs, r.byte)
            .map(|i| defs[i].id.clone())
            .unwrap_or_else(|| file_id.clone());
        let local: Vec<&DefInfo> = defs.iter().filter(|d| d.name == r.name).collect();
        if local.len() == 1 {
            fg.graph.add_edge(
                caller,
                local[0].id.clone(),
                EdgeKind::Calls,
                Confidence::Extracted,
            );
        } else {
            fg.pending.push(PendingEdge {
                src: caller,
                name: r.name.clone(),
                kind: EdgeKind::Calls,
            });
        }
    }

    // Inheritance: attach to the enclosing type definition.
    for b in &parsed.bases {
        let Some(src_idx) = enclosing_def(&defs, b.byte) else {
            continue;
        };
        let src = defs[src_idx].id.clone();
        let local: Vec<&DefInfo> = defs.iter().filter(|d| d.name == b.name).collect();
        if local.len() == 1 {
            fg.graph
                .add_edge(src, local[0].id.clone(), b.kind, Confidence::Extracted);
        } else {
            fg.pending.push(PendingEdge {
                src,
                name: b.name.clone(),
                kind: b.kind,
            });
        }
    }

    // Imports are resolved globally (against the whole file set) in `analyze`,
    // so they can point at real file/module targets and stay correct under
    // incremental updates. Record the raw targets here.
    fg.imports.extend(parsed.imports.iter().cloned());

    // Design-rationale comments become first-class nodes linked to their scope.
    for c in &parsed.comments {
        if !has_marker(&c.text) {
            continue;
        }
        let scope = enclosing_def(&defs, c.span.start_byte)
            .map(|i| defs[i].id.clone())
            .unwrap_or_else(|| file_id.clone());
        let cid = NodeId::derive(&[rel_path, "comment", &c.span.start_byte.to_string()]);
        fg.graph.add_node(Node {
            id: cid.clone(),
            kind: NodeKind::Comment,
            name: c.text.chars().take(60).collect(),
            qualified_name: String::new(),
            file: rel_path.to_string(),
            language: Some(lang.name().to_string()),
            span: Some(c.span),
            doc: Some(c.text.clone()),
        });
        fg.graph
            .add_edge(cid, scope, EdgeKind::Documents, Confidence::Extracted);
    }

    fg
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn node(id: &str, kind: NodeKind, start_byte: usize, end_byte: usize) -> Node {
        Node {
            id: NodeId(id.into()),
            kind,
            name: id.into(),
            qualified_name: id.into(),
            file: "f".into(),
            language: None,
            span: Some(Span {
                start_byte,
                end_byte,
                start_line: 0,
                end_line: 0,
            }),
            doc: None,
        }
    }

    // #130: a client call links to the innermost function/method that contains
    // its byte span.
    #[test]
    fn links_client_call_to_innermost_enclosing_fn() {
        let outer = node("outer", NodeKind::Function, 0, 100);
        let inner = node("inner", NodeKind::Method, 20, 60);
        let cc = node("cc", NodeKind::ClientCall, 30, 40);
        let edges = enclosing_call_edges(&[outer, inner], &[cc]);
        check!(edges.len() == 1);
        check!(edges[0].src == NodeId("inner".into()));
        check!(edges[0].dst == NodeId("cc".into()));
        check!(edges[0].kind == EdgeKind::Calls);
        check!(edges[0].confidence == Confidence::Extracted);
    }

    // A call site outside every function span yields no edge.
    #[test]
    fn no_edge_when_call_outside_any_fn() {
        let f = node("f", NodeKind::Function, 0, 10);
        let cc = node("cc", NodeKind::ClientCall, 50, 60);
        check!(enclosing_call_edges(&[f], &[cc]).is_empty());
    }
}
