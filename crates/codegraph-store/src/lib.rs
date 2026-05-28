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

    #[test]
    fn api_operations_persist_and_filter_by_kind() {
        use codegraph_core::{HttpMethod, OperationKey};

        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut endpoint = node("e1", "get_user", NodeKind::Endpoint);
        endpoint.file = "routes.rs".into();
        let mut client = node("c1", "fetchUser", NodeKind::ClientCall);
        client.file = "client.ts".into();
        store.insert_graph(&[endpoint, client], &[]).unwrap();
        store
            .insert_operations(&[
                (
                    NodeId("e1".into()),
                    OperationKey::rest(HttpMethod::Get, "/api/users/{id}"),
                ),
                (
                    NodeId("c1".into()),
                    OperationKey::rest(HttpMethod::Get, "/users/123"),
                ),
            ])
            .unwrap();

        let endpoints = store.operations_by_kind(NodeKind::Endpoint).unwrap();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].0, NodeId("e1".into()));
        assert_eq!(endpoints[0].1.path, "/api/users/{id}");
        assert_eq!(endpoints[0].1.method, Some(HttpMethod::Get));

        let calls = store.operations_by_kind(NodeKind::ClientCall).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, NodeId("c1".into()));

        // Incremental re-index of the endpoint's file drops its operation row.
        store.delete_file_data("routes.rs").unwrap();
        assert!(store.operations_by_kind(NodeKind::Endpoint).unwrap().is_empty());
        assert_eq!(store.operations_by_kind(NodeKind::ClientCall).unwrap().len(), 1);
    }

    #[test]
    fn delete_nodes_by_kind_removes_nodes_edges_and_ops() {
        use codegraph_core::{HttpMethod, OperationKey};

        let mut store = SqliteStore::open_in_memory().unwrap();
        let endpoint = node("e1", "get_user", NodeKind::Endpoint);
        let schema_op = node("s1", "GET /users", NodeKind::SchemaOp);
        let exposes = Edge {
            src: NodeId("e1".into()),
            dst: NodeId("s1".into()),
            kind: EdgeKind::Exposes,
            confidence: Confidence::Extracted,
        };
        store
            .insert_graph(&[endpoint, schema_op], &[exposes])
            .unwrap();
        store
            .insert_operations(&[(
                NodeId("s1".into()),
                OperationKey::rest(HttpMethod::Get, "/users"),
            )])
            .unwrap();

        store.delete_nodes_by_kind(NodeKind::SchemaOp).unwrap();

        // The schema op node, its operation row and the EXPOSES edge are gone;
        // the endpoint (a different kind) is untouched.
        assert!(store
            .operations_by_kind(NodeKind::SchemaOp)
            .unwrap()
            .is_empty());
        assert!(store.get_node("s1").unwrap().is_none());
        assert!(store.get_node("e1").unwrap().is_some());
        assert_eq!(store.stats().unwrap().edges, 0);
    }
}
