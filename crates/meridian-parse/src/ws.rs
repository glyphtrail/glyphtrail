//! WebSocket client-connection extraction (#51, connection boundary).
//!
//! Browser-native `new WebSocket(url)` (JS/TS/TSX) opens a connection via an
//! HTTP `GET` upgrade at `url`. We extract the connection path so it can link to
//! the server's upgrade route (a REST `GET` endpoint) via the existing matcher
//! — a WebSocket [`OperationKey`](meridian_core::OperationKey) shares a REST
//! `GET` signature. Event/channel *message* boundaries (socket.io, Centrifugo,
//! SignalR, …) are a separate concern tracked on the epic.
//!
//! Only string/template-literal URLs are extracted; dynamic URLs are out of
//! scope (they collapse to a dynamic segment when present in a template).

use meridian_core::{Language, Span};
use tree_sitter::{Node, Parser};

use crate::registry::grammar;

/// A client WebSocket connection site: the connection URL/path and its span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawWsConnect {
    /// Connection URL as written (literal or template); canonicalized later.
    pub path: String,
    pub span: Span,
}

/// Extract `new WebSocket(url)` connections from JS/TS/TSX `source`. Empty on
/// parse failure or for other languages.
pub fn extract_ws_connections(source: &str, lang: &Language) -> Vec<RawWsConnect> {
    if !matches!(
        lang,
        Language::JavaScript | Language::TypeScript | Language::Tsx
    ) {
        return Vec::new();
    }
    let mut parser = Parser::new();
    if parser
        .set_language(&grammar(lang).expect("built-in grammar"))
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    walk(tree.root_node(), &mut |n| {
        if n.kind() == "new_expression"
            && n.child_by_field_name("constructor")
                .map(|c| text(c, src))
                .as_deref()
                == Some("WebSocket")
            && let Some(url) = first_url_arg(n.child_by_field_name("arguments"), src)
        {
            out.push(RawWsConnect {
                path: url,
                span: span_of(n),
            });
        }
    });
    out
}

/// First argument as a string- or template-literal URL.
fn first_url_arg(args: Option<Node>, src: &[u8]) -> Option<String> {
    let args = args?;
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .find_map(|a| match a.kind() {
            "string" => {
                let mut c = a.walk();
                Some(
                    a.named_children(&mut c)
                        .filter(|ch| ch.kind() == "string_fragment")
                        .map(|ch| text(ch, src))
                        .collect::<String>(),
                )
            }
            "template_string" => Some(text(a, src).trim_matches('`').to_string()),
            _ => None,
        })
}

fn text(node: Node, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
}

fn span_of(node: Node) -> Span {
    Span {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    }
}

fn walk(node: Node, f: &mut dyn FnMut(Node)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn extracts_native_websocket_connection() {
        let src = r#"
const sock = new WebSocket("/ws/chat");
const url = `/rooms/${id}`;
const other = new WebSocket(url);          // dynamic var: skipped
const tmpl = new WebSocket(`/live/${id}`); // template: kept verbatim
"#;
        let conns = extract_ws_connections(src, &Language::JavaScript);
        let paths: Vec<&str> = conns.iter().map(|c| c.path.as_str()).collect();
        check!(paths.contains(&"/ws/chat"));
        check!(paths.contains(&"/live/${id}"));
        // A bare-variable URL is not a literal, so it is not extracted.
        check!(!paths.iter().any(|p| p.is_empty()));
        check!(conns.len() == 2);
    }

    #[test]
    fn skips_other_languages() {
        check!(extract_ws_connections("new WebSocket(\"/ws\")", &Language::Rust).is_empty());
    }
}
