use glyphtrail_core::{EdgeKind, Language, NodeKind, Span};
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

use crate::registry::{grammar, query_source};

#[derive(Debug, Clone)]
pub struct RawDef {
    pub kind: NodeKind,
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct RawRef {
    pub name: String,
    pub byte: usize,
    /// Immediate receiver/scope qualifier of the call, when the call is
    /// qualified (`Foo::bar`, `Foo.bar`, `obj.method`, `pkg.Func`) — the single
    /// segment directly before the name. `None` for a bare `name(...)` call or a
    /// `self`/`this`/`cls`/`super` receiver. Used to disambiguate same-named
    /// targets by enclosing scope (#5).
    pub scope: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RawBase {
    pub kind: EdgeKind,
    pub name: String,
    pub byte: usize,
}

#[derive(Debug, Clone)]
pub struct RawComment {
    pub text: String,
    pub span: Span,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedFile {
    pub defs: Vec<RawDef>,
    pub calls: Vec<RawRef>,
    pub imports: Vec<String>,
    pub bases: Vec<RawBase>,
    pub comments: Vec<RawComment>,
}

fn kind_from_suffix(suffix: &str) -> Option<NodeKind> {
    Some(match suffix {
        "function" => NodeKind::Function,
        "method" => NodeKind::Method,
        "class" => NodeKind::Class,
        "struct" => NodeKind::Struct,
        "enum" => NodeKind::Enum,
        "trait" => NodeKind::Trait,
        "interface" => NodeKind::Interface,
        "module" => NodeKind::Module,
        _ => return None,
    })
}

fn span_of(node: tree_sitter::Node) -> Span {
    Span {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    }
}

/// Strip surrounding quotes/angle brackets from an import token.
fn clean_import(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '<' || c == '>' || c == ';')
        .to_string()
}

/// Whether a captured `@call` identifier is actually a definition's name,
/// recognised by an immediately-preceding definition keyword (`fn generated`,
/// `struct Foo`, …). This only happens inside macro token trees, where the
/// raw-token call heuristic can't tell a definition from a call (#5); a real
/// call never has a bare `fn`/`struct`/… token directly before its name.
fn is_definition_name(node: tree_sitter::Node, src: &[u8]) -> bool {
    node.prev_sibling()
        .and_then(|s| s.utf8_text(src).ok())
        .is_some_and(|kw| {
            matches!(
                kw,
                "fn" | "struct"
                    | "enum"
                    | "trait"
                    | "impl"
                    | "mod"
                    | "type"
                    | "union"
                    | "class"
                    | "interface"
                    | "def"
            )
        })
}

/// The immediate receiver/scope of a captured `@call` name node, read from its
/// parent in the AST: `Foo::bar`→`Foo`, `Foo.bar`/`obj.method`→`Foo`/`obj`,
/// `pkg.Func`→`pkg`. Returns the single segment directly before the name (the
/// last segment of a dotted/scoped receiver), and `None` for a bare call or a
/// `self`/`this`/`cls`/`super` receiver. Field names cover the qualified-call
/// node kinds across the supported grammars (#5).
fn call_qualifier(node: tree_sitter::Node, src: &[u8]) -> Option<String> {
    let parent = node.parent()?;
    let recv = match parent.kind() {
        // Python attribute, JS/TS member, Java method invocation: `object`.
        "attribute" | "member_expression" | "method_invocation" => {
            parent.child_by_field_name("object")
        }
        "scoped_identifier" => parent.child_by_field_name("path"), // Rust `Foo::bar`
        "field_expression" => parent.child_by_field_name("value"), // Rust `obj.field()`
        "selector_expression" => parent.child_by_field_name("operand"), // Go `pkg.Func`
        "member_access_expression" => parent.child_by_field_name("expression"), // C#
        "call" => parent.child_by_field_name("receiver"),          // Ruby `recv.meth`
        _ => None,
    }?;
    let text = recv.utf8_text(src).ok()?;
    // Reduce a dotted/scoped receiver to its last segment (the immediate scope).
    let seg = text.rsplit(['.', ':']).next().unwrap_or(text).trim();
    if seg.is_empty() || matches!(seg, "self" | "this" | "cls" | "super") {
        return None;
    }
    Some(seg.to_string())
}

/// Parse `source` and extract raw definitions, calls, imports, bases and comments.
pub fn parse_source(lang: &Language, source: &str) -> anyhow::Result<ParsedFile> {
    let (Some(grammar), Some(query)) = (grammar(lang), query_source(lang)) else {
        anyhow::bail!(
            "no built-in grammar for language '{}'; use parse_with with a loaded grammar",
            lang.name()
        );
    };
    parse_with(&grammar, query, source)
}

/// Parse `source` with an explicit tree-sitter `grammar` and extraction
/// `query`, mapping the shared capture convention (`@def.<kind>` + `@name`,
/// `@call`, `@import`, `@extends`/`@implements`, `@comment`) into a
/// [`ParsedFile`]. Decoupled from the built-in [`Language`] enum so the same
/// extraction works for dynamically-loaded grammars.
pub fn parse_with(
    grammar: &tree_sitter::Language,
    query_src: &str,
    source: &str,
) -> anyhow::Result<ParsedFile> {
    let mut parser = Parser::new();
    parser.set_language(grammar)?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse"))?;

    let query = Query::new(grammar, query_src)?;
    let capture_names = query.capture_names();
    let src = source.as_bytes();

    let mut out = ParsedFile::default();
    let mut seen_defs: std::collections::HashSet<(usize, &'static str)> =
        std::collections::HashSet::new();

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), src);
    while let Some(m) = matches.next() {
        // Collect captures by capture name for this match.
        let mut def_kind: Option<NodeKind> = None;
        let mut def_node: Option<tree_sitter::Node> = None;
        let mut name_text: Option<String> = None;

        for cap in m.captures {
            let cap_name = capture_names[cap.index as usize];
            let node = cap.node;
            let text = node.utf8_text(src).unwrap_or("").to_string();
            match cap_name {
                "name" => name_text = Some(text),
                // Skip a "call" that is really a *definition* inside a macro body
                // (e.g. `fn generated()` in `quote! { fn generated() {} }`): the
                // raw-token heuristic (rust.scm) would otherwise read the defined
                // name as a call (#5).
                "call" if is_definition_name(node, src) => {}
                "call" => out.calls.push(RawRef {
                    name: text,
                    byte: node.start_byte(),
                    scope: call_qualifier(node, src),
                }),
                "import" => out.imports.push(clean_import(&text)),
                "extends" => out.bases.push(RawBase {
                    kind: EdgeKind::Extends,
                    name: text,
                    byte: node.start_byte(),
                }),
                "implements" => out.bases.push(RawBase {
                    kind: EdgeKind::Implements,
                    name: text,
                    byte: node.start_byte(),
                }),
                "comment" => out.comments.push(RawComment {
                    text: text.trim().to_string(),
                    span: span_of(node),
                }),
                other => {
                    if let Some(suffix) = other.strip_prefix("def.")
                        && let Some(k) = kind_from_suffix(suffix)
                    {
                        def_kind = Some(k);
                        def_node = Some(node);
                    }
                }
            }
        }

        if let (Some(kind), Some(node), Some(name)) = (def_kind, def_node, name_text.clone()) {
            // Dedup overlapping patterns that capture the same definition node.
            if seen_defs.insert((node.start_byte(), kind.as_str())) {
                out.defs.push(RawDef {
                    kind,
                    name,
                    span: span_of(node),
                });
            }
        }
    }

    Ok(out)
}
