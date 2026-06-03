//! Embedded database-query extraction (#416, Phase B) — Rust / sqlx first.
//!
//! sqlx carries the SQL as a string-literal argument in both its macro form
//! (`sqlx::query!("SELECT … FROM users")`, `query_as!(Row, "…")`) and its
//! function form (`sqlx::query("…")`, `query_as::<_, T>("…")`). This walks the
//! Rust AST for both, pulls the SQL literal, and parses it for the tables it
//! reads or writes via [`crate::sql::extract_query_access`]. The analyze layer
//! then ties each query site to its enclosing function and to the `Table` nodes
//! by name, producing `Reads`/`Writes` edges.
//!
//! Raw drivers (rusqlite, tokio-postgres) are also handled: a method call like
//! `conn.execute("…")` / `query_row` / `prepare` whose string argument looks like
//! SQL. A dynamically-built query (no string literal) is skipped; JDBC, the JS/
//! Python drivers, and ORMs are a follow-up.

use tree_sitter::{Node, Parser, Tree};

use glyphtrail_core::Language;

use crate::registry;
use crate::sql::{DbAccess, extract_query_access};

/// One embedded query site: where it sits (for enclosing-function resolution) and
/// the tables it accesses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDbQuery {
    /// Byte offset of the query macro, used to find its enclosing function.
    pub byte: usize,
    /// `(access, normalized table name)` pairs the query touches.
    pub accesses: Vec<(DbAccess, String)>,
}

/// sqlx query macros whose first string-literal argument is the SQL.
const SQLX_QUERY_MACROS: &[&str] = &[
    "query",
    "query_as",
    "query_scalar",
    "query_unchecked",
    "query_as_unchecked",
    "query_scalar_unchecked",
];

/// Extract embedded DB queries from `source`. Rust/sqlx only for now; other
/// languages yield nothing.
pub fn extract_db_queries(source: &str, lang: &Language) -> Vec<RawDbQuery> {
    if *lang != Language::Rust {
        return Vec::new();
    }
    let Some(tree) = parse(source) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    walk(tree.root_node(), &mut |n| {
        // A query is the sqlx macro/function form (definitely SQL), or a raw-driver
        // method call (`conn.execute("…")`, `query_row`, …) — the latter is gated
        // on the string actually looking like SQL, since the method names are common.
        let Some(needs_sql_gate) = query_form(n, src) else {
            return;
        };
        // Search only the call's own arguments for the SQL string, not the whole
        // expression — otherwise `sqlx::query("…").execute(db)` would read the
        // string out of its receiver and double-count it.
        let search = match n.kind() {
            "call_expression" => match n.child_by_field_name("arguments") {
                Some(a) => a,
                None => return,
            },
            _ => n,
        };
        let Some(sql) = first_string(search, src) else {
            return; // a non-literal (dynamically built) query — nothing to match
        };
        if needs_sql_gate && !looks_like_sql(&sql) {
            return;
        }
        let accesses = extract_query_access(&sql);
        if !accesses.is_empty() {
            out.push(RawDbQuery {
                byte: n.start_byte(),
                accesses,
            });
        }
    });
    out
}

/// Raw-driver methods (rusqlite, tokio-postgres, …) whose first string argument is
/// SQL. Names are common, so a match is only treated as a query when the string
/// also `looks_like_sql`.
const RAW_QUERY_METHODS: &[&str] = &[
    "execute",
    "execute_batch",
    "batch_execute",
    "query",
    "query_row",
    "query_map",
    "query_one",
    "query_opt",
    "query_raw",
    "prepare",
    "prepare_cached",
];

/// Whether `n` is a recognised query call, and whether the SQL string needs the
/// `looks_like_sql` gate (raw-driver method calls do; the namespaced sqlx forms
/// don't). `None` if `n` isn't a query call.
fn query_form(n: Node, src: &[u8]) -> Option<bool> {
    match n.kind() {
        "macro_invocation" => {
            let name = last_segment(&text(n.child_by_field_name("macro")?, src));
            SQLX_QUERY_MACROS.contains(&name.as_str()).then_some(false)
        }
        "call_expression" => {
            let func = n.child_by_field_name("function")?;
            if func.kind() == "field_expression" {
                // A method call `recv.<method>(…)` — a raw driver, gated.
                let method = text(func.child_by_field_name("field")?, src);
                RAW_QUERY_METHODS.contains(&method.as_str()).then_some(true)
            } else {
                // A free function — the sqlx forms, not gated.
                is_sqlx_query_call(&callee_path(func, src)?).then_some(false)
            }
        }
        _ => None,
    }
}

/// Whether a string begins with a SQL DML keyword (`SELECT`/`INSERT`/`UPDATE`/
/// `DELETE`/`WITH`), the gate that keeps a generic `.execute("…")` from matching a
/// non-SQL string. Leading SQL comments are skipped first, so a query that opens
/// with `-- …` or `/* … */` is still recognised.
fn looks_like_sql(s: &str) -> bool {
    let first = strip_leading_sql_comments(s)
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        first.as_str(),
        "select" | "insert" | "update" | "delete" | "with"
    )
}

/// Skip leading whitespace and `--`/`/* */` comments at the start of a SQL string.
fn strip_leading_sql_comments(s: &str) -> &str {
    let mut s = s.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.find('\n').map_or("", |i| &rest[i + 1..]).trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.find("*/").map_or("", |i| &rest[i + 2..]).trim_start();
        } else {
            return s;
        }
    }
}

/// The callable path of a `call_expression`'s `function`, unwrapping a turbofish
/// (`query_as::<_, T>` → `query_as`); `None` for non-path callees (a method call
/// `conn.query(…)` is a `field_expression`, not a sqlx free function).
fn callee_path(func: Node, src: &[u8]) -> Option<String> {
    let node = if func.kind() == "generic_function" {
        func.child_by_field_name("function").unwrap_or(func)
    } else {
        func
    };
    match node.kind() {
        "identifier" | "scoped_identifier" => Some(text(node, src)),
        _ => None,
    }
}

/// Whether a call path names a sqlx query free function. `query`/`query_unchecked`
/// require an explicit `sqlx::` so a bare `query(…)` from another crate isn't a
/// false positive; the `_as`/`_scalar` names are distinctive enough on their own.
fn is_sqlx_query_call(path: &str) -> bool {
    let seg = last_segment(path);
    match seg.as_str() {
        "query_as" | "query_scalar" | "query_as_unchecked" | "query_scalar_unchecked" => true,
        "query" | "query_unchecked" => path.contains("sqlx"),
        _ => false,
    }
}

fn parse(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&registry::grammar(&Language::Rust).expect("built-in grammar"))
        .ok()?;
    parser.parse(source, None)
}

/// The inner text of the first string literal anywhere under `node` (the
/// `string_content` child, so quotes / raw-string `r#"…"#` delimiters are already
/// stripped by the grammar).
fn first_string(node: Node, src: &[u8]) -> Option<String> {
    let mut found = None;
    walk(node, &mut |n| {
        if found.is_none() && n.kind() == "string_content" {
            found = Some(text(n, src));
        }
    });
    found
}

/// Last `::`-separated segment of a path (`sqlx::query` -> `query`).
fn last_segment(path: &str) -> String {
    path.rsplit("::").next().unwrap_or(path).trim().to_string()
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

    fn tables(src: &str) -> Vec<(DbAccess, String)> {
        let mut q: Vec<(DbAccess, String)> = extract_db_queries(src, &Language::Rust)
            .into_iter()
            .flat_map(|r| r.accesses)
            .collect();
        q.sort_by(|a, b| {
            (a.1.clone(), format!("{:?}", a.0)).cmp(&(b.1.clone(), format!("{:?}", b.0)))
        });
        q
    }

    #[test]
    fn extracts_sqlx_query_macros() {
        let src = r#"
            async fn load(db: &Pool) -> Vec<User> {
                sqlx::query_as!(User, "SELECT id, email FROM users WHERE id = $1", id)
                    .fetch_all(db).await.unwrap()
            }
            async fn record(db: &Pool) {
                sqlx::query!("INSERT INTO audit_log (msg) VALUES ($1)", m).execute(db).await.ok();
            }
        "#;
        check!(
            tables(src)
                == vec![
                    (DbAccess::Write, "audit_log".to_string()),
                    (DbAccess::Read, "users".to_string()),
                ]
        );
    }

    #[test]
    fn handles_raw_string_queries_and_bare_macro() {
        let src = r##"
            fn f(db: &Pool) {
                let _ = query!(r#"UPDATE accounts SET balance = balance - $1 WHERE id = $2"#, a, b);
            }
        "##;
        check!(tables(src) == vec![(DbAccess::Write, "accounts".to_string())]);
    }

    #[test]
    fn handles_the_function_form() {
        // `sqlx::query("…")` and `query_as::<_, T>("…")` — the dominant non-macro
        // sqlx usage.
        let src = r#"
            async fn f(db: &Pool) {
                sqlx::query("DELETE FROM sessions WHERE expired").execute(db).await.ok();
                let _ = query_as::<_, Row>("SELECT id FROM accounts WHERE id = $1").fetch_one(db).await;
            }
        "#;
        check!(
            tables(src)
                == vec![
                    (DbAccess::Read, "accounts".to_string()),
                    (DbAccess::Write, "sessions".to_string()),
                ]
        );
    }

    #[test]
    fn handles_raw_driver_method_calls() {
        // rusqlite / tokio-postgres: SQL in a `.execute`/`.query_row`/`.prepare` arg.
        let src = r#"
            fn f(conn: &Connection) {
                conn.execute("INSERT INTO logs (msg) VALUES (?1)", params![m]).unwrap();
                let _ = conn.query_row("SELECT id FROM users WHERE id = ?1", [id], row);
                let mut stmt = conn.prepare("SELECT * FROM accounts").unwrap();
            }
        "#;
        check!(
            tables(src)
                == vec![
                    (DbAccess::Read, "accounts".to_string()),
                    (DbAccess::Write, "logs".to_string()),
                    (DbAccess::Read, "users".to_string()),
                ]
        );
    }

    #[test]
    fn raw_query_with_leading_comment_is_recognised() {
        // A SQL string opening with a comment still passes the looks_like_sql gate.
        let src = r#"
            fn f(conn: &Connection) {
                conn.query_row("/* pick the row */ SELECT id FROM widgets WHERE id = ?1", [], r);
            }
        "#;
        check!(tables(src) == vec![(DbAccess::Read, "widgets".to_string())]);
    }

    #[test]
    fn raw_method_calls_are_gated_on_looking_like_sql() {
        // A generic `.execute`/`.query` whose string isn't SQL is not a query —
        // even though the string mentions `from`.
        let src = r#"
            fn f(x: &Runner, log: &Log) {
                x.execute("do the thing");
                log.query("loaded from the cache");
            }
        "#;
        check!(extract_db_queries(src, &Language::Rust).is_empty());
    }

    #[test]
    fn ignores_non_sqlx_macros_and_non_rust() {
        check!(extract_db_queries(r#"println!("SELECT FROM users");"#, &Language::Rust).is_empty());
        check!(
            extract_db_queries(r#"sqlx::query!("SELECT 1 FROM t");"#, &Language::Python).is_empty()
        );
        // A bare `query("…")` from some other crate is not assumed to be sqlx.
        check!(
            extract_db_queries(
                r#"fn f() { let _ = query("SELECT 1 FROM t"); }"#,
                &Language::Rust
            )
            .is_empty()
        );
    }
}
