//! SQLAlchemy declarative-model extraction (#440, Phase B — Python).
//!
//! A SQLAlchemy app describes its schema in code, not `.sql`: a declarative model
//! is a class carrying a `__tablename__ = "…"` assignment, and each class-level
//! `name = Column(…)` / `name = mapped_column(…)` (including the SQLAlchemy-2.0
//! typed form `name: Mapped[T] = mapped_column(…)`) is a column. This walks the
//! Python AST for them and emits the same schema shape JPA / Diesel / DDL produce:
//! a [`NodeKind::Table`] per model + a [`NodeKind::Column`] per field, joined by
//! `Contains` edges. The `Table`/`Column` node ids share the `sql_*` scheme, so a
//! raw `cursor.execute("SELECT … FROM users")` in the same repo (already extracted
//! by [`crate::db_client`]) resolves to the model-declared `users` table.
//!
//! `__tablename__` is the precise, SQLAlchemy-specific signal used to recognise a
//! model — there is no reliance on the (project-renamed) declarative `Base`
//! superclass. ORM *access* sites (`session.query(User)`, `User.query`, …) are a
//! follow-up; the `entity_tables` mapping is emitted now so that work can link a
//! class ref to its table.

use tree_sitter::{Node, Parser, Tree};

use glyphtrail_core::{
    CodeGraph, Confidence, EdgeKind, Language, Node as GraphNode, NodeId, NodeKind,
};

use crate::registry;
use crate::sql::{normalize_name, table_node_id};

/// The SQLAlchemy schema extracted from one Python file.
pub struct SqlAlchemyExtract {
    /// `Table` + `Column` nodes for the file's declarative models.
    pub graph: CodeGraph,
    /// `(normalized model class name, normalized table name)` per model, so the
    /// analyze layer can later map an ORM query on the class to its table.
    pub entity_tables: Vec<(String, String)>,
}

/// Constructors whose assignment marks a mapped column.
const COLUMN_CTORS: &[&str] = &["Column", "mapped_column"];

/// Extract SQLAlchemy declarative models from `source`. Python only.
pub fn extract_sqlalchemy(
    rel_path: &str,
    file_id: &NodeId,
    source: &str,
    lang: &Language,
) -> SqlAlchemyExtract {
    let mut graph = CodeGraph::new();
    let mut entity_tables = Vec::new();
    if *lang != Language::Python {
        return SqlAlchemyExtract {
            graph,
            entity_tables,
        };
    }
    let Some(tree) = parse(source) else {
        return SqlAlchemyExtract {
            graph,
            entity_tables,
        };
    };
    let src = source.as_bytes();
    walk(tree.root_node(), &mut |n| {
        if n.kind() != "class_definition" {
            return;
        }
        let Some(model) = extract_model(n, src) else {
            return;
        };
        let table_norm = normalize_name(&model.table_name);
        entity_tables.push((normalize_name(&model.class_name), table_norm.clone()));
        let table_id = table_node_id(rel_path, &table_norm);
        add_table(
            &mut graph,
            file_id,
            &table_id,
            &model.table_name,
            &table_norm,
            rel_path,
            model.byte,
            line_of(source, model.byte),
        );
        for col in &model.columns {
            add_column(&mut graph, rel_path, &table_id, &table_norm, col);
        }
    });
    SqlAlchemyExtract {
        graph,
        entity_tables,
    }
}

/// A parsed declarative model.
struct Model {
    class_name: String,
    table_name: String,
    columns: Vec<String>,
    byte: usize,
}

/// Parse a `class_definition` as a declarative model, or `None` if it has no
/// `__tablename__` (the SQLAlchemy-specific marker).
fn extract_model(class: Node, src: &[u8]) -> Option<Model> {
    let class_name = text(class.child_by_field_name("name")?, src);
    let body = class.child_by_field_name("body")?;
    let mut table_name = None;
    let mut columns = Vec::new();
    let mut cursor = body.walk();
    for stmt in body.named_children(&mut cursor) {
        // A class-body statement is `expression_statement → assignment`.
        let Some(assign) = assignment_of(stmt) else {
            continue;
        };
        let (Some(left), Some(right)) = (
            assign.child_by_field_name("left"),
            assign.child_by_field_name("right"),
        ) else {
            continue;
        };
        if left.kind() != "identifier" {
            continue; // skip tuple / attribute targets
        }
        let name = text(left, src);
        if name == "__tablename__" {
            if let Some(s) = py_string(right, src) {
                table_name = Some(s);
            }
        } else if let Some(ctor) = call_ctor(right, src)
            && COLUMN_CTORS.contains(&ctor.as_str())
        {
            // `Column("explicit", …)` names the column explicitly; otherwise the
            // attribute name is the column.
            columns.push(column_name(right, &name, src));
        }
    }
    Some(Model {
        class_name,
        table_name: table_name?,
        columns,
        byte: class.start_byte(),
    })
}

/// The `assignment` node a class-body statement carries (`x = …`, `x: T = …`), or
/// `None` (a bare expression, a docstring, a method, …).
fn assignment_of(stmt: Node) -> Option<Node> {
    if stmt.kind() != "expression_statement" {
        return None;
    }
    let mut cursor = stmt.walk();
    stmt.named_children(&mut cursor)
        .find(|c| c.kind() == "assignment")
}

/// The callee identifier of `node` when it is a direct call (`Column(…)`,
/// `mapped_column(…)`), else `None`. A namespaced `sa.Column(…)` is an
/// `attribute` callee — also resolved to its trailing name.
fn call_ctor(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "call" {
        return None;
    }
    let func = node.child_by_field_name("function")?;
    match func.kind() {
        "identifier" => Some(text(func, src)),
        "attribute" => func.child_by_field_name("attribute").map(|a| text(a, src)),
        _ => None,
    }
}

/// The column name for a `Column(…)`/`mapped_column(…)` call: the first positional
/// string argument if present (`Column("col", Integer)`), else the attribute name.
fn column_name(call: Node, attr: &str, src: &[u8]) -> String {
    let Some(args) = call.child_by_field_name("arguments") else {
        return attr.to_string();
    };
    let mut cursor = args.walk();
    for arg in args.named_children(&mut cursor) {
        if arg.kind() == "keyword_argument" {
            continue; // a kwarg isn't the positional name argument
        }
        // The first positional argument decides: a string literal is the explicit
        // column name; anything else (a type) means the attribute name stands.
        return if arg.kind() == "string" {
            py_string(arg, src).unwrap_or_else(|| attr.to_string())
        } else {
            attr.to_string()
        };
    }
    attr.to_string()
}

/// Inner text of a Python string literal (concatenated `string_content` chunks),
/// or `None` if `node` isn't a string.
fn py_string(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut parts = Vec::new();
    walk(node, &mut |n| {
        if n.kind() == "string_content" {
            parts.push(text(n, src));
        }
    });
    Some(parts.concat())
}

#[allow(clippy::too_many_arguments)]
fn add_table(
    graph: &mut CodeGraph,
    file_id: &NodeId,
    table_id: &NodeId,
    display: &str,
    table_norm: &str,
    rel_path: &str,
    byte: usize,
    line: usize,
) {
    graph.add_node(GraphNode {
        id: table_id.clone(),
        kind: NodeKind::Table,
        name: display.to_string(),
        qualified_name: table_norm.to_string(),
        file: rel_path.to_string(),
        language: Some("python".to_string()),
        span: Some(glyphtrail_core::Span {
            start_byte: byte,
            end_byte: byte,
            start_line: line,
            end_line: line,
        }),
        doc: Some("table".to_string()),
        signature: None,
    });
    graph.add_edge(
        file_id.clone(),
        table_id.clone(),
        EdgeKind::Contains,
        Confidence::Extracted,
    );
}

fn add_column(
    graph: &mut CodeGraph,
    rel_path: &str,
    table_id: &NodeId,
    table_norm: &str,
    col: &str,
) {
    let col_norm = col.to_ascii_lowercase();
    let col_id = NodeId::derive(&["sql_column", rel_path, table_norm, &col_norm]);
    graph.add_node(GraphNode {
        id: col_id.clone(),
        kind: NodeKind::Column,
        name: col.to_string(),
        qualified_name: format!("{table_norm}.{col_norm}"),
        file: rel_path.to_string(),
        language: Some("python".to_string()),
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

fn parse(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&registry::grammar(&Language::Python).expect("built-in grammar"))
        .ok()?;
    parser.parse(source, None)
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

/// 1-based line of `byte`, counting newlines over raw bytes so a non-char-boundary
/// offset never panics.
fn line_of(source: &str, byte: usize) -> usize {
    let end = byte.min(source.len());
    source.as_bytes()[..end]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn extract(src: &str) -> SqlAlchemyExtract {
        let fid = NodeId::derive(&["file", "models.py"]);
        extract_sqlalchemy("models.py", &fid, src, &Language::Python)
    }

    fn table_names(e: &SqlAlchemyExtract) -> Vec<String> {
        e.graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Table)
            .map(|n| n.qualified_name.clone())
            .collect()
    }

    fn column_names(e: &SqlAlchemyExtract) -> Vec<String> {
        let mut c: Vec<String> = e
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Column)
            .map(|n| n.name.clone())
            .collect();
        c.sort();
        c
    }

    #[test]
    fn declarative_model_becomes_a_table_with_columns() {
        let src = r#"
class User(Base):
    __tablename__ = "users"
    id = Column(Integer, primary_key=True)
    email = Column(String)
"#;
        let e = extract(src);
        check!(table_names(&e) == vec!["users".to_string()]);
        check!(column_names(&e) == vec!["email".to_string(), "id".to_string()]);
        check!(e.entity_tables == vec![("user".to_string(), "users".to_string())]);
    }

    #[test]
    fn sqlalchemy_2_typed_mapped_column_is_recognized() {
        // The SQLAlchemy-2.0 form: a typed `Mapped[...]` annotation + `mapped_column`.
        let src = r#"
class Account(Base):
    __tablename__ = "accounts"
    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str] = mapped_column()
"#;
        let e = extract(src);
        check!(table_names(&e) == vec!["accounts".to_string()]);
        check!(column_names(&e) == vec!["id".to_string(), "name".to_string()]);
    }

    #[test]
    fn explicit_column_name_overrides_the_attribute() {
        let src = r#"
class Thing(Base):
    __tablename__ = "things"
    status = Column("status_code", String)
"#;
        let e = extract(src);
        // The explicit first-string argument names the column, not the attribute.
        check!(column_names(&e) == vec!["status_code".to_string()]);
    }

    #[test]
    fn namespaced_constructor_is_recognized() {
        let src = r#"
class Widget(Base):
    __tablename__ = "widgets"
    id = sa.Column(sa.Integer, primary_key=True)
"#;
        let e = extract(src);
        check!(table_names(&e) == vec!["widgets".to_string()]);
        check!(column_names(&e) == vec!["id".to_string()]);
    }

    #[test]
    fn class_without_tablename_is_not_a_model() {
        // A plain class (no `__tablename__`) — even with Column-looking assignments —
        // is not a table; precision over recall.
        let src = r#"
class Helper:
    value = Column(String)
    def run(self):
        pass
"#;
        let e = extract(src);
        check!(table_names(&e).is_empty());
        check!(column_names(&e).is_empty());
        check!(e.entity_tables.is_empty());
    }

    #[test]
    fn table_to_column_edges_are_contains() {
        let e = extract(
            r#"
class User(Base):
    __tablename__ = "users"
    id = Column(Integer)
"#,
        );
        let table = e
            .graph
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Table)
            .expect("a Table node");
        let kinds: Vec<EdgeKind> = e
            .graph
            .edges
            .iter()
            .filter(|edge| edge.src == table.id)
            .map(|edge| edge.kind)
            .collect();
        check!(!kinds.is_empty());
        check!(kinds.iter().all(|k| *k == EdgeKind::Contains));
    }
}
