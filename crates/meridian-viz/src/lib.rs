#![forbid(unsafe_code)]

use std::collections::HashMap;

use meridian_core::{Edge, ImpactReport, Node, NodeId, OperationKey};
use serde_json::{Value, json};

pub const TEMPLATE: &str = include_str!("../assets/index.html");

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
    let item_by_id: HashMap<&str, &meridian_core::ClassifiedItem> = impact
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
        "<script>window.MERIDIAN_DATA = {};",
        serde_json::to_string(&data).unwrap_or_else(|_| "[]".into())
    );
    if let Some(s) = impact {
        inject.push_str(&format!(
            "window.MERIDIAN_IMPACT = {};",
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
