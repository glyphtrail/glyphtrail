//! LadybugDB (Cypher) graph backend (#8), behind the `ladybug` cargo feature.
//!
//! Mirrors the same graph the SQLite backend stores into LadybugDB node/rel
//! tables and translates the [`GraphStore`] surface to Cypher. Nodes live in a
//! `Node` table (plus side tables `File`, `ApiOp`, `Pending`, `Import`,
//! `Meta`); edges in one `Edge` rel table carrying `kind` + `confidence`.
//! `Connection` borrows the `Database`, so a short-lived connection is created
//! per operation. `reachable` is a Rust BFS over `neighbors` to avoid
//! version-specific recursive-Cypher syntax; full-text `search` is approximated
//! with `CONTAINS`.

use std::collections::{HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use glyphtrail_core::{
    Adjacency, ClassifiedItem, CommitMeta, Confidence, Direction, Edge, EdgeKind, Embedding,
    ImpactPolicy, Node, NodeId, NodeKind, OperationKey, PendingLink, Span, classify,
    compute_impact, is_cross_boundary_path,
};
use lbug::{Connection, Database, LogicalType, SystemConfig, Value};

use crate::Stats;
use crate::graph_store::GraphStore;

/// Max rows per `UNWIND` batch (see [`LadybugStore::exec_unwind`]). Bounds the
/// `$rows` param + query-plan intermediates so a large repo persists in steady,
/// bounded chunks instead of one unbounded execute.
const UNWIND_BATCH: usize = 4096;

/// Upsert edges, deduping on `(src, dst, kind)` and keeping/raising confidence.
/// Used for incremental updates; a fresh build bulk-loads via `copy_edges`.
const MERGE_EDGES: &str = "UNWIND $rows AS r MATCH (a:Node {id:r.src}), (b:Node {id:r.dst}) \
     MERGE (a)-[e:Edge {kind:r.ekind}]->(b) \
     ON CREATE SET e.confidence=r.conf \
     ON MATCH SET e.confidence = CASE WHEN r.conf = 'extracted' THEN 'extracted' ELSE e.confidence END";

const SCHEMA: &[&str] = &[
    "CREATE NODE TABLE IF NOT EXISTS Node(id STRING, kind STRING, name STRING, qualified_name STRING, file STRING, language STRING, start_byte INT64, end_byte INT64, start_line INT64, end_line INT64, doc STRING, signature STRING, PRIMARY KEY(id))",
    "CREATE REL TABLE IF NOT EXISTS Edge(FROM Node TO Node, kind STRING, confidence STRING)",
    "CREATE NODE TABLE IF NOT EXISTS File(path STRING, language STRING, hash STRING, PRIMARY KEY(path))",
    "CREATE NODE TABLE IF NOT EXISTS ApiOp(node_id STRING, protocol STRING, method STRING, path STRING, signature STRING, PRIMARY KEY(node_id))",
    // Atlas (#329/#330): commit attributes keyed by the Commit node's id; rows
    // carry `committed_at` (time-ordered queries) and `in_bounds` (0/1, the
    // date-window state).
    "CREATE NODE TABLE IF NOT EXISTS Commit(node_id STRING, hash STRING, author_email STRING, committed_at INT64, subject STRING, in_bounds INT64, PRIMARY KEY(node_id))",
    "CREATE NODE TABLE IF NOT EXISTS Pending(pk STRING, anchor STRING, name STRING, kind STRING, name_is_src INT64, PRIMARY KEY(pk))",
    "CREATE NODE TABLE IF NOT EXISTS Import(pk STRING, importer STRING, raw STRING, language STRING, PRIMARY KEY(pk))",
    // Atlas (#338): a dense embedding vector keyed by node id (a `Repo` today),
    // mirroring the `Commit` side-table. `vec` is the comma-joined floats; `model`
    // is the producing embedder's id, so a re-embed under a different model is
    // detectable.
    "CREATE NODE TABLE IF NOT EXISTS Embedding(node_id STRING, model STRING, dim INT64, vec STRING, PRIMARY KEY(node_id))",
    "CREATE NODE TABLE IF NOT EXISTS Meta(key STRING, value STRING, PRIMARY KEY(key))",
];

/// Property list a `Node` row returns, in order, so `row_to_node` can decode it.
const NODE_COLS: &str = "n.id, n.kind, n.name, n.qualified_name, n.file, n.language, n.start_byte, n.end_byte, n.start_line, n.end_line, n.doc, n.signature";

/// Bumped when the node/rel schema shape changes. A stored `schema_version`
/// other than this triggers a drop + recreate on open (#135); `analyze` then
/// rebuilds the index from source.
///
/// "2": the `Node` table gained a `signature` column (#344) — existing "1"
/// indexes must be rebuilt, else a COPY of the new 12-field rows fails.
pub const SCHEMA_VERSION: &str = "2";

pub struct LadybugStore {
    db: Database,
}

impl LadybugStore {
    /// Open (or create) a LadybugDB database at `path` and ensure the schema,
    /// rebuilding it if the stored schema version is stale (#135).
    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::new(path, SystemConfig::default())?;
        {
            let conn = Connection::new(&db)?;
            for ddl in SCHEMA {
                conn.query(ddl)?;
            }
        }
        let mut store = Self { db };
        store.migrate_schema()?;
        Ok(store)
    }

    /// Open without the schema migration that `open` runs. The migration
    /// drop+recreates the whole DB when `schema_version` differs, so a read-only
    /// consumer (e.g. `atlas graph-embed` reading kind counts) must not trigger it
    /// and silently wipe an out-of-date index. The idempotent `CREATE … IF NOT
    /// EXISTS` schema is still applied (only adds missing tables, never drops), so
    /// the read queries resolve.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let db = Database::new(path, SystemConfig::default())?;
        {
            let conn = Connection::new(&db)?;
            for ddl in SCHEMA {
                conn.query(ddl)?;
            }
        }
        Ok(Self { db })
    }

    /// Open a fresh store in a unique temporary directory. LadybugDB has no
    /// in-memory mode, so tests (here and in dependent crates) get an isolated
    /// on-disk database instead; the caller removes the directory when done.
    pub fn open_temp() -> Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("glyphtrail-lbug-{}-{nanos}", std::process::id()));
        Self::open(&dir)
    }

    /// Drop + recreate the LadybugDB schema when the stored `schema_version`
    /// differs from the current one. The index is rebuilt on the next `analyze`.
    fn migrate_schema(&mut self) -> Result<()> {
        if self.get_meta("schema_version")?.as_deref() == Some(SCHEMA_VERSION) {
            return Ok(());
        }
        {
            let conn = self.conn()?;
            // Drop the rel table before its node tables. Ignore "missing table"
            // on a partial DB; the schema is recreated immediately after.
            for tbl in [
                "Edge",
                "Node",
                "File",
                "ApiOp",
                "Commit",
                "Embedding",
                "Pending",
                "Import",
                "Meta",
            ] {
                let _ = conn.query(&format!("DROP TABLE {tbl}"));
            }
            for ddl in SCHEMA {
                conn.query(ddl)?;
            }
        }
        self.set_meta("schema_version", SCHEMA_VERSION)?;
        Ok(())
    }

    fn conn(&self) -> Result<Connection<'_>> {
        Ok(Connection::new(&self.db)?)
    }

    /// Run a parameterized query and collect the rows (owned values).
    fn run(&self, cypher: &str, params: Vec<(&str, Value)>) -> Result<Vec<Vec<Value>>> {
        let conn = self.conn()?;
        let rows: Vec<Vec<Value>> = if params.is_empty() {
            conn.query(cypher)
                .with_context(|| cypher.to_string())?
                .collect()
        } else {
            let mut stmt = conn.prepare(cypher).with_context(|| cypher.to_string())?;
            conn.execute(&mut stmt, params)
                .with_context(|| cypher.to_string())?
                .collect()
        };
        Ok(rows)
    }

    /// Execute `cypher` **once** over all `rows` passed as a single `$rows`
    /// LIST-of-STRUCT parameter, so the body runs server-side with
    /// `UNWIND $rows AS r …`. This collapses thousands of per-row FFI executes
    /// (and per-row query planning) into one call — the dominant cost of the
    /// resolve phase (measured ~3ms/row → ~20-65x faster in bulk). Every row must
    /// carry the same field set, in the same order, with consistent value types.
    fn exec_unwind(
        &self,
        conn: &Connection,
        cypher: &str,
        rows: Vec<Vec<(&str, Value)>>,
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        // Execute in bounded batches rather than one giant `$rows` param. A
        // single list of every node/edge on a large repo balloons memory and the
        // query plan's intermediates (and gives no progress), which made the
        // persist phase look hung. Each batch reuses the prepared statement.
        let mut st = conn.prepare(cypher).with_context(|| cypher.to_string())?;
        for batch in rows.chunks(UNWIND_BATCH) {
            let structs: Vec<Value> = batch
                .iter()
                .map(|r| Value::Struct(r.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()))
                .collect();
            let child: LogicalType = (&structs[0]).into();
            let list = Value::List(child, structs);
            conn.execute(&mut st, vec![("rows", list)])
                .with_context(|| cypher.to_string())?;
        }
        Ok(())
    }

    fn run_nodes(&self, cypher: &str, params: Vec<(&str, Value)>) -> Result<Vec<Node>> {
        Ok(self
            .run(cypher, params)?
            .iter()
            .map(|r| row_to_node(r))
            .collect())
    }

    /// Bulk-load CSV `body` into `table` via `COPY <table> FROM <tmpfile>`. Kùzu
    /// uses the optimized loader (hash-join on the primary key, O(n)) instead of
    /// the per-row node scan a property-`MATCH` from `UNWIND` does in this
    /// engine. The CSV is a temp file, removed after. COPY appends, so it can run
    /// once per batch/pass; the caller must pass primary-key-unique rows.
    fn copy_into(&self, table: &str, body: String) -> Result<()> {
        if body.is_empty() {
            return Ok(());
        }
        // Unique temp path per call (multiple COPYs run within one analysis).
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "glyphtrail-copy-{table}-{}-{seq}.csv",
            std::process::id()
        ));
        std::fs::write(&path, body)?;
        // PARALLEL=FALSE: a quoted field may contain a newline (e.g. a multi-line
        // doc comment), which the parallel CSV reader rejects. Serial parsing is
        // still O(n) and far faster than the per-row MERGE this replaces.
        let result = self
            .conn()?
            .query(&format!(
                "COPY {table} FROM '{}' (PARALLEL=FALSE)",
                path.display()
            ))
            .map(|_| ())
            .with_context(|| format!("COPY {table} FROM {}", path.display()));
        let _ = std::fs::remove_file(&path);
        result
    }

    /// Bulk-load nodes (CSV columns match the `Node` table order). The caller
    /// guarantees primary-key-unique `nodes` — COPY rejects a duplicate id.
    fn copy_nodes(&self, nodes: &[Node]) -> Result<()> {
        let mut csv = String::with_capacity(nodes.len() * 96);
        for n in nodes {
            let (sb, eb, sl, el) = n
                .span
                .map(|s| {
                    (
                        s.start_byte as i64,
                        s.end_byte as i64,
                        s.start_line as i64,
                        s.end_line as i64,
                    )
                })
                .unwrap_or((-1, -1, -1, -1));
            csv.push_str(&csv_field(&n.id.0));
            csv.push(',');
            csv.push_str(&csv_field(n.kind.as_str()));
            csv.push(',');
            csv.push_str(&csv_field(&n.name));
            csv.push(',');
            csv.push_str(&csv_field(&n.qualified_name));
            csv.push(',');
            csv.push_str(&csv_field(&n.file));
            csv.push(',');
            csv.push_str(&csv_field(n.language.as_deref().unwrap_or("")));
            csv.push_str(&format!(",{sb},{eb},{sl},{el},"));
            csv.push_str(&csv_field(n.doc.as_deref().unwrap_or("")));
            csv.push(',');
            csv.push_str(&csv_field(n.signature.as_deref().unwrap_or("")));
            csv.push('\n');
        }
        self.copy_into("Node", csv)
    }

    /// Bulk-load edges (CSV columns match the `Edge` rel table: from-pk, to-pk,
    /// kind, confidence). The caller guarantees `(src, dst, kind)`-unique edges.
    fn copy_edges(&self, edges: &[Edge]) -> Result<()> {
        let mut csv = String::with_capacity(edges.len() * 48);
        for e in edges {
            csv.push_str(&csv_field(&e.src.0));
            csv.push(',');
            csv.push_str(&csv_field(&e.dst.0));
            csv.push(',');
            csv.push_str(&csv_field(e.kind.as_str()));
            csv.push(',');
            csv.push_str(&csv_field(e.confidence.as_str()));
            csv.push('\n');
        }
        self.copy_into("Edge", csv)
    }
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

/// CSV-quote a field for Kùzu's COPY reader (RFC4180: wrap in quotes, double any
/// embedded quote) so a comma/quote/newline in an id can't break a row.
fn csv_field(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `$rows` structs for an edge insert ([`MERGE_EDGES`] / [`CREATE_EDGES`]).
fn edge_rows(edges: &[Edge]) -> Vec<Vec<(&str, Value)>> {
    edges
        .iter()
        .map(|e| {
            vec![
                ("src", s(&e.src.0)),
                ("dst", s(&e.dst.0)),
                ("ekind", s(e.kind.as_str())),
                ("conf", s(e.confidence.as_str())),
            ]
        })
        .collect()
}

/// A non-empty list of strings as a Cypher `Value::List`, for `IN $param`
/// filters. The caller guarantees `items` is non-empty (an empty kind selection
/// is handled as a `false` predicate, not an empty list).
fn str_list(items: &[String]) -> Value {
    let vals: Vec<Value> = items.iter().map(|v| Value::String(v.clone())).collect();
    let child: LogicalType = (&vals[0]).into();
    Value::List(child, vals)
}

/// Rank `search` candidates by where each term hits, most relevant first (#454):
/// a term in the name scores highest, then qualified name, then doc/file. Ties
/// break toward the shorter name (the more specific symbol). `terms` are already
/// lowercased when `case_sensitive` is false.
fn rank_search_hits(nodes: &mut [Node], terms: &[String], case_sensitive: bool) {
    let fold = |s: &str| {
        if case_sensitive {
            s.to_string()
        } else {
            s.to_lowercase()
        }
    };
    let score = |n: &Node| -> i32 {
        let name = fold(&n.name);
        let qname = fold(&n.qualified_name);
        let doc = n.doc.as_deref().map(fold).unwrap_or_default();
        let file = fold(&n.file);
        terms
            .iter()
            .map(|t| {
                if name.contains(t.as_str()) {
                    3
                } else if qname.contains(t.as_str()) {
                    2
                } else if doc.contains(t.as_str()) || file.contains(t.as_str()) {
                    1
                } else {
                    0
                }
            })
            .sum()
    };
    let scored: std::collections::HashMap<String, i32> =
        nodes.iter().map(|n| (n.id.0.clone(), score(n))).collect();
    nodes.sort_by(|a, b| {
        scored[&b.id.0]
            .cmp(&scored[&a.id.0])
            .then(a.name.len().cmp(&b.name.len()))
            // Fully deterministic order on ties (name, then id), since sort_by
            // is not stable.
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.0.cmp(&b.id.0))
    });
}

/// Encode an embedding vector as comma-joined floats. Rust's float formatting is
/// shortest-round-trip, so `decode_vec` recovers the same `f32`s.
fn encode_vec(v: &[f32]) -> String {
    v.iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Decode a comma-joined embedding vector; unparseable entries become `0.0`.
fn decode_vec(s: &str) -> Vec<f32> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(',').map(|t| t.parse().unwrap_or(0.0)).collect()
}

fn get_str(row: &[Value], idx: usize) -> String {
    match row.get(idx) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}
fn get_i64(row: &[Value], idx: usize) -> i64 {
    match row.get(idx) {
        Some(Value::Int64(n)) => *n,
        _ => -1,
    }
}
fn opt(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

fn row_to_node(row: &[Value]) -> Node {
    let sl = get_i64(row, 8);
    let span = (sl >= 0).then(|| Span {
        start_byte: get_i64(row, 6).max(0) as usize,
        end_byte: get_i64(row, 7).max(0) as usize,
        start_line: sl as usize,
        end_line: get_i64(row, 9).max(0) as usize,
    });
    Node {
        id: NodeId(get_str(row, 0)),
        kind: parse_kind(&get_str(row, 1)),
        name: get_str(row, 2),
        qualified_name: get_str(row, 3),
        file: get_str(row, 4),
        language: opt(get_str(row, 5)),
        span,
        doc: opt(get_str(row, 10)),
        signature: opt(get_str(row, 11)),
    }
}

fn parse_kind(s: &str) -> NodeKind {
    match s {
        "repo" => NodeKind::Repo,
        "directory" => NodeKind::Directory,
        "file" => NodeKind::File,
        "module" => NodeKind::Module,
        "function" => NodeKind::Function,
        "method" => NodeKind::Method,
        "class" => NodeKind::Class,
        "struct" => NodeKind::Struct,
        "interface" => NodeKind::Interface,
        "enum" => NodeKind::Enum,
        "trait" => NodeKind::Trait,
        "constant" => NodeKind::Constant,
        "comment" => NodeKind::Comment,
        "endpoint" => NodeKind::Endpoint,
        "client_call" => NodeKind::ClientCall,
        "router" => NodeKind::Router,
        "commit" => NodeKind::Commit,
        "author" => NodeKind::Author,
        "identity" => NodeKind::Identity,
        "topic" => NodeKind::Topic,
        "table" => NodeKind::Table,
        "column" => NodeKind::Column,
        _ => NodeKind::SchemaOp,
    }
}

fn parse_edge_kind(s: &str) -> EdgeKind {
    match s {
        "contains" => EdgeKind::Contains,
        "defines" => EdgeKind::Defines,
        "calls" => EdgeKind::Calls,
        "imports" => EdgeKind::Imports,
        "extends" => EdgeKind::Extends,
        "implements" => EdgeKind::Implements,
        "documents" => EdgeKind::Documents,
        "handles" => EdgeKind::Handles,
        "mounts" => EdgeKind::Mounts,
        "exposes" => EdgeKind::Exposes,
        "invokes" => EdgeKind::Invokes,
        "reads" => EdgeKind::Reads,
        "writes" => EdgeKind::Writes,
        "authored" => EdgeKind::Authored,
        "alias_of" => EdgeKind::AliasOf,
        "touched" => EdgeKind::Touched,
        "tagged" => EdgeKind::Tagged,
        "part_of" => EdgeKind::PartOf,
        _ => EdgeKind::References,
    }
}

fn parse_conf(s: &str) -> Confidence {
    match s {
        "extracted" => Confidence::Extracted,
        _ => Confidence::Inferred,
    }
}

fn parse_proto(s: &str) -> glyphtrail_core::Protocol {
    use glyphtrail_core::Protocol;
    match s {
        "grpc" => Protocol::Grpc,
        "graphql" => Protocol::GraphQl,
        _ => Protocol::Rest,
    }
}

fn op_from_row(row: &[Value]) -> (NodeId, OperationKey) {
    let method = opt(get_str(row, 2)).and_then(|m| glyphtrail_core::HttpMethod::parse(&m));
    (
        NodeId(get_str(row, 0)),
        OperationKey {
            protocol: parse_proto(&get_str(row, 1)),
            method,
            path: get_str(row, 3),
        },
    )
}

/// Decode a `Commit` side-table row (#330): node_id, hash, author_email,
/// committed_at, subject, in_bounds — in that column order.
fn commit_from_row(row: &[Value]) -> CommitMeta {
    CommitMeta {
        node_id: NodeId(get_str(row, 0)),
        hash: get_str(row, 1),
        author_email: get_str(row, 2),
        committed_at: get_i64(row, 3),
        subject: get_str(row, 4),
        in_bounds: get_i64(row, 5) != 0,
    }
}

impl Adjacency for LadybugStore {
    fn step(&self, node: &NodeId, kind: EdgeKind, dir: Direction) -> Vec<(NodeId, Confidence)> {
        let outgoing = matches!(dir, Direction::Outgoing);
        self.edge_step(&node.0, kind, outgoing).unwrap_or_default()
    }
}

impl LadybugStore {
    /// Run a raw Cypher query and return its formatted result (the `cypher`
    /// subcommand escape hatch, #9).
    pub fn cypher(&self, query: &str) -> Result<String> {
        let conn = self.conn()?;
        Ok(format!("{}", conn.query(query)?))
    }

    fn edge_step(
        &self,
        node: &str,
        kind: EdgeKind,
        outgoing: bool,
    ) -> Result<Vec<(NodeId, Confidence)>> {
        let cypher = if outgoing {
            "MATCH (a:Node {id:$id})-[e:Edge {kind:$k}]->(b:Node) RETURN b.id, e.confidence"
        } else {
            "MATCH (a:Node)-[e:Edge {kind:$k}]->(b:Node {id:$id}) RETURN a.id, e.confidence"
        };
        Ok(self
            .run(cypher, vec![("id", s(node)), ("k", s(kind.as_str()))])?
            .iter()
            .map(|r| (NodeId(get_str(r, 0)), parse_conf(&get_str(r, 1))))
            .collect())
    }
}

impl GraphStore for LadybugStore {
    fn clear(&mut self) -> Result<()> {
        // DETACH DELETE removes a node and its rels in one step (LadybugDB does
        // not support deleting undirected rels).
        self.run("MATCH (n:Node) DETACH DELETE n", vec![])?;
        // Note: `Meta` is intentionally preserved (matching the SQLite backend)
        // so the `schema_version` stamp survives a content clear; deleting it
        // made every reopen look unversioned and wipe the DB via migrate (#159).
        for tbl in ["File", "ApiOp", "Pending", "Import"] {
            self.run(&format!("MATCH (n:{tbl}) DELETE n"), vec![])?;
        }
        Ok(())
    }

    fn set_file(&mut self, path: &str, language: Option<&str>, hash: &str) -> Result<()> {
        self.run(
            "MERGE (f:File {path:$p}) SET f.language=$l, f.hash=$h",
            vec![
                ("p", s(path)),
                ("l", s(language.unwrap_or(""))),
                ("h", s(hash)),
            ],
        )?;
        Ok(())
    }

    fn set_files(&mut self, files: &[(String, Option<String>, String)]) -> Result<()> {
        // One UNWIND instead of a fresh connection + auto-commit per file (the
        // per-file loop was the resolve phase's second-slowest stage).
        let conn = self.conn()?;
        let rows: Vec<Vec<(&str, Value)>> = files
            .iter()
            .map(|(path, language, hash)| {
                vec![
                    ("p", s(path)),
                    ("l", s(language.as_deref().unwrap_or(""))),
                    ("h", s(hash)),
                ]
            })
            .collect();
        self.exec_unwind(
            &conn,
            "UNWIND $rows AS r MERGE (f:File {path:r.p}) SET f.language=r.l, f.hash=r.h",
            rows,
        )
    }

    fn delete_file_data(&mut self, path: &str) -> Result<()> {
        // Drop pending links anchored to a node in this file *before* the nodes
        // (and their ids) are deleted, else the Pending rows orphan and linger.
        self.run(
            "MATCH (n:Node {file:$f}), (p:Pending) WHERE p.anchor = n.id DELETE p",
            vec![("f", s(path))],
        )?;
        self.run(
            "MATCH (n:Node {file:$f}) DETACH DELETE n",
            vec![("f", s(path))],
        )?;
        self.run("MATCH (f:File {path:$f}) DELETE f", vec![("f", s(path))])?;
        self.run(
            "MATCH (i:Import {importer:$f}) DELETE i",
            vec![("f", s(path))],
        )?;
        Ok(())
    }

    fn delete_nodes_by_kind(&mut self, kind: NodeKind) -> Result<()> {
        self.run(
            "MATCH (n:Node {kind:$k}) DETACH DELETE n",
            vec![("k", s(kind.as_str()))],
        )?;
        Ok(())
    }

    fn insert_graph(&mut self, nodes: &[Node], edges: &[Edge]) -> Result<()> {
        let conn = self.conn()?;
        // Pass all rows as one `$rows` LIST-of-STRUCT param and let `UNWIND` drive
        // the MERGE server-side: one FFI execute + one query plan for the whole
        // batch instead of thousands of per-row crossings, which dominated the
        // resolve phase (#170 batched into a txn; this removes the per-row cost
        // itself). Integers ride inside the struct as INT64, so no top-level INT64
        // parameter is ever created.
        let node_rows: Vec<Vec<(&str, Value)>> = nodes
            .iter()
            .map(|n| {
                let (sb, eb, sl, el) = n
                    .span
                    .map(|sp| {
                        (
                            sp.start_byte as i64,
                            sp.end_byte as i64,
                            sp.start_line as i64,
                            sp.end_line as i64,
                        )
                    })
                    .unwrap_or((-1, -1, -1, -1));
                vec![
                    ("id", s(&n.id.0)),
                    ("kind", s(n.kind.as_str())),
                    ("name", s(&n.name)),
                    ("qn", s(&n.qualified_name)),
                    ("file", s(&n.file)),
                    ("lang", s(n.language.as_deref().unwrap_or(""))),
                    ("sb", Value::Int64(sb)),
                    ("eb", Value::Int64(eb)),
                    ("sl", Value::Int64(sl)),
                    ("el", Value::Int64(el)),
                    ("doc", s(n.doc.as_deref().unwrap_or(""))),
                    ("sig", s(n.signature.as_deref().unwrap_or(""))),
                ]
            })
            .collect();
        self.exec_unwind(
            &conn,
            "UNWIND $rows AS r MERGE (n:Node {id:r.id}) SET n.kind=r.kind, \
             n.name=r.name, n.qualified_name=r.qn, n.file=r.file, n.language=r.lang, \
             n.start_byte=r.sb, n.end_byte=r.eb, n.start_line=r.sl, n.end_line=r.el, \
             n.doc=r.doc, n.signature=r.sig",
            node_rows,
        )?;
        self.exec_unwind(&conn, MERGE_EDGES, edge_rows(edges))
    }

    fn insert_nodes(&mut self, nodes: &[Node], fresh: bool) -> Result<()> {
        // Same story as edges: `MERGE (n:Node {id})` from UNWIND does NOT use the
        // primary-key index in this engine, so it scans the node table per row —
        // O(nodes²) on a large repo. A fresh rebuild gets a pk-unique node set, so
        // bulk-load via COPY (hash-loaded on the pk, O(n)). Updates MERGE.
        if fresh {
            self.copy_nodes(nodes)
        } else {
            self.insert_graph(nodes, &[])
        }
    }

    fn insert_edges(&mut self, edges: &[Edge], fresh: bool) -> Result<()> {
        // On a fresh rebuild the store was just cleared and the caller passes a
        // de-duplicated edge set, so bulk-load via COPY: Kùzu hash-joins the
        // endpoints on the Node primary key in O(n). A per-row `MATCH (:Node
        // {id})` from UNWIND does NOT use the PK index in this engine — it scans
        // the node table per edge, going O(nodes × edges) and stalling a large
        // repo's persist (whether the verb is MERGE or CREATE). An incremental
        // update still MERGEs (small batches; can't dedup against on-disk edges).
        if fresh {
            self.copy_edges(edges)
        } else {
            let conn = self.conn()?;
            self.exec_unwind(&conn, MERGE_EDGES, edge_rows(edges))
        }
    }

    fn insert_operations(&mut self, ops: &[(NodeId, OperationKey)]) -> Result<()> {
        let conn = self.conn()?;
        let rows: Vec<Vec<(&str, Value)>> = ops
            .iter()
            .map(|(id, key)| {
                vec![
                    ("id", s(&id.0)),
                    ("p", s(key.protocol.as_str())),
                    ("m", s(key.method.map(|m| m.as_str()).unwrap_or(""))),
                    ("path", s(&key.path)),
                    ("sig", s(&key.signature())),
                ]
            })
            .collect();
        self.exec_unwind(
            &conn,
            "UNWIND $rows AS r MERGE (o:ApiOp {node_id:r.id}) \
             SET o.protocol=r.p, o.method=r.m, o.path=r.path, o.signature=r.sig",
            rows,
        )
    }

    fn set_commits(&mut self, commits: &[CommitMeta]) -> Result<()> {
        let conn = self.conn()?;
        let rows: Vec<Vec<(&str, Value)>> = commits
            .iter()
            .map(|c| {
                vec![
                    ("id", s(&c.node_id.0)),
                    ("h", s(&c.hash)),
                    ("e", s(&c.author_email)),
                    ("t", Value::Int64(c.committed_at)),
                    ("subj", s(&c.subject)),
                    ("ib", Value::Int64(i64::from(c.in_bounds))),
                ]
            })
            .collect();
        self.exec_unwind(
            &conn,
            "UNWIND $rows AS r MERGE (c:Commit {node_id:r.id}) \
             SET c.hash=r.h, c.author_email=r.e, c.committed_at=r.t, \
             c.subject=r.subj, c.in_bounds=r.ib",
            rows,
        )
    }

    fn set_embeddings(&mut self, embeddings: &[Embedding], model: &str) -> Result<()> {
        let conn = self.conn()?;
        let rows: Vec<Vec<(&str, Value)>> = embeddings
            .iter()
            .map(|e| {
                vec![
                    ("id", s(&e.node_id.0)),
                    ("m", s(model)),
                    ("d", Value::Int64(e.vector.len() as i64)),
                    ("v", s(&encode_vec(&e.vector))),
                ]
            })
            .collect();
        self.exec_unwind(
            &conn,
            "UNWIND $rows AS r MERGE (e:Embedding {node_id:r.id}) \
             SET e.model=r.m, e.dim=r.d, e.vec=r.v",
            rows,
        )
    }

    fn clear_embeddings(&mut self) -> Result<()> {
        self.run("MATCH (e:Embedding) DELETE e", vec![])?;
        Ok(())
    }

    fn clear_embeddings_by_model(&mut self, model: &str) -> Result<()> {
        self.run(
            "MATCH (e:Embedding) WHERE e.model = $m DELETE e",
            vec![("m", s(model))],
        )?;
        Ok(())
    }

    fn clear_embeddings_except_model(&mut self, model: &str) -> Result<()> {
        self.run(
            "MATCH (e:Embedding) WHERE e.model <> $m DELETE e",
            vec![("m", s(model))],
        )?;
        Ok(())
    }

    fn remark_commit_bounds(&mut self, since: Option<i64>, until: Option<i64>) -> Result<()> {
        // Inline the bounds (lbug caches a bound param's type by name across
        // statements, so an INT64 param here would clash with STRING params
        // elsewhere). Unbounded sides become the i64 extremes, which include
        // every row.
        let lo = since.unwrap_or(i64::MIN);
        let hi = until.unwrap_or(i64::MAX);
        self.run(
            &format!(
                "MATCH (c:Commit) SET c.in_bounds = \
                 CASE WHEN c.committed_at >= {lo} AND c.committed_at <= {hi} THEN 1 ELSE 0 END"
            ),
            vec![],
        )?;
        Ok(())
    }

    fn insert_pending(&mut self, links: &[PendingLink]) -> Result<()> {
        let conn = self.conn()?;
        let rows: Vec<Vec<(&str, Value)>> = links
            .iter()
            .map(|l| {
                let rowid = format!(
                    "{}|{}|{}|{}",
                    l.anchor.0,
                    l.name,
                    l.kind.as_str(),
                    l.name_is_src
                );
                vec![
                    ("r", s(&rowid)),
                    ("a", s(&l.anchor.0)),
                    ("n", s(&l.name)),
                    ("k", s(l.kind.as_str())),
                    ("nis", Value::Int64(l.name_is_src as i64)),
                ]
            })
            .collect();
        self.exec_unwind(
            &conn,
            "UNWIND $rows AS r MERGE (p:Pending {pk:r.r}) \
             SET p.anchor=r.a, p.name=r.n, p.kind=r.k, p.name_is_src=r.nis",
            rows,
        )
    }

    fn insert_imports(&mut self, imports: &[(String, String, String)]) -> Result<()> {
        let conn = self.conn()?;
        let rows: Vec<Vec<(&str, Value)>> = imports
            .iter()
            .map(|(importer, raw, language)| {
                let rowid = format!("{importer}|{raw}|{language}");
                vec![
                    ("pk", s(&rowid)),
                    ("importer", s(importer)),
                    ("raw", s(raw)),
                    ("lang", s(language)),
                ]
            })
            .collect();
        self.exec_unwind(
            &conn,
            "UNWIND $rows AS r MERGE (i:Import {pk:r.pk}) \
             SET i.importer=r.importer, i.raw=r.raw, i.language=r.lang",
            rows,
        )
    }

    fn delete_edges_by_confidence(&mut self, confidence: Confidence) -> Result<usize> {
        self.run(
            "MATCH ()-[e:Edge {confidence:$c}]->() DELETE e",
            vec![("c", s(confidence.as_str()))],
        )?;
        Ok(0)
    }

    fn delete_edges_by_kind(&mut self, kind: EdgeKind) -> Result<usize> {
        self.run(
            "MATCH ()-[e:Edge {kind:$k}]->() DELETE e",
            vec![("k", s(kind.as_str()))],
        )?;
        Ok(0)
    }

    fn prune_dangling_edges(&mut self) -> Result<usize> {
        // Rel endpoints always reference existing nodes in LadybugDB, so there
        // are no dangling edges to prune.
        Ok(0)
    }

    fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        self.run(
            "MERGE (m:Meta {key:$k}) SET m.value=$v",
            vec![("k", s(key)), ("v", s(value))],
        )?;
        Ok(())
    }

    fn file_hash(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .run(
                "MATCH (f:File {path:$p}) RETURN f.hash",
                vec![("p", s(path))],
            )?
            .first()
            .map(|r| get_str(r, 0)))
    }

    fn all_files(&self) -> Result<Vec<String>> {
        Ok(self
            .run("MATCH (f:File) RETURN f.path", vec![])?
            .iter()
            .map(|r| get_str(r, 0))
            .collect())
    }

    fn files_with_hashes(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .run("MATCH (f:File) RETURN f.path, f.hash", vec![])?
            .iter()
            .map(|r| (get_str(r, 0), get_str(r, 1)))
            .collect())
    }

    fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .run(
                "MATCH (m:Meta {key:$k}) RETURN m.value",
                vec![("k", s(key))],
            )?
            .first()
            .map(|r| get_str(r, 0)))
    }

    fn operations_by_kind(&self, kind: NodeKind) -> Result<Vec<(NodeId, OperationKey)>> {
        Ok(self
            .run(
                "MATCH (n:Node {kind:$k}), (o:ApiOp {node_id:n.id}) \
                 RETURN o.node_id, o.protocol, o.method, o.path, o.signature",
                vec![("k", s(kind.as_str()))],
            )?
            .iter()
            .map(|r| op_from_row(r))
            .collect())
    }

    fn all_operations(&self) -> Result<Vec<(NodeId, OperationKey)>> {
        Ok(self
            .run(
                "MATCH (o:ApiOp) RETURN o.node_id, o.protocol, o.method, o.path, o.signature",
                vec![],
            )?
            .iter()
            .map(|r| op_from_row(r))
            .collect())
    }

    fn commits_in_range(&self, since: Option<i64>, until: Option<i64>) -> Result<Vec<CommitMeta>> {
        // Inline the bounds: lbug caches a bound param's type by name across
        // statements, so binding INT64s here would clash with STRING params
        // elsewhere; inlining the integers sidesteps it.
        let mut filter = String::from("c.in_bounds = 1");
        if let Some(s) = since {
            filter.push_str(&format!(" AND c.committed_at >= {s}"));
        }
        if let Some(u) = until {
            filter.push_str(&format!(" AND c.committed_at <= {u}"));
        }
        Ok(self
            .run(
                &format!(
                    "MATCH (c:Commit) WHERE {filter} RETURN c.node_id, c.hash, \
                     c.author_email, c.committed_at, c.subject, c.in_bounds \
                     ORDER BY c.committed_at"
                ),
                vec![],
            )?
            .iter()
            .map(|r| commit_from_row(r))
            .collect())
    }

    fn commit_count(&self) -> Result<usize> {
        Ok(self
            .run("MATCH (c:Commit) RETURN COUNT(*)", vec![])?
            .first()
            .map(|r| get_i64(r, 0).max(0) as usize)
            .unwrap_or(0))
    }

    fn embeddings(&self) -> Result<Vec<Embedding>> {
        Ok(self
            .run(
                "MATCH (e:Embedding) RETURN e.node_id, e.vec ORDER BY e.node_id",
                vec![],
            )?
            .iter()
            .map(|r| Embedding {
                node_id: NodeId(get_str(r, 0)),
                vector: decode_vec(&get_str(r, 1)),
            })
            .collect())
    }

    fn atlas_topics(&self) -> Result<Vec<(String, usize)>> {
        Ok(self
            .run(
                "MATCH (cn:Node {kind:'commit'})-[:Edge {kind:'tagged'}]->(t:Node {kind:'topic'}) \
                 RETURN t.name, COUNT(*) AS n ORDER BY n DESC, t.name",
                vec![],
            )?
            .iter()
            .map(|r| (get_str(r, 0), get_i64(r, 1).max(0) as usize))
            .collect())
    }

    fn atlas_timeline(
        &self,
        since: Option<i64>,
        until: Option<i64>,
        topic: Option<&str>,
    ) -> Result<Vec<glyphtrail_core::AtlasTimelineRow>> {
        // Date bounds shared by both queries. Inline them (lbug param-type-cache
        // landmine); the in-bounds + commit-node join restricts every aggregate
        // to the same windowed commit set.
        let mut bounds = String::new();
        if let Some(s) = since {
            bounds.push_str(&format!(" AND c.committed_at >= {s}"));
        }
        if let Some(u) = until {
            bounds.push_str(&format!(" AND c.committed_at <= {u}"));
        }
        // Touched-file counts, one aggregate restricted to the windowed commits.
        let mut touched: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for r in self.run(
            &format!(
                "MATCH (c:Commit), (cn:Node)-[:Edge {{kind:'touched'}}]->(:Node) \
                 WHERE c.in_bounds = 1 AND cn.kind = 'commit' AND cn.id = c.node_id{bounds} \
                 RETURN cn.id, COUNT(*)"
            ),
            vec![],
        )? {
            touched.insert(get_str(&r, 0), get_i64(&r, 1).max(0) as u32);
        }
        // In-bounds commits in range, joined to their repo. An optional topic
        // join keeps only commits tagged with it (#334); the topic rides as a
        // string param (no int params here, so no type-cache clash).
        let mut tagged_match = String::new();
        let mut tagged_where = String::new();
        let mut params: Vec<(&str, Value)> = Vec::new();
        if let Some(t) = topic {
            tagged_match = ", (cn)-[:Edge {kind:'tagged'}]->(tp:Node {kind:'topic'})".to_string();
            tagged_where = " AND tp.name = $topic".to_string();
            params.push(("topic", s(t)));
        }
        Ok(self
            .run(
                &format!(
                    "MATCH (c:Commit), (cn:Node)-[:Edge {{kind:'part_of'}}]->(r:Node){tagged_match} \
                     WHERE c.in_bounds = 1 AND cn.kind = 'commit' AND cn.id = c.node_id \
                     AND r.kind = 'repo'{bounds}{tagged_where} \
                     RETURN c.node_id, c.hash, c.author_email, c.committed_at, c.subject, \
                     c.in_bounds, r.name ORDER BY c.committed_at"
                ),
                params,
            )?
            .iter()
            .map(|r| glyphtrail_core::AtlasTimelineRow {
                touched: touched.get(&get_str(r, 0)).copied().unwrap_or(0),
                repo: get_str(r, 6),
                commit: commit_from_row(r),
            })
            .collect())
    }

    fn all_pending(&self) -> Result<Vec<PendingLink>> {
        Ok(self
            .run(
                "MATCH (p:Pending) RETURN p.anchor, p.name, p.kind, p.name_is_src",
                vec![],
            )?
            .iter()
            .map(|r| PendingLink {
                anchor: NodeId(get_str(r, 0)),
                name: get_str(r, 1),
                kind: parse_edge_kind(&get_str(r, 2)),
                name_is_src: get_i64(r, 3) != 0,
            })
            .collect())
    }

    fn all_imports(&self) -> Result<Vec<(String, String, String)>> {
        Ok(self
            .run(
                "MATCH (i:Import) RETURN i.importer, i.raw, i.language",
                vec![],
            )?
            .iter()
            .map(|r| (get_str(r, 0), get_str(r, 1), get_str(r, 2)))
            .collect())
    }

    fn node_files(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .run("MATCH (n:Node) RETURN n.id, n.file", vec![])?
            .iter()
            .map(|r| (get_str(r, 0), get_str(r, 1)))
            .collect())
    }

    fn node_qualified_names(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .run("MATCH (n:Node) RETURN n.id, n.qualified_name", vec![])?
            .iter()
            .map(|r| (get_str(r, 0), get_str(r, 1)))
            .collect())
    }

    fn definition_index(&self) -> Result<Vec<(String, NodeId)>> {
        Ok(self
            .run("MATCH (n:Node) RETURN n.name, n.id", vec![])?
            .iter()
            .map(|r| (get_str(r, 0), NodeId(get_str(r, 1))))
            .collect())
    }

    fn tables_by_name(&self) -> Result<Vec<(NodeId, String)>> {
        Ok(self
            .run(
                "MATCH (n:Node {kind:'table'}) RETURN n.id, n.qualified_name",
                vec![],
            )?
            .iter()
            .map(|r| (NodeId(get_str(r, 0)), get_str(r, 1)))
            .collect())
    }

    fn get_node(&self, id: &str) -> Result<Option<Node>> {
        Ok(self
            .run_nodes(
                &format!("MATCH (n:Node {{id:$id}}) RETURN {NODE_COLS}"),
                vec![("id", s(id))],
            )?
            .into_iter()
            .next())
    }

    fn nodes_in_file(&self, file: &str) -> Result<Vec<Node>> {
        self.run_nodes(
            &format!("MATCH (n:Node {{file:$f}}) RETURN {NODE_COLS}"),
            vec![("f", s(file))],
        )
    }

    fn find_by_name(&self, name: &str) -> Result<Vec<Node>> {
        self.run_nodes(
            &format!(
                "MATCH (n:Node) WHERE n.name = $q OR n.qualified_name = $q RETURN {NODE_COLS} LIMIT 200"
            ),
            vec![("q", s(name))],
        )
    }

    fn search(&self, query: &str, limit: usize, case_sensitive: bool) -> Result<Vec<Node>> {
        // No native FTS; approximate with substring CONTAINS. A multi-word query is
        // AND-ed over its terms (#456): a node matches when *every* term occurs in
        // its name, qualified name, doc, or file path. Bare external-module import
        // nodes (kind=module, no file) are dropped — they aren't actionable (#454).
        // Case-insensitive by default (#367). Candidates are then ranked in Rust.
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|t| {
                if case_sensitive {
                    t.to_string()
                } else {
                    t.to_lowercase()
                }
            })
            .collect();
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let col = |c: &str| {
            if case_sensitive {
                c.to_string()
            } else {
                format!("lower({c})")
            }
        };
        let names: Vec<String> = (0..terms.len()).map(|i| format!("t{i}")).collect();
        let per_term: Vec<String> = names
            .iter()
            .map(|p| {
                format!(
                    "({} CONTAINS ${p} OR {} CONTAINS ${p} OR {} CONTAINS ${p} OR {} CONTAINS ${p})",
                    col("n.name"),
                    col("n.qualified_name"),
                    col("n.doc"),
                    col("n.file"),
                )
            })
            .collect();
        let params: Vec<(&str, Value)> = names
            .iter()
            .zip(&terms)
            .map(|(n, t)| (n.as_str(), s(t)))
            .collect();
        // Fetch a generous candidate set, then rank/truncate in Rust (no FTS
        // scoring). 500 covers the vast majority of real queries.
        let cap = limit.max(500);
        let cypher = format!(
            "MATCH (n:Node) WHERE {} AND NOT (n.kind = 'module' AND n.file = '') \
             RETURN {NODE_COLS} LIMIT {cap}",
            per_term.join(" AND "),
        );
        let mut nodes = self.run_nodes(&cypher, params)?;
        rank_search_hits(&mut nodes, &terms, case_sensitive);
        nodes.truncate(limit);
        Ok(nodes)
    }

    fn neighbors(
        &self,
        id: &str,
        kind: Option<EdgeKind>,
        outgoing: bool,
    ) -> Result<Vec<(Node, EdgeKind, Confidence)>> {
        let dir = if outgoing {
            format!(
                "MATCH (a:Node {{id:$id}})-[e:Edge]->(n:Node) RETURN {NODE_COLS}, e.kind, e.confidence"
            )
        } else {
            format!(
                "MATCH (n:Node)-[e:Edge]->(a:Node {{id:$id}}) RETURN {NODE_COLS}, e.kind, e.confidence"
            )
        };
        let rows = self.run(&dir, vec![("id", s(id))])?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    row_to_node(r),
                    // Edge columns follow the node's; NODE_COLS has 12 (incl.
                    // signature, #344), so e.kind/e.confidence are at 12/13.
                    parse_edge_kind(&get_str(r, 12)),
                    parse_conf(&get_str(r, 13)),
                )
            })
            .filter(|(_, k, _)| kind.is_none_or(|want| *k == want))
            .collect())
    }

    fn reachable(
        &self,
        id: &str,
        kind: EdgeKind,
        outgoing: bool,
        depth: usize,
    ) -> Result<Vec<Node>> {
        // BFS over neighbors so we don't depend on recursive-Cypher syntax.
        let mut seen: HashSet<String> = HashSet::from([id.to_string()]);
        let mut frontier: VecDeque<(String, usize)> = VecDeque::from([(id.to_string(), 0usize)]);
        let mut out = Vec::new();
        while let Some((cur, d)) = frontier.pop_front() {
            if d >= depth {
                continue;
            }
            for (nid, _) in self.edge_step(&cur, kind, outgoing)? {
                if seen.insert(nid.0.clone()) {
                    if let Some(node) = self.get_node(&nid.0)? {
                        out.push(node);
                    }
                    frontier.push_back((nid.0, d + 1));
                }
            }
        }
        Ok(out)
    }

    fn subgraph(&self, ids: &[String]) -> Result<(Vec<Node>, Vec<Edge>)> {
        let set: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut nodes = Vec::new();
        for id in ids {
            if let Some(n) = self.get_node(id)? {
                nodes.push(n);
            }
        }
        let edges = self
            .run(
                "MATCH (a:Node)-[e:Edge]->(b:Node) RETURN a.id, b.id, e.kind, e.confidence",
                vec![],
            )?
            .iter()
            .filter(|r| {
                set.contains(get_str(r, 0).as_str()) && set.contains(get_str(r, 1).as_str())
            })
            .map(|r| Edge {
                src: NodeId(get_str(r, 0)),
                dst: NodeId(get_str(r, 1)),
                kind: parse_edge_kind(&get_str(r, 2)),
                confidence: parse_conf(&get_str(r, 3)),
            })
            .collect();
        Ok((nodes, edges))
    }

    fn classify_impact(
        &self,
        seeds: &[NodeId],
        policy: &ImpactPolicy,
    ) -> Result<Vec<ClassifiedItem>> {
        let items = compute_impact(seeds, policy, self);
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            if let Some(node) = self.get_node(&it.node.0)? {
                out.push(ClassifiedItem {
                    id: node.id.0,
                    name: node.name,
                    qualified_name: node.qualified_name.clone(),
                    kind: node.kind,
                    file: node.file.clone(),
                    line: node.span.map(|sp| sp.start_line),
                    class: classify(node.kind, &node.file, &node.qualified_name),
                    distance: it.distance,
                    min_confidence: it.min_confidence,
                    cross_boundary: is_cross_boundary_path(&it.path),
                    path: it.path.iter().map(|k| k.as_str().to_string()).collect(),
                });
            }
        }
        Ok(out)
    }

    fn stats(&self) -> Result<Stats> {
        let count = |c: &str| -> Result<usize> {
            Ok(self
                .run(c, vec![])?
                .first()
                .map(|r| get_i64(r, 0))
                .unwrap_or(0) as usize)
        };
        let languages = self
            .run(
                "MATCH (f:File) RETURN CASE WHEN f.language = '' THEN '(unknown)' ELSE f.language END AS lang, COUNT(*) ORDER BY COUNT(*) DESC, lang",
                vec![],
            )?
            .iter()
            .map(|r| (get_str(r, 0), get_i64(r, 1).max(0) as usize))
            .collect();
        Ok(Stats {
            nodes: count("MATCH (n:Node) RETURN COUNT(n)")?,
            edges: count("MATCH ()-[e:Edge]->() RETURN COUNT(e)")?,
            files: count("MATCH (f:File) RETURN COUNT(f)")?,
            languages,
        })
    }

    fn node_kind_counts(&self) -> Result<Vec<(String, usize)>> {
        Ok(self
            .run(
                "MATCH (n:Node) RETURN n.kind, COUNT(*) ORDER BY n.kind",
                vec![],
            )?
            .iter()
            .map(|r| (get_str(r, 0), get_i64(r, 1).max(0) as usize))
            .collect())
    }

    fn edge_kind_counts(&self) -> Result<Vec<(String, usize)>> {
        Ok(self
            .run(
                "MATCH ()-[e:Edge]->() RETURN e.kind, COUNT(*) ORDER BY e.kind",
                vec![],
            )?
            .iter()
            .map(|r| (get_str(r, 0), get_i64(r, 1).max(0) as usize))
            .collect())
    }

    fn export_graph(&self, limit: usize) -> Result<(Vec<Node>, Vec<Edge>)> {
        self.export_filtered(None, None, limit)
    }

    fn export_filtered(
        &self,
        node_kinds: Option<&[String]>,
        edge_kinds: Option<&[String]>,
        limit: usize,
    ) -> Result<(Vec<Node>, Vec<Edge>)> {
        // Nodes: push the kind filter and the cap into the query.
        let mut nparams: Vec<(&str, Value)> = Vec::new();
        let nwhere = match node_kinds {
            None => String::new(),
            Some([]) => "WHERE false".into(),
            Some(ks) => {
                nparams.push(("nk", str_list(ks)));
                "WHERE n.kind IN $nk".into()
            }
        };
        let nodes = self.run_nodes(
            &format!("MATCH (n:Node) {nwhere} RETURN {NODE_COLS} LIMIT {limit}"),
            nparams,
        )?;

        // Edges: filter by edge kind and by both endpoints' kinds, so a trimmed
        // view never fetches the (often dominant) call/containment edges it would
        // immediately discard.
        let mut conds: Vec<&str> = Vec::new();
        let mut eparams: Vec<(&str, Value)> = Vec::new();
        match edge_kinds {
            None => {}
            Some([]) => conds.push("false"),
            Some(ks) => {
                eparams.push(("ek", str_list(ks)));
                conds.push("e.kind IN $ek");
            }
        }
        match node_kinds {
            None => {}
            Some([]) => conds.push("false"),
            Some(ks) => {
                eparams.push(("ak", str_list(ks)));
                eparams.push(("bk", str_list(ks)));
                conds.push("a.kind IN $ak");
                conds.push("b.kind IN $bk");
            }
        }
        let ewhere = if conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conds.join(" AND "))
        };
        let edges = self
            .run(
                &format!(
                    "MATCH (a:Node)-[e:Edge]->(b:Node) {ewhere} RETURN a.id, b.id, e.kind, e.confidence"
                ),
                eparams,
            )?
            .iter()
            .map(|r| Edge {
                src: NodeId(get_str(r, 0)),
                dst: NodeId(get_str(r, 1)),
                kind: parse_edge_kind(&get_str(r, 2)),
                confidence: parse_conf(&get_str(r, 3)),
            })
            .collect();
        Ok((nodes, edges))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn node(id: &str, name: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            kind: NodeKind::Function,
            name: name.into(),
            qualified_name: name.into(),
            file: "a.rs".into(),
            language: Some("rust".into()),
            span: Some(Span {
                start_byte: 0,
                end_byte: 1,
                start_line: 3,
                end_line: 4,
            }),
            doc: None,
            signature: None,
        }
    }

    // #456/#454: a multi-word query ANDs its terms across name/qualified/doc/file,
    // and bare external-module import nodes are excluded.
    #[test]
    fn search_ands_terms_and_drops_bare_imports() {
        let dir = tmp_dir("search-and");
        let mut lb = LadybugStore::open(&dir).unwrap();
        let mut cache = node("c", "UserCache");
        cache.kind = NodeKind::Struct;
        cache.file = "datastorage/users.rs".into();
        cache.qualified_name = "datastorage::UserCache".into();
        let mut import = node("m", "moka::Cache"); // bare external-module import
        import.kind = NodeKind::Module;
        import.file = String::new();
        import.qualified_name = "moka::Cache".into();
        let mut other = node("o", "Widget");
        other.file = "widget.rs".into();
        other.qualified_name = "Widget".into();
        lb.insert_nodes(&[cache, import, other], true).unwrap();

        // `user cache`: both terms must match — `cache` in the name, `user` in the
        // file path / qualified name → only UserCache (Widget and the import miss).
        let multi: Vec<String> = lb
            .search("user cache", 10, false)
            .unwrap()
            .into_iter()
            .map(|n| n.name)
            .collect();
        check!(multi == vec!["UserCache".to_string()]);

        // `cache` alone matches the struct and the import, but the bare import node
        // (module, empty file) is dropped.
        let single: Vec<String> = lb
            .search("cache", 10, false)
            .unwrap()
            .into_iter()
            .map(|n| n.name)
            .collect();
        check!(single.contains(&"UserCache".to_string()));
        check!(!single.iter().any(|n| n == "moka::Cache"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // #454: a name hit outranks a doc/file hit, so the relevant symbol surfaces
    // first instead of behind TODO comments.
    #[test]
    fn rank_search_hits_prefers_name_matches() {
        let mut by_doc = node("d", "Helper");
        by_doc.doc = Some("clears the cache on logout".into());
        let by_name = node("n", "UserCache");
        let mut nodes = vec![by_doc, by_name];
        rank_search_hits(&mut nodes, &["cache".to_string()], false);
        check!(nodes[0].name == "UserCache");
        check!(nodes[1].name == "Helper");
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("glyphtrail-lbug-{tag}-{nanos}"))
    }

    // Fresh-build bulk load (COPY) must survive special characters in fields —
    // newlines (multi-line doc comments), commas and quotes — round-tripping
    // intact. This is why COPY runs with PARALLEL=FALSE + RFC4180 quoting.
    #[test]
    fn copy_nodes_and_edges_preserve_special_chars() {
        let dir = tmp_dir("copy-special");
        let mut lb = LadybugStore::open(&dir).unwrap();
        let mut a = node("a", "na,me\"x");
        a.doc = Some("first line\n\"quoted, comma\"\nthird line".into());
        let b = node("b", "b");
        lb.insert_nodes(&[a.clone(), b.clone()], true).unwrap();
        let edge = Edge {
            src: NodeId("a".into()),
            dst: NodeId("b".into()),
            kind: EdgeKind::Calls,
            confidence: Confidence::Extracted,
        };
        lb.insert_edges(&[edge], true).unwrap();

        let got = lb.get_node("a").unwrap().unwrap();
        check!(got.name == a.name);
        check!(got.doc == a.doc);
        let neighbors = lb.neighbors("a", None, true).unwrap();
        check!(neighbors.len() == 1);
        check!(neighbors[0].0.id == NodeId("b".into()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn imports_insert_in_isolation() {
        let dir = tmp_dir("imports");
        let mut lb = LadybugStore::open(&dir).unwrap();
        lb.insert_imports(&[("a.rs".into(), "b".into(), "rust".into())])
            .unwrap();
        check!(lb.all_imports().unwrap().len() == 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    // #159: reopening must not wipe the DB. `clear()` (run by a non-update
    // analyze) used to delete the `Meta` node holding `schema_version`, so the
    // next open looked unversioned and `migrate_schema` rebuilt the schema,
    // dropping all data. `clear()` now preserves `Meta`.
    #[test]
    fn reopen_preserves_data_across_clear_and_migrate() {
        let dir = tmp_dir("reopen");
        {
            let mut lb = LadybugStore::open(&dir).unwrap();
            lb.clear().unwrap(); // mirrors a fresh (non-update) analyze
            lb.insert_graph(&[node("a", "keeper")], &[]).unwrap();
        }
        // Reopen in a second store instance: migrate must see the persisted
        // schema_version and leave the data intact.
        let lb = LadybugStore::open(&dir).unwrap();
        check!(
            !lb.find_by_name("keeper").unwrap().is_empty(),
            "node should survive reopen, but the DB was wiped"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // An index written by an older schema (e.g. before the #344 signature column)
    // is dropped + recreated on open, so the next analyze rebuilds it cleanly
    // instead of failing to COPY rows into a stale-shaped table.
    #[test]
    fn outdated_schema_version_is_rebuilt_on_open() {
        let dir = tmp_dir("schema-migrate");
        {
            let mut lb = LadybugStore::open(&dir).unwrap();
            lb.insert_graph(&[node("a", "stale")], &[]).unwrap();
            lb.set_meta("schema_version", "1").unwrap(); // a pre-#344 ("1") index
        }
        // Reopen: the version mismatch drops + recreates, wiping stale data.
        let lb = LadybugStore::open(&dir).unwrap();
        check!(lb.find_by_name("stale").unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    // #338: a read-only open must NOT migrate, so a reporting command (atlas
    // graph-embed) reading an out-of-date index can't wipe it.
    #[test]
    fn open_read_only_does_not_wipe_an_outdated_index() {
        let dir = tmp_dir("schema-readonly");
        {
            let mut lb = LadybugStore::open(&dir).unwrap();
            lb.insert_graph(&[node("a", "keeper")], &[]).unwrap();
            lb.set_meta("schema_version", "1").unwrap(); // a stale version
        }
        let lb = LadybugStore::open_read_only(&dir).unwrap();
        check!(
            !lb.find_by_name("keeper").unwrap().is_empty(),
            "read-only open must not drop+recreate the stale index"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // Atlas (#330): a Commit node (new kind) + its side-table row round-trip,
    // filtered by date window and in_bounds.
    #[test]
    fn atlas_commit_side_table_roundtrips() {
        let dir = tmp_dir("atlas-commit");
        let mut lb = LadybugStore::open(&dir).unwrap();
        let mut c = node("c1", "fix: thing");
        c.kind = NodeKind::Commit;
        lb.insert_graph(&[c], &[]).unwrap();
        let row = CommitMeta {
            node_id: NodeId("c1".into()),
            hash: "deadbeef".into(),
            author_email: "me@example.com".into(),
            committed_at: 1_700_000_000,
            subject: "fix: thing".into(),
            in_bounds: true,
        };
        lb.set_commits(std::slice::from_ref(&row)).unwrap();

        // Round-trips; the new node kind decodes back to Commit.
        let got = lb.commits_in_range(None, None).unwrap();
        check!(got == vec![row.clone()]);
        check!(lb.commit_count().unwrap() == 1);
        check!(lb.find_by_name("fix: thing").unwrap()[0].kind == NodeKind::Commit);
        // Date window filters on committed_at.
        check!(
            lb.commits_in_range(Some(1_700_000_001), None)
                .unwrap()
                .is_empty()
        );
        check!(
            lb.commits_in_range(None, Some(1_699_999_999))
                .unwrap()
                .is_empty()
        );
        // An out-of-bounds row is excluded (re-marked, not deleted).
        lb.set_commits(&[CommitMeta {
            in_bounds: false,
            ..row
        }])
        .unwrap();
        check!(lb.commits_in_range(None, None).unwrap().is_empty());
        // commit_count still sees the row — out-of-bounds is marked, not deleted.
        check!(lb.commit_count().unwrap() == 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    // Atlas (#338): embedding vectors round-trip through the side table, keyed by
    // node id, and an upsert replaces the prior vector.
    #[test]
    fn atlas_embedding_side_table_roundtrips() {
        let dir = tmp_dir("atlas-embed");
        let mut lb = LadybugStore::open(&dir).unwrap();
        let e = Embedding {
            node_id: NodeId("repo:demo".into()),
            vector: vec![0.5, -0.25, 0.0, 1.0],
        };
        lb.set_embeddings(std::slice::from_ref(&e), "lexical-hash-v1")
            .unwrap();
        check!(lb.embeddings().unwrap() == vec![e.clone()]);
        // Upsert by node id replaces, doesn't duplicate.
        let e2 = Embedding {
            node_id: NodeId("repo:demo".into()),
            vector: vec![1.0, 2.0],
        };
        lb.set_embeddings(std::slice::from_ref(&e2), "lexical-hash-v1")
            .unwrap();
        check!(lb.embeddings().unwrap() == vec![e2]);
        // clear_embeddings drops every row, so a re-embed starts clean.
        lb.clear_embeddings().unwrap();
        check!(lb.embeddings().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    // Atlas (#338): the text and graph embedding spaces share the side table; the
    // model-scoped clears delete one without touching the other.
    #[test]
    fn model_scoped_embedding_clears() {
        let dir = tmp_dir("atlas-embed-scoped");
        let mut lb = LadybugStore::open(&dir).unwrap();
        let text = Embedding {
            node_id: NodeId("repo:a".into()),
            vector: vec![1.0, 0.0],
        };
        let graph = Embedding {
            node_id: NodeId("graph:a".into()),
            vector: vec![0.0, 1.0],
        };
        lb.set_embeddings(std::slice::from_ref(&text), "lexical-hash-v1")
            .unwrap();
        lb.set_embeddings(std::slice::from_ref(&graph), "graph-struct-v1")
            .unwrap();
        check!(lb.embeddings().unwrap().len() == 2);
        // Clearing graph leaves text.
        lb.clear_embeddings_by_model("graph-struct-v1").unwrap();
        check!(lb.embeddings().unwrap() == vec![text.clone()]);
        // Re-add graph, then clear everything-except-graph leaves only graph.
        lb.set_embeddings(std::slice::from_ref(&graph), "graph-struct-v1")
            .unwrap();
        lb.clear_embeddings_except_model("graph-struct-v1").unwrap();
        check!(lb.embeddings().unwrap() == vec![graph]);
        std::fs::remove_dir_all(&dir).ok();
    }

    // Atlas (#338): node/edge-kind histograms feed the structural graph embedding.
    #[test]
    fn node_and_edge_kind_counts() {
        let dir = tmp_dir("kind-counts");
        let mut lb = LadybugStore::open(&dir).unwrap();
        let mut t = node("t", "users");
        t.kind = NodeKind::Table;
        lb.insert_nodes(&[node("a", "fa"), node("b", "fb"), t], true)
            .unwrap();
        lb.insert_edges(
            &[Edge {
                src: NodeId("a".into()),
                dst: NodeId("b".into()),
                kind: EdgeKind::Calls,
                confidence: Confidence::Extracted,
            }],
            true,
        )
        .unwrap();
        let nk = lb.node_kind_counts().unwrap();
        check!(nk.contains(&("function".to_string(), 2)));
        check!(nk.contains(&("table".to_string(), 1)));
        check!(lb.edge_kind_counts().unwrap() == vec![("calls".to_string(), 1)]);
        std::fs::remove_dir_all(&dir).ok();
    }

    // Atlas (#331): narrowing the window re-marks stored commits in/out of bounds
    // without deleting them, and widening brings them back.
    #[test]
    fn remark_commit_bounds_reflags_without_deleting() {
        let dir = tmp_dir("atlas-remark");
        let mut lb = LadybugStore::open(&dir).unwrap();
        let mk = |id: &str, at: i64| {
            let mut n = node(id, id);
            n.kind = NodeKind::Commit;
            (
                n,
                CommitMeta {
                    node_id: NodeId(id.into()),
                    hash: id.into(),
                    author_email: "me@x.dev".into(),
                    committed_at: at,
                    subject: id.into(),
                    in_bounds: true,
                },
            )
        };
        let (n_old, c_old) = mk("old", 1_000_000_000); // 2001
        let (n_new, c_new) = mk("new", 1_700_000_000); // 2023
        lb.insert_graph(&[n_old, n_new], &[]).unwrap();
        lb.set_commits(&[c_old, c_new]).unwrap();
        check!(lb.commits_in_range(None, None).unwrap().len() == 2);

        // Narrow to >= 2015: the 2001 commit falls out of bounds (still counted).
        lb.remark_commit_bounds(Some(1_420_070_400), None).unwrap();
        let in_bounds = lb.commits_in_range(None, None).unwrap();
        check!(in_bounds.len() == 1);
        check!(in_bounds[0].node_id == NodeId("new".into()));
        check!(lb.commit_count().unwrap() == 2);

        // Widen back to unbounded: both are in bounds again.
        lb.remark_commit_bounds(None, None).unwrap();
        check!(lb.commits_in_range(None, None).unwrap().len() == 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    // Atlas (#333): the timeline join resolves each in-bounds commit's repo name
    // and touched-file count, ordered by date, and honours the window.
    #[test]
    fn atlas_timeline_joins_repo_and_touched_count() {
        let dir = tmp_dir("atlas-timeline");
        let mut lb = LadybugStore::open(&dir).unwrap();
        let kinded = |id: &str, name: &str, k: NodeKind| {
            let mut n = node(id, name);
            n.kind = k;
            n
        };
        let edge = |src: &str, dst: &str, kind: EdgeKind| Edge {
            src: NodeId(src.into()),
            dst: NodeId(dst.into()),
            kind,
            confidence: Confidence::Extracted,
        };
        lb.insert_graph(
            &[
                kinded("cm", "did a thing", NodeKind::Commit),
                kinded("rp", "myrepo", NodeKind::Repo),
                kinded("f1", "a.rs", NodeKind::File),
                kinded("f2", "b.rs", NodeKind::File),
            ],
            &[
                edge("cm", "rp", EdgeKind::PartOf),
                edge("cm", "f1", EdgeKind::Touched),
                edge("cm", "f2", EdgeKind::Touched),
            ],
        )
        .unwrap();
        lb.set_commits(&[CommitMeta {
            node_id: NodeId("cm".into()),
            hash: "deadbeef".into(),
            author_email: "me@x.dev".into(),
            committed_at: 1_600_000_000,
            subject: "did a thing".into(),
            in_bounds: true,
        }])
        .unwrap();

        let rows = lb.atlas_timeline(None, None, None).unwrap();
        check!(rows.len() == 1);
        check!(rows[0].repo == "myrepo");
        check!(rows[0].touched == 2);
        check!(rows[0].commit.hash == "deadbeef");
        // Out-of-window excludes it.
        check!(
            lb.atlas_timeline(Some(1_600_000_001), None, None)
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // Atlas (#334): Tagged edges to Topic nodes drive `atlas_topics` counts and
    // the timeline topic filter.
    #[test]
    fn atlas_topics_and_topic_filter() {
        let dir = tmp_dir("atlas-topics");
        let mut lb = LadybugStore::open(&dir).unwrap();
        let kinded = |id: &str, name: &str, k: NodeKind| {
            let mut n = node(id, name);
            n.kind = k;
            n
        };
        let edge = |src: &str, dst: &str, kind: EdgeKind| Edge {
            src: NodeId(src.into()),
            dst: NodeId(dst.into()),
            kind,
            confidence: Confidence::Inferred,
        };
        // Two commits in one repo; only c1 is tagged "parser".
        lb.insert_graph(
            &[
                kinded("c1", "add parser", NodeKind::Commit),
                kinded("c2", "tweak ui", NodeKind::Commit),
                kinded("rp", "myrepo", NodeKind::Repo),
                kinded("tp", "parser", NodeKind::Topic),
            ],
            &[
                edge("c1", "rp", EdgeKind::PartOf),
                edge("c2", "rp", EdgeKind::PartOf),
                edge("c1", "tp", EdgeKind::Tagged),
            ],
        )
        .unwrap();
        let meta = |id: &str, at: i64| CommitMeta {
            node_id: NodeId(id.into()),
            hash: id.into(),
            author_email: "me@x.dev".into(),
            committed_at: at,
            subject: id.into(),
            in_bounds: true,
        };
        lb.set_commits(&[meta("c1", 1), meta("c2", 2)]).unwrap();

        let topics = lb.atlas_topics().unwrap();
        check!(topics == vec![("parser".to_string(), 1)]);
        // The topic filter keeps only the tagged commit.
        let rows = lb.atlas_timeline(None, None, Some("parser")).unwrap();
        check!(rows.len() == 1 && rows[0].commit.hash == "c1");
        // Unfiltered sees both.
        check!(lb.atlas_timeline(None, None, None).unwrap().len() == 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_filtered_pushes_kind_filters() {
        let dir = tmp_dir("export-filtered");
        let mut lb = LadybugStore::open(&dir).unwrap();
        let f = node("f", "fn"); // function
        let mut m = node("m", "mod");
        m.kind = NodeKind::Module;
        let mut c = node("c", "cmt");
        c.kind = NodeKind::Comment;
        let edges = vec![
            Edge {
                src: NodeId("f".into()),
                dst: NodeId("m".into()),
                kind: EdgeKind::Imports,
                confidence: Confidence::Extracted,
            },
            Edge {
                src: NodeId("f".into()),
                dst: NodeId("c".into()),
                kind: EdgeKind::Documents,
                confidence: Confidence::Extracted,
            },
        ];
        lb.insert_graph(&[f, m, c], &edges).unwrap();

        // function + module nodes, imports edges only.
        let (nodes, eds) = lb
            .export_filtered(
                Some(&["function".into(), "module".into()]),
                Some(&["imports".into()]),
                100,
            )
            .unwrap();
        let mut kinds: Vec<&str> = nodes.iter().map(|n| n.kind.as_str()).collect();
        kinds.sort();
        check!(kinds == vec!["function", "module"]); // comment dropped
        check!(eds.len() == 1 && eds[0].kind == EdgeKind::Imports); // documents + comment endpoint dropped

        // None keeps everything.
        let (all_n, all_e) = lb.export_filtered(None, None, 100).unwrap();
        check!(all_n.len() == 3 && all_e.len() == 2);

        // An explicit empty selection keeps nothing.
        let (none_n, none_e) = lb.export_filtered(Some(&[]), None, 100).unwrap();
        check!(none_n.is_empty() && none_e.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fixture_roundtrip_and_traversal() {
        let nodes = vec![node("a", "caller"), node("b", "callee")];
        let edges = vec![Edge {
            src: NodeId("a".into()),
            dst: NodeId("b".into()),
            kind: EdgeKind::Calls,
            confidence: Confidence::Extracted,
        }];

        let dir = tmp_dir("fixture");
        let mut lb = LadybugStore::open(&dir).unwrap();
        lb.insert_graph(&nodes, &edges).unwrap();

        // Inserted node/edge counts round-trip.
        let ls = lb.stats().unwrap();
        check!(ls.nodes == 2);
        check!(ls.edges == 1);

        // Core query set works against LadybugDB.
        check!(lb.find_by_name("caller").unwrap().len() == 1);
        check!(lb.get_node("b").unwrap().unwrap().name == "callee");
        let callers = lb.neighbors("b", Some(EdgeKind::Calls), false).unwrap();
        check!(callers.len() == 1);
        check!(callers[0].0.name == "caller");
        // reachable: who is impacted if b changes -> a (its caller).
        let impacted = lb.reachable("b", EdgeKind::Calls, false, 5).unwrap();
        check!(impacted.len() == 1);
        check!(impacted[0].name == "caller");

        // meta + files round-trip.
        lb.set_meta("tool_version", "9.9").unwrap();
        check!(lb.get_meta("tool_version").unwrap().as_deref() == Some("9.9"));
        lb.set_file("a.rs", Some("rust"), "h1").unwrap();
        check!(lb.file_hash("a.rs").unwrap().as_deref() == Some("h1"));

        // The analyze write sequence (operations + imports + pending) must work
        // after node inserts — exercises the mixed-write path that hit lbug's
        // parameter-type cache.
        lb.insert_operations(&[(
            NodeId("a".into()),
            glyphtrail_core::OperationKey::rest(glyphtrail_core::HttpMethod::Get, "/x"),
        )])
        .unwrap();
        lb.insert_imports(&[("a.rs".into(), "b".into(), "rust".into())])
            .unwrap();
        lb.insert_pending(&[glyphtrail_core::PendingLink {
            anchor: NodeId("a".into()),
            name: "callee".into(),
            kind: EdgeKind::Calls,
            name_is_src: false,
        }])
        .unwrap();
        check!(lb.all_imports().unwrap().len() == 1);
        check!(lb.all_pending().unwrap().len() == 1);
        check!(lb.operations_by_kind(NodeKind::Function).unwrap().len() == 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
