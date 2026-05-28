use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use meridian_core::config::{IGNORE_FILE, RepoPaths};
use meridian_core::{
    ClientCall, CodeGraph, Confidence, Config, Edge, EdgeKind, Endpoint, Language, Matcher, Node,
    NodeId, NodeKind, OperationKey, PendingLink, Protocol, RewriteEngine,
};
use meridian_parse::{
    PendingEdge, build_client_graph, build_file_graph, build_rest_graph, parse_source,
};

use crate::commands::schema;
use ignore::WalkBuilder;
use meridian_store::SqliteStore;

struct DiscoveredFile {
    rel_path: String,
    abs_path: std::path::PathBuf,
    language: Language,
    hash: String,
}

fn discover(root: &Path) -> Result<Vec<DiscoveredFile>> {
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .add_custom_ignore_filename(IGNORE_FILE);

    let mut out = Vec::new();
    for entry in walker.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let Some(language) = Language::from_path(path) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let rel = path.strip_prefix(root).unwrap_or(path);
        out.push(DiscoveredFile {
            rel_path: rel.to_string_lossy().replace('\\', "/"),
            abs_path: path.to_path_buf(),
            language,
            hash,
        });
    }
    Ok(out)
}

pub fn run(path: &Path, update: bool) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("cannot resolve path {}", path.display()))?;
    let paths = RepoPaths::new(&root);
    paths.ensure_index_dir()?;
    let mut store = SqliteStore::open(&paths.db_path)?;

    let files = discover(&root)?;
    tracing::info!("discovered {} source files", files.len());

    // Determine which files to (re)parse and which to drop.
    let current: HashMap<&str, &DiscoveredFile> =
        files.iter().map(|f| (f.rel_path.as_str(), f)).collect();

    if !update {
        store.clear()?;
    } else {
        for old in store.all_files()? {
            if !current.contains_key(old.as_str()) {
                store.delete_file_data(&old)?;
            }
        }
    }

    let mut changed: Vec<&DiscoveredFile> = Vec::new();
    for f in &files {
        let reparse = if update {
            store.file_hash(&f.rel_path)?.as_deref() != Some(f.hash.as_str())
        } else {
            true
        };
        if reparse {
            changed.push(f);
        }
    }
    tracing::info!("{} files need parsing", changed.len());

    // Accumulate the new fragment.
    let repo_name = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into());
    let repo_id = NodeId::derive(&["repo", &repo_name]);
    let mut graph = CodeGraph::new();
    graph.add_node(Node {
        id: repo_id.clone(),
        kind: NodeKind::Repo,
        name: repo_name,
        qualified_name: String::new(),
        file: String::new(),
        language: None,
        span: None,
        doc: None,
    });

    let mut pending: Vec<PendingEdge> = Vec::new();
    // API endpoints (kept separate so their operation keys persist after nodes).
    let mut operations: Vec<(NodeId, OperationKey)> = Vec::new();
    // (handler name, endpoint id) HANDLES links resolved against the global index.
    let mut pending_handlers: Vec<(String, NodeId)> = Vec::new();

    for f in &changed {
        if update {
            store.delete_file_data(&f.rel_path)?;
        }
        let source = match std::fs::read_to_string(&f.abs_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let file_id = NodeId::derive(&["file", &f.rel_path]);
        graph.add_node(Node {
            id: file_id.clone(),
            kind: NodeKind::File,
            name: f.rel_path.clone(),
            qualified_name: f.rel_path.clone(),
            file: f.rel_path.clone(),
            language: Some(f.language.name().to_string()),
            span: None,
            doc: None,
        });
        graph.add_edge(
            repo_id.clone(),
            file_id.clone(),
            EdgeKind::Contains,
            Confidence::Extracted,
        );

        match parse_source(f.language, &source) {
            Ok(parsed) => {
                let fg = build_file_graph(&f.rel_path, f.language, &file_id, &parsed);
                if f.language == Language::Rust {
                    let rg = build_rest_graph(&f.rel_path, &fg.symbols, &source);
                    graph.extend(rg.graph);
                    operations.extend(rg.operations);
                    pending_handlers.extend(rg.pending_handlers);
                }
                if matches!(
                    f.language,
                    Language::JavaScript | Language::TypeScript | Language::Tsx
                ) {
                    let cg = build_client_graph(&f.rel_path, &source, f.language);
                    graph.extend(cg.graph);
                    operations.extend(cg.operations);
                }
                graph.extend(fg.graph);
                pending.extend(fg.pending);
            }
            Err(e) => tracing::warn!("parse failed for {}: {e}", f.rel_path),
        }
    }

    // Ingest blessed schema artifacts into SchemaOp nodes (reconciled with code
    // endpoints as EXPOSES edges below). Schema ops are derived entirely from
    // config, so rebuild them from scratch each run: this drops entries whose
    // artifact or config line was removed or whose spec changed.
    let cfg = Config::load(&root)?;
    store.delete_nodes_by_kind(NodeKind::SchemaOp)?;
    ingest_schemas(&root, &cfg, &mut graph, &mut operations);

    // Persist nodes and high-confidence (extracted) edges first.
    let extracted: Vec<Edge> = graph
        .edges
        .iter()
        .filter(|e| e.confidence == Confidence::Extracted)
        .cloned()
        .collect();
    store.insert_graph(&graph.nodes, &extracted)?;
    store.insert_operations(&operations)?;

    // Persist this run's unresolved cross-file edges, then re-resolve *all*
    // persisted pending edges against the current global index. Stale pending
    // rows for changed files were already dropped (full clear / delete_file_data),
    // so the table reflects the whole current graph. Re-resolving everything --
    // not just this run's pending -- fixes inferred edges in unchanged files
    // that point at symbols added, removed or renamed elsewhere (#20).
    let mut pending_links: Vec<PendingLink> = pending
        .iter()
        .map(|p| PendingLink {
            anchor: p.src.clone(),
            name: p.name.clone(),
            kind: p.kind,
            name_is_src: false,
        })
        .collect();
    // HANDLES links whose handler is defined elsewhere: handler -> endpoint.
    pending_links.extend(
        pending_handlers
            .iter()
            .map(|(handler, endpoint_id)| PendingLink {
                anchor: endpoint_id.clone(),
                name: handler.clone(),
                kind: EdgeKind::Handles,
                name_is_src: true,
            }),
    );
    store.insert_pending(&pending_links)?;

    store.delete_edges_by_confidence(Confidence::Inferred)?;
    let mut index: HashMap<String, Vec<NodeId>> = HashMap::new();
    for (name, id) in store.definition_index()? {
        index.entry(name).or_default().push(id);
    }
    let inferred: Vec<Edge> = store
        .all_pending()?
        .into_iter()
        .filter_map(|l| {
            let candidates = index.get(&l.name)?;
            if candidates.len() != 1 {
                return None;
            }
            let other = candidates[0].clone();
            let (src, dst) = if l.name_is_src {
                (other, l.anchor)
            } else {
                (l.anchor, other)
            };
            Some(Edge {
                src,
                dst,
                kind: l.kind,
                confidence: Confidence::Inferred,
            })
        })
        .collect();
    store.insert_graph(&[], &inferred)?;

    // Cross-boundary linking: resolve client calls and schema operations
    // against the endpoints, through the same rewrite-aware matcher. Runs over
    // the full store (endpoints, calls and schema ops commonly live in
    // different, possibly unchanged, files/artifacts).
    let rewrite = RewriteEngine::from_config(&cfg.api);
    let endpoints: Vec<Endpoint> = store
        .operations_by_kind(NodeKind::Endpoint)?
        .into_iter()
        .map(|(id, key)| Endpoint { id, key })
        .collect();
    let matcher = Matcher::build(&endpoints, &rewrite);

    let as_calls = |ops: Vec<(NodeId, OperationKey)>| -> Vec<ClientCall> {
        ops.into_iter()
            .map(|(id, key)| ClientCall { id, key })
            .collect()
    };
    // Client call -> endpoint: INVOKES.
    let calls = as_calls(store.operations_by_kind(NodeKind::ClientCall)?);
    let mut edges: Vec<Edge> = matcher
        .resolve_all(&calls)
        .into_iter()
        .map(|m| Edge {
            src: m.client,
            dst: m.endpoint,
            kind: EdgeKind::Invokes,
            confidence: m.confidence,
        })
        .collect();
    // Endpoint -> schema op: EXPOSES (the code endpoint implements a declared
    // operation). Schema ops are matched like external callers.
    let schema_ops = as_calls(store.operations_by_kind(NodeKind::SchemaOp)?);
    edges.extend(matcher.resolve_all(&schema_ops).into_iter().map(|m| Edge {
        src: m.endpoint,
        dst: m.client,
        kind: EdgeKind::Exposes,
        confidence: m.confidence,
    }));
    store.insert_graph(&[], &edges)?;

    for f in &changed {
        store.set_file(&f.rel_path, Some(f.language.name()), &f.hash)?;
    }
    let pruned = store.prune_dangling_edges()?;
    if pruned > 0 {
        tracing::info!("pruned {pruned} dangling edges");
    }

    let stats = store.stats()?;
    println!(
        "Indexed {} files: {} nodes, {} edges",
        stats.files, stats.nodes, stats.edges
    );
    Ok(())
}

/// Read the configured REST schema artifacts and add a `SchemaOp` node (with
/// its operation key) for every operation they declare. Node ids are derived
/// from the artifact path + operation, so re-ingestion is idempotent.
fn ingest_schemas(
    root: &Path,
    cfg: &Config,
    graph: &mut CodeGraph,
    operations: &mut Vec<(NodeId, OperationKey)>,
) {
    for source in &cfg.api.schemas {
        if source.protocol != Protocol::Rest {
            continue; // gRPC/GraphQL schema ingestion is a follow-up.
        }
        let text = match std::fs::read_to_string(root.join(&source.path)) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("cannot read schema {}: {e}", source.path);
                continue;
            }
        };
        let ops = schema::openapi_rest_operations(&text);
        if ops.is_empty() {
            tracing::warn!("no REST operations parsed from schema {}", source.path);
        }
        for (method, path) in ops {
            let key = OperationKey::rest(method, &path);
            let id = NodeId::derive(&[&source.path, "schema_op", method.as_str(), &key.path]);
            graph.add_node(Node {
                id: id.clone(),
                kind: NodeKind::SchemaOp,
                name: format!("{} {}", method.as_str(), key.path),
                qualified_name: key.path.clone(),
                file: source.path.clone(),
                language: None,
                span: None,
                doc: None,
            });
            operations.push((id, key));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_repo(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("meridian-it-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn callers_of(dir: &Path, name: &str) -> Vec<String> {
        let store = SqliteStore::open(&RepoPaths::new(dir).db_path).unwrap();
        let target = store
            .find_by_name(name)
            .unwrap()
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("'{name}' should be defined"));
        store
            .neighbors(&target.id.0, Some(EdgeKind::Calls), false)
            .unwrap()
            .into_iter()
            .map(|(n, _, _)| n.name)
            .collect()
    }

    // #20: a pending cross-file edge in an *unchanged* file must be re-resolved
    // when its target definition is added elsewhere on a later `--update`.
    #[test]
    fn update_reresolves_inferred_edge_to_newly_added_definition() {
        let dir = temp_repo("reresolve-add");
        std::fs::write(dir.join("caller.rs"), "fn use_it() { foo(); }\n").unwrap();
        std::fs::write(dir.join("def.rs"), "\n").unwrap();

        // Full index: foo is undefined, so the call cannot resolve yet.
        run(&dir, false).unwrap();
        {
            let store = SqliteStore::open(&RepoPaths::new(&dir).db_path).unwrap();
            assert!(store.find_by_name("foo").unwrap().is_empty());
        }

        // Add the definition; caller.rs is unchanged, so only a global
        // re-resolution of persisted pending edges can create the link.
        std::fs::write(dir.join("def.rs"), "fn foo() {}\n").unwrap();
        run(&dir, true).unwrap();
        assert!(
            callers_of(&dir, "foo").contains(&"use_it".to_string()),
            "expected use_it -> foo after adding the definition on --update"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // The converse: removing the definition must drop the now-stale inferred
    // edge rather than leaving it dangling.
    #[test]
    fn update_drops_inferred_edge_when_definition_removed() {
        let dir = temp_repo("reresolve-remove");
        std::fs::write(dir.join("caller.rs"), "fn use_it() { foo(); }\n").unwrap();
        std::fs::write(dir.join("def.rs"), "fn foo() {}\n").unwrap();
        run(&dir, false).unwrap();
        assert!(callers_of(&dir, "foo").contains(&"use_it".to_string()));

        // Remove foo; the inferred use_it -> foo edge must disappear.
        std::fs::write(dir.join("def.rs"), "fn other() {}\n").unwrap();
        run(&dir, true).unwrap();
        let store = SqliteStore::open(&RepoPaths::new(&dir).db_path).unwrap();
        assert!(
            store.find_by_name("foo").unwrap().is_empty(),
            "foo should be gone after the edit"
        );
        // The previously-inferred use_it -> foo edge must be gone, not dangling.
        let use_it = store
            .find_by_name("use_it")
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert!(
            store
                .neighbors(&use_it.id.0, Some(EdgeKind::Calls), true)
                .unwrap()
                .is_empty(),
            "use_it should have no outgoing Calls edge after foo was removed"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
