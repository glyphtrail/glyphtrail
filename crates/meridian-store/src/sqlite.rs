use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use meridian_core::{
    Adjacency, ClassifiedItem, Confidence, Direction, Edge, EdgeKind, HttpMethod, ImpactPolicy,
    Node, NodeId, NodeKind, OperationKey, PendingLink, Protocol, Span, classify, compute_impact,
    is_cross_boundary_path,
};
use rusqlite::{Connection, params};

/// Persistent code graph backed by SQLite (+ FTS5 search). The first storage
/// backend; a LadybugDB backend implementing the same surface comes later.
pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(include_str!("schema.sql"))?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(include_str!("schema.sql"))?;
        Ok(Self { conn })
    }

    /// Wipe all indexed data (full reindex).
    pub fn clear(&mut self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM edges; DELETE FROM nodes; DELETE FROM files; DELETE FROM nodes_fts;
             DELETE FROM api_operations; DELETE FROM pending_edges; DELETE FROM pending_imports;",
        )?;
        Ok(())
    }

    /// Persisted hash for a file, if indexed before.
    pub fn file_hash(&self, path: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT hash FROM files WHERE path = ?1")?;
        let mut rows = stmt.query(params![path])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub fn all_files(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM files")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every indexed file with its stored content hash, for change detection.
    pub fn files_with_hashes(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT path, hash FROM files")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Read an index-wide marker from the `meta` table.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    /// Write an index-wide marker to the `meta` table.
    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }

    /// Remove a file's record and all nodes/edges it owns.
    pub fn delete_file_data(&mut self, path: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM edges WHERE src IN (SELECT id FROM nodes WHERE file = ?1)
                OR dst IN (SELECT id FROM nodes WHERE file = ?1)",
            params![path],
        )?;
        tx.execute(
            "DELETE FROM nodes_fts WHERE id IN (SELECT id FROM nodes WHERE file = ?1)",
            params![path],
        )?;
        tx.execute(
            "DELETE FROM api_operations WHERE node_id IN (SELECT id FROM nodes WHERE file = ?1)",
            params![path],
        )?;
        tx.execute(
            "DELETE FROM pending_edges WHERE anchor IN (SELECT id FROM nodes WHERE file = ?1)",
            params![path],
        )?;
        tx.execute(
            "DELETE FROM pending_imports WHERE importer = ?1",
            params![path],
        )?;
        tx.execute("DELETE FROM nodes WHERE file = ?1", params![path])?;
        tx.execute("DELETE FROM files WHERE path = ?1", params![path])?;
        tx.commit()?;
        Ok(())
    }

    /// Remove every node of `kind` along with its edges, operations and FTS
    /// rows. Used to fully rebuild config-derived nodes (e.g. schema ops) on
    /// each run, so entries removed from config/specs don't linger.
    pub fn delete_nodes_by_kind(&mut self, kind: NodeKind) -> Result<()> {
        let k = kind.as_str();
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM edges WHERE src IN (SELECT id FROM nodes WHERE kind = ?1)
                OR dst IN (SELECT id FROM nodes WHERE kind = ?1)",
            params![k],
        )?;
        tx.execute(
            "DELETE FROM api_operations WHERE node_id IN (SELECT id FROM nodes WHERE kind = ?1)",
            params![k],
        )?;
        tx.execute(
            "DELETE FROM nodes_fts WHERE id IN (SELECT id FROM nodes WHERE kind = ?1)",
            params![k],
        )?;
        tx.execute("DELETE FROM nodes WHERE kind = ?1", params![k])?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_file(&mut self, path: &str, language: Option<&str>, hash: &str) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.conn.execute(
            "INSERT INTO files(path, language, hash, indexed_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET language=?2, hash=?3, indexed_at=?4",
            params![path, language, hash, now],
        )?;
        Ok(())
    }

    /// Insert nodes and edges. Existing rows are merged (nodes by id, edges by
    /// (src,dst,kind)). On an edge conflict an `Extracted` row always wins over
    /// an `Inferred` one regardless of insertion order, so an inferred edge
    /// already in the DB cannot mask a later extracted edge.
    pub fn insert_graph(&mut self, nodes: &[Node], edges: &[Edge]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut node_stmt = tx.prepare(
                "INSERT INTO nodes(id, kind, name, qualified_name, file, language,
                    start_byte, end_byte, start_line, end_line, doc)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                 ON CONFLICT(id) DO UPDATE SET
                    kind=?2, name=?3, qualified_name=?4, file=?5, language=?6,
                    start_byte=?7, end_byte=?8, start_line=?9, end_line=?10, doc=?11",
            )?;
            // nodes_fts is a virtual table without a unique constraint, so an
            // upsert is not possible; clear any prior row for this id first to
            // keep FTS in sync with the (upserted) nodes row and avoid dupes.
            let mut fts_del = tx.prepare("DELETE FROM nodes_fts WHERE id = ?1")?;
            let mut fts_stmt = tx.prepare(
                "INSERT INTO nodes_fts(id, name, qualified_name, doc) VALUES (?1,?2,?3,?4)",
            )?;
            for n in nodes {
                let (sb, eb, sl, el) = match n.span {
                    Some(s) => (
                        Some(s.start_byte as i64),
                        Some(s.end_byte as i64),
                        Some(s.start_line as i64),
                        Some(s.end_line as i64),
                    ),
                    None => (None, None, None, None),
                };
                node_stmt.execute(params![
                    n.id.0,
                    n.kind.as_str(),
                    n.name,
                    n.qualified_name,
                    n.file,
                    n.language,
                    sb,
                    eb,
                    sl,
                    el,
                    n.doc,
                ])?;
                fts_del.execute(params![n.id.0])?;
                fts_stmt.execute(params![
                    n.id.0,
                    n.name,
                    n.qualified_name,
                    n.doc.as_deref().unwrap_or("")
                ])?;
            }

            let mut edge_stmt = tx.prepare(
                "INSERT INTO edges(src, dst, kind, confidence) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(src,dst,kind) DO UPDATE SET confidence='extracted'
                 WHERE excluded.confidence='extracted'",
            )?;
            for e in edges {
                edge_stmt.execute(params![
                    e.src.0,
                    e.dst.0,
                    e.kind.as_str(),
                    e.confidence.as_str()
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Persist the API operation key for endpoint / client-call / schema-op
    /// nodes. Upserts by node id; the precomputed signature is stored for
    /// signature-keyed lookup and matching.
    pub fn insert_operations(&mut self, ops: &[(NodeId, OperationKey)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO api_operations(node_id, protocol, method, path, signature)
                 VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(node_id) DO UPDATE SET
                    protocol=?2, method=?3, path=?4, signature=?5",
            )?;
            for (id, key) in ops {
                stmt.execute(params![
                    id.0,
                    key.protocol.as_str(),
                    key.method.map(|m| m.as_str()),
                    key.path,
                    key.signature(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// All operation keys for nodes of a given kind (e.g. endpoints or client
    /// calls), for feeding the cross-boundary matcher.
    pub fn operations_by_kind(&self, kind: NodeKind) -> Result<Vec<(NodeId, OperationKey)>> {
        let mut stmt = self.conn.prepare(
            "SELECT o.node_id, o.protocol, o.method, o.path
             FROM api_operations o JOIN nodes n ON n.id = o.node_id
             WHERE n.kind = ?1
             ORDER BY o.node_id",
        )?;
        let rows = stmt.query_map(params![kind.as_str()], |r| {
            let id = NodeId(r.get::<_, String>(0)?);
            let protocol = Protocol::parse(&r.get::<_, String>(1)?).unwrap_or(Protocol::Rest);
            let method = r
                .get::<_, Option<String>>(2)?
                .and_then(|m| HttpMethod::parse(&m));
            let path = r.get::<_, String>(3)?;
            Ok((
                id,
                OperationKey {
                    protocol,
                    method,
                    path,
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every persisted API operation, regardless of node kind. Used to attach
    /// protocol/method/path metadata to graph nodes for visualization.
    pub fn all_operations(&self) -> Result<Vec<(NodeId, OperationKey)>> {
        let mut stmt = self.conn.prepare(
            "SELECT node_id, protocol, method, path FROM api_operations ORDER BY node_id",
        )?;
        let rows = stmt.query_map([], |r| {
            let id = NodeId(r.get::<_, String>(0)?);
            let protocol = Protocol::parse(&r.get::<_, String>(1)?).unwrap_or(Protocol::Rest);
            let method = r
                .get::<_, Option<String>>(2)?
                .and_then(|m| HttpMethod::parse(&m));
            let path = r.get::<_, String>(3)?;
            Ok((
                id,
                OperationKey {
                    protocol,
                    method,
                    path,
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Persist unresolved cross-file edges for later global re-resolution.
    /// Upserts by the full key, so re-inserting the same link is a no-op.
    pub fn insert_pending(&mut self, links: &[PendingLink]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO pending_edges(anchor, name, kind, name_is_src) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(anchor, name, kind, name_is_src) DO NOTHING",
            )?;
            for l in links {
                stmt.execute(params![
                    l.anchor.0,
                    l.name,
                    l.kind.as_str(),
                    l.name_is_src as i64
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every persisted pending edge, for re-resolution against the full graph.
    pub fn all_pending(&self) -> Result<Vec<PendingLink>> {
        let mut stmt = self
            .conn
            .prepare("SELECT anchor, name, kind, name_is_src FROM pending_edges")?;
        let rows = stmt.query_map([], |r| {
            Ok(PendingLink {
                anchor: NodeId(r.get::<_, String>(0)?),
                name: r.get::<_, String>(1)?,
                kind: parse_edge_kind(&r.get::<_, String>(2)?),
                name_is_src: r.get::<_, i64>(3)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Delete every edge of a given confidence. Used to drop all `Inferred`
    /// edges before re-resolving them, so resolution is authoritative each run.
    pub fn delete_edges_by_confidence(&mut self, confidence: Confidence) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM edges WHERE confidence = ?1",
            params![confidence.as_str()],
        )?)
    }

    /// Delete every edge of a given kind. Used to rebuild all `IMPORTS` edges
    /// each run so they re-resolve against the current file set.
    pub fn delete_edges_by_kind(&mut self, kind: EdgeKind) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM edges WHERE kind = ?1", params![kind.as_str()])?)
    }

    /// Persist a file's raw import targets `(importer, raw, language)`.
    pub fn insert_imports(&mut self, imports: &[(String, String, String)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO pending_imports(importer, raw, language) VALUES (?1,?2,?3)
                 ON CONFLICT(importer, raw, language) DO NOTHING",
            )?;
            for (importer, raw, language) in imports {
                stmt.execute(params![importer, raw, language])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every persisted import `(importer, raw, language)`, for global resolution.
    pub fn all_imports(&self) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT importer, raw, language FROM pending_imports")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// `(node id, file)` for every node, for resolving which file a definition
    /// (or call site) lives in during cross-file disambiguation.
    pub fn node_files(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT id, file FROM nodes")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// (name, id) for every definition-like node, for global call resolution.
    pub fn definition_index(&self) -> Result<Vec<(String, NodeId)>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, id FROM nodes
             WHERE kind NOT IN ('file','directory','repo','module','comment')",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, NodeId(r.get::<_, String>(1)?)))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn row_to_node(row: &rusqlite::Row) -> rusqlite::Result<Node> {
        let span = match row.get::<_, Option<i64>>("start_byte")? {
            Some(sb) => Some(Span {
                start_byte: sb as usize,
                end_byte: row.get::<_, Option<i64>>("end_byte")?.unwrap_or(0) as usize,
                start_line: row.get::<_, Option<i64>>("start_line")?.unwrap_or(0) as usize,
                end_line: row.get::<_, Option<i64>>("end_line")?.unwrap_or(0) as usize,
            }),
            None => None,
        };
        Ok(Node {
            id: NodeId(row.get("id")?),
            kind: parse_kind(&row.get::<_, String>("kind")?),
            name: row.get("name")?,
            qualified_name: row.get("qualified_name")?,
            file: row.get("file")?,
            language: row.get("language")?,
            span,
            doc: row.get("doc")?,
        })
    }

    pub fn get_node(&self, id: &str) -> Result<Option<Node>> {
        let mut stmt = self.conn.prepare("SELECT * FROM nodes WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(Self::row_to_node(row)?)),
            None => Ok(None),
        }
    }

    /// All nodes living in a repo-relative file, for change-set seeding and
    /// span-overlap mapping.
    pub fn nodes_in_file(&self, file: &str) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare("SELECT * FROM nodes WHERE file = ?1")?;
        let rows = stmt.query_map(params![file], Self::row_to_node)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Find nodes by exact name or qualified-name suffix.
    pub fn find_by_name(&self, name: &str) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM nodes WHERE name = ?1 OR qualified_name = ?1
             ORDER BY kind, file LIMIT 200",
        )?;
        let rows = stmt.query_map(params![name], Self::row_to_node)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Full-text search over names and docs. The user input is treated as a
    /// literal FTS5 phrase (wrapped in double quotes, internal quotes doubled)
    /// so ordinary identifiers like `Foo::bar`, `c++` or `i18n-utils` don't get
    /// parsed as FTS5 operators and error out.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Node>> {
        let phrase = format!("\"{}\"", query.replace('"', "\"\""));
        let mut stmt = self.conn.prepare(
            "SELECT n.* FROM nodes_fts f JOIN nodes n ON n.id = f.id
             WHERE nodes_fts MATCH ?1 LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![phrase, limit as i64], Self::row_to_node)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Neighbours along edges. `outgoing` selects direction.
    pub fn neighbors(
        &self,
        id: &str,
        kind: Option<EdgeKind>,
        outgoing: bool,
    ) -> Result<Vec<(Node, EdgeKind, Confidence)>> {
        let (key_col, other_col) = if outgoing {
            ("src", "dst")
        } else {
            ("dst", "src")
        };
        let sql = format!(
            "SELECT n.*, e.kind AS ekind, e.confidence AS econf
             FROM edges e JOIN nodes n ON n.id = e.{other}
             WHERE e.{key} = ?1 {kind_filter}",
            other = other_col,
            key = key_col,
            kind_filter = if kind.is_some() {
                "AND e.kind = ?2"
            } else {
                ""
            },
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let map = |row: &rusqlite::Row| {
            let node = Self::row_to_node(row)?;
            let ek = parse_edge_kind(&row.get::<_, String>("ekind")?);
            let conf = parse_conf(&row.get::<_, String>("econf")?);
            Ok((node, ek, conf))
        };
        let rows = match kind {
            Some(k) => stmt.query_map(params![id, k.as_str()], map)?,
            None => stmt.query_map(params![id], map)?,
        };
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Transitive closure of dependents/dependencies up to `depth` (impact analysis).
    pub fn reachable(
        &self,
        id: &str,
        kind: EdgeKind,
        outgoing: bool,
        depth: usize,
    ) -> Result<Vec<Node>> {
        let (from_col, to_col) = if outgoing {
            ("src", "dst")
        } else {
            ("dst", "src")
        };
        let sql = format!(
            "WITH RECURSIVE reach(id, d) AS (
                 SELECT ?1, 0
                 UNION
                 SELECT e.{to_col}, r.d + 1
                 FROM edges e JOIN reach r ON e.{from_col} = r.id
                 WHERE e.kind = ?2 AND r.d < ?3
             )
             SELECT n.* FROM nodes n JOIN reach ON n.id = reach.id WHERE reach.id != ?1",
            to_col = to_col,
            from_col = from_col,
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![id, kind.as_str(), depth as i64], Self::row_to_node)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// One traversal hop: neighbour ids of `node` along `kind`, with each edge's
    /// confidence. `outgoing` follows `src = node` (→ `dst`); otherwise
    /// `dst = node` (→ `src`). Cheaper than [`Self::neighbors`] — no node join.
    pub fn edge_step(
        &self,
        node: &str,
        kind: EdgeKind,
        outgoing: bool,
    ) -> Result<Vec<(NodeId, Confidence)>> {
        let (key, other) = if outgoing {
            ("src", "dst")
        } else {
            ("dst", "src")
        };
        let sql = format!("SELECT {other}, confidence FROM edges WHERE {key} = ?1 AND kind = ?2");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![node, kind.as_str()], |r| {
            Ok((
                NodeId(r.get::<_, String>(0)?),
                parse_conf(&r.get::<_, String>(1)?),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The induced subgraph over `ids`: those nodes plus every edge whose both
    /// endpoints are in the set. Used to render a focused impact subgraph.
    pub fn subgraph(&self, ids: &[String]) -> Result<(Vec<Node>, Vec<Edge>)> {
        if ids.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let ph = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let nsql = format!("SELECT * FROM nodes WHERE id IN ({ph})");
        let mut nstmt = self.conn.prepare(&nsql)?;
        let nodes: Vec<Node> = nstmt
            .query_map(rusqlite::params_from_iter(ids), Self::row_to_node)?
            .collect::<rusqlite::Result<_>>()?;
        let esql = format!(
            "SELECT src, dst, kind, confidence FROM edges
             WHERE src IN ({ph}) AND dst IN ({ph})"
        );
        let mut estmt = self.conn.prepare(&esql)?;
        let edges: Vec<Edge> = estmt
            .query_map(
                rusqlite::params_from_iter(ids.iter().chain(ids.iter())),
                |r| {
                    Ok(Edge {
                        src: NodeId(r.get(0)?),
                        dst: NodeId(r.get(1)?),
                        kind: parse_edge_kind(&r.get::<_, String>(2)?),
                        confidence: parse_conf(&r.get::<_, String>(3)?),
                    })
                },
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok((nodes, edges))
    }

    /// Drop edges whose endpoints no longer exist (e.g. after a symbol was removed).
    pub fn prune_dangling_edges(&mut self) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM edges WHERE src NOT IN (SELECT id FROM nodes)
                OR dst NOT IN (SELECT id FROM nodes)",
            [],
        )?;
        Ok(n)
    }

    pub fn stats(&self) -> Result<Stats> {
        let nodes: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
        let edges: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))?;
        let files: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        let mut lstmt = self.conn.prepare(
            "SELECT COALESCE(language, '(unknown)') AS lang, COUNT(*)
             FROM files GROUP BY lang ORDER BY COUNT(*) DESC, lang",
        )?;
        let languages: Vec<(String, usize)> = lstmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Stats {
            nodes: nodes as usize,
            edges: edges as usize,
            files: files as usize,
            languages,
        })
    }

    /// Full graph (capped) for visualization export.
    pub fn export_graph(&self, limit: usize) -> Result<(Vec<Node>, Vec<Edge>)> {
        let mut nstmt = self.conn.prepare("SELECT * FROM nodes LIMIT ?1")?;
        let nodes: Vec<Node> = nstmt
            .query_map(params![limit as i64], Self::row_to_node)?
            .collect::<rusqlite::Result<_>>()?;
        let mut estmt = self
            .conn
            .prepare("SELECT src, dst, kind, confidence FROM edges")?;
        let edges: Vec<Edge> = estmt
            .query_map([], |r| {
                Ok(Edge {
                    src: NodeId(r.get(0)?),
                    dst: NodeId(r.get(1)?),
                    kind: parse_edge_kind(&r.get::<_, String>(2)?),
                    confidence: parse_conf(&r.get::<_, String>(3)?),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok((nodes, edges))
    }
}

#[derive(Debug, Clone)]
pub struct Stats {
    pub nodes: usize,
    pub edges: usize,
    pub files: usize,
    /// Indexed file counts per language, descending by count.
    pub languages: Vec<(String, usize)>,
}

impl Adjacency for SqliteStore {
    fn step(&self, node: &NodeId, kind: EdgeKind, dir: Direction) -> Vec<(NodeId, Confidence)> {
        // The engine is infallible; a query error (malformed DB) yields no
        // neighbours rather than aborting the whole traversal.
        self.edge_step(&node.0, kind, matches!(dir, Direction::Outgoing))
            .unwrap_or_default()
    }
}

impl SqliteStore {
    /// Run the impact engine from `seeds` under `policy`, resolve each impacted
    /// node and classify it. Shared by the CLI `impact` command and the MCP
    /// `impact` tool so the report has a single source of truth.
    pub fn classify_impact(
        &self,
        seeds: &[NodeId],
        policy: &ImpactPolicy,
    ) -> Result<Vec<ClassifiedItem>> {
        let items = compute_impact(seeds, policy, self);
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            let Some(node) = self.get_node(&it.node.0)? else {
                continue; // vanished between traversal and lookup
            };
            out.push(ClassifiedItem {
                id: node.id.0,
                name: node.name,
                qualified_name: node.qualified_name.clone(),
                kind: node.kind,
                file: node.file.clone(),
                line: node.span.map(|s| s.start_line),
                class: classify(node.kind, &node.file, &node.qualified_name),
                distance: it.distance,
                min_confidence: it.min_confidence,
                cross_boundary: is_cross_boundary_path(&it.path),
                path: it.path.iter().map(|k| k.as_str().to_string()).collect(),
            });
        }
        Ok(out)
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
        "endpoint" => NodeKind::Endpoint,
        "client_call" => NodeKind::ClientCall,
        "schema_op" => NodeKind::SchemaOp,
        _ => NodeKind::Comment,
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
        _ => EdgeKind::References,
    }
}

fn parse_conf(s: &str) -> Confidence {
    match s {
        "extracted" => Confidence::Extracted,
        _ => Confidence::Inferred,
    }
}
