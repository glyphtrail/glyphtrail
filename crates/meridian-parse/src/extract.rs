use meridian_core::{EdgeKind, Language, NodeKind, Span};
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

/// Parse `source` and extract raw definitions, calls, imports, bases and comments.
pub fn parse_source(lang: Language, source: &str) -> anyhow::Result<ParsedFile> {
    let mut parser = Parser::new();
    parser.set_language(&grammar(lang))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse"))?;

    let query = Query::new(&grammar(lang), query_source(lang))?;
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
                "call" => out.calls.push(RawRef {
                    name: text,
                    byte: node.start_byte(),
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
                    if let Some(suffix) = other.strip_prefix("def.") {
                        if let Some(k) = kind_from_suffix(suffix) {
                            def_kind = Some(k);
                            def_node = Some(node);
                        }
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
