use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use meridian_core::config::{IGNORE_FILE, RepoPaths};
use meridian_core::{
    ClientCall, CodeGraph, Confidence, Config, DynamicLanguage, Edge, EdgeKind, Endpoint, Language,
    Matcher, Node, NodeId, NodeKind, OperationKey, PendingLink, Protocol, RewriteEngine,
    SchemaFormat,
};
use meridian_parse::{
    DynamicGrammar, PendingEdge, build_client_graph, build_file_graph, build_graphql_client_graph,
    build_graphql_graph, build_grpc_client_graph, build_grpc_graph, build_rest_graph,
    build_ws_client_graph, build_ws_server_graph, load_dynamic, parse_source, resolve_import,
};
use std::collections::HashSet;

use crate::commands::backend::{self, BackendKind};
use crate::commands::schema;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;

/// Number of labeled stages in the reference-resolution phase, used as the
/// length of [`resolve_bar`]. Keep in sync with the `stage(...)` calls in `run`.
const RESOLVE_STAGES: u64 = 10;

/// A determinate bar for the reference-resolution phase: a `{bar} {pos}/{len}`
/// over the fixed stage count with `{msg}` naming the stage in flight, so the
/// user sees both how far along the phase is and what it is doing. Auto-hidden
/// when stderr is not a terminal (pipes, CI, tests), like the parse bar, so it
/// never pollutes captured output.
fn resolve_bar() -> ProgressBar {
    let pb = ProgressBar::new(RESOLVE_STAGES);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} resolving [{bar:24.cyan/blue}] {pos}/{len} {msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> "),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(120));
    pb
}

/// Build/output/dependency/VCS directories skipped during discovery regardless
/// of `.gitignore` (#144). Bare names match at any depth (gitignore semantics).
/// Repos can add more via `ignore_dirs` in `.meridian/config.toml`.
const DEFAULT_IGNORE_DIRS: &[&str] = &[
    "node_modules",
    "bower_components",
    "vendor",
    "target",
    "build",
    "bin",
    "obj",
    "dist",
    "out",
    "coverage",
    ".next",
    "turbo",
    ".turbo",
    ".gradle",
    ".nuget",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".idea",
    ".vs",
    ".vscode",
    ".meridian",
    ".gitnexus",
    "gitnexus",
    ".git",
    ".hg",
    ".svn",
    ".cvs",
    ".bzr",
];

/// Credential/key-material file globs never walked during discovery (#136), so
/// their contents stay out of the index and any agent-facing output. Kept to
/// unambiguous secret/key files to avoid excluding legitimate source.
const DEFAULT_SENSITIVE_FILES: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "*.pfx",
    "*.p12",
    "*.pkcs12",
    "*.keystore",
    "*.jks",
    "*.kdbx",
    "*.ppk",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "*.tfvars",
];
#[cfg(test)]
use meridian_store::SqliteStore;

struct DiscoveredFile {
    rel_path: String,
    abs_path: std::path::PathBuf,
    /// `None` marks a sensitive record-only file (#136): its existence is
    /// recorded as a `File` node but its contents are never read or parsed.
    language: Option<Language>,
    hash: String,
}

/// One file's parsed graph fragment plus the side lists merged into the global
/// accumulators after the parallel parse (#169).
struct FileOutput {
    graph: CodeGraph,
    operations: Vec<(NodeId, OperationKey)>,
    pending_handlers: Vec<(String, NodeId)>,
    pending: Vec<PendingEdge>,
    imports: Vec<(String, String, String)>,
    /// `(file, local symbol, module, language)` for symbol-level import
    /// resolution — links an imported router variable to its defining file (#167).
    import_symbols: Vec<(String, String, String, String)>,
}

/// Parse one discovered file and build its graph fragment + side lists. Pure and
/// self-contained (no DB, no shared mutable state), so it runs in parallel
/// (#169). `dyn_grammars` is pre-resolved so dynamic languages need no mutation
/// here. Returns `None` only when the file can't be read; a parse failure or a
/// missing dynamic grammar still yields the file node (matching the prior
/// sequential behavior).
fn parse_file(
    f: &DiscoveredFile,
    repo_id: &NodeId,
    dyn_grammars: &HashMap<String, Option<DynamicGrammar>>,
) -> Option<FileOutput> {
    let file_id = NodeId::derive(&["file", &f.rel_path]);
    let mut out = FileOutput {
        graph: CodeGraph::new(),
        operations: Vec::new(),
        pending_handlers: Vec::new(),
        pending: Vec::new(),
        imports: Vec::new(),
        import_symbols: Vec::new(),
    };

    // Sensitive record-only file (#136): record that it exists, never read it.
    let Some(language) = &f.language else {
        out.graph.add_node(Node {
            id: file_id.clone(),
            kind: NodeKind::File,
            name: f.rel_path.clone(),
            qualified_name: f.rel_path.clone(),
            file: f.rel_path.clone(),
            language: None,
            span: None,
            doc: Some("sensitive: contents excluded from the index".into()),
        });
        out.graph.add_edge(
            repo_id.clone(),
            file_id,
            EdgeKind::Contains,
            Confidence::Extracted,
        );
        return Some(out);
    };

    let source = std::fs::read_to_string(&f.abs_path).ok()?;
    out.graph.add_node(Node {
        id: file_id.clone(),
        kind: NodeKind::File,
        name: f.rel_path.clone(),
        qualified_name: f.rel_path.clone(),
        file: f.rel_path.clone(),
        language: Some(language.name().to_string()),
        span: None,
        doc: None,
    });
    out.graph.add_edge(
        repo_id.clone(),
        file_id.clone(),
        EdgeKind::Contains,
        Confidence::Extracted,
    );

    // Built-in languages parse via the compiled-in registry; a dynamic
    // `Language::Other` is parsed with its pre-resolved grammar + query.
    let parsed = match language {
        Language::Other(name) => match dyn_grammars.get(name).and_then(|o| o.as_ref()) {
            Some(dg) => meridian_parse::parse_with(&dg.grammar, &dg.query, &source),
            None => return Some(out),
        },
        builtin => parse_source(builtin, &source),
    };
    let parsed = match parsed {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("parse failed for {}: {e}", f.rel_path);
            return Some(out);
        }
    };

    let fg = build_file_graph(&f.rel_path, language, &file_id, &parsed);
    // REST server-route extraction runs for any language with a registered
    // extractor (registry decides; no-op otherwise).
    let rg = build_rest_graph(&f.rel_path, language, &fg.symbols, &source);
    out.graph.extend(rg.graph);
    out.operations.extend(rg.operations);
    out.pending_handlers.extend(rg.pending_handlers);
    // gRPC server-impl extraction (tonic/Rust).
    let gg = build_grpc_graph(&f.rel_path, language, &fg.symbols, &source);
    out.graph.extend(gg.graph);
    out.operations.extend(gg.operations);
    out.pending_handlers.extend(gg.pending_handlers);
    // GraphQL resolver extraction (async-graphql/Rust).
    let qg = build_graphql_graph(&f.rel_path, language, &fg.symbols, &source);
    out.graph.extend(qg.graph);
    out.operations.extend(qg.operations);
    out.pending_handlers.extend(qg.pending_handlers);
    // WebSocket server events (socket.io `on`) → Endpoint + HANDLES (#51).
    let wg = build_ws_server_graph(&f.rel_path, language, &fg.symbols, &source);
    out.graph.extend(wg.graph);
    out.operations.extend(wg.operations);
    out.pending_handlers.extend(wg.pending_handlers);
    // Client-call extraction (fetch/axios/reqwest/...).
    let cg = build_client_graph(&f.rel_path, &source, language);
    // GraphQL client operations (gql-tagged docs) → INVOKES.
    let qc = build_graphql_client_graph(&f.rel_path, &source, language);
    // gRPC client stub calls (tonic) → INVOKES.
    let gc = build_grpc_client_graph(&f.rel_path, &source, language);
    // WebSocket client connections (`new WebSocket(url)`) → INVOKES (#51).
    let wc = build_ws_client_graph(&f.rel_path, &source, language);
    // Link each client call site to its enclosing function (#130).
    let client_call_nodes: Vec<Node> = cg
        .graph
        .nodes
        .iter()
        .chain(&qc.graph.nodes)
        .chain(&gc.graph.nodes)
        .chain(&wc.graph.nodes)
        .filter(|n| n.kind == NodeKind::ClientCall)
        .cloned()
        .collect();
    let enclosing_edges = meridian_parse::enclosing_call_edges(&fg.graph.nodes, &client_call_nodes);
    out.graph.extend(cg.graph);
    out.operations.extend(cg.operations);
    out.graph.extend(qc.graph);
    out.operations.extend(qc.operations);
    out.graph.extend(gc.graph);
    out.operations.extend(gc.operations);
    out.graph.extend(wc.graph);
    out.operations.extend(wc.operations);
    out.graph.extend(fg.graph);
    for e in enclosing_edges {
        out.graph.add_edge(e.src, e.dst, e.kind, e.confidence);
    }
    out.pending.extend(fg.pending);
    out.imports.extend(
        fg.imports
            .into_iter()
            .map(|raw| (f.rel_path.clone(), raw, language.name().to_string())),
    );
    out.import_symbols.extend(
        meridian_parse::extract_import_symbols(&source, language)
            .into_iter()
            .map(|(sym, module)| (f.rel_path.clone(), sym, module, language.name().to_string())),
    );
    Some(out)
}

fn discover(
    root: &Path,
    dyn_langs: &[DynamicLanguage],
    extra_ignore_dirs: &[String],
    record_sensitive: bool,
) -> Result<Vec<DiscoveredFile>> {
    // Matcher for credential/key file names, used either to exclude them from the
    // walk (default) or to record their existence content-free (#136).
    let mut sensitive = globset::GlobSetBuilder::new();
    for g in DEFAULT_SENSITIVE_FILES {
        sensitive.add(
            globset::Glob::new(g).with_context(|| format!("invalid sensitive-file glob {g:?}"))?,
        );
    }
    let sensitive = sensitive
        .build()
        .context("building sensitive-file matcher")?;

    let mut walker = WalkBuilder::new(root);
    walker
        // Dotfiles are hidden by default; in record-sensitive mode they are
        // walked so hidden secrets (`.env`) can be recorded. Non-sensitive
        // dotfiles still fall through language detection and are skipped (#136).
        .hidden(!record_sensitive)
        .git_ignore(true)
        .git_exclude(true)
        .add_custom_ignore_filename(IGNORE_FILE);
    // Honor agent-exclusion files so sensitive data (secrets, key material) can
    // be kept out of the index entirely — and therefore out of any agent-facing
    // output (wiki, MCP, …). Listed alongside `.gitignore` and dotfile hiding.
    for ignore_file in [".aiignore", ".aiexclude", ".claudeignore"] {
        walker.add_custom_ignore_filename(ignore_file);
    }

    // Prune build/output/dependency dirs even when not gitignored (#144). An
    // override glob is matched gitignore-style but inverted, so a leading `!`
    // means "ignore"; a pure-ignore set keeps everything else. Bare names match
    // at any depth. A repo can extend the defaults via `ignore_dirs` in config.
    let mut overrides = OverrideBuilder::new(root);
    for dir in DEFAULT_IGNORE_DIRS
        .iter()
        .copied()
        .chain(extra_ignore_dirs.iter().map(String::as_str))
    {
        let glob = format!("!{}", dir.trim_start_matches('!'));
        overrides
            .add(&glob)
            .with_context(|| format!("invalid ignore_dirs entry {dir:?}"))?;
    }
    // Credential/key material (#136): excluded from the walk entirely by default
    // so their contents never reach the index. When `record_sensitive` is on we
    // instead let them through and record a content-free node below, so the graph
    // shows the file exists without exposing values.
    if !record_sensitive {
        for glob in DEFAULT_SENSITIVE_FILES {
            overrides
                .add(&format!("!{glob}"))
                .with_context(|| format!("invalid sensitive-file glob {glob:?}"))?;
        }
    }
    walker.overrides(overrides.build().context("building ignore overrides")?);

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
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        // A sensitive file reaches here only when `record_sensitive` is on
        // (otherwise it was excluded from the walk). Record its existence with a
        // path-derived hash and never read its bytes (#136).
        if record_sensitive
            && path
                .file_name()
                .map(|n| sensitive.is_match(n))
                .unwrap_or(false)
        {
            out.push(DiscoveredFile {
                hash: blake3::hash(format!("sensitive:{rel}").as_bytes())
                    .to_hex()
                    .to_string(),
                rel_path: rel,
                abs_path: path.to_path_buf(),
                language: None,
            });
            continue;
        }

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
        out.push(DiscoveredFile {
            rel_path: rel,
            abs_path: path.to_path_buf(),
            language: Some(language),
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
    let files = discover(
        &root,
        &cfg.languages,
        &cfg.ignore_dirs,
        cfg.security.record_sensitive_files,
    )?;
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

    // Per-file parse progress. Hidden automatically when stderr is not a TTY.
    let parse_progress = ProgressBar::new(changed.len() as u64);
    parse_progress.set_style(
        ProgressStyle::with_template("{spinner:.cyan} parsing [{bar:30}] {pos}/{len} {wide_msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=> "),
    );

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
    // (file, local symbol, module, language) for symbol→file resolution (#167).
    let mut import_symbols: Vec<(String, String, String, String)> = Vec::new();

    // Pre-resolve dynamic-language grammars once (sequential; warns once per
    // missing grammar) so the parallel parse can read them immutably.
    for f in &changed {
        if let Some(Language::Other(name)) = &f.language {
            dynamic_grammar(name, &cfg.languages, &mut dyn_grammars, &root);
        }
    }

    // On --update, drop each changed file's prior data before re-inserting. DB
    // mutation stays sequential; the parse below is parallel.
    if update {
        for f in &changed {
            store.delete_file_data(&f.rel_path)?;
        }
    }

    // Parse + build per-file fragments in parallel — CPU-bound and independent
    // per file (#169). rayon's `par_iter().filter_map().collect()` preserves
    // input order, so node insertion below stays deterministic.
    let outputs: Vec<FileOutput> = changed
        .par_iter()
        .filter_map(|f| {
            parse_progress.set_message(f.rel_path.clone());
            let out = parse_file(f, &repo_id, &dyn_grammars);
            parse_progress.inc(1);
            out
        })
        .collect();
    parse_progress.finish_and_clear();

    // Merge the fragments into the global accumulators, in file order.
    for out in outputs {
        graph.extend(out.graph);
        operations.extend(out.operations);
        pending_handlers.extend(out.pending_handlers);
        pending.extend(out.pending);
        imports.extend(out.imports);
        import_symbols.extend(out.import_symbols);
    }

    let resolve_progress = resolve_bar();
    // Advance the determinate bar one stage at a time: set the label to the work
    // about to run, do it, then `inc(1)` so `{pos}` counts completed stages.
    let stage = |label: &str| resolve_progress.set_message(label.to_string());

    // Ingest blessed schema artifacts into SchemaOp nodes (reconciled with code
    // endpoints as EXPOSES edges below). Schema ops are derived entirely from
    // config, so rebuild them from scratch each run: this drops entries whose
    // artifact or config line was removed or whose spec changed.
    stage("ingesting schema operations");
    store.delete_nodes_by_kind(NodeKind::SchemaOp)?;
    ingest_schemas(&root, &cfg, &mut graph, &mut operations);
    resolve_progress.inc(1);

    // Persist nodes and high-confidence (extracted) edges first.
    let extracted: Vec<Edge> = graph
        .edges
        .iter()
        .filter(|e| e.confidence == Confidence::Extracted)
        .cloned()
        .collect();
    stage("persisting nodes and edges");
    store.insert_graph(&graph.nodes, &extracted)?;
    resolve_progress.inc(1);

    stage("recording API operations and imports");
    store.insert_operations(&operations)?;
    store.insert_imports(&imports)?;
    resolve_progress.inc(1);

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
    stage("recording pending cross-file links");
    store.insert_pending(&pending_links)?;
    resolve_progress.inc(1);

    // Resolve imports against the discovered file set (exact path, then unique
    // suffix). Done before edge insertion so the importer -> target map can also
    // disambiguate ambiguous call/inheritance names below (#18, #19).
    stage("resolving imports to files");
    let file_rels: Vec<String> = files.iter().map(|f| f.rel_path.clone()).collect();
    let file_set: HashSet<&str> = file_rels.iter().map(|s| s.as_str()).collect();

    // (importing file, local symbol) -> defining file, from symbol-level imports,
    // so an imported router variable can be linked to its source file (#167).
    let mut symbol_file: HashMap<(String, String), String> = HashMap::new();
    for (file, symbol, module, lang_name) in &import_symbols {
        if let Some(target) = Language::ALL
            .into_iter()
            .find(|l| l.name() == lang_name)
            .and_then(|l| resolve_target(&resolve_import(file, module, &l), &file_rels, &file_set))
        {
            symbol_file.insert((file.clone(), symbol.clone()), target);
        }
    }

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
    resolve_progress.inc(1);

    // Re-resolve all persisted pending edges against the current global index.
    // A uniquely-named target resolves directly; an ambiguous name resolves
    // only if exactly one candidate sits in a file the anchor's file imports (#19).
    stage("inferring cross-file edges");
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
    resolve_progress.inc(1);

    // Rebuild IMPORTS edges from the resolved import set: real file targets
    // become file -> file (Inferred); the rest fall back to a module placeholder
    // (Extracted). Rebuilt globally each run so imports in unchanged files pick
    // up files added or removed elsewhere (#18).
    stage("rebuilding import edges");
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
    resolve_progress.inc(1);

    // Cross-boundary linking: resolve client calls and schema operations
    // against the endpoints, through the same rewrite-aware matcher. Runs over
    // the full store (endpoints, calls and schema ops commonly live in
    // different, possibly unchanged, files/artifacts).
    stage("linking client calls to endpoints");
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
    resolve_progress.inc(1);

    // Cross-file router-variable MOUNTS (#167): a synthetic `Router` node whose
    // variable was imported from another file gets a MOUNTS edge to that file, so
    // `app.use("/api", router)` where `router` is imported links end-to-end.
    stage("mounting cross-file routers");
    let router_edges: Vec<Edge> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Router)
        .filter_map(|n| {
            let target = symbol_file.get(&(n.file.clone(), n.name.clone()))?;
            Some(Edge {
                src: n.id.clone(),
                dst: NodeId::derive(&["file", target]),
                kind: EdgeKind::Mounts,
                confidence: Confidence::Inferred,
            })
        })
        .collect();
    store.insert_graph(&[], &router_edges)?;
    resolve_progress.inc(1);

    // Stamp the per-file hashes for the changed set in one batch so the next
    // `--update` run can skip them. One bulk write beats one fresh connection +
    // commit per file (the per-file loop was the phase's second-slowest stage).
    stage(&format!("recording {} file records", changed.len()));
    let file_records: Vec<(String, Option<String>, String)> = changed
        .iter()
        .map(|f| {
            (
                f.rel_path.clone(),
                f.language.as_ref().map(|l| l.name().to_string()),
                f.hash.clone(),
            )
        })
        .collect();
    store.set_files(&file_records)?;
    let pruned = store.prune_dangling_edges()?;
    if pruned > 0 {
        tracing::info!("pruned {pruned} dangling edges");
    }
    resolve_progress.inc(1);

    store.set_meta("tool_version", VERSION)?;
    resolve_progress.finish_and_clear();

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
        let keys: Vec<OperationKey> = match source.format {
            SchemaFormat::Hasura => schema::hasura_operations(&text),
            SchemaFormat::Auto => match source.protocol {
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
                // WebSocket has no schema-artifact format; connections are
                // extracted from code, not declared in a blessed schema.
                Protocol::WebSocket => Vec::new(),
            },
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

    // #144: build/output/dependency dirs are pruned from discovery; an extra
    // dir named in `ignore_dirs` is skipped too, while real source survives.
    #[test]
    fn discovery_skips_build_and_configured_dirs() {
        let dir = temp_repo("ignore-dirs");
        std::fs::write(dir.join("main.rs"), "fn f() {}\n").unwrap();
        for d in ["node_modules", "target", "dist", "__pycache__", "generated"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
            std::fs::write(dir.join(d).join("x.rs"), "fn g() {}\n").unwrap();
        }
        let found = discover(&dir, &[], &["generated".to_string()], false).unwrap();
        let rels: Vec<&str> = found.iter().map(|f| f.rel_path.as_str()).collect();
        check!(rels == ["main.rs"], "expected only main.rs, got {rels:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    // #130: a changed server handler propagates across the wire not just to the
    // client call site but to the function that makes the call and its callers.
    #[test]
    fn impact_propagates_through_client_call_to_callers() {
        use meridian_core::{ImpactPolicy, compute_impact};
        let dir = temp_repo("impact-transitive");
        std::fs::write(
            dir.join("server.rs"),
            "async fn list() {}\nfn app() -> Router { Router::new().route(\"/users\", get(list)) }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("client.ts"),
            "function consume() { return fetch(\"/users\"); }\nfunction caller() { return consume(); }\n",
        )
        .unwrap();
        run(&dir, false, BackendKind::Sqlite).unwrap();

        let store = SqliteStore::open(&RepoPaths::new(&dir).db_path).unwrap();
        let handler = store
            .find_by_name("list")
            .unwrap()
            .into_iter()
            .next()
            .expect("handler `list`");
        let impacted = compute_impact(&[handler.id], &ImpactPolicy::cross_boundary(8), &store);
        let names: Vec<String> = impacted
            .iter()
            .filter_map(|i| store.get_node(&i.node.0).ok().flatten().map(|n| n.name))
            .collect();
        check!(
            names.iter().any(|n| n == "consume"),
            "enclosing fn should be impacted across the wire, got {names:?}"
        );
        check!(
            names.iter().any(|n| n == "caller"),
            "caller of the call site should be impacted, got {names:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // #51: a `new WebSocket(url)` client links to the server's upgrade route
    // (a REST GET endpoint at the same path) via INVOKES.
    #[test]
    fn websocket_connection_invokes_upgrade_route() {
        let dir = temp_repo("ws-connect");
        std::fs::write(
            dir.join("server.rs"),
            "async fn ws_handler() {}\nfn app() -> Router { Router::new().route(\"/ws\", get(ws_handler)) }\n",
        )
        .unwrap();
        std::fs::write(dir.join("client.ts"), "const s = new WebSocket(\"/ws\");\n").unwrap();
        run(&dir, false, BackendKind::Sqlite).unwrap();

        let store = SqliteStore::open(&RepoPaths::new(&dir).db_path).unwrap();
        let endpoint = store
            .operations_by_kind(NodeKind::Endpoint)
            .unwrap()
            .into_iter()
            .find(|(_, k)| k.path == "/ws")
            .map(|(id, _)| id)
            .expect("GET /ws upgrade route");
        // Incoming INVOKES on the endpoint should include the WS client call.
        let invokers = store
            .neighbors(&endpoint.0, Some(EdgeKind::Invokes), false)
            .unwrap();
        check!(
            invokers.iter().any(|(n, _, _)| n.name.starts_with("ws ")),
            "WebSocket client should INVOKES the /ws route, got {:?}",
            invokers.iter().map(|(n, _, _)| &n.name).collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // #51 message boundary: a socket.io client `emit("e")` links to the server
    // `on("e")` event handler via INVOKES, matched by event name.
    #[test]
    fn socketio_emit_invokes_on_handler() {
        let dir = temp_repo("ws-events");
        std::fs::write(
            dir.join("server.js"),
            "function onMsg() {}\nsocket.on(\"chat:message\", onMsg);\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("client.js"),
            "socket.emit(\"chat:message\", payload);\n",
        )
        .unwrap();
        run(&dir, false, BackendKind::Sqlite).unwrap();

        let store = SqliteStore::open(&RepoPaths::new(&dir).db_path).unwrap();
        let endpoint = store
            .operations_by_kind(NodeKind::Endpoint)
            .unwrap()
            .into_iter()
            .find(|(_, k)| k.path == "event:chat:message")
            .map(|(id, _)| id)
            .expect("ws event endpoint");
        let invokers = store
            .neighbors(&endpoint.0, Some(EdgeKind::Invokes), false)
            .unwrap();
        check!(
            invokers
                .iter()
                .any(|(n, _, _)| n.name.starts_with("ws emit ")),
            "emit should INVOKES the on handler, got {:?}",
            invokers.iter().map(|(n, _, _)| &n.name).collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // #167: a router variable mounted in one file but imported from another links
    // the synthetic Router node to the defining file with a MOUNTS edge.
    #[test]
    fn cross_file_router_mount_links_to_defining_file() {
        let dir = temp_repo("xfile-router");
        std::fs::write(
            dir.join("app.js"),
            "import apiRouter from './api';\nconst app = express();\napp.use(\"/api\", apiRouter);\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("api.js"),
            "const apiRouter = express.Router();\napiRouter.get(\"/users\", getUsers);\nexport default apiRouter;\n",
        )
        .unwrap();
        run(&dir, false, BackendKind::Sqlite).unwrap();

        let store = SqliteStore::open(&RepoPaths::new(&dir).db_path).unwrap();
        // The mounted `apiRouter` Router node (in app.js) MOUNTS the api.js file.
        let router_id = NodeId::derive(&["app.js", "router", "apiRouter"]);
        let mounts = store
            .neighbors(&router_id.0, Some(EdgeKind::Mounts), true)
            .unwrap();
        check!(
            mounts.iter().any(|(n, _, _)| n.name == "api.js"),
            "expected apiRouter -> api.js MOUNTS, got {:?}",
            mounts.iter().map(|(n, _, _)| &n.name).collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // #136: credential/key-material files are never discovered, so their
    // contents stay out of the index and any agent-facing output.
    #[test]
    fn discovery_excludes_sensitive_files() {
        let dir = temp_repo("sensitive");
        std::fs::write(dir.join("main.rs"), "fn f() {}\n").unwrap();
        for s in [".env", "id_rsa", "server.pem", "prod.tfvars", "app.key"] {
            std::fs::write(dir.join(s), "SECRET=value\n").unwrap();
        }
        let found = discover(&dir, &[], &[], false).unwrap();
        let rels: Vec<&str> = found.iter().map(|f| f.rel_path.as_str()).collect();
        check!(
            rels == ["main.rs"],
            "sensitive files must be excluded, got {rels:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // #136 record mode: with record_sensitive on, sensitive files are discovered
    // as content-less records (language = None) — their bytes are never read.
    #[test]
    fn record_sensitive_discovers_existence_without_reading() {
        let dir = temp_repo("sensitive-record");
        std::fs::write(dir.join("main.rs"), "fn f() {}\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=value\n").unwrap();
        std::fs::write(dir.join("id_rsa"), "PRIVATE KEY\n").unwrap();

        let found = discover(&dir, &[], &[], true).unwrap();
        let env = found
            .iter()
            .find(|f| f.rel_path == ".env")
            .expect(".env recorded");
        // Recorded but flagged content-less, and the hash is path-derived (not a
        // hash of the file's secret bytes).
        check!(env.language.is_none());
        check!(env.hash == blake3::hash(b"sensitive:.env").to_hex().to_string());
        check!(
            found
                .iter()
                .any(|f| f.rel_path == "id_rsa" && f.language.is_none())
        );
        // Real source is still parsed normally.
        check!(
            found
                .iter()
                .any(|f| f.rel_path == "main.rs" && f.language.is_some())
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // #136 record mode end-to-end: a sensitive file becomes a File node carrying
    // an exclusion marker, and its secret value is nowhere in the index.
    #[test]
    fn record_sensitive_emits_marked_file_node_without_contents() {
        let dir = temp_repo("sensitive-e2e");
        std::fs::write(dir.join("main.rs"), "fn f() {}\n").unwrap();
        std::fs::write(dir.join(".env"), "API_TOKEN=supersecret\n").unwrap();
        std::fs::create_dir_all(dir.join(".meridian")).unwrap();
        std::fs::write(
            dir.join(".meridian/config.toml"),
            "[security]\nrecord_sensitive_files = true\n",
        )
        .unwrap();
        run(&dir, false, BackendKind::Sqlite).unwrap();

        let store = SqliteStore::open(&RepoPaths::new(&dir).db_path).unwrap();
        let env = store
            .find_by_name(".env")
            .unwrap()
            .into_iter()
            .next()
            .expect(".env File node");
        check!(env.kind == NodeKind::File);
        check!(env.doc.as_deref() == Some("sensitive: contents excluded from the index"));
        // The secret value never entered the index (no node mentions it).
        check!(store.search("supersecret", 50).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
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
