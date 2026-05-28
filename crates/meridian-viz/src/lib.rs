#![forbid(unsafe_code)]

use meridian_core::{Edge, Node};
use serde_json::{Value, json};

pub const TEMPLATE: &str = include_str!("../assets/index.html");

/// Convert nodes and edges into the Cytoscape `elements` JSON format.
pub fn to_elements(nodes: &[Node], edges: &[Edge]) -> Value {
    let mut elements = Vec::with_capacity(nodes.len() + edges.len());
    for n in nodes {
        elements.push(json!({
            "data": {
                "id": n.id.0,
                "label": n.name,
                "kind": n.kind.as_str(),
                "qualified_name": n.qualified_name,
                "file": n.file,
                "language": n.language,
                "line": n.span.map(|s| s.start_line),
                "doc": n.doc,
            }
        }));
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
pub fn static_html(nodes: &[Node], edges: &[Edge]) -> String {
    let data = to_elements(nodes, edges);
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
