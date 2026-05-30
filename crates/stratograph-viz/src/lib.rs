#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use stratograph_core::{Edge, ImpactReport, Node, NodeId, OperationKey};

pub const TEMPLATE: &str = include_str!("../assets/index.html");

/// A server-side view selection for the interactive explorer (#194): which node
/// and edge kinds to include and how many nodes to cap, so large graphs are
/// trimmed before they reach the browser instead of overwhelming the layout.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    /// Node kinds to keep (`NodeKind::as_str` values). `None` keeps every kind.
    pub kinds: Option<HashSet<String>>,
    /// Edge kinds to keep (`EdgeKind::as_str` values). `None` keeps every kind.
    pub edge_kinds: Option<HashSet<String>>,
    /// Maximum number of nodes to keep (input order preserved). `0` is unlimited.
    pub limit: usize,
}

/// Trim `nodes`/`edges` to a [`Selection`]: drop nodes whose kind is excluded,
/// cap the node count, then keep only edges whose kind is included *and* whose
/// endpoints both survived — so the result never references a dropped node.
pub fn select_graph(nodes: &[Node], edges: &[Edge], sel: &Selection) -> (Vec<Node>, Vec<Edge>) {
    let mut kept: Vec<Node> = nodes
        .iter()
        .filter(|n| {
            sel.kinds
                .as_ref()
                .is_none_or(|k| k.contains(n.kind.as_str()))
        })
        .cloned()
        .collect();
    if sel.limit > 0 && kept.len() > sel.limit {
        kept.truncate(sel.limit);
    }
    let ids: HashSet<&str> = kept.iter().map(|n| n.id.0.as_str()).collect();
    let kept_edges: Vec<Edge> = edges
        .iter()
        .filter(|e| {
            sel.edge_kinds
                .as_ref()
                .is_none_or(|k| k.contains(e.kind.as_str()))
                && ids.contains(e.src.0.as_str())
                && ids.contains(e.dst.0.as_str())
        })
        .cloned()
        .collect();
    (kept, kept_edges)
}

/// An impact overlay for the graph: which nodes are seeds and how each impacted
/// node was reached, so the frontend can highlight the blast radius.
pub struct ImpactView<'a> {
    pub seeds: &'a [String],
    pub report: &'a ImpactReport,
}

/// Convert nodes and edges into the Cytoscape `elements` JSON format. API
/// operations (keyed by node id) attach `protocol`/`method`/`path` to their
/// node so the frontend can color cross-boundary edges per protocol and show
/// an endpoint detail panel. When `impact` is set, seed and impacted nodes are
/// annotated for blast-radius highlighting.
pub fn to_elements(
    nodes: &[Node],
    edges: &[Edge],
    ops: &[(NodeId, OperationKey)],
    impact: Option<&ImpactView>,
) -> Value {
    let op_by_id: HashMap<&str, &OperationKey> =
        ops.iter().map(|(id, key)| (id.0.as_str(), key)).collect();
    let seed_set: std::collections::HashSet<&str> = impact
        .map(|v| v.seeds.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let item_by_id: HashMap<&str, &stratograph_core::ClassifiedItem> = impact
        .map(|v| v.report.items.iter().map(|i| (i.id.as_str(), i)).collect())
        .unwrap_or_default();

    let mut elements = Vec::with_capacity(nodes.len() + edges.len());
    for n in nodes {
        let mut data = json!({
            "id": n.id.0,
            "label": n.name,
            "kind": n.kind.as_str(),
            "qualified_name": n.qualified_name,
            "file": n.file,
            "language": n.language,
            "line": n.span.map(|s| s.start_line),
            "doc": n.doc,
        });
        let obj = data.as_object_mut().expect("node data is an object");
        if let Some(key) = op_by_id.get(n.id.0.as_str()) {
            obj.insert("protocol".into(), json!(key.protocol.as_str()));
            obj.insert("method".into(), json!(key.method.map(|m| m.as_str())));
            obj.insert("path".into(), json!(key.path));
        }
        if impact.is_some() {
            if seed_set.contains(n.id.0.as_str()) {
                obj.insert("seed".into(), json!(true));
            }
            if let Some(item) = item_by_id.get(n.id.0.as_str()) {
                obj.insert("impact_distance".into(), json!(item.distance));
                obj.insert("impact_conf".into(), json!(item.min_confidence.as_str()));
                obj.insert("impact_cross".into(), json!(item.cross_boundary));
                obj.insert(
                    "impact_class".into(),
                    json!(serde_json::to_value(item.class).unwrap_or(Value::Null)),
                );
                obj.insert("impact_path".into(), json!(item.path));
            }
        }
        elements.push(json!({ "data": data }));
    }
    for (i, e) in edges.iter().enumerate() {
        elements.push(json!({
            "data": {
                "id": format!("e{i}"),
                "source": e.src.0,
                "target": e.dst.0,
                "kind": e.kind.as_str(),
                "confidence": e.confidence.as_str(),
            }
        }));
    }
    Value::Array(elements)
}

/// Render a self-contained HTML page with the graph data inlined.
pub fn static_html(nodes: &[Node], edges: &[Edge], ops: &[(NodeId, OperationKey)]) -> String {
    render(to_elements(nodes, edges, ops, None), None)
}

/// Render a self-contained page highlighting the impact blast radius of a seed.
pub fn static_html_impact(
    nodes: &[Node],
    edges: &[Edge],
    ops: &[(NodeId, OperationKey)],
    view: &ImpactView,
) -> String {
    let data = to_elements(nodes, edges, ops, Some(view));
    let summary = json!({
        "summary": view.report.summary,
        "headline": view.report.headline(),
        "seeds": view.seeds,
    });
    render(data, Some(summary))
}

fn render(data: Value, impact: Option<Value>) -> String {
    let mut inject = format!(
        "<script>window.STRATOGRAPH_DATA = {};",
        serde_json::to_string(&data).unwrap_or_else(|_| "[]".into())
    );
    if let Some(s) = impact {
        inject.push_str(&format!(
            "window.STRATOGRAPH_IMPACT = {};",
            serde_json::to_string(&s).unwrap_or_else(|_| "null".into())
        ));
    }
    inject.push_str("</script>");
    // Inject the data just before the application script tag.
    TEMPLATE.replacen(
        "<script>\nconst KIND_COLOR",
        &format!("{inject}\n<script>\nconst KIND_COLOR"),
        1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use stratograph_core::{Confidence, EdgeKind, NodeKind};

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: NodeId(id.into()),
            kind,
            name: id.into(),
            qualified_name: id.into(),
            file: "a.rs".into(),
            language: Some("rust".into()),
            span: None,
            doc: None,
        }
    }
    fn edge(src: &str, dst: &str, kind: EdgeKind) -> Edge {
        Edge {
            src: NodeId(src.into()),
            dst: NodeId(dst.into()),
            kind,
            confidence: Confidence::Extracted,
        }
    }

    fn set(items: &[&str]) -> Option<HashSet<String>> {
        Some(items.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn none_selection_keeps_everything() {
        let nodes = [node("a", NodeKind::Function), node("b", NodeKind::Comment)];
        let edges = [edge("a", "b", EdgeKind::Documents)];
        let (n, e) = select_graph(&nodes, &edges, &Selection::default());
        check!(n.len() == 2);
        check!(e.len() == 1);
    }

    #[test]
    fn empty_kind_set_keeps_nothing() {
        // An explicit empty selection (every toolbar box off) must keep no nodes,
        // unlike `None` which keeps all.
        let nodes = [node("a", NodeKind::Function)];
        let edges = [edge("a", "a", EdgeKind::Calls)];
        let sel = Selection {
            kinds: Some(HashSet::new()),
            ..Default::default()
        };
        let (n, e) = select_graph(&nodes, &edges, &sel);
        check!(n.is_empty());
        check!(e.is_empty());
    }

    #[test]
    fn kind_filter_drops_nodes_and_their_edges() {
        let nodes = [node("a", NodeKind::Function), node("b", NodeKind::Comment)];
        let edges = [edge("a", "b", EdgeKind::Documents)];
        let sel = Selection {
            kinds: set(&["function"]),
            ..Default::default()
        };
        let (n, e) = select_graph(&nodes, &edges, &sel);
        check!(n.len() == 1 && n[0].id.0 == "a");
        // The edge to the dropped comment node is gone (no dangling endpoints).
        check!(e.is_empty());
    }

    #[test]
    fn edge_kind_filter_keeps_only_selected_edges() {
        let nodes = [node("a", NodeKind::Function), node("b", NodeKind::Function)];
        let edges = [
            edge("a", "b", EdgeKind::Calls),
            edge("a", "b", EdgeKind::Imports),
        ];
        let sel = Selection {
            edge_kinds: set(&["calls"]),
            ..Default::default()
        };
        let (_, e) = select_graph(&nodes, &edges, &sel);
        check!(e.len() == 1 && e[0].kind == EdgeKind::Calls);
    }

    #[test]
    fn limit_caps_nodes_and_prunes_now_dangling_edges() {
        let nodes = [
            node("a", NodeKind::Function),
            node("b", NodeKind::Function),
            node("c", NodeKind::Function),
        ];
        let edges = [
            edge("a", "b", EdgeKind::Calls),
            edge("a", "c", EdgeKind::Calls),
        ];
        let sel = Selection {
            limit: 2,
            ..Default::default()
        };
        let (n, e) = select_graph(&nodes, &edges, &sel);
        check!(n.len() == 2);
        // a->c is dropped because c was truncated; a->b survives.
        check!(e.len() == 1 && e[0].dst.0 == "b");
    }
}
