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
    /// A server-side API operation (REST route, gRPC method, GraphQL field).
    Endpoint,
    /// A client-side call site targeting a remote API operation.
    ClientCall,
    /// An operation declared in an API schema artifact (OpenAPI/proto/GraphQL).
    SchemaOp,
    /// A router value composed via a router variable (FastAPI `APIRouter`,
    /// Express `express.Router()`); the `MOUNTS` target of `include_router` /
    /// `app.use`.
    Router,
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
            NodeKind::Endpoint => "endpoint",
            NodeKind::ClientCall => "client_call",
            NodeKind::SchemaOp => "schema_op",
            NodeKind::Router => "router",
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
    /// A handler symbol serves an API endpoint.
    Handles,
    /// A router mounts a sub-router under a path prefix (nested route composition).
    Mounts,
    /// A code endpoint corresponds to an operation declared in a schema artifact.
    Exposes,
    /// A client call site invokes an API endpoint (cross web boundary).
    Invokes,
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
            EdgeKind::Handles => "handles",
            EdgeKind::Mounts => "mounts",
            EdgeKind::Exposes => "exposes",
            EdgeKind::Invokes => "invokes",
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

    /// Strength of evidence; higher is stronger. `Extracted` (straight from the
    /// AST) outranks `Inferred` (heuristically resolved).
    pub fn rank(self) -> u8 {
        match self {
            Confidence::Extracted => 2,
            Confidence::Inferred => 1,
        }
    }

    /// The weaker of two confidences — the min-confidence along an edge path.
    pub fn weaker(self, other: Self) -> Self {
        if other.rank() < self.rank() {
            other
        } else {
            self
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
    /// Declaration header (signature) captured at parse time, so `outline` reads
    /// it from the graph instead of re-slicing the live file — which garbles when
    /// the index is stale (#344). `None` for nodes without a declaration and for
    /// legacy indexes built before this field existed.
    #[serde(default)]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub src: NodeId,
    pub dst: NodeId,
    pub kind: EdgeKind,
    pub confidence: Confidence,
}

/// An unresolved cross-file edge: one endpoint (`anchor`) is a known node, the
/// other is identified only by `name` and resolved against the global symbol
/// index. Persisted so edits anywhere can be re-resolved incrementally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLink {
    pub anchor: NodeId,
    pub name: String,
    pub kind: EdgeKind,
    /// When true the resolved node is the edge *source* (`name -> anchor`);
    /// otherwise it is the *destination* (`anchor -> name`).
    pub name_is_src: bool,
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
