//! Diesel (Rust ORM) schema + query extraction (#440, the ORM tail of #416 Phase B).
//!
//! Diesel declares its schema with the `table!` macro (usually a generated
//! `schema.rs`): `table! { users (id) { id -> Int4, email -> Varchar, } }` defines
//! a table and its columns. Queries reference a table as a `<name>::table` path
//! inside a DSL chain — `users::table.filter(…).load(conn)` reads, while the
//! statement builders `insert_into(<t>::table)` / `update(…)` / `delete(…)` write.
//!
//! This walks the Rust AST for both: the `table!` macros become `Table` and
//! `Column` nodes (like the `.sql` DDL path), and each query site yields an
//! `(access, table)` pair the analyze layer ties to its enclosing function and to
//! the matching `Table` node. A dsl-imported bare table reference
//! (`use schema::users::dsl::*; users.filter(…)`) is not resolved — only the
//! explicit `<name>::table` form — so recall favours precision.

use tree_sitter::{Node, Parser, Tree};

use glyphtrail_core::{
    CodeGraph, Confidence, EdgeKind, Language, Node as GNode, NodeId, NodeKind, Span,
};

use crate::registry;
use crate::sql::{DbAccess, normalize_name, table_node_id};

/// The Diesel schema (tables/columns) + query access sites from one Rust file.
pub struct DieselExtract {
    /// `Table`/`Column` nodes for the `table!` macros in this file.
    pub graph: CodeGraph,
    /// `(byte, access, normalized table)` query sites; the byte locates the
    /// enclosing function, the table resolves to a `Table` node by name.
    pub accesses: Vec<(usize, DbAccess, String)>,
}

/// Statement builders whose first `<t>::table` argument is written.
const WRITE_BUILDERS: &[&str] = &[
    "insert_into",
    "insert_or_ignore_into",
    "replace_into",
    "update",
    "delete",
];

/// Terminal methods that execute a SELECT, so the `<t>::table` in their receiver
/// chain is read. `execute` is intentionally excluded — it terminates the write
/// builders; `get_result(s)` is a read here but is suppressed when the chain also
/// contains a write builder (an `INSERT … RETURNING`).
const READ_TERMINALS: &[&str] = &[
    "load",
    "load_iter",
    "load_stream",
    "first",
    "get_result",
    "get_results",
];

/// Extract Diesel schema + query access from `source` (Rust only).
pub fn extract_diesel(rel_path: &str, file_id: &NodeId, source: &str) -> DieselExtract {
    let mut graph = CodeGraph::new();
    let mut accesses = Vec::new();
    let Some(tree) = parse(source) else {
        return DieselExtract { graph, accesses };
    };
    let src = source.as_bytes();
    let mut seen_table: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    walk(tree.root_node(), &mut |n| match n.kind() {
        "macro_invocation" => {
            if macro_name(n, src).as_deref() != Some("table") {
                return;
            }
            add_table_schema(
                n,
                rel_path,
                file_id,
                source,
                src,
                &mut graph,
                &mut seen_table,
            );
        }
        "call_expression" => {
            if let Some((access, table)) = query_access(n, src) {
                accesses.push((n.start_byte(), access, table));
            }
        }
        _ => {}
    });
    DieselExtract { graph, accesses }
}

/// The last `::` segment of a macro's name (`diesel::table` → `table`).
fn macro_name(n: Node, src: &[u8]) -> Option<String> {
    let m = n.child_by_field_name("macro")?;
    Some(last_segment(m, src))
}

/// Parse one `table! { [schema.]name (pk) { col -> Type, … } }` and add its
/// `Table` + `Column` nodes (file→table, table→column `Contains` edges).
fn add_table_schema(
    n: Node,
    rel_path: &str,
    file_id: &NodeId,
    source: &str,
    src: &[u8],
    graph: &mut CodeGraph,
    seen_table: &mut std::collections::HashSet<NodeId>,
) {
    let Some(body) = n
        .named_children(&mut n.walk())
        .find(|c| c.kind() == "token_tree")
    else {
        return;
    };
    // The table path (`schema . name`) is the run of identifiers before the first
    // nested token tree; the columns live in the `{ … }` nested token tree.
    let mut idents: Vec<String> = Vec::new();
    let mut cols_text: Option<String> = None;
    for child in body.named_children(&mut body.walk()) {
        match child.kind() {
            "identifier" if cols_text.is_none() && !idents_done(&body, child) => {
                idents.push(text(child, src));
            }
            "token_tree" => {
                let t = text(child, src);
                if t.starts_with('{') && cols_text.is_none() {
                    cols_text = Some(t);
                }
            }
            _ => {}
        }
    }
    let Some(display) = idents.last().cloned() else {
        return;
    };
    let qualified = if idents.len() >= 2 {
        format!("{}.{}", idents[idents.len() - 2], display)
    } else {
        display.clone()
    };
    let norm = normalize_name(&qualified);
    let table_id = table_node_id(rel_path, &norm);
    let line = line_of(source, n.start_byte());
    if seen_table.insert(table_id.clone()) {
        graph.add_node(GNode {
            id: table_id.clone(),
            kind: NodeKind::Table,
            name: display.clone(),
            qualified_name: norm.clone(),
            file: rel_path.to_string(),
            language: Some("rust".to_string()),
            span: Some(Span {
                start_byte: n.start_byte(),
                end_byte: n.start_byte(),
                start_line: line,
                end_line: line,
            }),
            doc: Some("diesel table".to_string()),
            signature: None,
        });
        graph.add_edge(
            file_id.clone(),
            table_id.clone(),
            EdgeKind::Contains,
            Confidence::Extracted,
        );
    }
    let mut seen_col: std::collections::HashSet<String> = std::collections::HashSet::new();
    for col in cols_text.as_deref().map(diesel_columns).unwrap_or_default() {
        let col_norm = col.to_ascii_lowercase();
        if !seen_col.insert(col_norm.clone()) {
            continue;
        }
        let col_id = NodeId::derive(&["sql_column", rel_path, &norm, &col_norm]);
        graph.add_node(GNode {
            id: col_id.clone(),
            kind: NodeKind::Column,
            name: col.clone(),
            qualified_name: format!("{norm}.{col_norm}"),
            file: rel_path.to_string(),
            language: Some("rust".to_string()),
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
}

/// Whether `ident` sits after the first nested token tree (so it's a column type
/// or other inner token, not part of the leading table path).
fn idents_done(body: &Node, ident: Node) -> bool {
    body.named_children(&mut body.walk())
        .any(|c| c.kind() == "token_tree" && c.start_byte() < ident.start_byte())
}

/// Column names in a `table!` body's `{ … }` block: the identifier immediately
/// before each `->`. Splitting on `->`, the column for each arrow is the last word
/// of the text preceding it, which skips column types and `#[sql_name = "…"]`
/// attributes.
fn diesel_columns(block: &str) -> Vec<String> {
    let chunks: Vec<&str> = block.split("->").collect();
    let mut out = Vec::new();
    for chunk in &chunks[..chunks.len().saturating_sub(1)] {
        if let Some(word) = last_word(chunk) {
            out.push(word);
        }
    }
    out
}

/// The last `[A-Za-z0-9_]` run in `s`.
fn last_word(s: &str) -> Option<String> {
    let mut end: Option<usize> = None;
    for (i, c) in s.char_indices().rev() {
        let is_word = c.is_ascii_alphanumeric() || c == '_';
        match end {
            None if is_word => end = Some(i + c.len_utf8()),
            Some(e) if !is_word => return Some(s[i + c.len_utf8()..e].to_string()),
            _ => {}
        }
    }
    end.map(|e| s[..e].to_string())
}

/// The `(access, table)` a Diesel query call touches, or `None`. A write builder
/// (`insert_into(<t>::table)` / `update` / `delete`) writes its first `::table`
/// argument; a read terminal (`.load`/`.first`/…) reads the `<t>::table` in its
/// receiver chain, unless the chain also contains a write builder (an
/// `INSERT … RETURNING`, already counted as the write).
fn query_access(n: Node, src: &[u8]) -> Option<(DbAccess, String)> {
    let func = n.child_by_field_name("function")?;
    match func.kind() {
        // A free-function builder: `insert_into(users::table)`, `diesel::update(…)`.
        "identifier" | "scoped_identifier" => {
            let name = last_segment(func, src);
            if !WRITE_BUILDERS.contains(&name.as_str()) {
                return None;
            }
            let args = n.child_by_field_name("arguments")?;
            let first = args.named_children(&mut args.walk()).next()?;
            // The target may be a bare `users::table` or a query expression like
            // `users::table.filter(…)` / `users::table.find(1)`, so take the first
            // `<t>::table` anywhere in the first argument.
            first_table_path(first, src).map(|t| (DbAccess::Write, t))
        }
        // A method terminal: `<chain>.load(conn)`.
        "field_expression" => {
            let method = text(func.child_by_field_name("field")?, src);
            if !READ_TERMINALS.contains(&method.as_str()) || contains_write_builder(n, src) {
                return None;
            }
            first_table_path(n, src).map(|t| (DbAccess::Read, t))
        }
        _ => None,
    }
}

/// The table name of a `<path>::table` scoped identifier (`users::table` → `users`,
/// `schema::users::table` → `users`), normalized; `None` for any other node.
fn scoped_table_name(n: Node, src: &[u8]) -> Option<String> {
    if n.kind() != "scoped_identifier" {
        return None;
    }
    if text(n.child_by_field_name("name")?, src) != "table" {
        return None;
    }
    let path = n.child_by_field_name("path")?;
    Some(normalize_name(&last_segment(path, src)))
}

/// The first `<t>::table` table name anywhere under `node` (pre-order).
fn first_table_path(node: Node, src: &[u8]) -> Option<String> {
    let mut found = None;
    walk(node, &mut |n| {
        if found.is_none()
            && let Some(t) = scoped_table_name(n, src)
        {
            found = Some(t);
        }
    });
    found
}

/// Whether a write builder call appears anywhere under `node`.
fn contains_write_builder(node: Node, src: &[u8]) -> bool {
    let mut found = false;
    walk(node, &mut |n| {
        if found || n.kind() != "call_expression" {
            return;
        }
        if let Some(func) = n.child_by_field_name("function")
            && matches!(func.kind(), "identifier" | "scoped_identifier")
            && WRITE_BUILDERS.contains(&last_segment(func, src).as_str())
        {
            found = true;
        }
    });
    found
}

/// The last segment of a path/identifier node (`diesel::insert_into` →
/// `insert_into`, `users::table`'s `name` → `table`).
fn last_segment(n: Node, src: &[u8]) -> String {
    match n.kind() {
        "scoped_identifier" => n
            .child_by_field_name("name")
            .map(|x| text(x, src))
            .unwrap_or_else(|| text(n, src)),
        _ => text(n, src),
    }
}

fn parse(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&registry::grammar(&Language::Rust).expect("built-in grammar"))
        .ok()?;
    parser.parse(source, None)
}

fn line_of(source: &str, byte: usize) -> usize {
    // Count newlines over the byte slice, not `source[..byte]`, since a tree-sitter
    // byte offset can land inside a multibyte char and slicing the `&str` would
    // panic (same fix as sql.rs/jpa.rs).
    source.as_bytes()[..byte.min(source.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
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

    fn extract(src: &str) -> DieselExtract {
        extract_diesel("schema.rs", &NodeId::derive(&["file", "schema.rs"]), src)
    }

    fn tables(e: &DieselExtract) -> Vec<&str> {
        let mut t: Vec<&str> = e
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Table)
            .map(|n| n.name.as_str())
            .collect();
        t.sort();
        t
    }

    fn accesses(src: &str) -> Vec<(DbAccess, String)> {
        let mut a: Vec<(DbAccess, String)> = extract(src)
            .accesses
            .into_iter()
            .map(|(_, x, t)| (x, t))
            .collect();
        a.sort_by(|x, y| {
            (x.1.clone(), format!("{:?}", x.0)).cmp(&(y.1.clone(), format!("{:?}", y.0)))
        });
        a
    }

    #[test]
    fn table_macro_yields_table_and_columns() {
        let src = r#"
            diesel::table! {
                users (id) {
                    id -> Int4,
                    email -> Varchar,
                    name -> Nullable<Text>,
                }
            }
        "#;
        let e = extract(src);
        check!(tables(&e) == vec!["users"]);
        let mut cols: Vec<&str> = e
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Column)
            .map(|n| n.name.as_str())
            .collect();
        cols.sort();
        // `Nullable<Text>` is the column type, not a column — only the names land.
        check!(cols == vec!["email", "id", "name"]);
    }

    #[test]
    fn bare_table_macro_and_schema_qualified_name() {
        // No `(pk)` group, and a `schema.table` qualified name.
        let src = r#"
            table! {
                analytics.events (id) {
                    id -> Int8,
                    kind -> Text,
                }
            }
        "#;
        let e = extract(src);
        let t = e
            .graph
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Table)
            .unwrap();
        check!(t.name == "events");
        check!(t.qualified_name == "analytics.events");
    }

    #[test]
    fn select_chain_reads_the_table() {
        let src = r#"
            fn load(conn: &mut PgConnection) -> Vec<User> {
                users::table.filter(users::id.eq(1)).load(conn).unwrap()
            }
        "#;
        check!(accesses(src) == vec![(DbAccess::Read, "users".to_string())]);
    }

    #[test]
    fn builders_write_their_table() {
        let src = r#"
            fn mutate(conn: &mut PgConnection) {
                diesel::insert_into(users::table).values(&n).execute(conn).unwrap();
                diesel::update(accounts::table).set(balance.eq(0)).execute(conn).ok();
                let _ = diesel::delete(posts::table).execute(conn);
            }
        "#;
        check!(
            accesses(src)
                == vec![
                    (DbAccess::Write, "accounts".to_string()),
                    (DbAccess::Write, "posts".to_string()),
                    (DbAccess::Write, "users".to_string()),
                ]
        );
    }

    #[test]
    fn update_delete_on_a_filtered_query_target_still_writes() {
        // The target is a query expression, not a bare `<t>::table` — the `::table`
        // is still found inside the first argument (#440 review).
        let src = r#"
            fn mutate(conn: &mut PgConnection) {
                diesel::update(users::table.find(1)).set(email.eq("x")).execute(conn).ok();
                let _ = diesel::delete(posts::table.filter(posts::draft.eq(true))).execute(conn);
            }
        "#;
        check!(
            accesses(src)
                == vec![
                    (DbAccess::Write, "posts".to_string()),
                    (DbAccess::Write, "users".to_string()),
                ]
        );
    }

    #[test]
    fn insert_returning_is_a_single_write_not_also_a_read() {
        // `get_result` is a read terminal, but an `insert_into(…).get_result()` is
        // an INSERT … RETURNING — counted once as a write, not also a read.
        let src = r#"
            fn create(conn: &mut PgConnection) -> Account {
                diesel::insert_into(accounts::table).values(&n).get_result(conn).unwrap()
            }
        "#;
        check!(accesses(src) == vec![(DbAccess::Write, "accounts".to_string())]);
    }

    #[test]
    fn find_get_result_is_a_read() {
        let src = r#"
            fn one(conn: &mut PgConnection) -> User {
                users::table.find(1).get_result(conn).unwrap()
            }
        "#;
        check!(accesses(src) == vec![(DbAccess::Read, "users".to_string())]);
    }
}
