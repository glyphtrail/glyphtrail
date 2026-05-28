#![forbid(unsafe_code)]

pub mod sqlite;

pub use sqlite::{SqliteStore, Stats};

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use meridian_core::{Confidence, Edge, EdgeKind, Node, NodeId, NodeKind, PendingLink, Span};

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
        check!(s.nodes == 2);
        check!(s.edges == 1);

        // callee's incoming Calls neighbour is the caller.
        let callers = store.neighbors("b", Some(EdgeKind::Calls), false).unwrap();
        check!(callers.len() == 1);
        check!(callers[0].0.name == "caller");

        // FTS finds by name.
        check!(store.search("callee", 10).unwrap().len() == 1);

        // Reachability: who is impacted if b changes -> a.
        let impacted = store.reachable("b", EdgeKind::Calls, false, 5).unwrap();
        check!(impacted.len() == 1);
        check!(impacted[0].name == "caller");
    }

    #[test]
    fn meta_and_file_hashes_roundtrip() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        check!(store.get_meta("tool_version").unwrap() == None);
        store.set_meta("tool_version", "9.9.9").unwrap();
        store.set_meta("tool_version", "1.2.3").unwrap(); // upsert
        check!(store.get_meta("tool_version").unwrap().as_deref() == Some("1.2.3"));

        store.set_file("a.rs", Some("rust"), "h1").unwrap();
        store.set_file("b.py", Some("python"), "h2").unwrap();
        let mut got = store.files_with_hashes().unwrap();
        got.sort();
        check!(got == vec![("a.rs".into(), "h1".into()), ("b.py".into(), "h2".into())]);
    }

    #[test]
    fn api_operations_persist_and_filter_by_kind() {
        use meridian_core::{HttpMethod, OperationKey};

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
        check!(endpoints.len() == 1);
        check!(endpoints[0].0 == NodeId("e1".into()));
        check!(endpoints[0].1.path == "/api/users/{id}");
        check!(endpoints[0].1.method == Some(HttpMethod::Get));

        let calls = store.operations_by_kind(NodeKind::ClientCall).unwrap();
        check!(calls.len() == 1);
        check!(calls[0].0 == NodeId("c1".into()));

        // all_operations returns every row regardless of node kind.
        let all = store.all_operations().unwrap();
        check!(all.len() == 2);
        check!(all.iter().any(|(id, _)| id == &NodeId("e1".into())));
        check!(all.iter().any(|(id, _)| id == &NodeId("c1".into())));

        // Incremental re-index of the endpoint's file drops its operation row.
        store.delete_file_data("routes.rs").unwrap();
        check!(
            store
                .operations_by_kind(NodeKind::Endpoint)
                .unwrap()
                .is_empty()
        );
        check!(
            store
                .operations_by_kind(NodeKind::ClientCall)
                .unwrap()
                .len()
                == 1
        );
    }

    #[test]
    fn pending_edges_persist_and_clear_with_their_anchor_file() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let caller = node("caller", "use_it", NodeKind::Function); // file a.rs
        store.insert_graph(&[caller], &[]).unwrap();
        let link = PendingLink {
            anchor: NodeId("caller".into()),
            name: "foo".into(),
            kind: EdgeKind::Calls,
            name_is_src: false,
        };
        store.insert_pending(std::slice::from_ref(&link)).unwrap();
        check!(store.all_pending().unwrap() == vec![link.clone()]);
        // Re-inserting the same link is idempotent.
        store.insert_pending(&[link]).unwrap();
        check!(store.all_pending().unwrap().len() == 1);
        // Deleting the anchor's file drops its pending rows.
        store.delete_file_data("a.rs").unwrap();
        check!(store.all_pending().unwrap().is_empty());
    }

    #[test]
    fn delete_edges_by_confidence_removes_only_that_confidence() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store
            .insert_graph(
                &[
                    node("a", "a", NodeKind::Function),
                    node("b", "b", NodeKind::Function),
                ],
                &[
                    Edge {
                        src: NodeId("a".into()),
                        dst: NodeId("b".into()),
                        kind: EdgeKind::Calls,
                        confidence: Confidence::Inferred,
                    },
                    Edge {
                        src: NodeId("b".into()),
                        dst: NodeId("a".into()),
                        kind: EdgeKind::Calls,
                        confidence: Confidence::Extracted,
                    },
                ],
            )
            .unwrap();
        check!(store.stats().unwrap().edges == 2);
        check!(
            store
                .delete_edges_by_confidence(Confidence::Inferred)
                .unwrap()
                == 1
        );
        check!(store.stats().unwrap().edges == 1);
    }

    #[test]
    fn delete_nodes_by_kind_removes_nodes_edges_and_ops() {
        use meridian_core::{HttpMethod, OperationKey};

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
        check!(
            store
                .operations_by_kind(NodeKind::SchemaOp)
                .unwrap()
                .is_empty()
        );
        check!(store.get_node("s1").unwrap().is_none());
        check!(store.get_node("e1").unwrap().is_some());
        check!(store.stats().unwrap().edges == 0);
    }
}
