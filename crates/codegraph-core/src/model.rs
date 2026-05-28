use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable identifier for a node, derived from repo-relative path, qualified name and kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl NodeId {
    /// Derive a stable id by hashing the parts so re-indexing yields identical ids.
    pub fn derive(parts: &[&str]) -> Self {
        let mut hasher = blake3::Hasher::new();
        for p in parts {
            hasher.update(p.as_bytes());
            hasher.update(&[0u8]);
        }
        NodeId(hasher.finalize().to_hex()[..16].to_string())
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Repo,
    Directory,
    File,
    Module,
    Function,
    Method,
    Class,
    Struct,
    Interface,
    Enum,
    Trait,
    Comment,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Repo => "repo",
            NodeKind::Directory => "directory",
            NodeKind::File => "file",
            NodeKind::Module => "module",
            NodeKind::Function => "function",
            NodeKind::Method => "method",
            NodeKind::Class => "class",
            NodeKind::Struct => "struct",
            NodeKind::Interface => "interface",
            NodeKind::Enum => "enum",
            NodeKind::Trait => "trait",
            NodeKind::Comment => "comment",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Structural containment: dir -> file -> symbol.
    Contains,
    /// A scope defines a symbol.
    Defines,
    /// A function/method calls another symbol.
    Calls,
    /// A file imports a module/symbol.
    Imports,
    /// Class extends a base class.
    Extends,
    /// Type implements an interface/trait.
    Implements,
    /// A comment documents a symbol (design rationale colocation).
    Documents,
    /// Generic reference that is not a call.
    References,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Contains => "contains",
            EdgeKind::Defines => "defines",
            EdgeKind::Calls => "calls",
            EdgeKind::Imports => "imports",
            EdgeKind::Extends => "extends",
            EdgeKind::Implements => "implements",
            EdgeKind::Documents => "documents",
            EdgeKind::References => "references",
        }
    }
}

/// How an edge was derived. `Extracted` comes straight from the AST; `Inferred`
/// is resolved heuristically (e.g. cross-file call resolution by name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Extracted,
    Inferred,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Extracted => "extracted",
            Confidence::Inferred => "inferred",
        }
    }
}

/// Byte/line span within a source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Span {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    /// Short name (e.g. `parse_file`).
    pub name: String,
    /// Fully-qualified name within the file/module where known (e.g. `Parser::parse_file`).
    pub qualified_name: String,
    /// Repo-relative path of the file this node lives in (empty for the repo node).
    pub file: String,
    pub language: Option<String>,
    pub span: Option<Span>,
    /// Colocated design-rationale / doc comment text attached to this symbol.
    pub doc: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub src: NodeId,
    pub dst: NodeId,
    pub kind: EdgeKind,
    pub confidence: Confidence,
}

/// In-memory graph accumulated by the parser before being persisted.
#[derive(Debug, Default, Clone)]
pub struct CodeGraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl CodeGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_node(&mut self, node: Node) -> NodeId {
        let id = node.id.clone();
        self.nodes.push(node);
        id
    }

    pub fn add_edge(&mut self, src: NodeId, dst: NodeId, kind: EdgeKind, confidence: Confidence) {
        self.edges.push(Edge {
            src,
            dst,
            kind,
            confidence,
        });
    }

    pub fn extend(&mut self, other: CodeGraph) {
        self.nodes.extend(other.nodes);
        self.edges.extend(other.edges);
    }
}
