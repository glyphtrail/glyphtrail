//! Backend-agnostic graph storage interface (#7).
//!
//! `GraphStore` is the surface the analyze (write) and query (read) paths use,
//! so the storage engine can change without touching command code. The sole
//! implementation today is the LadybugDB / `lbug` store ([`crate::LadybugStore`],
//! #8); the trait is kept so a future backend can be slotted in behind it.

use anyhow::Result;
use glyphtrail_core::{
    Adjacency, ClassifiedItem, CommitMeta, Confidence, Edge, EdgeKind, ImpactPolicy, Node, NodeId,
    NodeKind, OperationKey, PendingLink,
};

/// Aggregate counts for an index, returned by [`GraphStore::stats`].
#[derive(Debug, Clone)]
pub struct Stats {
    pub nodes: usize,
    pub edges: usize,
    pub files: usize,
    /// Indexed file counts per language, descending by count.
    pub languages: Vec<(String, usize)>,
}

/// The storage operations shared by every backend. `Adjacency` is a supertrait
/// so the impact engine can traverse any backend's graph.
pub trait GraphStore: Adjacency {
    // --- write path (analyze) ---
    fn clear(&mut self) -> Result<()>;
    fn set_file(&mut self, path: &str, language: Option<&str>, hash: &str) -> Result<()>;
    /// Stamp many file records at once. Backends override this to write them in a
    /// single transaction; the default falls back to one [`Self::set_file`] per
    /// row. Each tuple is `(path, language, content hash)`.
    fn set_files(&mut self, files: &[(String, Option<String>, String)]) -> Result<()> {
        for (path, language, hash) in files {
            self.set_file(path, language.as_deref(), hash)?;
        }
        Ok(())
    }
    fn delete_file_data(&mut self, path: &str) -> Result<()>;
    fn delete_nodes_by_kind(&mut self, kind: NodeKind) -> Result<()>;
    fn insert_graph(&mut self, nodes: &[Node], edges: &[Edge]) -> Result<()>;
    /// Insert nodes. `fresh` signals a full rebuild against a just-cleared store
    /// with a primary-key-unique node set, letting a backend bulk-load (avoiding
    /// a per-row MERGE that can't use the id index). The default merges.
    fn insert_nodes(&mut self, nodes: &[Node], fresh: bool) -> Result<()> {
        let _ = fresh;
        self.insert_graph(nodes, &[])
    }
    /// Insert edges. `fresh` signals a full rebuild against a just-cleared store
    /// with a de-duplicated edge set, letting a backend skip MERGE's per-edge
    /// existence check (which goes quadratic on high-degree hub nodes). The
    /// default merges, which is always safe; backends override for speed.
    fn insert_edges(&mut self, edges: &[Edge], fresh: bool) -> Result<()> {
        let _ = fresh;
        self.insert_graph(&[], edges)
    }
    fn insert_operations(&mut self, ops: &[(NodeId, OperationKey)]) -> Result<()>;
    /// Upsert atlas `Commit` side-table rows keyed by node id (#330).
    fn set_commits(&mut self, commits: &[CommitMeta]) -> Result<()>;
    /// Re-evaluate every stored atlas commit's `in_bounds` against a date window
    /// (unix seconds, inclusive; `None` = unbounded), so narrowing the window
    /// re-marks rows out of bounds rather than deleting them (#331).
    fn remark_commit_bounds(&mut self, since: Option<i64>, until: Option<i64>) -> Result<()>;
    fn insert_pending(&mut self, links: &[PendingLink]) -> Result<()>;
    fn insert_imports(&mut self, imports: &[(String, String, String)]) -> Result<()>;
    fn delete_edges_by_confidence(&mut self, confidence: Confidence) -> Result<usize>;
    fn delete_edges_by_kind(&mut self, kind: EdgeKind) -> Result<usize>;
    fn prune_dangling_edges(&mut self) -> Result<usize>;
    fn set_meta(&mut self, key: &str, value: &str) -> Result<()>;

    // --- read path (analyze re-resolution + query) ---
    fn file_hash(&self, path: &str) -> Result<Option<String>>;
    fn all_files(&self) -> Result<Vec<String>>;
    fn files_with_hashes(&self) -> Result<Vec<(String, String)>>;
    fn get_meta(&self, key: &str) -> Result<Option<String>>;
    fn operations_by_kind(&self, kind: NodeKind) -> Result<Vec<(NodeId, OperationKey)>>;
    fn all_operations(&self) -> Result<Vec<(NodeId, OperationKey)>>;
    /// In-bounds atlas commits in `[since, until]` (unix seconds, inclusive; `None`
    /// = unbounded), ordered by `committed_at` (#330).
    fn commits_in_range(&self, since: Option<i64>, until: Option<i64>) -> Result<Vec<CommitMeta>>;
    /// Total atlas commits in the side-table (cheap `COUNT`, no materialization).
    fn commit_count(&self) -> Result<usize>;
    /// In-bounds atlas commits in `[since, until]` (unix seconds, inclusive;
    /// `None` = unbounded), each joined to its repo name and touched-file count,
    /// ordered by `committed_at` ascending (#333). Visibility/author filtering is
    /// the caller's.
    fn atlas_timeline(
        &self,
        since: Option<i64>,
        until: Option<i64>,
    ) -> Result<Vec<glyphtrail_core::AtlasTimelineRow>>;
    fn all_pending(&self) -> Result<Vec<PendingLink>>;
    fn all_imports(&self) -> Result<Vec<(String, String, String)>>;
    fn node_files(&self) -> Result<Vec<(String, String)>>;
    /// `(node id, qualified name)` for every node, so cross-file resolution can
    /// match a call's receiver against a candidate's enclosing scope (#5).
    fn node_qualified_names(&self) -> Result<Vec<(String, String)>>;
    fn definition_index(&self) -> Result<Vec<(String, NodeId)>>;
    fn get_node(&self, id: &str) -> Result<Option<Node>>;
    fn nodes_in_file(&self, file: &str) -> Result<Vec<Node>>;
    fn find_by_name(&self, name: &str) -> Result<Vec<Node>>;
    /// Substring search over name/qualified-name/doc. Case-insensitive unless
    /// `case_sensitive` (#367).
    fn search(&self, query: &str, limit: usize, case_sensitive: bool) -> Result<Vec<Node>>;
    fn neighbors(
        &self,
        id: &str,
        kind: Option<EdgeKind>,
        outgoing: bool,
    ) -> Result<Vec<(Node, EdgeKind, Confidence)>>;
    fn reachable(
        &self,
        id: &str,
        kind: EdgeKind,
        outgoing: bool,
        depth: usize,
    ) -> Result<Vec<Node>>;
    fn subgraph(&self, ids: &[String]) -> Result<(Vec<Node>, Vec<Edge>)>;
    fn classify_impact(
        &self,
        seeds: &[NodeId],
        policy: &ImpactPolicy,
    ) -> Result<Vec<ClassifiedItem>>;
    fn stats(&self) -> Result<Stats>;
    fn export_graph(&self, limit: usize) -> Result<(Vec<Node>, Vec<Edge>)>;
    /// Export nodes/edges with kind filters pushed into the query, so a trimmed
    /// view doesn't transfer the whole graph (#194). `node_kinds`/`edge_kinds`
    /// of `None` mean "all kinds"; `Some(&[])` means none. `limit` caps nodes.
    /// Edges are filtered by their own kind and both endpoints' kinds; the
    /// caller still prunes edges to any node dropped by `limit`.
    fn export_filtered(
        &self,
        node_kinds: Option<&[String]>,
        edge_kinds: Option<&[String]>,
        limit: usize,
    ) -> Result<(Vec<Node>, Vec<Edge>)>;
}
