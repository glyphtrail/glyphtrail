//! Backend-agnostic graph storage interface (#7).
//!
//! `GraphStore` is the surface the analyze (write) and query (read) paths use,
//! so an alternative backend (the LadybugDB / `lbug` store, #8) can be dropped
//! in without touching command code. `SqliteStore` is the reference
//! implementation; it forwards to its inherent methods so there is no behavior
//! change for the SQLite path.

use anyhow::Result;
use meridian_core::{
    Adjacency, ClassifiedItem, Confidence, Edge, EdgeKind, ImpactPolicy, Node, NodeId, NodeKind,
    OperationKey, PendingLink,
};

use crate::{SqliteStore, Stats};

/// The storage operations shared by every backend. `Adjacency` is a supertrait
/// so the impact engine can traverse any backend's graph.
pub trait GraphStore: Adjacency {
    // --- write path (analyze) ---
    fn clear(&mut self) -> Result<()>;
    fn set_file(&mut self, path: &str, language: Option<&str>, hash: &str) -> Result<()>;
    fn delete_file_data(&mut self, path: &str) -> Result<()>;
    fn delete_nodes_by_kind(&mut self, kind: NodeKind) -> Result<()>;
    fn insert_graph(&mut self, nodes: &[Node], edges: &[Edge]) -> Result<()>;
    fn insert_operations(&mut self, ops: &[(NodeId, OperationKey)]) -> Result<()>;
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
    fn all_pending(&self) -> Result<Vec<PendingLink>>;
    fn all_imports(&self) -> Result<Vec<(String, String, String)>>;
    fn node_files(&self) -> Result<Vec<(String, String)>>;
    fn definition_index(&self) -> Result<Vec<(String, NodeId)>>;
    fn get_node(&self, id: &str) -> Result<Option<Node>>;
    fn nodes_in_file(&self, file: &str) -> Result<Vec<Node>>;
    fn find_by_name(&self, name: &str) -> Result<Vec<Node>>;
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Node>>;
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
}

impl GraphStore for SqliteStore {
    fn clear(&mut self) -> Result<()> {
        SqliteStore::clear(self)
    }
    fn set_file(&mut self, path: &str, language: Option<&str>, hash: &str) -> Result<()> {
        SqliteStore::set_file(self, path, language, hash)
    }
    fn delete_file_data(&mut self, path: &str) -> Result<()> {
        SqliteStore::delete_file_data(self, path)
    }
    fn delete_nodes_by_kind(&mut self, kind: NodeKind) -> Result<()> {
        SqliteStore::delete_nodes_by_kind(self, kind)
    }
    fn insert_graph(&mut self, nodes: &[Node], edges: &[Edge]) -> Result<()> {
        SqliteStore::insert_graph(self, nodes, edges)
    }
    fn insert_operations(&mut self, ops: &[(NodeId, OperationKey)]) -> Result<()> {
        SqliteStore::insert_operations(self, ops)
    }
    fn insert_pending(&mut self, links: &[PendingLink]) -> Result<()> {
        SqliteStore::insert_pending(self, links)
    }
    fn insert_imports(&mut self, imports: &[(String, String, String)]) -> Result<()> {
        SqliteStore::insert_imports(self, imports)
    }
    fn delete_edges_by_confidence(&mut self, confidence: Confidence) -> Result<usize> {
        SqliteStore::delete_edges_by_confidence(self, confidence)
    }
    fn delete_edges_by_kind(&mut self, kind: EdgeKind) -> Result<usize> {
        SqliteStore::delete_edges_by_kind(self, kind)
    }
    fn prune_dangling_edges(&mut self) -> Result<usize> {
        SqliteStore::prune_dangling_edges(self)
    }
    fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        SqliteStore::set_meta(self, key, value)
    }

    fn file_hash(&self, path: &str) -> Result<Option<String>> {
        SqliteStore::file_hash(self, path)
    }
    fn all_files(&self) -> Result<Vec<String>> {
        SqliteStore::all_files(self)
    }
    fn files_with_hashes(&self) -> Result<Vec<(String, String)>> {
        SqliteStore::files_with_hashes(self)
    }
    fn get_meta(&self, key: &str) -> Result<Option<String>> {
        SqliteStore::get_meta(self, key)
    }
    fn operations_by_kind(&self, kind: NodeKind) -> Result<Vec<(NodeId, OperationKey)>> {
        SqliteStore::operations_by_kind(self, kind)
    }
    fn all_operations(&self) -> Result<Vec<(NodeId, OperationKey)>> {
        SqliteStore::all_operations(self)
    }
    fn all_pending(&self) -> Result<Vec<PendingLink>> {
        SqliteStore::all_pending(self)
    }
    fn all_imports(&self) -> Result<Vec<(String, String, String)>> {
        SqliteStore::all_imports(self)
    }
    fn node_files(&self) -> Result<Vec<(String, String)>> {
        SqliteStore::node_files(self)
    }
    fn definition_index(&self) -> Result<Vec<(String, NodeId)>> {
        SqliteStore::definition_index(self)
    }
    fn get_node(&self, id: &str) -> Result<Option<Node>> {
        SqliteStore::get_node(self, id)
    }
    fn nodes_in_file(&self, file: &str) -> Result<Vec<Node>> {
        SqliteStore::nodes_in_file(self, file)
    }
    fn find_by_name(&self, name: &str) -> Result<Vec<Node>> {
        SqliteStore::find_by_name(self, name)
    }
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Node>> {
        SqliteStore::search(self, query, limit)
    }
    fn neighbors(
        &self,
        id: &str,
        kind: Option<EdgeKind>,
        outgoing: bool,
    ) -> Result<Vec<(Node, EdgeKind, Confidence)>> {
        SqliteStore::neighbors(self, id, kind, outgoing)
    }
    fn reachable(
        &self,
        id: &str,
        kind: EdgeKind,
        outgoing: bool,
        depth: usize,
    ) -> Result<Vec<Node>> {
        SqliteStore::reachable(self, id, kind, outgoing, depth)
    }
    fn subgraph(&self, ids: &[String]) -> Result<(Vec<Node>, Vec<Edge>)> {
        SqliteStore::subgraph(self, ids)
    }
    fn classify_impact(
        &self,
        seeds: &[NodeId],
        policy: &ImpactPolicy,
    ) -> Result<Vec<ClassifiedItem>> {
        SqliteStore::classify_impact(self, seeds, policy)
    }
    fn stats(&self) -> Result<Stats> {
        SqliteStore::stats(self)
    }
    fn export_graph(&self, limit: usize) -> Result<(Vec<Node>, Vec<Edge>)> {
        SqliteStore::export_graph(self, limit)
    }
}
