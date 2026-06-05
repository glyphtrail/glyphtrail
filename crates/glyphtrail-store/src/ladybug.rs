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
use std::path::{Path, PathBuf};

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
    // Atlas (#338/#473): embedding namespaces. Vectors live in native `FLOAT[dim]`
    // columns in a per-`(space, model)` table (`Vec_<space>_<slug>_<hash>`), created
    // on demand, so storage is compact and similarity runs server-side
    // (`array_cosine_similarity`, or the HNSW index when the vector extension is
    // loaded). This catalog is one row per namespace — what's stored, where —
    // `pk = space \x1f model`. (Pre-#473 `Embedding`/`EmbeddingV2` tables are dropped
    // on open; those embeddings are regenerable via `atlas embed*`.)
    "CREATE NODE TABLE IF NOT EXISTS EmbeddingNs(pk STRING, space STRING, model STRING, dim INT64, PRIMARY KEY(pk))",
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

/// Versions the *embedding* tables (`EmbeddingNs` + the per-namespace `Vec_*`
/// columns) independently of [`SCHEMA_VERSION`]. Atlas embeddings can be expensive
/// to recompute (a paid API call per document), so a code-graph schema bump must
/// NOT discard them: `migrate_schema` rebuilds only the code-graph tables and
/// leaves the embedding tables alone. They are rebuilt solely when *their own*
/// shape changes and this version is bumped. A DB that predates this versioning
/// (no stored `embedding_schema_version`) is grandfathered as current, never wiped.
pub const EMBEDDING_SCHEMA_VERSION: &str = "1";

pub struct LadybugStore {
    db: Database,
    /// The database directory, so embedding writes can mirror a portable Parquet
    /// backup into `<root>/embeddings-backup/` (#473).
    root: PathBuf,
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
            // Drop pre-#473 embedding tables if an older atlas left them; those
            // embeddings are regenerable via `atlas embed*` (#338/#473). Best-effort.
            // The per-(space,model) `Vec_*` tables are left (no fixed names to drop);
            // a re-embed replaces them.
            let _ = conn.query("DROP TABLE Embedding");
            let _ = conn.query("DROP TABLE EmbeddingV2");
        }
        let mut store = Self {
            db,
            root: path.to_path_buf(),
        };
        store.migrate_schema()?;
        store.migrate_embeddings()?;
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
        Ok(Self {
            db,
            root: path.to_path_buf(),
        })
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

    /// Drop + recreate the *code-graph* tables when the stored `schema_version`
    /// differs from the current one; the index is rebuilt on the next `analyze`.
    /// The embedding tables (`EmbeddingNs` + the `Vec_*` columns) and `Meta` are
    /// deliberately preserved — atlas embeddings are expensive to recompute and
    /// don't depend on the code-graph schema, so a code schema bump must not lose
    /// them (their own migration is [`Self::migrate_embeddings`]). #473.
    fn migrate_schema(&mut self) -> Result<()> {
        if self.get_meta("schema_version")?.as_deref() == Some(SCHEMA_VERSION) {
            return Ok(());
        }
        {
            let conn = self.conn()?;
            // Drop the rel table before its node tables. Ignore "missing table"
            // on a partial DB; the schema is recreated immediately after. Only the
            // code-graph tables (+ the legacy pre-#473 embedding STRING tables) are
            // dropped — `EmbeddingNs`, the `Vec_*` tables, and `Meta` are kept.
            for tbl in [
                "Edge",
                "Node",
                "File",
                "ApiOp",
                "Commit",
                "Embedding", // pre-#473 STRING embeddings, dropped if present
                "EmbeddingV2",
                "Pending",
                "Import",
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

    /// Migrate the embedding tables, versioned independently of the code graph so a
    /// [`SCHEMA_VERSION`] bump never discards paid-for atlas embeddings. A DB with
    /// no stored `embedding_schema_version` is grandfathered as current (its
    /// embeddings, if any, are kept); only a genuinely stale stored version — i.e.
    /// a future change to the embedding table shape — wipes them. #338/#473.
    fn migrate_embeddings(&mut self) -> Result<()> {
        match self.get_meta("embedding_schema_version")?.as_deref() {
            Some(EMBEDDING_SCHEMA_VERSION) => return Ok(()),
            None => {} // grandfather: stamp current, never wipe existing embeddings
            Some(_stale) => self.wipe_embeddings()?, // embedding shape changed
        }
        self.set_meta("embedding_schema_version", EMBEDDING_SCHEMA_VERSION)?;
        Ok(())
    }

    /// Drop every embedding namespace (the catalog rows, their `Vec_*` tables, and
    /// the active-model pointers). Used only by an embedding-schema-version bump;
    /// callers re-embed or re-import afterwards. Best-effort.
    fn wipe_embeddings(&mut self) -> Result<()> {
        let namespaces = self.run("MATCH (n:EmbeddingNs) RETURN n.space, n.model", vec![])?;
        for row in &namespaces {
            let (space, model) = (get_str(row, 0), get_str(row, 1));
            if space.is_empty() && model.is_empty() {
                continue;
            }
            self.drop_vec_table(&glyphtrail_core::vec_table(&space, &model));
        }
        let _ = self.run("MATCH (n:EmbeddingNs) DELETE n", vec![]);
        for space in ["text", "graph", "commit"] {
            let _ = self.set_meta(&format!("active_model_{space}"), "");
        }
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

/// Decode a `FLOAT[]` column value (an lbug `Array` of `Float`) into `Vec<f32>`
/// (#473); anything unexpected yields an empty vector.
fn decode_float_array(value: Option<&Value>) -> Vec<f32> {
    match value {
        Some(Value::Array(_, items)) | Some(Value::List(_, items)) => items
            .iter()
            .map(|v| match v {
                Value::Float(x) => *x,
                Value::Double(x) => *x as f32,
                _ => 0.0,
            })
            .collect(),
        _ => Vec::new(),
    }
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
/// A `FLOAT[n]` array parameter value for the lbug vector extension (#338): the
/// first field is the *child* type (`Float`); the array length is the vec length.
fn float_array(v: &[f32]) -> Value {
    Value::Array(
        LogicalType::Float,
        v.iter().map(|&x| Value::Float(x)).collect(),
    )
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

    /// Load the lbug `vector` extension (HNSW ANN) if it's already installed, so
    /// `vector_search` can use a `QUERY_VECTOR_INDEX`. Cheap and offline; returns
    /// whether it loaded. A loaded extension persists for the open `Database`, so one
    /// call per process suffices (#338, #473).
    pub fn load_vector_ext(&self) -> bool {
        self.run("LOAD EXTENSION VECTOR", vec![]).is_ok()
    }

    /// Ensure the `vector` extension is available, installing it first if needed.
    /// `INSTALL` performs a one-time download of the platform extension binary
    /// (network) — used only on the write path (`atlas embed`/`graph-embed`), where
    /// a documented, opt-in network fetch is acceptable. Returns whether it's usable.
    pub fn install_vector_ext(&self) -> bool {
        if self.load_vector_ext() {
            return true;
        }
        let _ = self.run("INSTALL VECTOR", vec![]);
        self.load_vector_ext()
    }

    /// Drop a namespace's `FLOAT[]` table, removing any HNSW index first (a table
    /// with a vector index can't be dropped directly). Best-effort — a missing
    /// table / index / extension is ignored.
    fn drop_vec_table(&self, table: &str) {
        let _ = self.run(
            &format!("CALL DROP_VECTOR_INDEX('{table}', '{table}_idx')"),
            vec![],
        );
        let _ = self.run(&format!("DROP TABLE {table}"), vec![]);
    }

    /// The portable embedding backup directory beside the database. LadybugDB stores
    /// the database as a single file, so the backup is a sibling (`<db>-embeddings-
    /// backup/`), never a child.
    fn backup_dir(&self) -> PathBuf {
        let name = self.root.file_name().map(|n| {
            let mut s = n.to_os_string();
            s.push("-embeddings-backup");
            s
        });
        match name {
            Some(n) => self.root.with_file_name(n),
            None => self.root.join("embeddings-backup"),
        }
    }

    /// Mirror every embedding namespace to a portable Parquet backup under
    /// `<root>/embeddings-backup/` (one `<table>.parquet` per namespace + a
    /// `manifest.json`), so the paid-for vectors survive even a full database loss
    /// and can be copied to another machine. Driven off the `EmbeddingNs` catalog so
    /// the backup always matches the live data; an empty catalog removes the backup.
    /// Best-effort: a backup write never fails an embedding store (callers ignore the
    /// error). Uses LadybugDB's native `COPY … TO` (no extra dependency). #473.
    fn write_embedding_backup(&self) -> Result<()> {
        let dir = self.backup_dir();
        let catalog = self.run(
            "MATCH (n:EmbeddingNs) RETURN n.space, n.model, n.dim ORDER BY n.space, n.model",
            vec![],
        )?;
        if catalog.is_empty() {
            // No embeddings left — drop a stale backup so it can't resurrect deleted
            // namespaces on a later restore.
            let _ = std::fs::remove_dir_all(&dir);
            return Ok(());
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut entries = Vec::new();
        for row in &catalog {
            let (space, model, dim) = (get_str(row, 0), get_str(row, 1), get_i64(row, 2));
            let table = glyphtrail_core::vec_table(&space, &model);
            let file = dir.join(format!("{table}.parquet"));
            // COPY TO refuses to overwrite, so clear a prior dump first.
            let _ = std::fs::remove_file(&file);
            self.run(
                &format!(
                    "COPY (MATCH (v:{table}) RETURN v.node_id, v.vec) TO '{}'",
                    file.display()
                ),
                vec![],
            )?;
            let active =
                self.get_meta(&format!("active_model_{space}"))?.as_deref() == Some(&model);
            let base_url = self
                .get_meta(&format!("active_base_url_{space}"))?
                .unwrap_or_default();
            entries.push(serde_json::json!({
                "space": space,
                "model": model,
                "dim": dim,
                "file": format!("{table}.parquet"),
                "active": active,
                "base_url": base_url,
            }));
        }
        let manifest = serde_json::json!({
            "embedding_schema_version": EMBEDDING_SCHEMA_VERSION,
            "namespaces": entries,
        });
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        Ok(())
    }

    /// Restore embeddings from the Parquet backup written by
    /// [`Self::write_embedding_backup`], replacing each namespace and re-stamping the
    /// active-model pointers. Returns the number of namespaces restored. Skips a
    /// backup whose `embedding_schema_version` doesn't match this binary's (its
    /// table shape would be stale). #473.
    pub fn restore_embedding_backup(&mut self) -> Result<usize> {
        let dir = self.backup_dir();
        let path = dir.join("manifest.json");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("no embedding backup at {}", path.display()))?;
        let manifest: serde_json::Value = serde_json::from_str(&text)?;
        if manifest["embedding_schema_version"].as_str() != Some(EMBEDDING_SCHEMA_VERSION) {
            anyhow::bail!(
                "embedding backup was written for a different embedding schema version; \
                 re-embed or use `atlas embed-import` instead"
            );
        }
        let Some(namespaces) = manifest["namespaces"].as_array() else {
            return Ok(0);
        };
        let mut restored = 0;
        for ns in namespaces {
            let (Some(space), Some(model), Some(dim), Some(file)) = (
                ns["space"].as_str(),
                ns["model"].as_str(),
                ns["dim"].as_i64(),
                ns["file"].as_str(),
            ) else {
                continue;
            };
            let parquet = dir.join(file);
            if !parquet.exists() {
                continue;
            }
            let table = glyphtrail_core::vec_table(space, model);
            self.drop_vec_table(&table);
            self.run(
                &format!(
                    "CREATE NODE TABLE {table}(node_id STRING, vec FLOAT[{dim}], PRIMARY KEY(node_id))"
                ),
                vec![],
            )?;
            self.run(
                &format!("COPY {table} FROM '{}'", parquet.display()),
                vec![],
            )?;
            self.run(
                "MERGE (n:EmbeddingNs {pk:$pk}) SET n.space=$sp, n.model=$m, n.dim=$d",
                vec![
                    ("pk", s(&format!("{space}\u{1f}{model}"))),
                    ("sp", s(space)),
                    ("m", s(model)),
                    ("d", Value::Int64(dim)),
                ],
            )?;
            if ns["active"].as_bool() == Some(true) {
                self.set_meta(&format!("active_model_{space}"), model)?;
                self.set_meta(&format!("active_dim_{space}"), &dim.to_string())?;
                if let Some(base) = ns["base_url"].as_str().filter(|b| !b.is_empty()) {
                    self.set_meta(&format!("active_base_url_{space}"), base)?;
                }
            }
            restored += 1;
        }
        Ok(restored)
    }

    /// Build the HNSW cosine index on a namespace's `FLOAT[]` table when the vector
    /// extension is loaded, for fast ANN; returns whether it was built. The table is
    /// created by [`GraphStore::set_embeddings`]; this just adds the index. No-op
    /// (returns `false`) when the extension isn't available — search then falls back
    /// to server-side `array_cosine_similarity` over the same column.
    pub fn build_vector_index(&self, space: &str, model: &str) -> Result<bool> {
        if !self.load_vector_ext() {
            return Ok(false);
        }
        let table = glyphtrail_core::vec_table(space, model);
        self.run(
            &format!("CALL CREATE_VECTOR_INDEX('{table}','{table}_idx','vec', metric := 'cosine')"),
            vec![],
        )?;
        Ok(true)
    }

    /// Cosine-nearest `k` neighbours of `query` in a `(space, model)` namespace, as
    /// `(node_id, similarity)` (higher is nearer), nearest-first. Uses the HNSW index
    /// when present, else server-side `array_cosine_similarity` over the `FLOAT[]`
    /// column — so search works with or without the extension.
    pub fn vector_search(
        &self,
        space: &str,
        model: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(NodeId, f32)>> {
        let table = glyphtrail_core::vec_table(space, model);
        // HNSW path (extension + index): distance → similarity = 1 - distance.
        if self.load_vector_ext()
            && let Ok(rows) = self.run(
                &format!(
                    "CALL QUERY_VECTOR_INDEX('{table}','{table}_idx',$q,{k}) \
                     RETURN node.node_id AS id, distance ORDER BY distance"
                ),
                vec![("q", float_array(query))],
            )
        {
            return Ok(rows
                .iter()
                .map(|r| {
                    let distance = match r.get(1) {
                        Some(Value::Double(x)) => *x as f32,
                        Some(Value::Float(x)) => *x,
                        _ => f32::INFINITY,
                    };
                    (NodeId(get_str(r, 0)), 1.0 - distance)
                })
                .collect());
        }
        // Server-side exact cosine (no extension); the function returns similarity.
        let rows = self.run(
            &format!(
                "MATCH (v:{table}) RETURN v.node_id, array_cosine_similarity(v.vec, $q) AS sim \
                 ORDER BY sim DESC LIMIT {k}"
            ),
            vec![("q", float_array(query))],
        )?;
        Ok(rows
            .iter()
            .map(|r| {
                let sim = match r.get(1) {
                    Some(Value::Float(x)) => *x,
                    Some(Value::Double(x)) => *x as f32,
                    _ => 0.0,
                };
                (NodeId(get_str(r, 0)), sim)
            })
            .collect())
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

    fn set_embeddings(&mut self, space: &str, model: &str, embeddings: &[Embedding]) -> Result<()> {
        let Some(first) = embeddings.first() else {
            // Nothing to store; ensure the namespace is cleared.
            return self.clear_embeddings_for(space, model);
        };
        let dim = first.vector.len();
        if dim == 0 {
            anyhow::bail!("cannot store zero-length embeddings");
        }
        if embeddings.iter().any(|e| e.vector.len() != dim) {
            anyhow::bail!("cannot store mixed-dimension embeddings in one namespace");
        }
        let table = glyphtrail_core::vec_table(space, model);
        // Replace the namespace's `FLOAT[dim]` table (vectors are the source of
        // truth; storage is native, search runs server-side / via HNSW).
        self.drop_vec_table(&table);
        self.run(
            &format!(
                "CREATE NODE TABLE {table}(node_id STRING, vec FLOAT[{dim}], PRIMARY KEY(node_id))"
            ),
            vec![],
        )?;
        for e in embeddings {
            self.run(
                &format!("CREATE (v:{table} {{node_id:$id, vec:$e}})"),
                vec![("id", s(&e.node_id.0)), ("e", float_array(&e.vector))],
            )?;
        }
        // Catalog the namespace (one row), so `embedding_index` can enumerate them.
        self.run(
            "MERGE (n:EmbeddingNs {pk:$pk}) SET n.space=$sp, n.model=$m, n.dim=$d",
            vec![
                ("pk", s(&format!("{space}\u{1f}{model}"))),
                ("sp", s(space)),
                ("m", s(model)),
                ("d", Value::Int64(dim as i64)),
            ],
        )?;
        // Refresh the portable Parquet backup so the paid-for vectors survive a DB
        // loss / upgrade; never fail the store on a backup error.
        let _ = self.write_embedding_backup();
        Ok(())
    }

    fn clear_embeddings_for(&mut self, space: &str, model: &str) -> Result<()> {
        self.drop_vec_table(&glyphtrail_core::vec_table(space, model));
        self.run(
            "MATCH (n:EmbeddingNs) WHERE n.pk = $pk DELETE n",
            vec![("pk", s(&format!("{space}\u{1f}{model}")))],
        )?;
        let _ = self.write_embedding_backup();
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

    fn embeddings_for(&self, space: &str, model: &str) -> Result<Vec<Embedding>> {
        let table = glyphtrail_core::vec_table(space, model);
        // The namespace may not exist (nothing embedded for it yet); a missing table
        // is not an error here — return empty.
        let Ok(rows) = self.run(
            &format!("MATCH (v:{table}) RETURN v.node_id, v.vec ORDER BY v.node_id"),
            vec![],
        ) else {
            return Ok(Vec::new());
        };
        Ok(rows
            .iter()
            .map(|r| Embedding {
                node_id: NodeId(get_str(r, 0)),
                vector: decode_float_array(r.get(1)),
            })
            .collect())
    }

    fn embedding_index(&self) -> Result<Vec<(String, String, usize, usize)>> {
        // Counts come from the per-namespace tables; the catalog gives space/model/dim.
        let mut out = Vec::new();
        for r in self.run(
            "MATCH (n:EmbeddingNs) RETURN n.space, n.model, n.dim ORDER BY n.space, n.model",
            vec![],
        )? {
            let (space, model, dim) = (
                get_str(&r, 0),
                get_str(&r, 1),
                get_i64(&r, 2).max(0) as usize,
            );
            let table = glyphtrail_core::vec_table(&space, &model);
            let count = self
                .run(&format!("MATCH (v:{table}) RETURN COUNT(*)"), vec![])
                .ok()
                .and_then(|rs| rs.first().map(|r| get_i64(r, 0).max(0) as usize))
                .unwrap_or(0);
            out.push((space, model, count, dim));
        }
        Ok(out)
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

    fn atlas_commit_rows(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<glyphtrail_core::AtlasTimelineRow>> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        // Inline the ids as a Cypher string-list literal (node ids are derived
        // hex/identifier hashes, so quote-stripping is belt-and-braces). This
        // restricts the commit→repo join to just the ANN hit set.
        let list = node_ids
            .iter()
            .map(|i| format!("'{}'", i.replace('\'', "")))
            .collect::<Vec<_>>()
            .join(",");
        Ok(self
            .run(
                &format!(
                    "MATCH (c:Commit), (cn:Node)-[:Edge {{kind:'part_of'}}]->(r:Node) \
                     WHERE cn.kind = 'commit' AND cn.id = c.node_id AND r.kind = 'repo' \
                     AND c.node_id IN [{list}] \
                     RETURN c.node_id, c.hash, c.author_email, c.committed_at, c.subject, \
                     c.in_bounds, r.name"
                ),
                vec![],
            )?
            .iter()
            .map(|r| glyphtrail_core::AtlasTimelineRow {
                touched: 0,
                repo: get_str(r, 6),
                commit: commit_from_row(r),
            })
            .collect())
    }

    fn commit_touched_paths(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .run(
                "MATCH (c:Node {kind:'commit'})-[:Edge {kind:'touched'}]->(f:Node {kind:'file'}) \
                 RETURN c.id, f.name",
                vec![],
            )?
            .iter()
            .map(|r| (get_str(r, 0), get_str(r, 1)))
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

    // #473: FLOAT[] storage round-trips and `vector_search` ranks by cosine —
    // server-side without the extension (always), and via HNSW when it's loaded.
    #[test]
    fn vector_search_float_storage_and_cosine() {
        let dir = tmp_dir("vec-search");
        let mut lb = LadybugStore::open(&dir).unwrap();
        let embs = vec![
            Embedding {
                node_id: NodeId("a".into()),
                vector: vec![1.0, 0.0, 0.0],
            },
            Embedding {
                node_id: NodeId("b".into()),
                vector: vec![0.0, 1.0, 0.0],
            },
            Embedding {
                node_id: NodeId("c".into()),
                vector: vec![0.9, 0.1, 0.0],
            },
        ];
        lb.set_embeddings("text", "m", &embs).unwrap();
        // FLOAT[] storage round-trips (the vectors are the source of truth).
        check!(lb.embeddings_for("text", "m").unwrap() == embs);
        check!(lb.embedding_index().unwrap() == vec![("text".into(), "m".into(), 3, 3)]);

        // Server-side cosine — no extension needed.
        let hits = lb.vector_search("text", "m", &[1.0, 0.05, 0.0], 2).unwrap();
        check!(hits.len() == 2);
        check!(hits[0].0 == NodeId("a".into())); // nearest
        check!(hits[0].1 > hits[1].1 && hits[0].1 > 0.99);

        // HNSW path when the extension is available (skips offline).
        if lb.build_vector_index("text", "m").unwrap() {
            let h = lb.vector_search("text", "m", &[1.0, 0.05, 0.0], 2).unwrap();
            check!(h[0].0 == NodeId("a".into()) && h[0].1 > 0.99);
        }

        // Clearing the namespace drops its table + catalog row.
        lb.clear_embeddings_for("text", "m").unwrap();
        check!(lb.embeddings_for("text", "m").unwrap().is_empty());
        check!(lb.embedding_index().unwrap().is_empty());
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

    // #473: a code-graph schema bump must rebuild the code tables WITHOUT discarding
    // the (expensive, API-derived) atlas embeddings. Simulates an upgrade by stamping
    // a stale `schema_version`, then checks the embeddings survive the reopen.
    #[test]
    fn embeddings_survive_a_code_schema_bump() {
        let dir = tmp_dir("embed-migrate");
        let embs = vec![
            Embedding {
                node_id: NodeId("a".into()),
                vector: vec![1.0, 0.0, 0.0],
            },
            Embedding {
                node_id: NodeId("b".into()),
                vector: vec![0.0, 1.0, 0.0],
            },
        ];
        {
            let mut lb = LadybugStore::open(&dir).unwrap();
            lb.insert_graph(&[node("a", "code_node")], &[]).unwrap();
            lb.set_embeddings("text", "openai:m", &embs).unwrap();
            lb.set_meta("active_model_text", "openai:m").unwrap();
            // Simulate a future binary whose SCHEMA_VERSION moved on.
            lb.set_meta("schema_version", "1").unwrap();
        }
        // Reopen with the current binary: migrate rebuilds the code graph (so the
        // code node is gone, awaiting re-analyze) but the embeddings are untouched.
        let lb = LadybugStore::open(&dir).unwrap();
        check!(lb.find_by_name("code_node").unwrap().is_empty());
        check!(lb.embeddings_for("text", "openai:m").unwrap() == embs);
        check!(lb.embedding_index().unwrap() == vec![("text".into(), "openai:m".into(), 2, 3)]);
        check!(lb.get_meta("active_model_text").unwrap().as_deref() == Some("openai:m"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // #473: `set_embeddings` mirrors the vectors to a portable Parquet backup, so
    // even a full database loss is recoverable. Simulate the loss by restoring the
    // backup into a fresh database in a second directory.
    #[test]
    fn embedding_parquet_backup_round_trips() {
        let dir = tmp_dir("embed-backup");
        let embs = vec![
            Embedding {
                node_id: NodeId("a".into()),
                vector: vec![1.0, 0.0, 0.0],
            },
            Embedding {
                node_id: NodeId("b".into()),
                vector: vec![0.0, 1.0, 0.0],
            },
        ];
        // The backup is a sibling of the single-file database.
        let backup_src = dir.with_file_name(format!(
            "{}-embeddings-backup",
            dir.file_name().unwrap().to_string_lossy()
        ));
        {
            let mut lb = LadybugStore::open(&dir).unwrap();
            lb.set_meta("active_model_text", "openai:m").unwrap();
            lb.set_embeddings("text", "openai:m", &embs).unwrap();
            // The backup was mirrored automatically.
            check!(backup_src.join("manifest.json").exists());
        }
        // A fresh database elsewhere with only the copied backup dir (the DB itself
        // is "lost").
        let dir2 = tmp_dir("embed-restore");
        let dst = dir2.with_file_name(format!(
            "{}-embeddings-backup",
            dir2.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&dst).unwrap();
        for entry in std::fs::read_dir(&backup_src).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), dst.join(entry.file_name())).unwrap();
        }
        let mut lb2 = LadybugStore::open(&dir2).unwrap();
        check!(lb2.embeddings_for("text", "openai:m").unwrap().is_empty()); // lost
        let restored = lb2.restore_embedding_backup().unwrap();
        check!(restored == 1);
        check!(lb2.embeddings_for("text", "openai:m").unwrap() == embs);
        check!(lb2.embedding_index().unwrap() == vec![("text".into(), "openai:m".into(), 2, 3)]);
        check!(lb2.get_meta("active_model_text").unwrap().as_deref() == Some("openai:m"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dir2).ok();
        std::fs::remove_dir_all(&backup_src).ok();
        std::fs::remove_dir_all(&dst).ok();
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

    // Atlas (#338): embeddings round-trip through the side table per (space, model)
    // namespace; an upsert replaces the prior vector for the same node+model.
    #[test]
    fn atlas_embedding_side_table_roundtrips() {
        let dir = tmp_dir("atlas-embed");
        let mut lb = LadybugStore::open(&dir).unwrap();
        let e = Embedding {
            node_id: NodeId("repo:demo".into()),
            vector: vec![0.5, -0.25, 0.0, 1.0],
        };
        lb.set_embeddings("text", "lexical-hash-v1", std::slice::from_ref(&e))
            .unwrap();
        check!(lb.embeddings_for("text", "lexical-hash-v1").unwrap() == vec![e.clone()]);
        // Upsert by (node, model) replaces, doesn't duplicate.
        let e2 = Embedding {
            node_id: NodeId("repo:demo".into()),
            vector: vec![1.0, 2.0],
        };
        lb.set_embeddings("text", "lexical-hash-v1", std::slice::from_ref(&e2))
            .unwrap();
        check!(lb.embeddings_for("text", "lexical-hash-v1").unwrap() == vec![e2]);
        // clear_embeddings_for drops just that namespace.
        lb.clear_embeddings_for("text", "lexical-hash-v1").unwrap();
        check!(
            lb.embeddings_for("text", "lexical-hash-v1")
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // Atlas (#338): several (space, model) namespaces coexist without mixing — the
    // same repo embedded under two models is two rows; clearing one leaves the rest.
    #[test]
    fn embedding_namespaces_coexist_without_mixing() {
        let dir = tmp_dir("atlas-embed-ns");
        let mut lb = LadybugStore::open(&dir).unwrap();
        let lex = Embedding {
            node_id: NodeId("repo:a".into()),
            vector: vec![1.0, 0.0],
        };
        let neural = Embedding {
            node_id: NodeId("repo:a".into()), // SAME node, different model
            vector: vec![0.0, 1.0, 0.5, 0.5],
        };
        let graph = Embedding {
            node_id: NodeId("graph:a".into()),
            vector: vec![0.2, 0.8],
        };
        lb.set_embeddings("text", "lexical-hash-v1", std::slice::from_ref(&lex))
            .unwrap();
        lb.set_embeddings("text", "openai:m", std::slice::from_ref(&neural))
            .unwrap();
        lb.set_embeddings("graph", "graph-struct-v1", std::slice::from_ref(&graph))
            .unwrap();
        // Three distinct namespaces, each isolated.
        let mut idx = lb.embedding_index().unwrap();
        idx.sort();
        check!(
            idx == vec![
                ("graph".into(), "graph-struct-v1".into(), 1, 2),
                ("text".into(), "lexical-hash-v1".into(), 1, 2),
                ("text".into(), "openai:m".into(), 1, 4),
            ]
        );
        check!(lb.embeddings_for("text", "lexical-hash-v1").unwrap() == vec![lex]);
        check!(lb.embeddings_for("text", "openai:m").unwrap() == vec![neural]);
        // Clearing one namespace leaves the others.
        lb.clear_embeddings_for("text", "lexical-hash-v1").unwrap();
        check!(
            lb.embeddings_for("text", "lexical-hash-v1")
                .unwrap()
                .is_empty()
        );
        check!(lb.embeddings_for("text", "openai:m").unwrap().len() == 1);
        check!(lb.embeddings_for("graph", "graph-struct-v1").unwrap().len() == 1);
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
