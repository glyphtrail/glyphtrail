//! Embedded database-query extraction (#416, Phase B).
//!
//! sqlx carries the SQL as a string-literal argument in both its macro form
//! (`sqlx::query!("SELECT … FROM users")`, `query_as!(Row, "…")`) and its
//! function form (`sqlx::query("…")`, `query_as::<_, T>("…")`). This walks the
//! Rust AST for both, pulls the SQL literal, and parses it for the tables it
//! reads or writes via [`crate::sql::extract_query_access`]. The analyze layer
//! then ties each query site to its enclosing function and to the `Table` nodes
//! by name, producing `Reads`/`Writes` edges.
//!
//! Raw drivers are also handled by a method-call whose string argument looks like
//! SQL: Rust (rusqlite, tokio-postgres — `conn.execute("…")` / `query_row` /
//! `prepare`), JS/TS (`pg`/`mysql2` `pool.query("…")`, knex `db.raw("…")`), and
//! Python (DB-API `cursor.execute("…")` / `executemany`, and SQLAlchemy core's
//! `text("…")` argument incidentally) (#440). A dynamically-built query with no
//! string literal is skipped; the schema-aware ORMs (Diesel/ActiveRecord/
//! SQLAlchemy-models/Prisma) are a follow-up.

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

/// Extract embedded DB queries from `source`, dispatched by language: Rust (sqlx,
/// raw drivers, `format!` templates), JS/TS, and Python raw drivers. Any other
/// language yields nothing.
pub fn extract_db_queries(source: &str, lang: &Language) -> Vec<RawDbQuery> {
    match lang {
        Language::Rust => extract_rust(source),
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            extract_driver_calls(source, lang, &JS_DIALECT)
        }
        Language::Python => extract_driver_calls(source, lang, &PYTHON_DIALECT),
        _ => Vec::new(),
    }
}

/// Rust: sqlx macro/function forms, raw-driver method calls, and `format!`
/// templates (see [`query_form`]).
fn extract_rust(source: &str) -> Vec<RawDbQuery> {
    let Some(tree) = parse(source) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    walk(tree.root_node(), &mut |n| {
        // The sqlx macro/function form (definitely SQL), a raw-driver method call
        // (`conn.execute("…")`), or a `format!`-built query template — the last two
        // gated on the string looking like SQL, since those forms are common.
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

/// Per-language node kinds + driver method names for the JS/Python member-call
/// extractor, so one walk serves both grammars.
struct DriverDialect {
    /// The call node kind (`call_expression` JS, `call` Python).
    call_kind: &'static str,
    /// The member/attribute access node kind for the callee.
    member_kind: &'static str,
    /// The field on the member node holding the method name.
    method_field: &'static str,
    /// Method names whose first SQL-looking string argument is a query.
    methods: &'static [&'static str],
}

/// JS/TS: `pg`/`mysql2` `pool.query("…")` / `.execute("…")`, knex `db.raw("…")`.
/// The names are common, so each is gated on the string looking like SQL.
const JS_DIALECT: DriverDialect = DriverDialect {
    call_kind: "call_expression",
    member_kind: "member_expression",
    method_field: "property",
    methods: &["query", "execute", "raw"],
};

/// Python: DB-API 2.0 cursor/connection methods (psycopg, sqlite3, mysql-connector,
/// …). SQLAlchemy core's `connection.execute(text("…"))` is also caught — the SQL
/// string sits inside the `text(...)` argument and is found by the same scan.
const PYTHON_DIALECT: DriverDialect = DriverDialect {
    call_kind: "call",
    member_kind: "attribute",
    method_field: "attribute",
    methods: &["execute", "executemany", "executescript"],
};

/// JS/Python raw drivers (#440): a method call `recv.<method>("SQL …")` whose
/// method is a known driver entry point and whose first string/template argument
/// looks like SQL. The string is taken only from the call's own arguments (not its
/// receiver), so a chained `pool.query("…").then(…)` isn't double-counted.
fn extract_driver_calls(source: &str, lang: &Language, d: &DriverDialect) -> Vec<RawDbQuery> {
    let Some(tree) = parse_with(source, lang) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    walk(tree.root_node(), &mut |n| {
        if n.kind() != d.call_kind {
            return;
        }
        let Some(func) = n.child_by_field_name("function") else {
            return;
        };
        if func.kind() != d.member_kind {
            return;
        }
        let Some(method) = func
            .child_by_field_name(d.method_field)
            .map(|m| text(m, src))
        else {
            return;
        };
        if !d.methods.contains(&method.as_str()) {
            return;
        }
        let Some(args) = n.child_by_field_name("arguments") else {
            return;
        };
        let Some(sql) = sql_literal(args, src) else {
            return; // a dynamically-built query (no string literal) — nothing to match
        };
        if !looks_like_sql(&sql) {
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

/// The first string/template literal under `node`, fragment chunks concatenated
/// with spaces so an interpolation (`${…}` / f-string `{…}`) or placeholder gap
/// can't fuse two SQL tokens. Covers JS `string`/`template_string` (`string_fragment`
/// chunks) and Python `string` (`string_content` chunks).
fn sql_literal(node: Node, src: &[u8]) -> Option<String> {
    let str_node = find_first(node, &["string", "template_string"])?;
    let mut parts = Vec::new();
    walk(str_node, &mut |n| {
        if matches!(n.kind(), "string_fragment" | "string_content") {
            parts.push(text(n, src));
        }
    });
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// The first node under `node` (pre-order, so outermost) whose kind is in `kinds`.
fn find_first<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut found = None;
    walk(node, &mut |n| {
        if found.is_none() && kinds.contains(&n.kind()) {
            found = Some(n);
        }
    });
    found
}

/// Parse `source` with the built-in grammar for `lang`.
fn parse_with(source: &str, lang: &Language) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&registry::grammar(lang)?).ok()?;
    parser.parse(source, None)
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
            if SQLX_QUERY_MACROS.contains(&name.as_str()) {
                Some(false)
            } else if matches!(name.as_str(), "format" | "format_args") {
                // A `format!`-built query template (#446): scan the literal part,
                // gated on it looking like SQL so a non-query `format!` is ignored.
                Some(true)
            } else {
                None
            }
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

    fn tables_in(src: &str, lang: &Language) -> Vec<(DbAccess, String)> {
        let mut q: Vec<(DbAccess, String)> = extract_db_queries(src, lang)
            .into_iter()
            .flat_map(|r| r.accesses)
            .collect();
        q.sort_by(|a, b| {
            (a.1.clone(), format!("{:?}", a.0)).cmp(&(b.1.clone(), format!("{:?}", b.0)))
        });
        q
    }

    fn tables(src: &str) -> Vec<(DbAccess, String)> {
        tables_in(src, &Language::Rust)
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
    fn format_built_query_template_is_caught() {
        // A query assembled with `format!` (missed by the precise call detection,
        // since the call gets a non-literal) is read from its template (#446); the
        // `{unit}` interpolation collapses to a dynamic path-segment, the static
        // `FROM` table survives.
        let src = r#"
            fn report(db: &Pool, unit: &str) {
                let q = format!("SELECT * FROM public.items_segments seg WHERE g = {unit}");
                let _ = sqlx::query(&q).fetch_all(db);
            }
        "#;
        check!(tables(src) == vec![(DbAccess::Read, "public.items_segments".to_string())]);
    }

    #[test]
    fn non_query_format_is_not_matched() {
        // A `format!` that isn't a query (no SQL leading keyword) is ignored.
        let src = r#"fn f() { let _ = format!("loaded {} rows from cache", n); }"#;
        check!(extract_db_queries(src, &Language::Rust).is_empty());
    }

    #[test]
    fn format_with_interpolated_table_does_not_emit_a_keyword_table() {
        // `FROM {}` (an interpolated table) must not collapse onto `WHERE` as a
        // bogus table; the real JOINed table is still read (#446 review).
        let src = r#"
            fn f(db: &Pool, t: &str) {
                let q = format!("SELECT * FROM {} JOIN real_table r WHERE x = 1", t);
                let _ = sqlx::query(&q).fetch_all(db);
            }
        "#;
        check!(tables(src) == vec![(DbAccess::Read, "real_table".to_string())]);
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
    fn extracts_js_driver_queries() {
        // pg/mysql2 `.query`/`.execute` + knex `.raw`, across string and template
        // forms. #440
        let src = r#"
            async function load(pool) {
                await pool.query('SELECT id, email FROM users WHERE id = $1', [id]);
                await db.execute("INSERT INTO audit_log (msg) VALUES (?)", [m]);
                await knex.raw(`UPDATE accounts SET balance = balance - 1`);
            }
        "#;
        check!(
            tables_in(src, &Language::JavaScript)
                == vec![
                    (DbAccess::Write, "accounts".to_string()),
                    (DbAccess::Write, "audit_log".to_string()),
                    (DbAccess::Read, "users".to_string()),
                ]
        );
    }

    #[test]
    fn js_driver_methods_are_gated_on_looking_like_sql() {
        // A generic `.query`/`.execute`/`.raw` whose string isn't SQL is ignored,
        // even when it mentions `from`.
        let src = r#"
            function f(logger, el) {
                logger.query("loaded from the cache");
                el.execute("do the thing");
            }
        "#;
        check!(extract_db_queries(src, &Language::JavaScript).is_empty());
    }

    #[test]
    fn ts_driver_queries_extract_too() {
        // The TypeScript grammar uses the same member-call shape.
        let src = r#"
            async function f(pool: Pool): Promise<void> {
                await pool.query("DELETE FROM sessions WHERE expired");
            }
        "#;
        check!(
            tables_in(src, &Language::TypeScript)
                == vec![(DbAccess::Write, "sessions".to_string())]
        );
    }

    #[test]
    fn extracts_python_dbapi_queries() {
        // DB-API 2.0 cursor methods (psycopg/sqlite3/…). #440
        let src = "\
def load(cur):
    cur.execute(\"SELECT id FROM users WHERE id = %s\", (uid,))
    cur.executemany(\"INSERT INTO logs (msg) VALUES (%s)\", rows)
";
        check!(
            tables_in(src, &Language::Python)
                == vec![
                    (DbAccess::Write, "logs".to_string()),
                    (DbAccess::Read, "users".to_string()),
                ]
        );
    }

    #[test]
    fn python_fstring_query_collapses_interpolation() {
        // An f-string query keeps its static tables; the `{unit}` interpolation
        // collapses rather than fusing tokens.
        let src = "\
def report(cur, unit):
    cur.execute(f\"SELECT * FROM items WHERE g = {unit}\")
";
        check!(tables_in(src, &Language::Python) == vec![(DbAccess::Read, "items".to_string())]);
    }

    #[test]
    fn python_sqlalchemy_text_argument_is_found() {
        // SQLAlchemy core: `conn.execute(text("…"))` — the SQL is inside the
        // `text(...)` call argument and still resolves.
        let src = "\
def purge(conn):
    conn.execute(text(\"DELETE FROM sessions WHERE expired\"))
";
        check!(
            tables_in(src, &Language::Python) == vec![(DbAccess::Write, "sessions".to_string())]
        );
    }

    #[test]
    fn python_non_db_execute_is_ignored() {
        let src = "\
def f(job):
    job.execute(\"run the task\")
";
        check!(extract_db_queries(src, &Language::Python).is_empty());
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
