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
use lbug::{Connection, Database, SystemConfig, Value};
use meridian_core::{
    Adjacency, ClassifiedItem, Confidence, Direction, Edge, EdgeKind, ImpactPolicy, Node, NodeId,
    NodeKind, OperationKey, PendingLink, Span, classify, compute_impact, is_cross_boundary_path,
};

use crate::Stats;
use crate::graph_store::GraphStore;

const SCHEMA: &[&str] = &[
    "CREATE NODE TABLE IF NOT EXISTS Node(id STRING, kind STRING, name STRING, qualified_name STRING, file STRING, language STRING, start_byte INT64, end_byte INT64, start_line INT64, end_line INT64, doc STRING, PRIMARY KEY(id))",
    "CREATE REL TABLE IF NOT EXISTS Edge(FROM Node TO Node, kind STRING, confidence STRING)",
    "CREATE NODE TABLE IF NOT EXISTS File(path STRING, language STRING, hash STRING, PRIMARY KEY(path))",
    "CREATE NODE TABLE IF NOT EXISTS ApiOp(node_id STRING, protocol STRING, method STRING, path STRING, signature STRING, PRIMARY KEY(node_id))",
    "CREATE NODE TABLE IF NOT EXISTS Pending(pk STRING, anchor STRING, name STRING, kind STRING, name_is_src INT64, PRIMARY KEY(pk))",
    "CREATE NODE TABLE IF NOT EXISTS Import(pk STRING, importer STRING, raw STRING, language STRING, PRIMARY KEY(pk))",
    "CREATE NODE TABLE IF NOT EXISTS Meta(key STRING, value STRING, PRIMARY KEY(key))",
];

/// Property list a `Node` row returns, in order, so `row_to_node` can decode it.
const NODE_COLS: &str = "n.id, n.kind, n.name, n.qualified_name, n.file, n.language, n.start_byte, n.end_byte, n.start_line, n.end_line, n.doc";

pub struct LadybugStore {
    db: Database,
}

impl LadybugStore {
    /// Open (or create) a LadybugDB database at `path` and ensure the schema.
    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::new(path, SystemConfig::default())?;
        {
            let conn = Connection::new(&db)?;
            for ddl in SCHEMA {
                conn.query(ddl)?;
            }
        }
        Ok(Self { db })
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

    /// Prepare + execute one statement, attaching the query as error context.
    fn exec(&self, conn: &Connection, cypher: &str, params: Vec<(&str, Value)>) -> Result<()> {
        let mut st = conn.prepare(cypher).with_context(|| cypher.to_string())?;
        conn.execute(&mut st, params)
            .with_context(|| cypher.to_string())?;
        Ok(())
    }

    fn run_nodes(&self, cypher: &str, params: Vec<(&str, Value)>) -> Result<Vec<Node>> {
        Ok(self
            .run(cypher, params)?
            .iter()
            .map(|r| row_to_node(r))
            .collect())
    }
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
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
        "comment" => NodeKind::Comment,
        "endpoint" => NodeKind::Endpoint,
        "client_call" => NodeKind::ClientCall,
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
        _ => EdgeKind::References,
    }
}

fn parse_conf(s: &str) -> Confidence {
    match s {
        "extracted" => Confidence::Extracted,
        _ => Confidence::Inferred,
    }
}

fn parse_proto(s: &str) -> meridian_core::Protocol {
    use meridian_core::Protocol;
    match s {
        "grpc" => Protocol::Grpc,
        "graphql" => Protocol::GraphQl,
        _ => Protocol::Rest,
    }
}

fn op_from_row(row: &[Value]) -> (NodeId, OperationKey) {
    let method = opt(get_str(row, 2)).and_then(|m| meridian_core::HttpMethod::parse(&m));
    (
        NodeId(get_str(row, 0)),
        OperationKey {
            protocol: parse_proto(&get_str(row, 1)),
            method,
            path: get_str(row, 3),
        },
    )
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
        for tbl in ["File", "ApiOp", "Pending", "Import", "Meta"] {
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

    fn delete_file_data(&mut self, path: &str) -> Result<()> {
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
        // lbug caches a parameter's type by name within a connection, so each
        // statement uses its own freshly-prepared connection (via `exec`) to keep
        // INT64 and STRING params from colliding across the analyze flow.
        for n in nodes {
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
            // Integer columns are inlined (not bound) so no INT64 parameter is
            // ever created; see `lit`.
            self.exec(
                &conn,
                &format!(
                    "MERGE (n:Node {{id:$id}}) SET n.kind=$kind, n.name=$name, \
                     n.qualified_name=$qn, n.file=$file, n.language=$lang, \
                     n.start_byte={sb}, n.end_byte={eb}, n.start_line={sl}, \
                     n.end_line={el}, n.doc=$doc"
                ),
                vec![
                    ("id", s(&n.id.0)),
                    ("kind", s(n.kind.as_str())),
                    ("name", s(&n.name)),
                    ("qn", s(&n.qualified_name)),
                    ("file", s(&n.file)),
                    ("lang", s(n.language.as_deref().unwrap_or(""))),
                    ("doc", s(n.doc.as_deref().unwrap_or(""))),
                ],
            )?;
        }
        for e in edges {
            self.exec(
                &conn,
                "MATCH (a:Node {id:$src}), (b:Node {id:$dst}) \
                 MERGE (a)-[e:Edge {kind:$ekind}]->(b) \
                 ON CREATE SET e.confidence=$conf \
                 ON MATCH SET e.confidence = CASE WHEN $conf = 'extracted' THEN 'extracted' ELSE e.confidence END",
                vec![
                    ("src", s(&e.src.0)),
                    ("dst", s(&e.dst.0)),
                    ("ekind", s(e.kind.as_str())),
                    ("conf", s(e.confidence.as_str())),
                ],
            )?;
        }
        Ok(())
    }

    fn insert_operations(&mut self, ops: &[(NodeId, OperationKey)]) -> Result<()> {
        let conn = self.conn()?;
        for (id, key) in ops {
            self.exec(
                &conn,
                "MERGE (o:ApiOp {node_id:$id}) SET o.protocol=$p, o.method=$m, o.path=$path, o.signature=$sig",
                vec![
                    ("id", s(&id.0)),
                    ("p", s(key.protocol.as_str())),
                    ("m", s(key.method.map(|m| m.as_str()).unwrap_or(""))),
                    ("path", s(&key.path)),
                    ("sig", s(&key.signature())),
                ],
            )?;
        }
        Ok(())
    }

    fn insert_pending(&mut self, links: &[PendingLink]) -> Result<()> {
        let conn = self.conn()?;
        for l in links {
            let rowid = format!(
                "{}|{}|{}|{}",
                l.anchor.0,
                l.name,
                l.kind.as_str(),
                l.name_is_src
            );
            self.exec(
                &conn,
                &format!(
                    "MERGE (p:Pending {{pk:$r}}) SET p.anchor=$a, p.name=$n, \
                     p.kind=$k, p.name_is_src={}",
                    l.name_is_src as i64
                ),
                vec![
                    ("r", s(&rowid)),
                    ("a", s(&l.anchor.0)),
                    ("n", s(&l.name)),
                    ("k", s(l.kind.as_str())),
                ],
            )?;
        }
        Ok(())
    }

    fn insert_imports(&mut self, imports: &[(String, String, String)]) -> Result<()> {
        let conn = self.conn()?;
        for (importer, raw, language) in imports {
            let rowid = format!("{importer}|{raw}|{language}");
            self.exec(
                &conn,
                "MERGE (i:Import {pk:$im_rowid}) SET i.importer=$im_importer, i.raw=$im_raw, i.language=$im_lang",
                vec![
                    ("im_rowid", s(&rowid)),
                    ("im_importer", s(importer)),
                    ("im_raw", s(raw)),
                    ("im_lang", s(language)),
                ],
            )?;
        }
        Ok(())
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

    fn definition_index(&self) -> Result<Vec<(String, NodeId)>> {
        Ok(self
            .run("MATCH (n:Node) RETURN n.name, n.id", vec![])?
            .iter()
            .map(|r| (get_str(r, 0), NodeId(get_str(r, 1))))
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

    fn search(&self, query: &str, limit: usize) -> Result<Vec<Node>> {
        // No native FTS; approximate with substring CONTAINS over name/qname/doc.
        self.run_nodes(
            &format!(
                "MATCH (n:Node) WHERE n.name CONTAINS $q OR n.qualified_name CONTAINS $q OR n.doc CONTAINS $q RETURN {NODE_COLS} LIMIT {limit}"
            ),
            vec![("q", s(query))],
        )
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
                    parse_edge_kind(&get_str(r, 11)),
                    parse_conf(&get_str(r, 12)),
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

    fn export_graph(&self, limit: usize) -> Result<(Vec<Node>, Vec<Edge>)> {
        let nodes = self.run_nodes(
            &format!("MATCH (n:Node) RETURN {NODE_COLS} LIMIT {limit}"),
            vec![],
        )?;
        let edges = self
            .run(
                "MATCH (a:Node)-[e:Edge]->(b:Node) RETURN a.id, b.id, e.kind, e.confidence",
                vec![],
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
    use crate::SqliteStore;
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
        }
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("meridian-lbug-{tag}-{nanos}"))
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

    #[test]
    fn parity_with_sqlite_on_a_fixture() {
        let nodes = vec![node("a", "caller"), node("b", "callee")];
        let edges = vec![Edge {
            src: NodeId("a".into()),
            dst: NodeId("b".into()),
            kind: EdgeKind::Calls,
            confidence: Confidence::Extracted,
        }];

        let mut sq = SqliteStore::open_in_memory().unwrap();
        sq.insert_graph(&nodes, &edges).unwrap();

        let dir = tmp_dir("parity");
        let mut lb = LadybugStore::open(&dir).unwrap();
        lb.insert_graph(&nodes, &edges).unwrap();

        // Same node/edge counts as the SQLite backend (#8 parity acceptance).
        let (ss, ls) = (sq.stats().unwrap(), lb.stats().unwrap());
        check!(ls.nodes == ss.nodes);
        check!(ls.edges == ss.edges);

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
            meridian_core::OperationKey::rest(meridian_core::HttpMethod::Get, "/x"),
        )])
        .unwrap();
        lb.insert_imports(&[("a.rs".into(), "b".into(), "rust".into())])
            .unwrap();
        lb.insert_pending(&[meridian_core::PendingLink {
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
