#![forbid(unsafe_code)]

pub mod build;
pub mod client;
pub mod extract;
pub mod imports;
pub mod registry;
pub mod rest;

pub use build::{
    ClientGraph, FileGraph, PendingEdge, RestGraph, SymbolEntry, build_client_graph,
    build_file_graph, build_rest_graph,
};
pub use client::{RawClientCall, extract_client_calls};
pub use extract::{ParsedFile, parse_source, parse_with};
pub use imports::resolve_import;
pub use rest::{
    RawEndpoint, RawMount, extract_axum, extract_axum_mounts, extract_utoipa, extract_utoipa_mounts,
};

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
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

    // parse_with (grammar + query) is the decoupled core of parse_source; for a
    // built-in language the two must produce identical results.
    #[test]
    fn parse_with_matches_parse_source_for_builtin() {
        let src = "fn helper() {}\nfn main() { helper(); }\n";
        let via_lang = parse_source(Language::Rust, src).unwrap();
        let via_grammar = parse_with(
            &registry::grammar(Language::Rust),
            registry::query_source(Language::Rust),
            src,
        )
        .unwrap();
        check!(via_lang.defs.len() == via_grammar.defs.len());
        check!(via_lang.calls.len() == via_grammar.calls.len());
        check!(via_grammar.defs.iter().any(|d| d.name == "helper"));
        check!(via_grammar.calls.iter().any(|c| c.name == "helper"));
    }

    // #23: every supported language extracts a top-level function definition
    // named `f` into a graph node, exercising each grammar + query end to end.
    #[test]
    fn extracts_a_function_definition_per_language() {
        use meridian_core::NodeId;
        let cases = [
            (Language::Rust, "fn f() {}"),
            (Language::Python, "def f():\n    pass\n"),
            (Language::JavaScript, "function f() {}"),
            (Language::TypeScript, "function f(): void {}"),
            (Language::Tsx, "function f() { return null; }"),
            (Language::Go, "package main\nfunc f() {}"),
            (Language::Java, "class C { void f() {} }"),
            (Language::C, "void f() {}"),
            (Language::Cpp, "void f() {}"),
            (Language::CSharp, "class C { void f() {} }"),
        ];
        for (lang, src) in cases {
            let parsed = parse_source(lang, src)
                .unwrap_or_else(|e| panic!("parse failed for {}: {e}", lang.name()));
            let file_id = NodeId::derive(&["file", "x"]);
            let fg = build_file_graph("x", lang, &file_id, &parsed);
            check!(
                fg.graph.nodes.iter().any(|n| n.name == "f"),
                "{} should extract a definition named `f`, got {:?}",
                lang.name(),
                fg.graph.nodes.iter().map(|n| &n.name).collect::<Vec<_>>()
            );
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
        check!(names.contains(&"helper"));
        check!(names.contains(&"main"));
        check!(parsed.calls.iter().any(|c| c.name == "helper"));
        check!(parsed.comments.iter().any(|c| c.text.contains("WHY")));
    }

    #[test]
    fn extracts_definitions_per_language() {
        let cases = [
            (
                Language::Python,
                "class Foo:\n    def bar(self):\n        pass\n",
                "bar",
            ),
            (Language::Go, "package m\nfunc Run() {}\n", "Run"),
            (Language::Java, "class A { void go() {} }", "go"),
            (Language::TypeScript, "function f(): void {}\n", "f"),
            (Language::C, "int add(int a) { return a; }\n", "add"),
            (
                Language::Cpp,
                "struct S {}; int main() { return 0; }\n",
                "main",
            ),
            (Language::JavaScript, "function hi() {}\n", "hi"),
        ];
        for (lang, src, expect) in cases {
            let parsed = parse_source(lang, src).unwrap();
            let names: Vec<_> = parsed.defs.iter().map(|d| d.name.as_str()).collect();
            check!(
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
        check!(handle.kind == NodeKind::Method);
        check!(handle.qualified_name == "Service::handle");

        // The NOTE marker becomes a comment node.
        check!(fg.graph.nodes.iter().any(|n| n.kind == NodeKind::Comment));
        // handle() calls helper() within the same file -> resolved locally.
        let helper_id = &fg
            .graph
            .nodes
            .iter()
            .find(|n| n.name == "helper")
            .unwrap()
            .id;
        check!(
            fg.graph
                .edges
                .iter()
                .any(|e| &e.dst == helper_id && e.kind == meridian_core::EdgeKind::Calls)
        );
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
        let rg = build_rest_graph("r.rs", Language::Rust, &fg.symbols, src);

        let ep = rg
            .graph
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Endpoint)
            .expect("endpoint node");
        check!(ep.name == "GET /users");
        check!(rg.operations.len() == 1);
        check!(rg.operations[0].0 == ep.id);
        check!(rg.operations[0].1.path == "/users");

        // HANDLES is emitted handler -> endpoint at Extracted confidence.
        let list_id = &fg.symbols.iter().find(|s| s.name == "list").unwrap().id;
        check!(rg.graph.edges.iter().any(|e| e.kind == EdgeKind::Handles
            && &e.src == list_id
            && e.dst == ep.id
            && e.confidence == Confidence::Extracted));
        check!(rg.pending_handlers.is_empty());
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
        let rg = build_rest_graph("r.rs", Language::Rust, &fg.symbols, src);

        let ep = rg
            .graph
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Endpoint)
            .expect("endpoint node");
        // No local def named `list`, so the handler link is deferred, not emitted.
        check!(
            !rg.graph
                .edges
                .iter()
                .any(|e| e.kind == meridian_core::EdgeKind::Handles)
        );
        check!(rg.pending_handlers == vec![("list".to_string(), ep.id.clone())]);
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
        let rg = build_rest_graph("r.rs", Language::Rust, &fg.symbols, src);

        let sym = |name: &str| {
            fg.symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap()
                .id
                .clone()
        };
        check!(rg.graph.edges.iter().any(|e| e.kind == EdgeKind::Mounts
            && e.src == sym("app")
            && e.dst == sym("users_router")
            && e.confidence == Confidence::Extracted));
    }
}
