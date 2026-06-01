#![forbid(unsafe_code)]

pub mod build;
pub mod client;
pub mod dynamic;
pub mod extract;
pub mod graphql;
pub mod grpc;
pub mod import_symbols;
pub mod imports;
pub mod registry;
pub mod rest;
pub mod ws;

pub use build::{
    ClientGraph, FileGraph, PendingEdge, RestGraph, SymbolEntry, build_client_graph,
    build_file_graph, build_graphql_client_graph, build_graphql_graph, build_grpc_client_graph,
    build_grpc_graph, build_rest_graph, build_ws_client_graph, build_ws_server_graph,
    enclosing_call_edges,
};
pub use client::{RawClientCall, extract_client_calls};
pub use dynamic::{DynamicGrammar, load_dynamic};
pub use extract::{ParsedFile, parse_source, parse_with};
pub use import_symbols::extract_import_symbols;
pub use imports::resolve_import;
pub use rest::{
    RawEndpoint, RawMount, extract_axum, extract_axum_mounts, extract_utoipa, extract_utoipa_mounts,
};
pub use ws::{RawWsConnect, RawWsEvent, WsEventKind, extract_ws_connections, extract_ws_events};

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use glyphtrail_core::Language;
    use tree_sitter::Query;

    #[test]
    fn all_queries_compile() {
        for lang in Language::ALL {
            let grammar = registry::grammar(&lang).expect("built-in grammar");
            let src = registry::query_source(&lang).expect("built-in query");
            Query::new(&grammar, src)
                .unwrap_or_else(|e| panic!("query for {} failed: {e}", lang.name()));
        }
    }

    // parse_with (grammar + query) is the decoupled core of parse_source; for a
    // built-in language the two must produce identical results.
    #[test]
    fn parse_with_matches_parse_source_for_builtin() {
        let src = "fn helper() {}\nfn main() { helper(); }\n";
        let via_lang = parse_source(&Language::Rust, src).unwrap();
        let via_grammar = parse_with(
            &registry::grammar(&Language::Rust).expect("built-in grammar"),
            registry::query_source(&Language::Rust).expect("built-in query"),
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
        use glyphtrail_core::NodeId;
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
            (Language::Ruby, "def f\nend\n"),
            (Language::Kotlin, "fun f() {}\n"),
            (Language::Bash, "f() { :; }\n"),
            (Language::Php, "<?php\nfunction f() {}\n"),
            (Language::Scala, "def f(): Unit = {}\n"),
            (Language::OCaml, "let f x = x\n"),
            (Language::Haskell, "f x = x\n"),
            (Language::Lua, "local function f() end\n"),
            (Language::Swift, "func f() {}\n"),
            (Language::Elixir, "def f(x), do: x\n"),
            (Language::Zig, "fn f() void {}\n"),
            (Language::R, "f <- function() 1\n"),
            (Language::Dart, "void f() {}\n"),
            (Language::Merlin6502, "f rts\n"),
        ];
        for (lang, src) in cases {
            let parsed = parse_source(&lang, src)
                .unwrap_or_else(|e| panic!("parse failed for {}: {e}", lang.name()));
            let file_id = NodeId::derive(&["file", "x"]);
            let fg = build_file_graph("x", &lang, &file_id, &parsed, src);
            check!(
                fg.graph.nodes.iter().any(|n| n.name == "f"),
                "{} should extract a definition named `f`, got {:?}",
                lang.name(),
                fg.graph.nodes.iter().map(|n| &n.name).collect::<Vec<_>>()
            );
        }
    }

    // #5/#131: calls in an async fn body and calls nested inside a macro
    // invocation (e.g. `helper()` in `println!("{}", helper())`) are both
    // captured. The macro body is a raw token tree, so a macro-arg callee is
    // matched as an identifier immediately followed by a parenthesized token
    // tree (see rust.scm).
    #[test]
    fn captures_calls_in_async_and_macro_bodies() {
        let src = "async fn a() { helper().await; }\nfn m() { println!(\"{}\", helper()); }\n";
        let parsed = parse_source(&Language::Rust, src).unwrap();
        let helper_calls = parsed.calls.iter().filter(|c| c.name == "helper").count();
        check!(
            helper_calls == 2,
            "expected the async-body and macro-arg calls, got {helper_calls}"
        );
        // The macro name itself is still captured as a call/reference.
        check!(parsed.calls.iter().any(|c| c.name == "println"));
    }

    // #5: a definition inside a macro body (e.g. a proc-macro `quote!`) is not
    // mistaken for a call by the raw-token heuristic; real calls in the body
    // still are.
    #[test]
    fn macro_body_definition_is_not_a_call() {
        let src = "fn m() { quote! { fn generated() { real_call(); } }; }";
        let parsed = parse_source(&Language::Rust, src).unwrap();
        let names: Vec<&str> = parsed.calls.iter().map(|c| c.name.as_str()).collect();
        check!(
            !names.contains(&"generated"),
            "the defined name should not be a call, got {names:?}"
        );
        check!(names.contains(&"real_call"));
    }

    // #21: the newly added languages extract a call and a comment, exercising
    // each new grammar + query beyond the bare definition check above.
    #[test]
    fn extracts_calls_and_comments_for_new_languages() {
        let cases = [
            (Language::Bash, "# note\nf() { g; }\n", "g"),
            (
                Language::Php,
                "<?php\n// note\nfunction f() { g(); }\n",
                "g",
            ),
            (Language::Scala, "// note\ndef f(): Unit = { g() }\n", "g"),
            (Language::OCaml, "(* note *)\nlet f x = g x\n", "g"),
            (Language::Haskell, "-- note\nf x = g x\n", "g"),
            (
                Language::Lua,
                "-- note\nlocal function f(x) return g(x) end\n",
                "g",
            ),
            (Language::Swift, "// note\nfunc f() { g() }\n", "g"),
            (Language::Elixir, "# note\ndef f(x), do: g(x)\n", "g"),
            (Language::Zig, "// note\nfn f() void { g(); }\n", "g"),
            (Language::R, "# note\nf <- function() { g() }\n", "g"),
            (Language::Dart, "// note\nvoid f() { g(); }\n", "g"),
            (Language::Merlin6502, "* note\nf jsr g\n", "g"),
        ];
        for (lang, src, call) in cases {
            let parsed = parse_source(&lang, src)
                .unwrap_or_else(|e| panic!("parse failed for {}: {e}", lang.name()));
            check!(
                parsed.calls.iter().any(|c| c.name == call),
                "{}: expected call '{call}', got {:?}",
                lang.name(),
                parsed.calls.iter().map(|c| &c.name).collect::<Vec<_>>()
            );
            check!(
                !parsed.comments.is_empty(),
                "{}: expected a comment node",
                lang.name()
            );
        }
    }

    // #368: in a multi-line assembly routine the `jsr` is on a line after the
    // label, so it is contained by no def span. It must attribute to the routine
    // (nearest preceding label), not the file — else the asm callgraph is empty.
    #[test]
    fn merlin_attributes_calls_in_a_multiline_routine() {
        use glyphtrail_core::{EdgeKind, NodeId};
        let src = "handler lda x\n jsr offleft\n rts\noffleft rts\n";
        let parsed = parse_source(&Language::Merlin6502, src).unwrap();
        let file_id = NodeId::derive(&["file", "x.S"]);
        let fg = build_file_graph("x.S", &Language::Merlin6502, &file_id, &parsed, src);
        let id = |name: &str| {
            fg.graph
                .nodes
                .iter()
                .find(|n| n.name == name)
                .map(|n| n.id.clone())
        };
        let (handler, offleft) = (id("handler").unwrap(), id("offleft").unwrap());
        check!(
            fg.graph
                .edges
                .iter()
                .any(|e| e.src == handler && e.dst == offleft && e.kind == EdgeKind::Calls),
            "expected handler -> offleft, got {:?}",
            fg.graph
                .edges
                .iter()
                .map(|e| (&e.src, &e.kind, &e.dst))
                .collect::<Vec<_>>()
        );
    }

    // #5: `new Foo()` constructor instantiation is a reference to the class, so
    // it is captured as a call in JS/TS/TSX and Java (Python `Foo()` already is).
    #[test]
    fn captures_constructor_instantiations() {
        let cases = [
            (
                Language::JavaScript,
                "function f(){ const a = new Foo(); }\n",
            ),
            (
                Language::TypeScript,
                "function f(){ const a = new Foo(); }\n",
            ),
            (Language::Tsx, "function f(){ return new Foo(); }\n"),
            (
                Language::Java,
                "class C { void m(){ Foo a = new Foo(); } }\n",
            ),
        ];
        for (lang, src) in cases {
            let parsed = parse_source(&lang, src).unwrap();
            check!(
                parsed.calls.iter().any(|c| c.name == "Foo"),
                "{}: expected constructor call `Foo`, got {:?}",
                lang.name(),
                parsed.calls.iter().map(|c| &c.name).collect::<Vec<_>>()
            );
        }
    }

    // #5: class inheritance/conformance is captured as base references, so a
    // subtype links to its supertype(s) across languages.
    #[test]
    fn captures_inheritance_bases() {
        let cases = [
            (Language::Ruby, "class A < B\nend\n", vec!["B"]),
            (
                Language::Scala,
                "class A extends B with T\n",
                vec!["B", "T"],
            ),
            (Language::Swift, "class A: B, P {}\n", vec!["B", "P"]),
            (
                Language::Php,
                "<?php\nclass A extends B implements I {}\n",
                vec!["B", "I"],
            ),
        ];
        for (lang, src, want) in cases {
            let parsed = parse_source(&lang, src).unwrap();
            let bases: Vec<&str> = parsed.bases.iter().map(|b| b.name.as_str()).collect();
            for w in want {
                check!(
                    bases.contains(&w),
                    "{}: expected base '{w}', got {bases:?}",
                    lang.name()
                );
            }
        }
    }

    // #136: a secret hardcoded in a design-rationale comment is redacted before
    // it becomes a Comment node, so it never reaches the index / search / wiki.
    #[test]
    fn scrubs_secrets_from_comment_nodes() {
        use glyphtrail_core::{NodeId, NodeKind};
        let token = "ghp_0123456789abcdefABCDEF0123456789abcd";
        let src = format!("// NOTE: rotate {token} soon\nfn f() {{}}\n");
        let parsed = parse_source(&Language::Rust, &src).unwrap();
        let file_id = NodeId::derive(&["file", "c.rs"]);
        let fg = build_file_graph("c.rs", &Language::Rust, &file_id, &parsed, &src);
        let comment = fg
            .graph
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Comment)
            .expect("comment node");
        check!(comment.doc.as_deref().unwrap().contains("[REDACTED]"));
        // The raw token appears nowhere in the built graph (name or doc).
        let leaked =
            fg.graph.nodes.iter().any(|n| {
                n.name.contains(token) || n.doc.as_deref().is_some_and(|d| d.contains(token))
            });
        check!(!leaked, "secret token leaked into the graph");
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
        let parsed = parse_source(&Language::Rust, src).unwrap();
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
            let parsed = parse_source(&lang, src).unwrap();
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
        use glyphtrail_core::{NodeId, NodeKind};
        let src = r#"
class Service:
    # NOTE: this is the entrypoint
    def handle(self):
        self.helper()

    def helper(self):
        pass
"#;
        let parsed = parse_source(&Language::Python, src).unwrap();
        let file_id = NodeId::derive(&["file", "svc.py"]);
        let fg = build_file_graph("svc.py", &Language::Python, &file_id, &parsed, src);

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
                .any(|e| &e.dst == helper_id && e.kind == glyphtrail_core::EdgeKind::Calls)
        );
    }

    #[test]
    fn rest_endpoints_link_local_handlers() {
        use glyphtrail_core::{Confidence, EdgeKind, NodeId, NodeKind};
        let src = r#"
async fn list() {}
fn app() -> Router {
    Router::new().route("/users", get(list))
}
"#;
        let parsed = parse_source(&Language::Rust, src).unwrap();
        let file_id = NodeId::derive(&["file", "r.rs"]);
        let fg = build_file_graph("r.rs", &Language::Rust, &file_id, &parsed, src);
        let rg = build_rest_graph("r.rs", &Language::Rust, &fg.symbols, src);

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
        use glyphtrail_core::{NodeId, NodeKind};
        let src = r#"
fn app() -> Router {
    Router::new().route("/users", get(handlers::list))
}
"#;
        let parsed = parse_source(&Language::Rust, src).unwrap();
        let file_id = NodeId::derive(&["file", "r.rs"]);
        let fg = build_file_graph("r.rs", &Language::Rust, &file_id, &parsed, src);
        let rg = build_rest_graph("r.rs", &Language::Rust, &fg.symbols, src);

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
                .any(|e| e.kind == glyphtrail_core::EdgeKind::Handles)
        );
        check!(rg.pending_handlers == vec![("list".to_string(), ep.id.clone())]);
    }

    #[test]
    fn rest_mounts_link_router_builders() {
        use glyphtrail_core::{Confidence, EdgeKind, NodeId};
        let src = r#"
async fn list() {}
fn users_router() -> Router {
    Router::new().route("/", get(list))
}
fn app() -> Router {
    Router::new().nest("/api/users", users_router())
}
"#;
        let parsed = parse_source(&Language::Rust, src).unwrap();
        let file_id = NodeId::derive(&["file", "r.rs"]);
        let fg = build_file_graph("r.rs", &Language::Rust, &file_id, &parsed, src);
        let rg = build_rest_graph("r.rs", &Language::Rust, &fg.symbols, src);

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

    // #128: FastAPI `include_router` composes via router *variables*, which
    // become synthetic `Router` nodes linked host -> mounted with `MOUNTS`.
    #[test]
    fn router_variable_mounts_emit_router_nodes_and_edges() {
        use glyphtrail_core::{Confidence, EdgeKind, NodeKind};
        let src =
            "router = APIRouter(prefix=\"/users\")\napp.include_router(router, prefix=\"/api\")\n";
        let rg = build_rest_graph("app.py", &Language::Python, &[], src);
        let routers: Vec<_> = rg
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Router)
            .collect();
        check!(routers.len() == 2);
        let host = rg
            .graph
            .nodes
            .iter()
            .find(|n| n.name == "app" && n.kind == NodeKind::Router)
            .expect("app router node");
        let mounted = rg
            .graph
            .nodes
            .iter()
            .find(|n| n.name == "router" && n.kind == NodeKind::Router)
            .expect("mounted router node");
        check!(rg.graph.edges.iter().any(|e| e.kind == EdgeKind::Mounts
            && e.src == host.id
            && e.dst == mounted.id
            && e.confidence == Confidence::Extracted));
    }
}
