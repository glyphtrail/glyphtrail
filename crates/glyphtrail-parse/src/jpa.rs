//! JPA / Hibernate extraction (#416, Phase B — Java).
//!
//! A Spring Data / JPA app describes its schema in code, not `.sql`: an
//! `@Entity` class is a table (named by `@Table(name=…)` or the class), its fields
//! are columns. The access side is the repositories and `@Query` methods. This
//! walks the Java AST for both:
//!
//! - `@Entity` → a [`NodeKind::Table`] + a [`NodeKind::Column`] per field;
//! - a Spring Data repository (`extends JpaRepository<Entity, …>`) → each method
//!   reads/writes the managed entity: a derived name (`findBy…` vs `save`/`delete…`)
//!   or a `@Query` (native SQL via [`crate::sql::extract_query_access`], JPQL via
//!   the same parser since `FROM Entity` reads identically).
//!
//! There are two name spaces — **entity** names (used by repositories and JPQL)
//! and **table** names (`@Table` / native SQL). The extractor emits both the
//! per-entity `(entity, table)` mapping and access refs verbatim; the analyze
//! layer maps an entity ref to its table before linking, so all three (entity,
//! native table, and a `.sql` migration table of the same name) line up.

use tree_sitter::{Node, Parser, Tree};

use glyphtrail_core::{
    CodeGraph, Confidence, EdgeKind, Language, Node as GraphNode, NodeId, NodeKind,
};

use crate::registry;
use crate::sql::{DbAccess, extract_query_access, normalize_name, table_node_id};

/// A repository method or `@Query` that reads/writes tables/entities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JpaAccess {
    /// Byte offset of the method, for enclosing-method resolution.
    pub byte: usize,
    /// `(access, normalized entity-or-table name)` the method touches.
    pub accesses: Vec<(DbAccess, String)>,
}

/// The JPA schema + access sites extracted from one Java file.
pub struct JpaExtract {
    /// Table + column nodes for the file's `@Entity` classes.
    pub graph: CodeGraph,
    /// `(normalized entity name, normalized table name)` for each entity, so the
    /// analyze layer can map an entity ref (from a repo/JPQL) to its table.
    pub entity_tables: Vec<(String, String)>,
    /// Repository/`@Query` access sites.
    pub accesses: Vec<JpaAccess>,
}

/// Spring Data repository base interfaces whose first type argument is the entity.
const REPO_BASES: &[&str] = &[
    "JpaRepository",
    "CrudRepository",
    "PagingAndSortingRepository",
    "ReactiveCrudRepository",
    "Repository",
    "JpaSpecificationExecutor",
];

/// Derived-query method-name prefixes that read.
const READ_PREFIXES: &[&str] = &[
    "find", "get", "read", "query", "count", "exists", "search", "stream",
];
/// Derived-query method-name prefixes that write.
const WRITE_PREFIXES: &[&str] = &[
    "save", "delete", "remove", "insert", "update", "modify", "persist",
];

/// Extract the JPA schema + access sites from `source`. Java only.
pub fn extract_jpa(rel_path: &str, file_id: &NodeId, source: &str, lang: &Language) -> JpaExtract {
    let mut graph = CodeGraph::new();
    let mut entity_tables = Vec::new();
    let mut accesses = Vec::new();
    if *lang != Language::Java {
        return JpaExtract {
            graph,
            entity_tables,
            accesses,
        };
    }
    let Some(tree) = parse(source) else {
        return JpaExtract {
            graph,
            entity_tables,
            accesses,
        };
    };
    let src = source.as_bytes();
    walk(tree.root_node(), &mut |n| match n.kind() {
        "class_declaration" => {
            if let Some(entity) = extract_entity(n, src) {
                let table_norm = normalize_name(&entity.table_name);
                entity_tables.push((normalize_name(&entity.entity_name), table_norm.clone()));
                let table_id = table_node_id(rel_path, &table_norm);
                let line = line_of(source, entity.byte);
                add_table(
                    &mut graph,
                    file_id,
                    &table_id,
                    &entity.table_name,
                    &table_norm,
                    rel_path,
                    entity.byte,
                    line,
                );
                for col in &entity.columns {
                    add_column(&mut graph, rel_path, &table_id, &table_norm, col);
                }
            }
        }
        "interface_declaration" => {
            if let Some(entity) = repository_entity(n, src) {
                collect_repo_accesses(n, src, &entity, &mut accesses);
            }
        }
        _ => {}
    });
    JpaExtract {
        graph,
        entity_tables,
        accesses,
    }
}

/// A parsed `@Entity` class.
struct Entity {
    entity_name: String,
    table_name: String,
    columns: Vec<String>,
    byte: usize,
}

/// Parse a `class_declaration` as a JPA entity, or `None` if it isn't `@Entity`.
fn extract_entity(class: Node, src: &[u8]) -> Option<Entity> {
    let mods = modifiers_of(class)?;
    let entity_anno = find_annotation(mods, "Entity", src)?;
    let name_node = class.child_by_field_name("name")?;
    let class_name = text(name_node, src);
    // Entity name (what JPQL/repos reference): @Entity(name="…") overrides the class.
    let entity_name = anno_string(entity_anno, "name", src).unwrap_or_else(|| class_name.clone());
    // Table name: @Table(name="…") wins, else the entity name (Hibernate default).
    let table_name = find_annotation(mods, "Table", src)
        .and_then(|t| anno_string(t, "name", src))
        .unwrap_or_else(|| entity_name.clone());
    let mut columns = Vec::new();
    if let Some(body) = class.child_by_field_name("body") {
        let mut cur = body.walk();
        for field in body.named_children(&mut cur) {
            if field.kind() != "field_declaration" {
                continue;
            }
            // @Transient fields aren't persisted; static fields aren't state.
            if let Some(m) = modifiers_of(field)
                && (find_annotation(m, "Transient", src).is_some() || has_static(m, src))
            {
                continue;
            }
            // Column name: @Column(name=…) / @JoinColumn(name=…) override the field.
            let col_override = modifiers_of(field)
                .and_then(|m| {
                    find_annotation(m, "Column", src)
                        .or_else(|| find_annotation(m, "JoinColumn", src))
                })
                .and_then(|a| anno_string(a, "name", src));
            if let Some(decl) = field.child_by_field_name("declarator")
                && let Some(fname) = decl.child_by_field_name("name")
            {
                columns.push(col_override.unwrap_or_else(|| text(fname, src)));
            }
        }
    }
    Some(Entity {
        entity_name,
        table_name,
        columns,
        byte: class.start_byte(),
    })
}

/// The entity type a repository interface manages: the first type argument of an
/// `extends`ed Spring Data base (`JpaRepository<User, Long>` → `User`).
fn repository_entity(iface: Node, src: &[u8]) -> Option<String> {
    let ext = iface
        .named_children(&mut iface.walk())
        .find(|c| c.kind() == "extends_interfaces")?;
    let mut entity = None;
    walk(ext, &mut |n| {
        if entity.is_some() || n.kind() != "generic_type" {
            return;
        }
        // The base may be a bare `JpaRepository` or a qualified
        // `org.springframework….JpaRepository`; match on the last segment.
        let base = n
            .named_children(&mut n.walk())
            .find(|c| matches!(c.kind(), "type_identifier" | "scoped_type_identifier"))
            .map(|c| type_last_segment(&text(c, src)));
        if base.as_deref().is_some_and(|b| REPO_BASES.contains(&b))
            && let Some(args) = n
                .named_children(&mut n.walk())
                .find(|c| c.kind() == "type_arguments")
            && let Some(first) = args
                .named_children(&mut args.walk())
                .find(|c| matches!(c.kind(), "type_identifier" | "scoped_type_identifier"))
        {
            entity = Some(type_last_segment(&text(first, src)));
        }
    });
    entity
}

/// Every method in a repository → an access on the managed entity (or the
/// tables/entities of its `@Query`).
fn collect_repo_accesses(iface: Node, src: &[u8], entity: &str, out: &mut Vec<JpaAccess>) {
    let entity_ref = normalize_name(entity);
    let Some(body) = iface.child_by_field_name("body") else {
        return;
    };
    let mut cur = body.walk();
    for m in body.named_children(&mut cur) {
        if m.kind() != "method_declaration" {
            continue;
        }
        // Use the method name's offset (inside the method node's span) so the
        // enclosing-method resolution in analyze lands on this method.
        let byte = m
            .child_by_field_name("name")
            .map(|n| n.start_byte())
            .unwrap_or_else(|| m.start_byte());
        // A @Query (native SQL or JPQL) is authoritative; else fall back to the
        // derived method name.
        let query = modifiers_of(m).and_then(|mods| find_annotation(mods, "Query", src));
        let accesses = if let Some(q) = query {
            let sql = anno_string(q, "value", src).or_else(|| anno_positional_string(q, src));
            match sql {
                // JPQL `FROM Entity` and native `FROM table` both parse here; an
                // entity ref is mapped to its table in the analyze layer.
                Some(s) => extract_query_access(&s),
                None => Vec::new(),
            }
        } else if let Some(name) = m.child_by_field_name("name").map(|n| text(n, src)) {
            match derived_access(&name) {
                Some(a) => vec![(a, entity_ref.clone())],
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        if !accesses.is_empty() {
            out.push(JpaAccess { byte, accesses });
        }
    }
}

/// The access a Spring Data derived method name implies (`findByEmail` → read,
/// `deleteAllByOrg` → write), or `None` for a name that isn't a known verb.
fn derived_access(method: &str) -> Option<DbAccess> {
    let lower = method.to_ascii_lowercase();
    if WRITE_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        Some(DbAccess::Write)
    } else if READ_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        Some(DbAccess::Read)
    } else {
        None
    }
}

// --- Java annotation helpers ---------------------------------------------------

/// The `modifiers` child of a declaration (annotations + keywords). It's a child
/// node, not a named field, so it's located by kind rather than `child_by_field_name`.
fn modifiers_of(node: Node) -> Option<Node> {
    node.named_children(&mut node.walk())
        .find(|c| c.kind() == "modifiers")
}

/// The annotation node named `name` (`@Table`, `@Query`, …) on a `modifiers`
/// node, matching both `marker_annotation` and `annotation` forms.
fn find_annotation<'a>(modifiers: Node<'a>, name: &str, src: &[u8]) -> Option<Node<'a>> {
    modifiers.named_children(&mut modifiers.walk()).find(|c| {
        matches!(c.kind(), "marker_annotation" | "annotation")
            && c.child_by_field_name("name")
                .map(|i| type_last_segment(&text(i, src)))
                .as_deref()
                == Some(name)
    })
}

/// The last `.`-separated segment of a (possibly-qualified) type/annotation name.
fn type_last_segment(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).trim().to_string()
}

/// The string value of `key` in `@X(key = "…")`.
fn anno_string(anno: Node, key: &str, src: &[u8]) -> Option<String> {
    let args = anno.child_by_field_name("arguments")?;
    args.named_children(&mut args.walk())
        .filter(|c| c.kind() == "element_value_pair")
        .find(|p| {
            p.named_children(&mut p.walk())
                .find(|i| i.kind() == "identifier")
                .map(|i| text(i, src))
                .as_deref()
                == Some(key)
        })
        .and_then(|p| string_value(p, src))
}

/// The string of a positional argument, `@Query("…")`.
fn anno_positional_string(anno: Node, src: &[u8]) -> Option<String> {
    let args = anno.child_by_field_name("arguments")?;
    args.named_children(&mut args.walk())
        .find(|c| c.kind() == "string_literal")
        .and_then(|s| string_fragment(s, src))
}

/// The string literal value anywhere under `node` (the `string_fragment`, so
/// quotes are already stripped).
fn string_value(node: Node, src: &[u8]) -> Option<String> {
    let mut found = None;
    walk(node, &mut |n| {
        if found.is_none() && n.kind() == "string_fragment" {
            found = Some(text(n, src));
        }
    });
    found
}

fn string_fragment(string_literal: Node, src: &[u8]) -> Option<String> {
    string_literal
        .named_children(&mut string_literal.walk())
        .find(|c| c.kind() == "string_fragment")
        .map(|c| text(c, src))
}

/// Whether a `modifiers` node carries the `static` keyword.
fn has_static(modifiers: Node, src: &[u8]) -> bool {
    text(modifiers, src)
        .split_whitespace()
        .any(|w| w == "static")
}

// --- shared low-level helpers --------------------------------------------------

fn parse(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&registry::grammar(&Language::Java).expect("built-in grammar"))
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
/// offset (tree-sitter offsets can land inside a multibyte character) never panics.
fn line_of(source: &str, byte: usize) -> usize {
    let end = byte.min(source.len());
    source.as_bytes()[..end]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

/// Add a `Table` node for a JPA entity (located at its class declaration `byte`)
/// plus the `file → table` containment edge. File-scoped id, matching the `.sql`
/// scheme so the same table name across sources resolves by name in analyze.
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
        language: Some("java".to_string()),
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
        language: Some("java".to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn extract(src: &str) -> JpaExtract {
        let fid = NodeId::derive(&["file", "X.java"]);
        extract_jpa("X.java", &fid, src, &Language::Java)
    }

    #[test]
    fn entity_becomes_a_table_with_columns() {
        let src = r#"
            @Entity
            @Table(name = "users")
            public class User {
                @Id Long id;
                @Column(name = "email_addr") private String email;
                @Transient private String tmp;
            }
        "#;
        let e = extract(src);
        let tables: Vec<_> = e
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Table)
            .collect();
        check!(tables.len() == 1);
        check!(tables[0].qualified_name == "users");
        let cols: Vec<&str> = e
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Column)
            .map(|n| n.name.as_str())
            .collect();
        check!(cols.contains(&"id"));
        check!(cols.contains(&"email_addr")); // @Column(name) override
        check!(!cols.contains(&"tmp")); // @Transient excluded
        check!(e.entity_tables == vec![("user".to_string(), "users".to_string())]);
    }

    #[test]
    fn entity_without_table_annotation_uses_class_name() {
        let e = extract("@Entity class Org { Long id; }");
        check!(e.entity_tables == vec![("org".to_string(), "org".to_string())]);
    }

    #[test]
    fn entity_name_override_is_used_for_jpql_mapping() {
        // @Entity(name="Acct") sets the JPQL entity name; the table defaults to it.
        let e = extract("@Entity(name = \"Acct\") class AccountEntity { Long id; }");
        check!(e.entity_tables == vec![("acct".to_string(), "acct".to_string())]);
    }

    fn refs(e: &JpaExtract) -> Vec<(DbAccess, String)> {
        let mut a: Vec<_> = e.accesses.iter().flat_map(|x| x.accesses.clone()).collect();
        a.sort_by(|x, y| {
            (x.1.clone(), format!("{:?}", x.0)).cmp(&(y.1.clone(), format!("{:?}", y.0)))
        });
        a
    }

    #[test]
    fn repository_derived_methods_read_and_write_the_entity() {
        let src = r#"
            public interface UserRepository extends JpaRepository<User, Long> {
                Optional<User> findByEmail(String e);
                void deleteByOrgId(Long id);
                String somethingElse();
            }
        "#;
        // findBy… reads `user`, deleteBy… writes `user`; the non-verb method is ignored.
        check!(
            refs(&extract(src))
                == vec![
                    (DbAccess::Read, "user".to_string()),
                    (DbAccess::Write, "user".to_string()),
                ]
        );
    }

    #[test]
    fn query_native_and_jpql() {
        let src = r#"
            public interface R extends CrudRepository<User, Long> {
                @Query(value = "SELECT * FROM users WHERE id = ?1", nativeQuery = true) User raw(Long id);
                @Query("SELECT u FROM Account u WHERE u.active = true") java.util.List<Account> active();
            }
        "#;
        let r = refs(&extract(src));
        // native query → table `users`; JPQL → entity `account` (mapped to a table later).
        check!(r.contains(&(DbAccess::Read, "users".to_string())));
        check!(r.contains(&(DbAccess::Read, "account".to_string())));
    }
}
