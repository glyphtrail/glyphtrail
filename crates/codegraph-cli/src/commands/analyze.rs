use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use codegraph_core::config::{RepoPaths, IGNORE_FILE};
use codegraph_core::{
    CodeGraph, Confidence, Edge, EdgeKind, Language, Node, NodeId, NodeKind,
};
use codegraph_parse::{build_file_graph, parse_source, PendingEdge};
use codegraph_store::SqliteStore;
use ignore::WalkBuilder;

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
                graph.extend(fg.graph);
                pending.extend(fg.pending);
            }
            Err(e) => tracing::warn!("parse failed for {}: {e}", f.rel_path),
        }
    }

    // Persist nodes and high-confidence (extracted) edges first.
    let extracted: Vec<Edge> = graph
        .edges
        .iter()
        .filter(|e| e.confidence == Confidence::Extracted)
        .cloned()
        .collect();
    store.insert_graph(&graph.nodes, &extracted)?;

    // Resolve deferred edges against the global symbol index.
    let mut index: HashMap<String, Vec<NodeId>> = HashMap::new();
    for (name, id) in store.definition_index()? {
        index.entry(name).or_default().push(id);
    }
    let mut inferred: Vec<Edge> = Vec::new();
    for p in &pending {
        if let Some(candidates) = index.get(&p.name) {
            if candidates.len() == 1 {
                inferred.push(Edge {
                    src: p.src.clone(),
                    dst: candidates[0].clone(),
                    kind: p.kind,
                    confidence: Confidence::Inferred,
                });
            }
        }
    }
    store.insert_graph(&[], &inferred)?;

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
