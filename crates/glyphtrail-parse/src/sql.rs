//! SQL DDL extraction (#416, Phase A).
//!
//! A hand-rolled DDL reader for `.sql` files (schema dumps + migrations), in the
//! spirit of the artifact extractors in `glyphtrail-analyze/src/schema.rs` rather
//! than a tree-sitter grammar: `CREATE TABLE` is fairly dialect-uniform, and a
//! targeted scan avoids both the dialect-grammar mismatch and a heavy dependency.
//!
//! It recognises `CREATE TABLE` and `CREATE [MATERIALIZED] VIEW`, yielding a
//! [`NodeKind::Table`] per object, a [`NodeKind::Column`] per column (contained by
//! the table), and a `References` edge for a foreign key / a view's `FROM` target
//! that is defined in the same file. Node ids are file-scoped (the declaring
//! file's path is part of the id) so the store's per-file incremental cleanup
//! removes them cleanly; a single cross-file identity for the same table across
//! migrations is a deliberate follow-up.

use std::collections::HashSet;

use glyphtrail_core::{CodeGraph, Confidence, EdgeKind, Node, NodeId, NodeKind, Span};

/// One DDL object (table or view) read from a SQL file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlTable {
    /// Normalised identity: schema-qualified, unquoted, lowercased.
    pub name: String,
    /// The name as written (unquoted), for display.
    pub display: String,
    /// Column names as written, in declaration order (empty for a view).
    pub columns: Vec<String>,
    /// Normalised names of tables this one references (FK targets / view `FROM`).
    pub references: Vec<String>,
    /// A `CREATE VIEW`, as opposed to a base table.
    pub is_view: bool,
    /// Byte offset of the object's name, for the node span.
    pub byte: usize,
}

/// Replace comment and string-literal bytes with spaces, preserving length and
/// newlines, so a keyword or `(`/`,` inside a comment or string can't confuse the
/// structural scan while line numbers stay aligned to the original source.
fn blank_noise(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let two = |j: usize| j + 1 < bytes.len();
        if b == b'-' && two(i) && bytes[i + 1] == b'-' {
            // line comment to end of line
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(b' ');
                i += 1;
            }
        } else if b == b'/' && two(i) && bytes[i + 1] == b'*' {
            // block comment
            out.push(b' ');
            out.push(b' ');
            i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && two(i) && bytes[i + 1] == b'/') {
                out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                i += 1;
            }
            if i < bytes.len() {
                out.push(b' ');
                out.push(b' ');
                i += 2;
            }
        } else if b == b'\'' {
            // single-quoted string literal — blank the contents so a keyword or
            // `(` inside it can't be read as DDL. (Double-quotes and backticks are
            // quoted *identifiers* in SQL, so they're left for the tokenizer.)
            out.push(b);
            i += 1;
            while i < bytes.len() && bytes[i] != b'\'' {
                out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                i += 1;
            }
            if i < bytes.len() {
                out.push(b'\'');
                i += 1;
            }
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// A lexical token with its byte offset in the (noise-blanked) source.
#[derive(Debug, Clone)]
struct Tok {
    text: String,
    byte: usize,
}

/// Tokenise into identifier runs (incl. `.` for `schema.table`, quotes kept) and
/// the single-char punctuation the parser cares about (`( ) , ;`).
fn tokenize(clean: &str) -> Vec<Tok> {
    let bytes = clean.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    let is_word =
        |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'"' | b'`' | b'$');
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if matches!(c, b'(' | b')' | b',' | b';') {
            toks.push(Tok {
                text: (c as char).to_string(),
                byte: i,
            });
            i += 1;
        } else if is_word(c) {
            let start = i;
            while i < bytes.len() && is_word(bytes[i]) {
                i += 1;
            }
            toks.push(Tok {
                text: clean[start..i].to_string(),
                byte: start,
            });
        } else {
            i += 1; // operators, etc. — irrelevant to DDL structure
        }
    }
    toks
}

/// Strip surrounding quotes/backticks from one dotted identifier segment.
fn unquote(s: &str) -> String {
    s.split('.')
        .map(|seg| seg.trim_matches(|c| c == '"' || c == '`').to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// Lowercase, unquoted, schema-qualified identity for a table name.
fn normalize(name: &str) -> String {
    unquote(name).to_ascii_lowercase()
}

/// Keywords that begin a table-level constraint clause, not a column.
fn is_constraint_kw(w: &str) -> bool {
    matches!(
        w.to_ascii_lowercase().as_str(),
        "primary"
            | "foreign"
            | "unique"
            | "check"
            | "constraint"
            | "key"
            | "exclude"
            | "index"
            | "like"
    )
}

/// Extract every table/view declared in `source`.
pub fn extract_sql_schema(source: &str) -> Vec<SqlTable> {
    let clean = blank_noise(source);
    let toks = tokenize(&clean);
    let lc: Vec<String> = toks.iter().map(|t| t.text.to_ascii_lowercase()).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if lc[i] != "create" {
            i += 1;
            continue;
        }
        // Skip CREATE modifiers (OR REPLACE / TEMP / TEMPORARY / GLOBAL / UNLOGGED).
        let mut j = i + 1;
        let is_view = loop {
            match lc.get(j).map(String::as_str) {
                Some("or" | "replace" | "temp" | "temporary" | "global" | "local" | "unlogged") => {
                    j += 1
                }
                Some("materialized") => j += 1,
                Some("table") => break false,
                Some("view") => break true,
                _ => {
                    j = usize::MAX;
                    break false;
                }
            }
        };
        if j == usize::MAX {
            i += 1;
            continue;
        }
        j += 1; // past TABLE/VIEW
        // optional IF NOT EXISTS
        if lc.get(j).map(String::as_str) == Some("if")
            && lc.get(j + 1).map(String::as_str) == Some("not")
            && lc.get(j + 2).map(String::as_str) == Some("exists")
        {
            j += 3;
        }
        let Some(name_tok) = toks.get(j) else {
            i += 1;
            continue;
        };
        let display = unquote(&name_tok.text);
        let name = normalize(&name_tok.text);
        let name_byte = name_tok.byte;
        j += 1;

        let (columns, references, end) = if is_view {
            (Vec::new(), view_references(&toks, &lc, j), j)
        } else {
            parse_columns(&toks, &lc, j)
        };
        out.push(SqlTable {
            name,
            display,
            columns,
            references,
            is_view,
            byte: name_byte,
        });
        i = end.max(j);
    }
    out
}

/// Parse a `CREATE TABLE` column list starting at the token after the name.
/// Returns `(columns, referenced tables, index past the closing paren)`.
fn parse_columns(toks: &[Tok], lc: &[String], start: usize) -> (Vec<String>, Vec<String>, usize) {
    let mut columns = Vec::new();
    let mut references = Vec::new();
    // Find the opening paren of the column list.
    let mut k = start;
    while k < toks.len() && toks[k].text != "(" && toks[k].text != ";" {
        k += 1;
    }
    if k >= toks.len() || toks[k].text != "(" {
        return (columns, references, k);
    }
    k += 1; // past '('
    let mut depth = 1;
    let mut item_start = true;
    while k < toks.len() && depth > 0 {
        match toks[k].text.as_str() {
            "(" => {
                depth += 1;
                item_start = false;
            }
            ")" => {
                depth -= 1;
            }
            "," if depth == 1 => item_start = true,
            _ => {
                if depth == 1 {
                    // The first word of a top-level item is a column name unless it
                    // opens a table-level constraint clause.
                    if item_start && !is_constraint_kw(&toks[k].text) {
                        columns.push(unquote(&toks[k].text));
                    }
                    // A `REFERENCES target` anywhere in the item is an FK target.
                    if lc[k] == "references"
                        && let Some(t) = toks.get(k + 1)
                    {
                        references.push(normalize(&t.text));
                    }
                    item_start = false;
                }
            }
        }
        k += 1;
    }
    (columns, references, k)
}

/// Table names a view reads from: the identifiers after `FROM`/`JOIN`, up to the
/// statement terminator.
fn view_references(toks: &[Tok], lc: &[String], start: usize) -> Vec<String> {
    let mut refs = Vec::new();
    let mut k = start;
    while k < toks.len() && toks[k].text != ";" {
        if (lc[k] == "from" || lc[k] == "join")
            && let Some(t) = toks.get(k + 1)
            && t.text != "("
        {
            refs.push(normalize(&t.text));
        }
        k += 1;
    }
    refs.sort();
    refs.dedup();
    refs
}

/// 1-based line number of `byte` in `source`.
fn line_of(source: &str, byte: usize) -> usize {
    source[..byte.min(source.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1
}

/// Deterministic id for a table, scoped to the file that declares it so the
/// store's per-file incremental cleanup (`delete_file_data`, which deletes nodes
/// by `Node.file`) removes it cleanly when the file changes. References therefore
/// resolve only within a file (see [`build_sql_graph`]); a single cross-file
/// identity for the same table across migrations is a follow-up.
pub fn table_node_id(rel_path: &str, name: &str) -> NodeId {
    NodeId::derive(&["sql_table", rel_path, name])
}

/// Build the SQL schema fragment for one `.sql` file: table + column nodes,
/// `Contains` edges (file→table, table→column), and same-file `References` edges.
/// Cross-file FK references are left to a follow-up (a reference is only emitted
/// when its target table is also defined in this file, so every edge endpoint
/// exists at insert time).
pub fn build_sql_graph(rel_path: &str, file_id: &NodeId, source: &str) -> CodeGraph {
    let mut graph = CodeGraph::new();
    let tables = extract_sql_schema(source);
    let defined: HashSet<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    let mut seen_table: HashSet<NodeId> = HashSet::new();

    for t in &tables {
        let table_id = table_node_id(rel_path, &t.name);
        let line = line_of(source, t.byte);
        let span = Some(Span {
            start_byte: t.byte,
            end_byte: t.byte,
            start_line: line,
            end_line: line,
        });
        if seen_table.insert(table_id.clone()) {
            graph.add_node(Node {
                id: table_id.clone(),
                kind: NodeKind::Table,
                name: t.display.clone(),
                qualified_name: t.name.clone(),
                file: rel_path.to_string(),
                language: Some("sql".to_string()),
                span,
                doc: Some(if t.is_view {
                    "view".into()
                } else {
                    "table".into()
                }),
                signature: None,
            });
            graph.add_edge(
                file_id.clone(),
                table_id.clone(),
                EdgeKind::Contains,
                Confidence::Extracted,
            );
        }

        let mut seen_col: HashSet<String> = HashSet::new();
        for col in &t.columns {
            let col_norm = col.to_ascii_lowercase();
            if !seen_col.insert(col_norm.clone()) {
                continue;
            }
            let col_id = NodeId::derive(&["sql_column", rel_path, &t.name, &col_norm]);
            graph.add_node(Node {
                id: col_id.clone(),
                kind: NodeKind::Column,
                name: col.clone(),
                qualified_name: format!("{}.{}", t.name, col_norm),
                file: rel_path.to_string(),
                language: Some("sql".to_string()),
                // No per-column byte offset is tracked yet, so omit the span
                // rather than point every column at the table's location.
                span: None,
                doc: None,
                signature: None,
            });
            graph.add_edge(
                table_id.clone(),
                col_id,
                EdgeKind::Contains,
                Confidence::Extracted,
            );
        }

        let mut seen_ref: HashSet<&str> = HashSet::new();
        for r in &t.references {
            // Only same-file targets, so the edge's destination node exists.
            if r != &t.name && defined.contains(r.as_str()) && seen_ref.insert(r.as_str()) {
                graph.add_edge(
                    table_id.clone(),
                    table_node_id(rel_path, r),
                    EdgeKind::References,
                    Confidence::Extracted,
                );
            }
        }
    }
    graph
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn extracts_tables_columns_and_fk_reference() {
        let src = r#"
            -- a comment with CREATE TABLE noise
            CREATE TABLE orgs (
                id   INTEGER PRIMARY KEY,
                name TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS users (
                id     SERIAL PRIMARY KEY,
                email  TEXT UNIQUE NOT NULL,
                org_id INTEGER REFERENCES orgs(id),
                FOREIGN KEY (org_id) REFERENCES orgs (id)
            );
        "#;
        let tables = extract_sql_schema(src);
        check!(tables.len() == 2);
        let users = tables.iter().find(|t| t.name == "users").unwrap();
        check!(users.columns == vec!["id", "email", "org_id"]); // constraint line excluded
        check!(users.references.contains(&"orgs".to_string()));
        check!(!users.is_view);
    }

    #[test]
    fn quoted_and_schema_qualified_names_normalize() {
        let src = r#"CREATE TABLE "Public"."Foo" ("Id" int);"#;
        let t = &extract_sql_schema(src)[0];
        check!(t.name == "public.foo");
        check!(t.display == "Public.Foo");
        check!(t.columns == vec!["Id"]);
    }

    #[test]
    fn view_references_its_from_tables() {
        let src =
            "CREATE VIEW active_users AS SELECT u.id FROM users u JOIN orgs o ON o.id = u.org_id;";
        let t = &extract_sql_schema(src)[0];
        check!(t.is_view);
        check!(t.references == vec!["orgs".to_string(), "users".to_string()]);
    }

    #[test]
    fn string_literals_do_not_create_phantom_tables() {
        let src = "INSERT INTO log (msg) VALUES ('CREATE TABLE not_a_real_table (x int)');";
        check!(extract_sql_schema(src).is_empty());
    }

    #[test]
    fn graph_has_table_column_and_same_file_reference_edges() {
        let src = "CREATE TABLE a (id int); CREATE TABLE b (id int, a_id int REFERENCES a(id));";
        let file_id = NodeId::derive(&["file", "schema.sql"]);
        let g = build_sql_graph("schema.sql", &file_id, src);
        let tables = g.nodes.iter().filter(|n| n.kind == NodeKind::Table).count();
        let cols = g
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Column)
            .count();
        check!(tables == 2);
        check!(cols == 3);
        // a table node is reachable from the file, and b references a.
        let a = table_node_id("schema.sql", "a");
        let b = table_node_id("schema.sql", "b");
        check!(
            g.edges
                .iter()
                .any(|e| e.src == b && e.dst == a && e.kind == EdgeKind::References)
        );
        check!(
            g.edges
                .iter()
                .any(|e| e.src == file_id && e.dst == a && e.kind == EdgeKind::Contains)
        );
    }
}
