pub mod sqlite;

pub use sqlite::{SqliteStore, Stats};

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_core::{Confidence, Edge, EdgeKind, Node, NodeId, NodeKind, Span};

    fn node(id: &str, name: &str, kind: NodeKind) -> Node {
        Node {
            id: NodeId(id.into()),
            kind,
            name: name.into(),
            qualified_name: name.into(),
            file: "a.rs".into(),
            language: Some("rust".into()),
            span: Some(Span {
                start_byte: 0,
                end_byte: 1,
                start_line: 1,
                end_line: 2,
            }),
            doc: None,
        }
    }

    #[test]
    fn roundtrip_and_traversal() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let nodes = vec![
            node("a", "caller", NodeKind::Function),
            node("b", "callee", NodeKind::Function),
        ];
        let edges = vec![Edge {
            src: NodeId("a".into()),
            dst: NodeId("b".into()),
            kind: EdgeKind::Calls,
            confidence: Confidence::Extracted,
        }];
        store.insert_graph(&nodes, &edges).unwrap();

        let s = store.stats().unwrap();
        assert_eq!(s.nodes, 2);
        assert_eq!(s.edges, 1);

        // callee's incoming Calls neighbour is the caller.
        let callers = store.neighbors("b", Some(EdgeKind::Calls), false).unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].0.name, "caller");

        // FTS finds by name.
        assert_eq!(store.search("callee", 10).unwrap().len(), 1);

        // Reachability: who is impacted if b changes -> a.
        let impacted = store.reachable("b", EdgeKind::Calls, false, 5).unwrap();
        assert_eq!(impacted.len(), 1);
        assert_eq!(impacted[0].name, "caller");
    }
}
