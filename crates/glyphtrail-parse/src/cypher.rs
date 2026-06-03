//! Cypher graph-query extraction (#416, Phase C).
//!
//! The graph-DB analogue of the SQL linking: a Cypher node/relationship **label**
//! is the table, and a query that `MATCH`es / `MERGE`s / `CREATE`s a label reads
//! or writes it. This dogfoods the project itself, which embeds lbug (kuzu) Cypher
//! throughout `glyphtrail-store`:
//!
//! - kuzu DDL `CREATE NODE TABLE <Label>` / `CREATE REL TABLE <Label>` declares a
//!   label → a [`NodeKind::Table`] node (the schema side);
//! - a `MATCH (n:Label)` reads it, a `MERGE`/`CREATE (n:Label)` writes it (access).
//!
//! Embedded Cypher is the string argument of a graph method call (`conn.query`,
//! `run`, `exec_unwind`, …) that `looks_like_cypher`; a `.cypher`/`.cql` file is a
//! follow-up.

use tree_sitter::{Node, Parser, Tree};

use glyphtrail_core::{CodeGraph, Confidence, EdgeKind, Language, NodeId, NodeKind, Span};

use crate::sql::{DbAccess, normalize_name, table_node_id};

/// An embedded Cypher query site: byte offset (for enclosing-function resolution)
/// and the `(access, normalized label)` pairs it touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CypherAccess {
    pub byte: usize,
    pub accesses: Vec<(DbAccess, String)>,
}

/// The Cypher schema (labels) + access sites extracted from one file.
pub struct CypherExtract {
    /// `Table` nodes for labels declared via kuzu DDL.
    pub graph: CodeGraph,
    /// Embedded query access sites (Rust).
    pub accesses: Vec<CypherAccess>,
}

/// Graph-DB method names whose string argument may be Cypher (lbug/kuzu, neo4rs,
/// …). The string is only treated as a query when it also `looks_like_cypher`.
const CYPHER_METHODS: &[&str] = &[
    "query",
    "run",
    "execute",
    "exec_unwind",
    "prepare",
    "prepare_cached",
];

/// Extract Cypher labels + accesses from `source`. Rust only for now (embedded
/// strings); other languages yield nothing.
pub fn extract_cypher(
    rel_path: &str,
    file_id: &NodeId,
    source: &str,
    lang: &Language,
) -> CypherExtract {
    let mut graph = CodeGraph::new();
    let mut accesses = Vec::new();
    if *lang != Language::Rust {
        return CypherExtract { graph, accesses };
    }
    let Some(tree) = parse(source) else {
        return CypherExtract { graph, accesses };
    };
    let src = source.as_bytes();
    // Same-file `const`/`static` string values, so a query passed by name
    // (`self.run(MERGE_EDGES, …)`) resolves to its literal (#444, cf. #405).
    let consts = rust_str_consts(tree.root_node(), src);
    let mut seen_label: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    walk(tree.root_node(), &mut |n| {
        match n.kind() {
            // kuzu DDL labels can sit in any string (often a `const SCHEMA: &[&str]`
            // array, not an inline query call), so scan every string literal.
            "string_content" => {
                let s = text(n, src);
                add_cypher_labels(
                    &mut graph,
                    file_id,
                    rel_path,
                    source,
                    n.start_byte(),
                    &s,
                    &mut seen_label,
                );
            }
            // Access: a graph method call (`conn.query("MATCH …")`) whose first
            // argument is Cypher — an inline string or a same-file `const` name —
            // attributed to the enclosing function.
            "call_expression" => {
                let Some(func) = n.child_by_field_name("function") else {
                    return;
                };
                if func.kind() != "field_expression" {
                    return;
                }
                let Some(method) = func.child_by_field_name("field").map(|f| text(f, src)) else {
                    return;
                };
                if !CYPHER_METHODS.contains(&method.as_str()) {
                    return;
                }
                let Some(args) = n.child_by_field_name("arguments") else {
                    return;
                };
                // Prefer an inline Cypher string; otherwise resolve a `const`-named
                // query (the query isn't always the first argument, e.g.
                // `exec_unwind(&conn, MERGE_EDGES, rows)`).
                let Some(cypher) = first_string(args, src)
                    .filter(|s| looks_like_cypher(s))
                    .or_else(|| const_query_arg(args, src, &consts))
                else {
                    return;
                };
                let acc = extract_cypher_access(&cypher);
                if !acc.is_empty() {
                    accesses.push(CypherAccess {
                        byte: n.start_byte(),
                        accesses: acc,
                    });
                }
            }
            _ => {}
        }
    });
    CypherExtract { graph, accesses }
}

/// Extract a standalone `.cypher`/`.cql` file (#444): its DDL/pattern labels become
/// `Table` nodes and the whole file's `MATCH`/`MERGE`/`CREATE` access is attributed
/// to the file itself (there's no enclosing function). The caller turns each
/// `(access, label)` into a `File`→`Reads`/`Writes`→`Table` edge.
pub fn extract_cypher_file(rel_path: &str, file_id: &NodeId, source: &str) -> CypherExtract {
    let mut graph = CodeGraph::new();
    let mut seen_label: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    add_cypher_labels(
        &mut graph,
        file_id,
        rel_path,
        source,
        0,
        source,
        &mut seen_label,
    );
    let acc = extract_cypher_access(source);
    let accesses = if acc.is_empty() {
        Vec::new()
    } else {
        vec![CypherAccess {
            byte: 0,
            accesses: acc,
        }]
    };
    CypherExtract { graph, accesses }
}

/// Add `Table` nodes for the labels in `s`: kuzu DDL labels (scanned in any string,
/// as the keywords are unambiguous) plus, when `s` looks like Cypher, the
/// pattern-only labels a schema-free graph declares solely via `MERGE`/`CREATE`/
/// `MATCH (n:Label)` (#444). Deduplicated by node id via `seen`.
fn add_cypher_labels(
    graph: &mut CodeGraph,
    file_id: &NodeId,
    rel_path: &str,
    source: &str,
    byte: usize,
    s: &str,
    seen: &mut std::collections::HashSet<NodeId>,
) {
    let mut add = |graph: &mut CodeGraph, display: &str, norm: &str| {
        let id = table_node_id(rel_path, norm);
        if seen.insert(id.clone()) {
            add_label(graph, file_id, &id, display, norm, rel_path, byte, source);
        }
    };
    for label in cypher_ddl_labels(s) {
        let norm = normalize_name(&label);
        add(graph, &label, &norm);
    }
    if looks_like_cypher(s) {
        for (display, norm) in cypher_pattern_labels(s) {
            add(graph, &display, &norm);
        }
    }
}

/// Same-file `const NAME: &str = "…"` / `static NAME: &str = "…"` string values, so
/// a query referenced by name resolves to its literal (#444). Best-effort: only
/// plain string-literal initializers are captured.
fn rust_str_consts(root: Node, src: &[u8]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    walk(root, &mut |n| {
        if !matches!(n.kind(), "const_item" | "static_item") {
            return;
        }
        let Some(name) = n.child_by_field_name("name").map(|x| text(x, src)) else {
            return;
        };
        let Some(value) = n.child_by_field_name("value") else {
            return;
        };
        if value.kind() == "string_literal" {
            out.insert(name, literal_text(value, src));
        }
    });
    out
}

/// The full text of a string literal: all `string_content` chunks concatenated.
/// A multi-line query split by a `\`-newline continuation parses as several
/// `string_content` nodes around `escape_sequence`s, so unlike `first_string` this
/// reassembles the whole query (the continuation's dropped whitespace collapses,
/// matching Rust's own semantics).
fn literal_text(value: Node, src: &[u8]) -> String {
    let mut out = String::new();
    walk(value, &mut |n| {
        if n.kind() == "string_content" {
            out.push_str(&text(n, src));
        }
    });
    out
}

/// Resolve a `const`-named query: the first argument that is a bare identifier
/// whose same-file `const` value looks like Cypher. Scans all arguments (not just
/// the first) so `exec_unwind(&conn, MERGE_EDGES, rows)` resolves even though the
/// query sits after the connection handle.
fn const_query_arg(
    args: Node,
    src: &[u8],
    consts: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .filter(|a| a.kind() == "identifier")
        .find_map(|a| consts.get(&text(a, src)).filter(|q| looks_like_cypher(q)))
        .cloned()
}

/// Whether a string is Cypher rather than SQL: a Cypher-only leading keyword, or a
/// node/relationship pattern (`(:`, `-[`, `]->`).
pub fn looks_like_cypher(s: &str) -> bool {
    let t = s.trim_start();
    let first = t
        .split(|c: char| c.is_whitespace())
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(
        first.as_str(),
        "match" | "merge" | "unwind" | "optional" | "detach"
    ) || t.contains("(:")
        || t.contains("-[")
        || t.contains("]->")
        || t.contains("]-(")
    {
        return true;
    }
    // kuzu DDL: `CREATE NODE TABLE …` / `CREATE REL TABLE …`.
    let low = t.to_ascii_lowercase();
    low.contains("node table") || low.contains("rel table")
}

/// Labels declared by kuzu DDL (`CREATE NODE TABLE <Label>` / `CREATE REL TABLE`).
pub fn cypher_ddl_labels(cypher: &str) -> Vec<String> {
    let toks = tokenize(cypher);
    let lc: Vec<String> = toks.iter().map(|t| t.to_ascii_lowercase()).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 < toks.len() {
        if lc[i] == "create" && matches!(lc[i + 1].as_str(), "node" | "rel") && lc[i + 2] == "table"
        {
            let mut j = i + 3;
            // optional IF NOT EXISTS
            if lc.get(j).map(String::as_str) == Some("if")
                && lc.get(j + 1).map(String::as_str) == Some("not")
                && lc.get(j + 2).map(String::as_str) == Some("exists")
            {
                j += 3;
            }
            if let Some(name) = toks.get(j) {
                out.push(name.clone());
            }
            i = j;
        }
        i += 1;
    }
    out
}

/// `(access, normalized label)` for the labels a Cypher query touches: a label
/// under `MATCH`/`OPTIONAL`/`WITH`/`UNWIND` reads, under `CREATE`/`MERGE` writes.
/// A label is `:Name` inside a node `(...)` or relationship `[...]` pattern (not
/// inside a `{}` map, where `:` is a property key).
pub fn extract_cypher_access(cypher: &str) -> Vec<(DbAccess, String)> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (writes, _display, norm) in label_occurrences(cypher) {
        let access = if writes {
            DbAccess::Write
        } else {
            DbAccess::Read
        };
        if seen.insert((writes, norm.clone())) {
            out.push((access, norm));
        }
    }
    out
}

/// Labels named in node/relationship patterns (`(n:Label)`, `[r:REL]`), regardless
/// of read/write, as `(display, normalized)` deduped by normalized name. For a
/// schema-free graph (e.g. Neo4j) that declares no kuzu DDL, this is the only
/// source of label nodes (#444): the labels exist solely in `MERGE`/`CREATE`/
/// `MATCH` patterns.
pub fn cypher_pattern_labels(cypher: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (_writes, display, norm) in label_occurrences(cypher) {
        if seen.insert(norm.clone()) {
            out.push((display, norm));
        }
    }
    out
}

/// Every label occurrence in node/relationship patterns, in source order, as
/// `(writes, display, normalized)`. `writes` reflects the enclosing clause
/// (`CREATE`/`MERGE` write; `MATCH` and friends read). A label is `:Name` inside a
/// node `(...)` or relationship `[...]` pattern, not inside a `{}` property map
/// (where `:` is a key). Shared by both the access and the label extractors.
fn label_occurrences(cypher: &str) -> Vec<(bool, String, String)> {
    let toks = tokenize(cypher);
    let mut out = Vec::new();
    let mut clause_write = false; // current clause writes (CREATE/MERGE)?
    let mut pattern_depth = 0i32; // inside ( or [
    let mut map_depth = 0i32; // inside {
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        match t.as_str() {
            "(" | "[" => pattern_depth += 1,
            ")" | "]" => pattern_depth = (pattern_depth - 1).max(0),
            // A `{` is a property map only inside a node/rel pattern; at the top
            // level it opens a subquery block (`CALL { … }`), whose labels count.
            "{" if pattern_depth > 0 => map_depth += 1,
            "}" if pattern_depth > 0 && map_depth > 0 => map_depth -= 1,
            ":" => {
                if pattern_depth > 0
                    && map_depth == 0
                    && let Some(label) = toks.get(i + 1)
                    && is_ident(label)
                {
                    out.push((clause_write, label.clone(), normalize_name(label)));
                    i += 1;
                }
            }
            _ => match t.to_ascii_lowercase().as_str() {
                "create" | "merge" => clause_write = true,
                "match" | "optional" | "with" | "unwind" | "where" | "return" | "set"
                | "delete" | "remove" | "call" | "on" => clause_write = false,
                _ => {}
            },
        }
        i += 1;
    }
    out
}

/// Tokenise Cypher into identifiers and the punctuation the scan needs
/// (`( ) [ ] { } : ;`), skipping `//`/`/* */` comments and `'…'`/`"…"` strings.
fn tokenize(src: &str) -> Vec<String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let is_word = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'$');
    while i < b.len() {
        let c = b[i];
        if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if c == b'\'' || c == b'"' {
            i += 1;
            while i < b.len() && b[i] != c {
                i += 1;
            }
            i += 1;
        } else if matches!(c, b'(' | b')' | b'[' | b']' | b'{' | b'}' | b':' | b';') {
            out.push((c as char).to_string());
            i += 1;
        } else if is_word(c) {
            let start = i;
            while i < b.len() && is_word(b[i]) {
                i += 1;
            }
            out.push(src[start..i].to_string());
        } else {
            i += 1;
        }
    }
    out
}

fn is_ident(s: &str) -> bool {
    s.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

#[allow(clippy::too_many_arguments)]
fn add_label(
    graph: &mut CodeGraph,
    file_id: &NodeId,
    label_id: &NodeId,
    display: &str,
    norm: &str,
    rel_path: &str,
    byte: usize,
    source: &str,
) {
    let line = source[..byte.min(source.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1;
    graph.add_node(glyphtrail_core::Node {
        id: label_id.clone(),
        kind: NodeKind::Table,
        name: display.to_string(),
        qualified_name: norm.to_string(),
        file: rel_path.to_string(),
        language: Some("cypher".to_string()),
        span: Some(Span {
            start_byte: byte,
            end_byte: byte,
            start_line: line,
            end_line: line,
        }),
        doc: Some("graph label".to_string()),
        signature: None,
    });
    graph.add_edge(
        file_id.clone(),
        label_id.clone(),
        EdgeKind::Contains,
        Confidence::Extracted,
    );
}

fn parse(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&crate::registry::grammar(&Language::Rust).expect("built-in grammar"))
        .ok()?;
    parser.parse(source, None)
}

fn first_string(node: Node, src: &[u8]) -> Option<String> {
    let mut found = None;
    walk(node, &mut |n| {
        if found.is_none() && n.kind() == "string_content" {
            found = Some(n.utf8_text(src).unwrap_or("").to_string());
        }
    });
    found
}

fn walk<'a>(node: Node<'a>, f: &mut dyn FnMut(Node<'a>)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, f);
    }
}

fn text(node: Node, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn acc(c: &str) -> Vec<(DbAccess, String)> {
        let mut a = extract_cypher_access(c);
        a.sort_by(|x, y| {
            (x.1.clone(), format!("{:?}", x.0)).cmp(&(y.1.clone(), format!("{:?}", y.0)))
        });
        a
    }

    #[test]
    fn match_reads_and_merge_writes_labels() {
        check!(acc("MATCH (n:Node) RETURN n.id") == vec![(DbAccess::Read, "node".into())]);
        check!(acc("MERGE (a:Account {id: 1})") == vec![(DbAccess::Write, "account".into())]);
        // MATCH then MERGE: the matched label reads, the merged writes.
        check!(
            acc("MATCH (a:Node), (b:Node) MERGE (a)-[e:Edge]->(b)")
                == vec![
                    (DbAccess::Write, "edge".into()),
                    (DbAccess::Read, "node".into())
                ]
        );
        // A `{}` map's `:` (property key) is not a label.
        check!(
            acc("CREATE (n:User {name: 'x', role: 'y'})") == vec![(DbAccess::Write, "user".into())]
        );
        // A label inside a `CALL { … }` subquery block is still found — that `{}`
        // is a block, not a property map.
        check!(
            acc("CALL { MATCH (w:Widget {id: 1}) RETURN w }")
                == vec![(DbAccess::Read, "widget".into())]
        );
    }

    #[test]
    fn ddl_labels_from_kuzu_create_table() {
        check!(
            cypher_ddl_labels("CREATE NODE TABLE IF NOT EXISTS Node(id STRING)") == vec!["Node"]
        );
        check!(cypher_ddl_labels("CREATE REL TABLE Edge(FROM Node TO Node)") == vec!["Edge"]);
        check!(cypher_ddl_labels("SELECT 1").is_empty());
    }

    #[test]
    fn looks_like_cypher_distinguishes_from_sql() {
        check!(looks_like_cypher("MATCH (n:Node) RETURN n"));
        check!(looks_like_cypher("MERGE (a)-[:REL]->(b)"));
        check!(!looks_like_cypher("SELECT id FROM users"));
        check!(!looks_like_cypher("INSERT INTO t VALUES (1)"));
    }

    #[test]
    fn embedded_cypher_in_rust_yields_labels_and_accesses() {
        let src = r#"
            fn schema(conn: &Conn) {
                conn.query("CREATE NODE TABLE IF NOT EXISTS Widget(id STRING, PRIMARY KEY(id))").unwrap();
            }
            fn load(conn: &Conn) {
                let _ = conn.query("MATCH (w:Widget) RETURN w.id");
            }
        "#;
        let e = extract_cypher(
            "store.rs",
            &NodeId::derive(&["file", "store.rs"]),
            src,
            &Language::Rust,
        );
        check!(
            e.graph
                .nodes
                .iter()
                .any(|n| n.kind == NodeKind::Table && n.name == "Widget")
        );
        let labels: Vec<&str> = e
            .accesses
            .iter()
            .flat_map(|a| a.accesses.iter().map(|(_, l)| l.as_str()))
            .collect();
        check!(labels.contains(&"widget"));
    }

    // #444: a schema-free graph declares no DDL, so labels come only from the
    // node/rel patterns of MERGE/CREATE/MATCH.
    #[test]
    fn pattern_labels_from_schema_free_queries() {
        let mut got = cypher_pattern_labels("MATCH (a:Account)-[:OWNS]->(c:Card) RETURN c");
        got.sort();
        check!(
            got == vec![
                ("Account".to_string(), "account".to_string()),
                ("Card".to_string(), "card".to_string()),
                ("OWNS".to_string(), "owns".to_string()),
            ]
        );
    }

    // #444: a pattern-only label (no kuzu DDL) still becomes a `Table` node, so the
    // MERGE/MATCH access has something to resolve to.
    #[test]
    fn pattern_only_label_becomes_a_table_node() {
        let src = r#"
            fn upsert(conn: &Conn) {
                conn.query("MERGE (a:Account {id: 1})").unwrap();
            }
        "#;
        let e = extract_cypher(
            "store.rs",
            &NodeId::derive(&["file", "store.rs"]),
            src,
            &Language::Rust,
        );
        check!(
            e.graph
                .nodes
                .iter()
                .any(|n| n.kind == NodeKind::Table && n.name == "Account")
        );
        let labels: Vec<&str> = e
            .accesses
            .iter()
            .flat_map(|a| a.accesses.iter().map(|(_, l)| l.as_str()))
            .collect();
        check!(labels == vec!["account"]);
    }

    // #444: a query passed by `const` name resolves to its literal, so the access
    // is attributed even though the call site has no inline string.
    #[test]
    fn const_named_query_access_is_resolved() {
        // The query const is the *second* argument (after the connection), as in
        // this repo's own `exec_unwind(&conn, MERGE_EDGES, rows)`.
        let src = r#"
            const MERGE_EDGES: &str = "MERGE (a:Node)-[e:Edge]->(b:Node)";
            fn link(&self, conn: &Conn) {
                self.exec_unwind(&conn, MERGE_EDGES, rows).unwrap();
            }
        "#;
        let e = extract_cypher(
            "store.rs",
            &NodeId::derive(&["file", "store.rs"]),
            src,
            &Language::Rust,
        );
        let mut labels: Vec<&str> = e
            .accesses
            .iter()
            .flat_map(|a| a.accesses.iter().map(|(_, l)| l.as_str()))
            .collect();
        labels.sort();
        labels.dedup();
        check!(labels == vec!["edge", "node"]);
    }

    // #444: a standalone `.cypher` file yields label nodes and file-level access.
    #[test]
    fn cypher_file_yields_labels_and_file_access() {
        let file = "queries/links.cypher";
        let src = "MATCH (a:Account) MERGE (a)-[:OWNS]->(c:Card);";
        let e = extract_cypher_file(file, &NodeId::derive(&["file", file]), src);
        let mut tables: Vec<&str> = e
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Table)
            .map(|n| n.name.as_str())
            .collect();
        tables.sort();
        check!(tables == vec!["Account", "Card", "OWNS"]);
        // MATCH reads Account; MERGE writes the OWNS relationship.
        let acc = &e.accesses[0].accesses;
        check!(acc.contains(&(DbAccess::Read, "account".to_string())));
        check!(acc.contains(&(DbAccess::Write, "owns".to_string())));
    }
}
