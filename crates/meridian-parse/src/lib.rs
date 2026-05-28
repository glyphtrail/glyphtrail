pub mod build;
pub mod client;
pub mod extract;
pub mod registry;
pub mod rest;

pub use build::{
    build_client_graph, build_file_graph, build_rest_graph, ClientGraph, FileGraph, PendingEdge,
    RestGraph, SymbolEntry,
};
pub use client::{extract_client_calls, RawClientCall};
pub use extract::{parse_source, ParsedFile};
pub use rest::{
    extract_axum, extract_axum_mounts, extract_utoipa, extract_utoipa_mounts, RawEndpoint, RawMount,
};

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_core::Language;
    use tree_sitter::Query;

    #[test]
    fn all_queries_compile() {
        for lang in Language::ALL {
            let grammar = registry::grammar(lang);
            let src = registry::query_source(lang);
            Query::new(&grammar, src)
                .unwrap_or_else(|e| panic!("query for {} failed: {e}", lang.name()));
        }
    }

    #[test]
    fn extracts_rust_function_and_call() {
        let src = r#"
// WHY: keep it simple
fn helper() {}
fn main() {
    helper();
}
"#;
        let parsed = parse_source(Language::Rust, src).unwrap();
        let names: Vec<_> = parsed.defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"helper"));
        assert!(names.contains(&"main"));
        assert!(parsed.calls.iter().any(|c| c.name == "helper"));
        assert!(parsed.comments.iter().any(|c| c.text.contains("WHY")));
    }

    #[test]
    fn extracts_definitions_per_language() {
        let cases = [
            (Language::Python, "class Foo:\n    def bar(self):\n        pass\n", "bar"),
            (Language::Go, "package m\nfunc Run() {}\n", "Run"),
            (Language::Java, "class A { void go() {} }", "go"),
            (Language::TypeScript, "function f(): void {}\n", "f"),
            (Language::C, "int add(int a) { return a; }\n", "add"),
            (Language::Cpp, "struct S {}; int main() { return 0; }\n", "main"),
            (Language::JavaScript, "function hi() {}\n", "hi"),
        ];
        for (lang, src, expect) in cases {
            let parsed = parse_source(lang, src).unwrap();
            let names: Vec<_> = parsed.defs.iter().map(|d| d.name.as_str()).collect();
            assert!(
                names.contains(&expect),
                "{}: expected def '{expect}', got {names:?}",
                lang.name()
            );
        }
    }

    #[test]
    fn builds_graph_with_methods_and_docs() {
        use meridian_core::{NodeId, NodeKind};
        let src = r#"
class Service:
    # NOTE: this is the entrypoint
    def handle(self):
        self.helper()

    def helper(self):
        pass
"#;
        let parsed = parse_source(Language::Python, src).unwrap();
        let file_id = NodeId::derive(&["file", "svc.py"]);
        let fg = build_file_graph("svc.py", Language::Python, &file_id, &parsed);

        // Methods nested in a class are reclassified from function to method.
        let handle = fg
            .graph
            .nodes
            .iter()
            .find(|n| n.name == "handle")
            .expect("handle node");
        assert_eq!(handle.kind, NodeKind::Method);
        assert_eq!(handle.qualified_name, "Service::handle");

        // The NOTE marker becomes a comment node.
        assert!(fg.graph.nodes.iter().any(|n| n.kind == NodeKind::Comment));
        // handle() calls helper() within the same file -> resolved locally.
        let helper_id = &fg
            .graph
            .nodes
            .iter()
            .find(|n| n.name == "helper")
            .unwrap()
            .id;
        assert!(fg
            .graph
            .edges
            .iter()
            .any(|e| &e.dst == helper_id && e.kind == meridian_core::EdgeKind::Calls));
    }

    #[test]
    fn rest_endpoints_link_local_handlers() {
        use meridian_core::{Confidence, EdgeKind, NodeId, NodeKind};
        let src = r#"
async fn list() {}
fn app() -> Router {
    Router::new().route("/users", get(list))
}
"#;
        let parsed = parse_source(Language::Rust, src).unwrap();
        let file_id = NodeId::derive(&["file", "r.rs"]);
        let fg = build_file_graph("r.rs", Language::Rust, &file_id, &parsed);
        let rg = build_rest_graph("r.rs", &fg.symbols, src);

        let ep = rg
            .graph
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Endpoint)
            .expect("endpoint node");
        assert_eq!(ep.name, "GET /users");
        assert_eq!(rg.operations.len(), 1);
        assert_eq!(rg.operations[0].0, ep.id);
        assert_eq!(rg.operations[0].1.path, "/users");

        // HANDLES is emitted handler -> endpoint at Extracted confidence.
        let list_id = &fg.symbols.iter().find(|s| s.name == "list").unwrap().id;
        assert!(rg.graph.edges.iter().any(|e| e.kind == EdgeKind::Handles
            && &e.src == list_id
            && e.dst == ep.id
            && e.confidence == Confidence::Extracted));
        assert!(rg.pending_handlers.is_empty());
    }

    #[test]
    fn rest_handler_deferred_when_not_local() {
        use meridian_core::{NodeId, NodeKind};
        let src = r#"
fn app() -> Router {
    Router::new().route("/users", get(handlers::list))
}
"#;
        let parsed = parse_source(Language::Rust, src).unwrap();
        let file_id = NodeId::derive(&["file", "r.rs"]);
        let fg = build_file_graph("r.rs", Language::Rust, &file_id, &parsed);
        let rg = build_rest_graph("r.rs", &fg.symbols, src);

        let ep = rg
            .graph
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Endpoint)
            .expect("endpoint node");
        // No local def named `list`, so the handler link is deferred, not emitted.
        assert!(!rg
            .graph
            .edges
            .iter()
            .any(|e| e.kind == meridian_core::EdgeKind::Handles));
        assert_eq!(
            rg.pending_handlers,
            vec![("list".to_string(), ep.id.clone())]
        );
    }

    #[test]
    fn rest_mounts_link_router_builders() {
        use meridian_core::{Confidence, EdgeKind, NodeId};
        let src = r#"
async fn list() {}
fn users_router() -> Router {
    Router::new().route("/", get(list))
}
fn app() -> Router {
    Router::new().nest("/api/users", users_router())
}
"#;
        let parsed = parse_source(Language::Rust, src).unwrap();
        let file_id = NodeId::derive(&["file", "r.rs"]);
        let fg = build_file_graph("r.rs", Language::Rust, &file_id, &parsed);
        let rg = build_rest_graph("r.rs", &fg.symbols, src);

        let sym = |name: &str| {
            fg.symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap()
                .id
                .clone()
        };
        assert!(rg.graph.edges.iter().any(|e| e.kind == EdgeKind::Mounts
            && e.src == sym("app")
            && e.dst == sym("users_router")
            && e.confidence == Confidence::Extracted));
    }
}
