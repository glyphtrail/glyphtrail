#![forbid(unsafe_code)]

pub mod build;
pub mod client;
pub mod cypher;
pub mod db_client;
pub mod diesel;
pub mod dynamic;
pub mod extract;
pub mod graphql;
pub mod grpc;
pub mod import_symbols;
pub mod imports;
pub mod jpa;
pub mod registry;
pub mod rest;
pub mod sql;
pub mod sqlalchemy;
pub mod ws;

pub use build::{
    ClientGraph, FileGraph, PendingEdge, RestGraph, SymbolEntry, build_client_graph,
    build_file_graph, build_graphql_client_graph, build_graphql_graph, build_grpc_client_graph,
    build_grpc_graph, build_rest_graph, build_ws_client_graph, build_ws_server_graph,
    enclosing_call_edges,
};
#[allow(deprecated)]
pub use client::module_string_constants;
pub use client::{ModuleConsts, RawClientCall, extract_client_calls, module_constants};
pub use cypher::{
    CypherAccess, CypherExtract, cypher_pattern_labels, extract_cypher, extract_cypher_file,
};
pub use db_client::{RawDbQuery, extract_db_queries};
pub use diesel::{DieselExtract, extract_diesel};
pub use dynamic::{DynamicGrammar, load_dynamic};
pub use extract::{ParsedFile, parse_source, parse_with};
pub use import_symbols::extract_import_symbols;
pub use imports::resolve_import;
pub use jpa::{JpaAccess, JpaExtract, extract_jpa};
pub use rest::{
    RawEndpoint, RawMount, extract_axum, extract_axum_mounts, extract_utoipa, extract_utoipa_mounts,
};
pub use sql::{
    DbAccess, SqlTable, build_sql_graph, extract_query_access, extract_sql_schema, normalize_name,
    table_node_id,
};
pub use sqlalchemy::{SqlAlchemyExtract, extract_sqlalchemy};
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
            (Language::Pascal, "procedure f;\nbegin\nend;\n"),
            (Language::Vb6, "Sub f()\nEnd Sub\n"),
            (
                Language::VbNet,
                "Module M\n    Sub f()\n    End Sub\nEnd Module\n",
            ),
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

    // #5: arrow-function and function-expression assignments (`const f = () => {}`,
    // `const g = function () {}`) — the dominant modern JS/TS/TSX form, including
    // React components — are extracted as function definitions; non-function
    // bindings are not.
    #[test]
    fn extracts_named_arrow_and_function_expressions() {
        use glyphtrail_core::NodeId;
        let cases = [
            (
                Language::JavaScript,
                "const arrow = () => {};\nconst expr = function(){};\nconst notFn = 5;\n",
            ),
            (
                Language::TypeScript,
                "export const handler = (x: number) => x;\nconst notFn = 5;\n",
            ),
            (Language::Tsx, "export const App = () => null;\n"),
        ];
        for (lang, src) in cases {
            let parsed = parse_source(&lang, src).unwrap();
            let fid = NodeId::derive(&["file", "x"]);
            let fg = build_file_graph("x", &lang, &fid, &parsed, src);
            let names: Vec<&str> = fg.graph.nodes.iter().map(|n| n.name.as_str()).collect();
            let has = |want: &str| names.contains(&want);
            match lang {
                Language::JavaScript => {
                    check!(has("arrow"), "got {names:?}");
                    check!(has("expr"), "got {names:?}");
                    check!(!has("notFn"));
                }
                Language::TypeScript => {
                    check!(has("handler"), "got {names:?}");
                    check!(!has("notFn"));
                }
                Language::Tsx => {
                    check!(has("App"), "got {names:?}");
                }
                _ => {}
            }
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

    // #453: Rust `const`/`static` items are indexed as Constant definitions, so
    // they're visible to definition/search/neighbors.
    #[test]
    fn extracts_rust_const_and_static_items() {
        use glyphtrail_core::{NodeId, NodeKind};
        let src = "pub const MAX_RETRIES: usize = 5;\nstatic ID_CACHE: u32 = 0;\n";
        let parsed = parse_source(&Language::Rust, src).unwrap();
        let fid = NodeId::derive(&["file", "x"]);
        let fg = build_file_graph("x", &Language::Rust, &fid, &parsed, src);
        let consts: Vec<&str> = fg
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Constant)
            .map(|n| n.name.as_str())
            .collect();
        check!(consts.contains(&"MAX_RETRIES"), "got {consts:?}");
        check!(consts.contains(&"ID_CACHE"), "got {consts:?}");
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
            (
                Language::Pascal,
                "{ note }\nprocedure f;\nbegin\n  g(1);\nend;\n",
                "g",
            ),
            (Language::Vb6, "' note\nSub f()\n    g 1\nEnd Sub\n", "g"),
            (
                Language::VbNet,
                "' note\nModule M\n    Sub f()\n        g(1)\n    End Sub\nEnd Module\n",
                "g",
            ),
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

    // #369: a code label is a callable Function; a label on an assembler
    // directive (equate `=`, storage `ds`) is a Constant value, not a routine.
    #[test]
    fn merlin_splits_code_routines_from_data_constants() {
        use glyphtrail_core::NodeKind;
        let parsed =
            parse_source(&Language::Merlin6502, "routine rts\nwidth = 5\nbuf ds 30\n").unwrap();
        let kind = |name: &str| parsed.defs.iter().find(|d| d.name == name).map(|d| d.kind);
        check!(kind("routine") == Some(NodeKind::Function));
        check!(kind("width") == Some(NodeKind::Constant));
        check!(kind("buf") == Some(NodeKind::Constant));
    }

    // #555: a Pascal routine is declared twice (interface header + `defProc`
    // body; class member + `TFoo.Bar` body). Only the body is a def, so each
    // routine is one node and a local call resolves to it unambiguously.
    #[test]
    fn pascal_extracts_one_def_per_routine_and_resolves_calls() {
        use glyphtrail_core::{EdgeKind, NodeId, NodeKind};
        let src = "unit Shapes;
interface
uses SysUtils, Geometry.Vectors;
type
  TShape = class(TObject, IDrawable)
    procedure Draw;
  end;
  TPoint = record X, Y: Integer; end;
  TColor = (Red, Green);
  IDrawable = interface
    procedure Draw;
  end;
  TBox<T> = class(TList<T>) end;
  TStrHelper = record helper for string
    function Shout: string;
  end;
procedure Helper(X: Integer);
implementation
procedure Helper(X: Integer);
begin
end;
procedure TShape.Draw;
begin
  Helper(1);
  Canvas.Rectangle(0, 0);
  Refresh;
end;
end.
";
        let parsed = parse_source(&Language::Pascal, src).unwrap();
        let defs = |name: &str| {
            parsed
                .defs
                .iter()
                .filter(|d| d.name == name)
                .map(|d| d.kind)
                .collect::<Vec<_>>()
        };
        check!(defs("TShape") == vec![NodeKind::Class]);
        check!(defs("TPoint") == vec![NodeKind::Struct]);
        check!(defs("TColor") == vec![NodeKind::Enum]);
        check!(defs("IDrawable") == vec![NodeKind::Interface]);
        check!(defs("TBox") == vec![NodeKind::Class]);
        check!(defs("TStrHelper") == vec![NodeKind::Class]);
        check!(defs("Helper") == vec![NodeKind::Function]);
        // The `TShape.Draw` body and the `IDrawable` member; not the class header.
        check!(defs("Draw") == vec![NodeKind::Method, NodeKind::Method]);

        let bases: Vec<&str> = parsed.bases.iter().map(|b| b.name.as_str()).collect();
        check!(bases == vec!["TObject", "IDrawable", "TList"]);
        check!(parsed.imports == vec!["SysUtils", "Geometry.Vectors"]);
        let calls: Vec<&str> = parsed.calls.iter().map(|c| c.name.as_str()).collect();
        check!(calls == vec!["Helper", "Rectangle", "Refresh"]);

        let file_id = NodeId::derive(&["file", "Shapes.pas"]);
        let fg = build_file_graph("Shapes.pas", &Language::Pascal, &file_id, &parsed, src);
        // The `TShape.Draw` body is top-level (`Draw`); the interface member is
        // `IDrawable::Draw`.
        let node = |qualified: &str| {
            fg.graph
                .nodes
                .iter()
                .find(|n| n.qualified_name == qualified)
                .map(|n| n.id.clone())
                .unwrap()
        };
        let (draw, helper) = (node("Draw"), node("Helper"));
        check!(
            fg.graph
                .edges
                .iter()
                .any(|e| e.src == draw && e.dst == helper && e.kind == EdgeKind::Calls)
        );
    }

    // Classic VB: procedures, `Property Get`/`Let` merged into one def, types,
    // `Implements`, and the call forms (`Foo x`, `Call Foo(x)`, `Foo(x)` in an
    // expression, `obj.Foo`, `New Foo`), resolved locally in a `.cls` module.
    #[test]
    fn vb6_extracts_procedures_types_and_calls() {
        use glyphtrail_core::{EdgeKind, NodeId, NodeKind};
        let src = "VERSION 1.0 CLASS
BEGIN
  MultiUse = -1  'True
END
Attribute VB_Name = \"Account\"
Option Explicit
Implements IAccount

Private Const MAX_ITEMS As Integer = 10
Public Type Point
    X As Long
End Type
Public Enum Kind
    Small
End Enum

Public Property Get Balance() As Currency
    Balance = 1
End Property

Public Property Let Balance(ByVal v As Currency)
End Property

Private Sub Deposit(ByVal amt As Currency)
    Log \"deposit\"
    Call Audit(amt)
    total = Twice(amt)
    Form1.Show
    Set c = New Collection
End Sub

Private Sub Audit(ByVal amt As Currency)
End Sub

Private Function Twice(ByVal n As Long) As Long
    Twice = n * 2
End Function
";
        let parsed = parse_source(&Language::Vb6, src).unwrap();
        let kind = |name: &str| parsed.defs.iter().find(|d| d.name == name).map(|d| d.kind);
        check!(kind("Deposit") == Some(NodeKind::Function));
        check!(kind("Twice") == Some(NodeKind::Function));
        check!(kind("Balance") == Some(NodeKind::Method));
        check!(kind("Point") == Some(NodeKind::Struct));
        check!(kind("Kind") == Some(NodeKind::Enum));
        check!(kind("MAX_ITEMS") == Some(NodeKind::Constant));
        let bases: Vec<&str> = parsed.bases.iter().map(|b| b.name.as_str()).collect();
        check!(bases == vec!["IAccount"]);
        let calls: Vec<&str> = parsed.calls.iter().map(|c| c.name.as_str()).collect();
        for want in ["Log", "Audit", "Twice", "Show", "Collection"] {
            check!(calls.contains(&want), "missing call {want}: {calls:?}");
        }

        let file_id = NodeId::derive(&["file", "Account.cls"]);
        let fg = build_file_graph("Account.cls", &Language::Vb6, &file_id, &parsed, src);
        // Get + Let derive the same id, so analyze stores the property as one node.
        let balance: Vec<_> = fg
            .graph
            .nodes
            .iter()
            .filter(|n| n.name == "Balance")
            .map(|n| &n.id)
            .collect();
        check!(balance.len() == 2);
        check!(balance[0] == balance[1]);
        let id = |name: &str| {
            fg.graph
                .nodes
                .iter()
                .find(|n| n.name == name)
                .map(|n| n.id.clone())
                .unwrap()
        };
        let (deposit, audit) = (id("Deposit"), id("Audit"));
        check!(
            fg.graph
                .edges
                .iter()
                .any(|e| e.src == deposit && e.dst == audit && e.kind == EdgeKind::Calls)
        );
    }

    // VB.NET: types and members nest under their block, `Imports` is recorded,
    // and `Foo(x)`, `obj.Foo()`, a parameterless `Foo` and `New Foo()` are calls.
    #[test]
    fn vbnet_extracts_types_members_and_calls() {
        use glyphtrail_core::NodeKind;
        let src = "Imports System.Collections.Generic

Namespace Geometry
    Public Class Shape
        Public Sub Draw()
            Helper(1)
            Me.Refresh()
            Reset
            Dim sb = New StringBuilder()
        End Sub
        Private Function Helper(x As Integer) As Integer
            Return x * 2
        End Function
        Public Property Name As String
    End Class
    Public Interface IDrawable
        Sub Draw()
    End Interface
    Public Structure Pt
        Public X As Integer
    End Structure
    Public Enum Color
        Red
    End Enum
    Public Module Util
        Public Const Max As Integer = 3
    End Module
End Namespace
";
        let parsed = parse_source(&Language::VbNet, src).unwrap();
        let kind = |name: &str| parsed.defs.iter().find(|d| d.name == name).map(|d| d.kind);
        check!(kind("Shape") == Some(NodeKind::Class));
        check!(kind("Helper") == Some(NodeKind::Method));
        check!(kind("Name") == Some(NodeKind::Method));
        check!(kind("IDrawable") == Some(NodeKind::Interface));
        check!(kind("Pt") == Some(NodeKind::Struct));
        check!(kind("Color") == Some(NodeKind::Enum));
        check!(kind("Util") == Some(NodeKind::Module));
        check!(kind("Max") == Some(NodeKind::Constant));
        check!(parsed.imports == vec!["System.Collections.Generic"]);
        let calls: Vec<&str> = parsed.calls.iter().map(|c| c.name.as_str()).collect();
        for want in ["Helper", "Refresh", "Reset", "StringBuilder"] {
            check!(calls.contains(&want), "missing call {want}: {calls:?}");
        }
        // `Me.Refresh()` has no receiver scope.
        check!(parsed.calls.iter().all(|c| c.scope.is_none()));
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
