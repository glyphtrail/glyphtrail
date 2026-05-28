#![forbid(unsafe_code)]

use std::collections::HashMap;

use meridian_core::{Edge, Node, NodeId, OperationKey};
use serde_json::{Value, json};

pub const TEMPLATE: &str = include_str!("../assets/index.html");

/// Convert nodes and edges into the Cytoscape `elements` JSON format. API
/// operations (keyed by node id) attach `protocol`/`method`/`path` to their
/// node so the frontend can color cross-boundary edges per protocol and show
/// an endpoint detail panel.
pub fn to_elements(nodes: &[Node], edges: &[Edge], ops: &[(NodeId, OperationKey)]) -> Value {
    let op_by_id: HashMap<&str, &OperationKey> =
        ops.iter().map(|(id, key)| (id.0.as_str(), key)).collect();
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
        if let Some(key) = op_by_id.get(n.id.0.as_str()) {
            let obj = data.as_object_mut().expect("node data is an object");
            obj.insert("protocol".into(), json!(key.protocol.as_str()));
            obj.insert("method".into(), json!(key.method.map(|m| m.as_str())));
            obj.insert("path".into(), json!(key.path));
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
    let data = to_elements(nodes, edges, ops);
    let inject = format!(
        "<script>window.MERIDIAN_DATA = {};</script>",
        serde_json::to_string(&data).unwrap_or_else(|_| "[]".into())
    );
    // Inject the data just before the application script tag.
    TEMPLATE.replacen(
        "<script>\nconst KIND_COLOR",
        &format!("{inject}\n<script>\nconst KIND_COLOR"),
        1,
    )
}
