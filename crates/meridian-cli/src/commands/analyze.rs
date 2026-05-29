use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use meridian_core::config::{IGNORE_FILE, RepoPaths};
use meridian_core::{
    ClientCall, CodeGraph, Confidence, Config, DynamicLanguage, Edge, EdgeKind, Endpoint, Language,
    Matcher, Node, NodeId, NodeKind, OperationKey, PendingLink, Protocol, RewriteEngine,
};
use meridian_parse::{
    DynamicGrammar, PendingEdge, build_client_graph, build_file_graph, build_rest_graph,
    load_dynamic, parse_source, resolve_import,
};
use std::collections::HashSet;

use crate::commands::backend::{self, BackendKind};
use crate::commands::schema;
use ignore::WalkBuilder;
#[cfg(test)]
use meridian_store::SqliteStore;

struct DiscoveredFile {
    rel_path: String,
    abs_path: std::path::PathBuf,
    language: Language,
    hash: String,
}

fn discover(root: &Path, dyn_langs: &[DynamicLanguage]) -> Result<Vec<DiscoveredFile>> {
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
        // Built-in language by extension, else a configured dynamic language.
        let language = match Language::from_path(path) {
            Some(l) => l,
            None => {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                match dyn_langs
                    .iter()
                    .find(|d| d.extensions.iter().any(|e| e == ext))
                {
                    Some(d) => Language::Other(d.name.clone()),
                    None => continue,
                }
            }
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

pub fn run(path: &Path, update: bool, backend: BackendKind) -> Result<()> {
    let root = path
        .canonicalize()
        .with_context(|| format!("cannot resolve path {}", path.display()))?;
    let paths = RepoPaths::new(&root);
    paths.ensure_index_dir()?;
    let mut store = backend::open(&paths, backend)?;

    let cfg = Config::load(&root)?;
    let files = discover(&root, &cfg.languages)?;
    tracing::info!("discovered {} source files", files.len());

    // Fast path (#110): when every discovered file matches the stored
    // (path, hash) set and the index was produced by this tool version, nothing
    // has changed — skip parsing, re-resolution and writes entirely. A version
    // mismatch forces a rebuild so extractor changes between releases take
    // effect even on an unchanged tree.
    const VERSION: &str = env!("CARGO_PKG_VERSION");
    let current_set: std::collections::BTreeSet<(String, String)> = files
        .iter()
        .map(|f| (f.rel_path.clone(), f.hash.clone()))
        .collect();
    let stored_set: std::collections::BTreeSet<(String, String)> =
        store.files_with_hashes()?.into_iter().collect();
    if !files.is_empty()
        && current_set == stored_set
        && store.get_meta("tool_version")?.as_deref() == Some(VERSION)
    {
        println!("Index up to date ({} files); nothing changed.", files.len());
        return Ok(());
    }

    // Lazily-loaded dynamic grammars, keyed by language name. `None` records a
    // load failure so we warn once and skip that language's files thereafter.
    let mut dyn_grammars: HashMap<String, Option<meridian_parse::DynamicGrammar>> = HashMap::new();

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
    // (importer rel-path, raw import, language name) resolved against the file set.
    let mut imports: Vec<(String, String, String)> = Vec::new();

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

        // Built-in languages parse via the compiled-in registry; a dynamic
        // `Language::Other` is parsed with its runtime-loaded grammar + query.
        let parsed = match &f.language {
            Language::Other(name) => {
                match dynamic_grammar(name, &cfg.languages, &mut dyn_grammars, &root) {
                    Some(dg) => meridian_parse::parse_with(&dg.grammar, &dg.query, &source),
                    None => continue,
                }
            }
            builtin => parse_source(builtin, &source),
        };
        match parsed {
            Ok(parsed) => {
                let fg = build_file_graph(&f.rel_path, &f.language, &file_id, &parsed);
                // REST server-route extraction runs for any language with a
                // registered extractor (registry decides; no-op otherwise).
                let rg = build_rest_graph(&f.rel_path, &f.language, &fg.symbols, &source);
                graph.extend(rg.graph);
                operations.extend(rg.operations);
                pending_handlers.extend(rg.pending_handlers);
                // Client-call extraction runs for any language with a client
                // extractor (the extractor decides; no-op otherwise).
                let cg = build_client_graph(&f.rel_path, &source, &f.language);
                graph.extend(cg.graph);
                operations.extend(cg.operations);
                graph.extend(fg.graph);
                pending.extend(fg.pending);
                imports.extend(
                    fg.imports
                        .into_iter()
                        .map(|raw| (f.rel_path.clone(), raw, f.language.name().to_string())),
                );
            }
            Err(e) => tracing::warn!("parse failed for {}: {e}", f.rel_path),
        }
    }

    // Ingest blessed schema artifacts into SchemaOp nodes (reconciled with code
    // endpoints as EXPOSES edges below). Schema ops are derived entirely from
    // config, so rebuild them from scratch each run: this drops entries whose
    // artifact or config line was removed or whose spec changed.
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
    store.insert_imports(&imports)?;

    // Persist this run's unresolved cross-file edges (calls / inheritance /
    // handlers). They are re-resolved globally below, so edits anywhere keep
    // inferred edges in unchanged files correct (#20).
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

    // Resolve imports against the discovered file set (exact path, then unique
    // suffix). Done before edge insertion so the importer -> target map can also
    // disambiguate ambiguous call/inheritance names below (#18, #19).
    let file_rels: Vec<String> = files.iter().map(|f| f.rel_path.clone()).collect();
    let file_set: HashSet<&str> = file_rels.iter().map(|s| s.as_str()).collect();
    let mut resolved_imports: Vec<(String, String, Option<String>)> = Vec::new();
    let mut import_map: HashMap<String, HashSet<String>> = HashMap::new();
    for (importer, raw, lang_name) in store.all_imports()? {
        let target = Language::ALL
            .into_iter()
            .find(|l| l.name() == lang_name)
            .and_then(|l| {
                resolve_target(&resolve_import(&importer, &raw, &l), &file_rels, &file_set)
            });
        if let Some(t) = &target {
            import_map
                .entry(importer.clone())
                .or_default()
                .insert(t.clone());
        }
        resolved_imports.push((importer, raw, target));
    }

    // Re-resolve all persisted pending edges against the current global index.
    // A uniquely-named target resolves directly; an ambiguous name resolves
    // only if exactly one candidate sits in a file the anchor's file imports (#19).
    store.delete_edges_by_confidence(Confidence::Inferred)?;
    let mut index: HashMap<String, Vec<NodeId>> = HashMap::new();
    for (name, id) in store.definition_index()? {
        index.entry(name).or_default().push(id);
    }
    let node_file: HashMap<String, String> = store.node_files()?.into_iter().collect();
    let inferred: Vec<Edge> = store
        .all_pending()?
        .into_iter()
        .filter_map(|l| {
            let candidates = index.get(&l.name)?;
            let other = match candidates.as_slice() {
                [one] => one.clone(),
                _ => disambiguate_import(&l.anchor, candidates, &node_file, &import_map)?,
            };
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

    // Rebuild IMPORTS edges from the resolved import set: real file targets
    // become file -> file (Inferred); the rest fall back to a module placeholder
    // (Extracted). Rebuilt globally each run so imports in unchanged files pick
    // up files added or removed elsewhere (#18).
    store.delete_edges_by_kind(EdgeKind::Imports)?;
    let mut import_nodes: Vec<Node> = Vec::new();
    let mut import_edges: Vec<Edge> = Vec::new();
    for (importer, raw, target) in resolved_imports {
        let importer_id = NodeId::derive(&["file", &importer]);
        match target {
            Some(rel) => import_edges.push(Edge {
                src: importer_id,
                dst: NodeId::derive(&["file", &rel]),
                kind: EdgeKind::Imports,
                confidence: Confidence::Inferred,
            }),
            None => {
                let mod_id = NodeId::derive(&["module", &raw]);
                import_nodes.push(Node {
                    id: mod_id.clone(),
                    kind: NodeKind::Module,
                    name: raw.clone(),
                    qualified_name: raw,
                    file: String::new(),
                    language: None,
                    span: None,
                    doc: None,
                });
                import_edges.push(Edge {
                    src: importer_id,
                    dst: mod_id,
                    kind: EdgeKind::Imports,
                    confidence: Confidence::Extracted,
                });
            }
        }
    }
    store.insert_graph(&import_nodes, &import_edges)?;

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

    store.set_meta("tool_version", VERSION)?;

    let stats = store.stats()?;
    println!(
        "Indexed {} files: {} nodes, {} edges",
        stats.files, stats.nodes, stats.edges
    );
    if !stats.languages.is_empty() {
        println!(
            "Languages: {}",
            super::status::format_languages(&stats.languages)
        );
    }
    Ok(())
}

/// Lazily compile + load the grammar for a dynamic language by name, caching the
/// result (including a failed load, so we warn only once). Returns `None` when
/// the language isn't configured or its grammar fails to load.
fn dynamic_grammar<'a>(
    name: &str,
    langs: &[DynamicLanguage],
    cache: &'a mut HashMap<String, Option<DynamicGrammar>>,
    root: &Path,
) -> Option<&'a DynamicGrammar> {
    if !cache.contains_key(name) {
        let loaded = langs.iter().find(|d| d.name == name).and_then(|d| {
            match load_dynamic(&root.join(&d.grammar), &root.join(&d.query)) {
                Ok(g) => Some(g),
                Err(e) => {
                    tracing::warn!("dynamic language '{name}' not loaded: {e}");
                    None
                }
            }
        });
        cache.insert(name.to_string(), loaded);
    }
    cache.get(name).and_then(|o| o.as_ref())
}

/// Pick the repository file an import resolves to: an exact path match first,
/// then a *unique* path-suffix match (so source roots like `src/` resolve),
/// else `None`. Ambiguous suffix matches are left unresolved on purpose.
fn resolve_target(
    candidates: &[String],
    files: &[String],
    file_set: &HashSet<&str>,
) -> Option<String> {
    for c in candidates {
        if file_set.contains(c.as_str()) {
            return Some(c.clone());
        }
    }
    for c in candidates {
        let suffix = format!("/{c}");
        let mut hits = files.iter().filter(|f| f.ends_with(&suffix));
        if let (Some(f), None) = (hits.next(), hits.next()) {
            return Some(f.clone());
        }
    }
    None
}

/// Disambiguate an ambiguous name match using import context: pick the single
/// candidate defined in a file that the anchor's file imports. Returns `None`
/// when zero or more than one candidate qualifies (so the edge is left dropped
/// rather than guessed).
fn disambiguate_import(
    anchor: &NodeId,
    candidates: &[NodeId],
    node_file: &HashMap<String, String>,
    import_map: &HashMap<String, HashSet<String>>,
) -> Option<NodeId> {
    let anchor_file = node_file.get(&anchor.0)?;
    let imported = import_map.get(anchor_file)?;
    let mut hit: Option<&NodeId> = None;
    for c in candidates {
        if let Some(f) = node_file.get(&c.0)
            && imported.contains(f)
        {
            if hit.is_some() {
                return None; // ambiguous even within imported files
            }
            hit = Some(c);
        }
    }
    hit.cloned()
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
        let text = match std::fs::read_to_string(root.join(&source.path)) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("cannot read schema {}: {e}", source.path);
                continue;
            }
        };
        let keys: Vec<OperationKey> = match source.protocol {
            Protocol::Rest => schema::openapi_rest_operations(&text)
                .into_iter()
                .map(|(method, path)| OperationKey::rest(method, &path))
                .collect(),
            Protocol::Grpc => schema::proto_grpc_operations(&text)
                .into_iter()
                .map(|path| OperationKey::opaque(Protocol::Grpc, path))
                .collect(),
            Protocol::GraphQl => schema::graphql_operations(&text)
                .into_iter()
                .map(|path| OperationKey::opaque(Protocol::GraphQl, path))
                .collect(),
        };
        if keys.is_empty() {
            tracing::warn!("no operations parsed from schema {}", source.path);
        }
        for key in keys {
            let method = key.method.map(|m| m.as_str()).unwrap_or("");
            let id = NodeId::derive(&[
                &source.path,
                "schema_op",
                key.protocol.as_str(),
                method,
                &key.path,
            ]);
            graph.add_node(Node {
                id: id.clone(),
                kind: NodeKind::SchemaOp,
                name: key.to_string(),
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
    use assert2::check;
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
        run(&dir, false, BackendKind::Sqlite).unwrap();
        {
            let store = SqliteStore::open(&RepoPaths::new(&dir).db_path).unwrap();
            check!(store.find_by_name("foo").unwrap().is_empty());
        }

        // Add the definition; caller.rs is unchanged, so only a global
        // re-resolution of persisted pending edges can create the link.
        std::fs::write(dir.join("def.rs"), "fn foo() {}\n").unwrap();
        run(&dir, true, BackendKind::Sqlite).unwrap();
        check!(
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
        run(&dir, false, BackendKind::Sqlite).unwrap();
        check!(callers_of(&dir, "foo").contains(&"use_it".to_string()));

        // Remove foo; the inferred use_it -> foo edge must disappear.
        std::fs::write(dir.join("def.rs"), "fn other() {}\n").unwrap();
        run(&dir, true, BackendKind::Sqlite).unwrap();
        let store = SqliteStore::open(&RepoPaths::new(&dir).db_path).unwrap();
        check!(
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
        check!(
            store
                .neighbors(&use_it.id.0, Some(EdgeKind::Calls), true)
                .unwrap()
                .is_empty(),
            "use_it should have no outgoing Calls edge after foo was removed"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Outgoing IMPORTS neighbours of a file, as (qualified_name, kind).
    fn import_targets(dir: &Path, importer_rel: &str) -> Vec<(String, String)> {
        let store = SqliteStore::open(&RepoPaths::new(dir).db_path).unwrap();
        let id = NodeId::derive(&["file", importer_rel]);
        store
            .neighbors(&id.0, Some(EdgeKind::Imports), true)
            .unwrap()
            .into_iter()
            .map(|(n, _, _)| (n.qualified_name, n.kind.as_str().to_string()))
            .collect()
    }

    // #18: a relative import resolves to the real target file node, not a
    // module placeholder.
    #[test]
    fn relative_import_resolves_to_real_file_node() {
        let dir = temp_repo("import-resolve");
        std::fs::create_dir_all(dir.join("web")).unwrap();
        std::fs::write(dir.join("web/app.ts"), "import { x } from \"./util\";\n").unwrap();
        std::fs::write(dir.join("web/util.ts"), "export const x = 1;\n").unwrap();
        std::fs::write(dir.join("web/ext.ts"), "import \"react\";\n").unwrap();
        run(&dir, false, BackendKind::Sqlite).unwrap();

        let targets = import_targets(&dir, "web/app.ts");
        check!(
            targets.contains(&("web/util.ts".to_string(), "file".to_string())),
            "expected app.ts -> web/util.ts (file), got {targets:?}"
        );
        // A bare specifier stays a module placeholder.
        let ext = import_targets(&dir, "web/ext.ts");
        check!(
            ext.iter().any(|(n, k)| n == "react" && k == "module"),
            "expected react to remain a module placeholder, got {ext:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // #18 + #20: an unresolved import in an *unchanged* file resolves once its
    // target file is added on a later `--update`.
    #[test]
    fn update_resolves_import_after_target_file_added() {
        let dir = temp_repo("import-add");
        std::fs::create_dir_all(dir.join("web")).unwrap();
        std::fs::write(dir.join("web/app.ts"), "import { x } from \"./util\";\n").unwrap();
        run(&dir, false, BackendKind::Sqlite).unwrap();
        // No util.ts yet: the import is a placeholder module.
        check!(
            import_targets(&dir, "web/app.ts")
                .iter()
                .any(|(n, k)| n == "./util" && k == "module"),
            "expected unresolved placeholder before the target exists"
        );

        // Add the target; app.ts is unchanged, so only the global rebuild links it.
        std::fs::write(dir.join("web/util.ts"), "export const x = 1;\n").unwrap();
        run(&dir, true, BackendKind::Sqlite).unwrap();
        let targets = import_targets(&dir, "web/app.ts");
        check!(
            targets.contains(&("web/util.ts".to_string(), "file".to_string())),
            "expected app.ts -> web/util.ts after --update, got {targets:?}"
        );
        check!(
            !targets.iter().any(|(n, _)| n == "./util"),
            "stale ./util placeholder edge should be gone, got {targets:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Incoming Calls neighbour names of the `helper` definition in `file`.
    fn callers_of_def_in(dir: &Path, name: &str, file: &str) -> Vec<String> {
        let store = SqliteStore::open(&RepoPaths::new(dir).db_path).unwrap();
        let def = store
            .find_by_name(name)
            .unwrap()
            .into_iter()
            .find(|n| n.file == file)
            .unwrap_or_else(|| panic!("'{name}' should be defined in {file}"));
        store
            .neighbors(&def.id.0, Some(EdgeKind::Calls), false)
            .unwrap()
            .into_iter()
            .map(|(n, _, _)| n.name)
            .collect()
    }

    // #19: when two files define the same name, a call resolves to the one in
    // the file the caller imports -- not the other, and not dropped.
    #[test]
    fn ambiguous_call_resolves_via_import_context() {
        let dir = temp_repo("disambiguate");
        std::fs::write(dir.join("b.ts"), "export function helper() { return 1; }\n").unwrap();
        std::fs::write(dir.join("c.ts"), "export function helper() { return 2; }\n").unwrap();
        std::fs::write(
            dir.join("a.ts"),
            "import { helper } from \"./b\";\nexport function use() { helper(); }\n",
        )
        .unwrap();
        run(&dir, false, BackendKind::Sqlite).unwrap();

        // The call resolves to b.ts (imported), not c.ts.
        check!(
            callers_of_def_in(&dir, "helper", "b.ts").contains(&"use".to_string()),
            "use() should resolve to the imported helper in b.ts"
        );
        check!(
            callers_of_def_in(&dir, "helper", "c.ts").is_empty(),
            "the un-imported helper in c.ts should have no caller"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let dst = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy_dir(&entry.path(), &dst);
            } else {
                std::fs::copy(entry.path(), &dst).unwrap();
            }
        }
    }

    // #23: a full `analyze` run over the committed fixture repo asserts the
    // cross-file links the graph should contain. The fixture is copied to a
    // temp dir so the run's `.meridian/` index doesn't pollute the source tree.
    #[test]
    fn analyzes_fixture_repo_with_cross_file_links() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample");
        let dir = temp_repo("fixture");
        copy_dir(&fixture, &dir);
        run(&dir, false, BackendKind::Sqlite).unwrap();

        let store = SqliteStore::open(&RepoPaths::new(&dir).db_path).unwrap();
        // Four source files (2 Rust, 2 Python).
        check!(store.stats().unwrap().files == 4);

        // Cross-file calls resolve in both languages.
        check!(
            callers_of(&dir, "helper").contains(&"run".to_string()),
            "rust app.rs::run should call lib.rs::helper"
        );
        check!(
            callers_of(&dir, "shared").contains(&"go".to_string()),
            "python main.py::go should call util.py::shared"
        );

        // Python import resolves to the real file (suffix match under py/).
        check!(
            import_targets(&dir, "py/main.py")
                .contains(&("py/util.py".to_string(), "file".to_string())),
            "main.py should import util.py as a file node"
        );

        // The NOTE design-rationale comment becomes a Comment node.
        let (nodes, _) = store.export_graph(10_000).unwrap();
        check!(
            nodes.iter().any(|n| n.kind == NodeKind::Comment),
            "the NOTE comment should be a Comment node"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
