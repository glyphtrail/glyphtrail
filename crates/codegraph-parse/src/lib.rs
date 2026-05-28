pub mod build;
pub mod extract;
pub mod registry;

pub use build::{build_file_graph, FileGraph, PendingEdge, SymbolEntry};
pub use extract::{parse_source, ParsedFile};

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_core::Language;
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
        use codegraph_core::{NodeId, NodeKind};
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
            .any(|e| &e.dst == helper_id && e.kind == codegraph_core::EdgeKind::Calls));
    }
}
