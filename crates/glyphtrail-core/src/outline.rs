//! Symbol outline: a file's definitions with their signatures, at adjustable
//! detail — the compact "shape of the code" view an agent (or human) reads to
//! get oriented before diving into impact or queries.
//!
//! Signatures are sliced from source **on demand**: a definition node's span
//! starts at the `fn`/`struct`/`def` keyword, so the header is
//! `source[span.start_byte .. body_opener]`. This needs no stored signature and
//! no reindex; a missing/edited source simply drops back to name + kind.
//!
//! The slicer is a small best-effort lexer, not a parser: it tracks `()`/`[]`
//! depth and string literals so a `{` inside an argument or a string doesn't
//! end the header early, and it is bounded so a pathological span can't run
//! away. It does not track block comments — a `{` inside a comment in a header
//! is rare enough to accept.

use serde::Serialize;

use crate::model::{Node, NodeKind, Span};

/// How much of each symbol to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// `kind name` only.
    Minimal,
    /// + the sliced signature header (the default).
    Standard,
    /// + the first line of the doc comment.
    Full,
}

impl Detail {
    /// Parse `--detail` (case-insensitive); `None`/unknown → `Standard`.
    pub fn parse(s: &str) -> Detail {
        match s.trim().to_ascii_lowercase().as_str() {
            "minimal" | "min" => Detail::Minimal,
            "full" => Detail::Full,
            _ => Detail::Standard,
        }
    }
}

/// One symbol in an outline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutlineSymbol {
    pub name: String,
    pub qualified_name: String,
    pub kind: NodeKind,
    /// 1-based start line.
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

/// Whether a node kind is a definition worth listing in an outline — the
/// declared symbols, not structural (Repo/Dir/File/Comment) or API-surface
/// (Endpoint/ClientCall/SchemaOp/Router) kinds.
pub fn is_outline_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Function
            | NodeKind::Method
            | NodeKind::Class
            | NodeKind::Struct
            | NodeKind::Interface
            | NodeKind::Enum
            | NodeKind::Trait
            | NodeKind::Module
    )
}

/// Build an [`OutlineSymbol`] for `node` at `detail`, slicing the signature from
/// `source` (the node's file's contents) when available.
pub fn outline_symbol(node: &Node, source: Option<&str>, detail: Detail) -> OutlineSymbol {
    let line = node.span.map(|s| s.start_line).unwrap_or(0);
    let signature = match detail {
        Detail::Minimal => None,
        // Prefer the signature captured at parse time (#344); fall back to slicing
        // the source for legacy indexes built before that field existed.
        Detail::Standard | Detail::Full => node.signature.clone().or_else(|| {
            source
                .zip(node.span)
                .and_then(|(src, span)| slice_signature(src, span, node.language.as_deref()))
        }),
    };
    let doc = match detail {
        Detail::Full => node
            .doc
            .as_deref()
            .and_then(|d| d.lines().map(str::trim).find(|l| !l.is_empty()))
            .map(str::to_string),
        _ => None,
    };
    OutlineSymbol {
        name: node.name.clone(),
        qualified_name: node.qualified_name.clone(),
        kind: node.kind,
        line,
        signature,
        doc,
    }
}

/// Largest window scanned for a header, and the line cap, so a span whose body
/// brace is far away (or absent) can't slurp a huge slice.
const MAX_SCAN: usize = 4096;
const MAX_HEADER_LINES: usize = 3;

/// Languages whose definition header ends at the body's opening `{`. Everything
/// else (Python/Ruby/Lua/Haskell/OCaml/Elixir and unknown) uses the first line.
fn is_brace_family(language: Option<&str>) -> bool {
    matches!(
        language,
        Some(
            "rust"
                | "javascript"
                | "typescript"
                | "tsx"
                | "go"
                | "java"
                | "c"
                | "cpp"
                | "csharp"
                | "kotlin"
                | "php"
                | "scala"
                | "swift"
                | "zig"
                | "bash"
        )
    )
}

/// Slice a one-line signature header from `source` at `span`, per `language`.
/// `None` when the span is out of range or the header is empty.
pub fn slice_signature(source: &str, span: Span, language: Option<&str>) -> Option<String> {
    let rest = source.get(span.start_byte..)?; // node start is a char boundary
    let header = if is_brace_family(language) {
        brace_header(rest).unwrap_or_else(|| first_line(rest))
    } else {
        first_line(rest)
    };
    let collapsed = header.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then_some(collapsed)
}

/// The slice up to the body's opening `{` at top level (not inside `()`/`[]` or
/// a string). `None` if no such `{` within the bounds → caller uses first line.
fn brace_header(rest: &str) -> Option<&str> {
    let mut depth: i32 = 0;
    let mut string: Option<char> = None;
    let mut escaped = false;
    let mut lines = 0usize;
    for (i, c) in rest.char_indices() {
        if i >= MAX_SCAN {
            return None;
        }
        if let Some(q) = string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                string = None;
            }
            continue;
        }
        match c {
            '"' | '`' => string = Some(c),
            // A `'` opens a string-like span to skip only for a char literal
            // (`'x'`, `'\n'`). A Rust lifetime (`'a`, `&'a`, `'static`) is code:
            // treating it as a string used to swallow the rest of the header and
            // miss the body `{`, so signatures bled into the body (#344).
            '\'' => {
                let b = rest.as_bytes();
                if b.get(i + 1) == Some(&b'\\') || b.get(i + 2) == Some(&b'\'') {
                    string = Some('\'');
                }
            }
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '{' if depth <= 0 => return Some(&rest[..i]),
            '\n' => {
                lines += 1;
                if lines >= MAX_HEADER_LINES {
                    return None;
                }
            }
            _ => {}
        }
    }
    None
}

/// The first line, bounded to `MAX_SCAN` bytes.
fn first_line(rest: &str) -> &str {
    let window = rest.get(..rest.len().min(MAX_SCAN)).unwrap_or(rest);
    match window.find('\n') {
        Some(nl) => &window[..nl],
        None => window,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn sig(src: &str, lang: &str) -> Option<String> {
        slice_signature(
            src,
            Span {
                start_byte: 0,
                end_byte: src.len(),
                start_line: 1,
                end_line: 1,
            },
            Some(lang),
        )
    }

    #[test]
    fn rust_header_stops_at_body_brace() {
        check!(
            sig("fn parse(src: &[u8]) -> Result<()> {\n  body\n}", "rust")
                == Some("fn parse(src: &[u8]) -> Result<()>".to_string())
        );
    }

    #[test]
    fn rust_multiline_generics_collapse() {
        let s = "fn go<T, U>(a: T, b: U)\n  -> Vec<T>\n  where T: Clone {\n}";
        // capped at 3 lines, whitespace collapsed.
        check!(sig(s, "rust") == Some("fn go<T, U>(a: T, b: U) -> Vec<T> where T: Clone".into()));
    }

    #[test]
    fn struct_and_bodyless_trait_decl() {
        check!(sig("struct Node { id: u32 }", "rust") == Some("struct Node".into()));
        // a body-less declaration has no `{` -> first line.
        check!(sig("trait Backing;\n", "rust") == Some("trait Backing;".into()));
    }

    #[test]
    fn brace_inside_string_or_arg_is_ignored() {
        // `{` inside a default-arg object stays inside `(...)`.
        check!(
            sig("function f(o = {a: 1}) {\n}", "typescript")
                == Some("function f(o = {a: 1})".into())
        );
        // `{` inside a string literal is ignored.
        check!(sig("fn f() -> &str { \"{\" }", "rust") == Some("fn f() -> &str".into()));
    }

    #[test]
    fn rust_lifetimes_do_not_swallow_the_body() {
        // `'a` is a lifetime, not a char literal: the scanner must still find the
        // body `{` rather than treating `'...'` as a string and bleeding the
        // header into the body (#344).
        check!(
            sig("fn f<'a>(x: &'a str) -> &'a str {\n  x\n}", "rust")
                == Some("fn f<'a>(x: &'a str) -> &'a str".into())
        );
    }

    #[test]
    fn python_uses_first_line() {
        check!(
            sig("def run(self, x: int) -> str:\n    pass", "python")
                == Some("def run(self, x: int) -> str:".into())
        );
        check!(sig("class Item(Base):\n    pass", "python") == Some("class Item(Base):".into()));
    }

    #[test]
    fn go_func() {
        check!(
            sig("func Parse(b []byte) (Node, error) {\n}", "go")
                == Some("func Parse(b []byte) (Node, error)".into())
        );
    }

    #[test]
    fn out_of_range_span_is_none() {
        check!(
            slice_signature(
                "abc",
                Span {
                    start_byte: 99,
                    end_byte: 100,
                    start_line: 1,
                    end_line: 1
                },
                Some("rust")
            ) == None
        );
    }

    #[test]
    fn detail_parses() {
        check!(Detail::parse("minimal") == Detail::Minimal);
        check!(Detail::parse("FULL") == Detail::Full);
        check!(Detail::parse("") == Detail::Standard);
        check!(Detail::parse("weird") == Detail::Standard);
    }

    #[test]
    fn outline_prefers_the_stored_signature_over_slicing() {
        let node = Node {
            id: crate::model::NodeId("n".into()),
            kind: NodeKind::Function,
            name: "go".into(),
            qualified_name: "go".into(),
            file: "a.rs".into(),
            language: Some("rust".into()),
            span: Some(Span {
                start_byte: 0,
                end_byte: 5,
                start_line: 1,
                end_line: 1,
            }),
            doc: None,
            signature: Some("fn go(stored: bool) -> Stored".into()),
        };
        // Even with (deliberately wrong) source, the parse-time signature wins —
        // so a stale working tree can't garble outline (#344).
        let sym = outline_symbol(&node, Some("fn WRONG() {}"), Detail::Standard);
        check!(sym.signature.as_deref() == Some("fn go(stored: bool) -> Stored"));
    }

    #[test]
    fn outline_symbol_respects_detail() {
        let node = Node {
            id: crate::model::NodeId("n".into()),
            kind: NodeKind::Function,
            name: "go".into(),
            qualified_name: "go".into(),
            file: "a.rs".into(),
            language: Some("rust".into()),
            span: Some(Span {
                start_byte: 0,
                end_byte: 10,
                start_line: 5,
                end_line: 7,
            }),
            doc: Some("Does the thing.\nMore.".into()),
            signature: None,
        };
        let src = "fn go() -> u8 {\n  1\n}";
        let min = outline_symbol(&node, Some(src), Detail::Minimal);
        check!(min.signature == None && min.doc == None && min.line == 5);
        let std = outline_symbol(&node, Some(src), Detail::Standard);
        check!(std.signature == Some("fn go() -> u8".into()) && std.doc == None);
        let full = outline_symbol(&node, Some(src), Detail::Full);
        check!(
            full.signature == Some("fn go() -> u8".into())
                && full.doc == Some("Does the thing.".into())
        );
        // No source -> no signature, even at standard.
        check!(outline_symbol(&node, None, Detail::Standard).signature == None);
    }
}
