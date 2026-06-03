use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use glyphtrail_core::config::{IGNORE_FILE, INDEX_DIR, LEGACY_IGNORE_FILE, RepoPaths};
use glyphtrail_core::{
    ClientCall, CodeGraph, Confidence, Config, DynamicLanguage, Ecosystem, Edge, EdgeKind,
    Endpoint, ExternalUse, IndexedPackage, Language, META_EXTERNAL_USES, META_PACKAGES, Matcher,
    Node, NodeId, NodeKind, OperationKey, PackageExport, PackageIdentity, PendingLink, Protocol,
    Registry, RewriteEngine, SchemaFormat, parse_cargo_manifest, parse_csproj, workspace_members,
};
use glyphtrail_parse::{
    DynamicGrammar, PendingEdge, build_client_graph, build_file_graph, build_graphql_client_graph,
    build_graphql_graph, build_grpc_client_graph, build_grpc_graph, build_rest_graph,
    build_ws_client_graph, build_ws_server_graph, load_dynamic, parse_source, resolve_import,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::backend;
use crate::schema;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;

/// Number of labeled stages in the reference-resolution phase, used as the
/// length of [`resolve_bar`]. Keep in sync with the `stage(...)` calls in `run`.
const RESOLVE_STAGES: u64 = 11;

/// Worker-thread stack for the parallel parse pool. AST extraction recurses, and
/// a deeply nested file overflows the default (~2MB) worker stack on a large
/// repo; this is a generous safety net on top of the iterative walkers.
const PARSE_STACK_BYTES: usize = 256 * 1024 * 1024;

/// Shared rayon pool for parsing, built once with [`PARSE_STACK_BYTES`] stacks
/// and reused across every repo in a `repo scan`, so we don't churn pools per
/// repo or rely on the small default global-pool stacks.
fn parse_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .stack_size(PARSE_STACK_BYTES)
            .build()
            .expect("build parse thread pool")
    })
}

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
/// Repos can add more via `ignore_dirs` in `.glyphtrail/config.toml`.
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
    ".glyphtrail",
    ".gitnexus",
    "gitnexus",
    ".git",
    ".hg",
    ".svn",
    ".cvs",
    ".bzr",
];

/// Version-control markers that mark a directory as a (nested) repository root.
/// A subdirectory holding one of these is a submodule / vendored checkout and is
/// pruned from the walk so its code lands only in its own index.
const NESTED_VCS_MARKERS: [&str; 4] = [".git", ".svn", ".bzr", ".hg"];

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
use glyphtrail_store::GraphStore;
#[cfg(test)]
use glyphtrail_store::LadybugStore;

struct DiscoveredFile {
    rel_path: String,
    abs_path: std::path::PathBuf,
    /// The tree-sitter language, or `None`. `None` does not by itself mean
    /// "sensitive": it also marks the `sql`/`cypher` artifacts below, which a
    /// dedicated DDL/Cypher extractor handles instead of a grammar. A sensitive
    /// record-only file (#136) is the `None` case with neither flag set: its
    /// existence is recorded as a `File` node but its contents are never read.
    language: Option<Language>,
    /// A `.sql` schema/migration file (#416): parsed by the DDL extractor rather
    /// than a tree-sitter grammar, so it carries no `Language`.
    sql: bool,
    /// A `.cypher`/`.cql` graph-query file (#444): parsed by the Cypher extractor
    /// (labels + access), so it likewise carries no `Language`.
    cypher: bool,
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
    /// `(file, name, value)` module-scope string constants, so a client URL built
    /// from an *imported* constant base resolves to a concrete path (#405).
    string_consts: Vec<(String, String, String)>,
    /// `(file, name, referenced key)` constant references (`const X = OBJ.PROP`,
    /// object props naming another const), for the Angular `environment` chain.
    const_refs: Vec<(String, String, String)>,
    /// `(enclosing fn id, access, table name)` embedded-query accesses (#416 B);
    /// the table name is resolved to `Table` node(s) after the global parse.
    db_accesses: Vec<(NodeId, glyphtrail_parse::DbAccess, String)>,
    /// `(normalized entity name, normalized table name)` from JPA `@Entity`
    /// classes (#416 B, Java), so an entity ref in a repo/`@Query` resolves to
    /// its table.
    entity_tables: Vec<(String, String)>,
    /// `(owning table id, related entity-or-table name)` JPA relationship FKs
    /// (#433), resolved to table→table `References` edges after the parse.
    db_references: Vec<(NodeId, String)>,
}

/// The innermost function/method node whose span contains `byte`, for attributing
/// an embedded query to the routine that runs it (#416 Phase B).
fn enclosing_fn(nodes: &[Node], byte: usize) -> Option<NodeId> {
    nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method))
        .filter_map(|n| n.span.map(|s| (n, s)))
        .filter(|(_, s)| s.start_byte <= byte && byte < s.end_byte)
        .min_by_key(|(_, s)| s.end_byte - s.start_byte)
        .map(|(n, _)| n.id.clone())
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
        string_consts: Vec::new(),
        const_refs: Vec::new(),
        db_accesses: Vec::new(),
        entity_tables: Vec::new(),
        db_references: Vec::new(),
    };

    // SQL schema/migration file (#416): the DDL extractor produces table/column
    // nodes instead of the code pipeline below.
    if f.sql {
        let source = std::fs::read_to_string(&f.abs_path).ok()?;
        out.graph.add_node(Node {
            id: file_id.clone(),
            kind: NodeKind::File,
            name: f.rel_path.clone(),
            qualified_name: f.rel_path.clone(),
            file: f.rel_path.clone(),
            language: Some("sql".to_string()),
            span: None,
            doc: None,
            signature: None,
        });
        out.graph.add_edge(
            repo_id.clone(),
            file_id.clone(),
            EdgeKind::Contains,
            Confidence::Extracted,
        );
        out.graph.extend(glyphtrail_parse::build_sql_graph(
            &f.rel_path,
            &file_id,
            &source,
        ));
        return Some(out);
    }

    // Cypher graph-query file (#444): the Cypher extractor produces label `Table`
    // nodes; the file's `MATCH`/`MERGE`/`CREATE` access is attributed to the file
    // itself (no enclosing function), resolved to those tables after the parse.
    if f.cypher {
        let source = std::fs::read_to_string(&f.abs_path).ok()?;
        out.graph.add_node(Node {
            id: file_id.clone(),
            kind: NodeKind::File,
            name: f.rel_path.clone(),
            qualified_name: f.rel_path.clone(),
            file: f.rel_path.clone(),
            language: Some("cypher".to_string()),
            span: None,
            doc: None,
            signature: None,
        });
        out.graph.add_edge(
            repo_id.clone(),
            file_id.clone(),
            EdgeKind::Contains,
            Confidence::Extracted,
        );
        let cyp = glyphtrail_parse::extract_cypher_file(&f.rel_path, &file_id, &source);
        out.graph.extend(cyp.graph);
        for q in cyp.accesses {
            for (access, label) in q.accesses {
                out.db_accesses.push((file_id.clone(), access, label));
            }
        }
        return Some(out);
    }

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
            signature: None,
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
        signature: None,
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
            Some(dg) => glyphtrail_parse::parse_with(&dg.grammar, &dg.query, &source),
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

    let fg = build_file_graph(&f.rel_path, language, &file_id, &parsed, &source);
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
    let enclosing_edges =
        glyphtrail_parse::enclosing_call_edges(&fg.graph.nodes, &client_call_nodes);
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
    // Embedded DB queries (sqlx) → the enclosing function reads/writes a table
    // (#416 Phase B). The function is resolved now (same file); the table name is
    // matched to `Table` node(s) after the global parse.
    for q in glyphtrail_parse::extract_db_queries(&source, language) {
        if let Some(fn_id) = enclosing_fn(&out.graph.nodes, q.byte) {
            for (access, table) in q.accesses {
                out.db_accesses.push((fn_id.clone(), access, table));
            }
        }
    }
    // Embedded Cypher (#416 Phase C, Rust): kuzu DDL `CREATE NODE/REL TABLE`
    // declares a label (a `Table` node); `MATCH`/`MERGE`/`CREATE (n:Label)` reads
    // or writes it, attributed to the enclosing function.
    if *language == Language::Rust {
        let cyp = glyphtrail_parse::extract_cypher(&f.rel_path, &file_id, &source, language);
        out.graph.extend(cyp.graph);
        for q in cyp.accesses {
            if let Some(fn_id) = enclosing_fn(&out.graph.nodes, q.byte) {
                for (access, label) in q.accesses {
                    out.db_accesses.push((fn_id.clone(), access, label));
                }
            }
        }
        // Diesel ORM (#440): `table!` macros declare `Table`/`Column` nodes; the
        // query DSL (`<t>::table.load(…)`, `insert_into(<t>::table)`, …) reads or
        // writes them, attributed to the enclosing function.
        let dsl = glyphtrail_parse::extract_diesel(&f.rel_path, &file_id, &source);
        out.graph.extend(dsl.graph);
        for (byte, access, table) in dsl.accesses {
            if let Some(fn_id) = enclosing_fn(&out.graph.nodes, byte) {
                out.db_accesses.push((fn_id.clone(), access, table));
            }
        }
    }
    // JPA/Hibernate (#416 Phase B, Java): `@Entity` classes become tables, and
    // repository methods / `@Query` annotations read/write them. Entity refs are
    // mapped to their tables in the global resolution below.
    if *language == Language::Java {
        let jpa = glyphtrail_parse::extract_jpa(&f.rel_path, &file_id, &source, language);
        out.graph.extend(jpa.graph);
        out.entity_tables.extend(jpa.entity_tables);
        out.db_references.extend(jpa.references);
        for q in jpa.accesses {
            if let Some(fn_id) = enclosing_fn(&out.graph.nodes, q.byte) {
                for (access, table) in q.accesses {
                    out.db_accesses.push((fn_id.clone(), access, table));
                }
            }
        }
    }
    out.pending.extend(fg.pending);
    out.imports.extend(
        fg.imports
            .into_iter()
            .map(|raw| (f.rel_path.clone(), raw, language.name().to_string())),
    );
    out.import_symbols.extend(
        glyphtrail_parse::extract_import_symbols(&source, language)
            .into_iter()
            .map(|(sym, module)| (f.rel_path.clone(), sym, module, language.name().to_string())),
    );
    let consts = glyphtrail_parse::module_constants(&source, language);
    out.string_consts.extend(
        consts
            .strings
            .into_iter()
            .map(|(name, value)| (f.rel_path.clone(), name, value)),
    );
    out.const_refs.extend(
        consts
            .refs
            .into_iter()
            .map(|(name, target)| (f.rel_path.clone(), name, target)),
    );
    Some(out)
}

fn discover(
    root: &Path,
    dyn_langs: &[DynamicLanguage],
    extra_ignore_dirs: &[String],
    record_sensitive: bool,
    user_ignore: Option<&Path>,
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
        .add_custom_ignore_filename(IGNORE_FILE)
        // Also honor the pre-rename name (#293) — a repo's `.stratographignore`
        // is a tracked file we don't move, so keep reading it.
        .add_custom_ignore_filename(LEGACY_IGNORE_FILE);

    // Treat a nested repository (submodule, vendored checkout) as a boundary:
    // its files belong to *that* repo's index, not this one. Without this a
    // parent indexes its submodules' code, and `repo scan --recursive` then
    // indexes the same files again under the submodule. The root itself
    // (depth 0) is never pruned even though it holds a VCS directory.
    walker.filter_entry(|entry| {
        if entry.depth() == 0 || !entry.file_type().is_some_and(|t| t.is_dir()) {
            return true;
        }
        let dir = entry.path();
        !NESTED_VCS_MARKERS.iter().any(|m| dir.join(m).exists())
    });
    // Honor agent-exclusion files so sensitive data (secrets, key material) can
    // be kept out of the index entirely — and therefore out of any agent-facing
    // output (wiki, MCP, …). Listed alongside `.gitignore` and dotfile hiding.
    for ignore_file in [".aiignore", ".aiexclude", ".claudeignore"] {
        walker.add_custom_ignore_filename(ignore_file);
    }
    // A user-wide ignore file (#269): gitignore-format patterns applied to every
    // repo, handy when bulk-indexing whole work directories. Lower precedence
    // than a repo's own ignore files, which can re-include with `!`.
    if let Some(user_ignore) = user_ignore.filter(|p| p.is_file()) {
        walker.add_ignore(user_ignore);
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
                sql: false,
                cypher: false,
            });
            continue;
        }

        // SQL schema/migration files (#416): handled by the DDL extractor, not a
        // tree-sitter grammar, so they carry no `Language`.
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext.eq_ignore_ascii_case("sql") {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let hash = blake3::hash(&bytes).to_hex().to_string();
            out.push(DiscoveredFile {
                rel_path: rel,
                abs_path: path.to_path_buf(),
                language: None,
                sql: true,
                cypher: false,
                hash,
            });
            continue;
        }

        // Cypher graph-query files (#444): handled by the Cypher extractor (labels
        // + access), not a tree-sitter grammar, so they carry no `Language`.
        if ext.eq_ignore_ascii_case("cypher") || ext.eq_ignore_ascii_case("cql") {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let hash = blake3::hash(&bytes).to_hex().to_string();
            out.push(DiscoveredFile {
                rel_path: rel,
                abs_path: path.to_path_buf(),
                language: None,
                sql: false,
                cypher: true,
                hash,
            });
            continue;
        }

        // Built-in language by extension, else a configured dynamic language.
        let language = match Language::from_path(path) {
            Some(l) => l,
            None => match dyn_langs
                .iter()
                .find(|d| d.extensions.iter().any(|e| e == ext))
            {
                Some(d) => Language::Other(d.name.clone()),
                None => continue,
            },
        };
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let hash = blake3::hash(&bytes).to_hex().to_string();
        out.push(DiscoveredFile {
            rel_path: rel,
            abs_path: path.to_path_buf(),
            language: Some(language),
            sql: false,
            cypher: false,
            hash,
        });
    }
    Ok(out)
}

/// A package a repo publishes, paired with the repo-relative directory of its
/// manifest. The directory lets a workspace's many packages be told apart by
/// file location, so each symbol is attributed to the package that owns it.
/// Ecosystem-neutral so the same producer/consumer machinery serves Cargo and
/// .NET (and future ecosystems).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiscoveredPackage {
    ecosystem: Ecosystem,
    /// Manifest directory, repo-root-relative and forward-slashed; "" is the
    /// repo root.
    dir: String,
    /// Package name as other repos depend on it (the cross-repo match key).
    name: String,
    version: Option<String>,
    /// Declared dependencies, the consumer side of cross-repo links.
    deps: Vec<DiscoveredDep>,
}

/// A declared dependency: its real package name (recorded on the cross-repo
/// link) and the name code references it by (Cargo: rename/underscored crate;
/// .NET: the package id used as a `using` namespace).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiscoveredDep {
    name: String,
    code_name: String,
}

/// Discover the packages a repo publishes (#220), across ecosystems: Cargo
/// crates and .NET/NuGet projects. De-duplicated by `(ecosystem, name)` and
/// sorted, so the result is stable across runs. Manifests are not parsed source
/// languages, so this is a separate, best-effort pass: an unreadable or
/// malformed manifest is skipped rather than failing the analysis.
fn discover_packages(root: &Path) -> Vec<DiscoveredPackage> {
    let mut packages = discover_cargo_packages(root);
    packages.extend(discover_dotnet_packages(root));
    packages.sort_by(|a, b| (a.ecosystem as u8, &a.name).cmp(&(b.ecosystem as u8, &b.name)));
    packages.dedup_by(|a, b| a.ecosystem == b.ecosystem && a.name == b.name);
    packages
}

/// Cargo crates: the root `Cargo.toml` plus every workspace member (globs like
/// `crates/*` expanded against the filesystem). A virtual workspace root
/// contributes only its members.
fn discover_cargo_packages(root: &Path) -> Vec<DiscoveredPackage> {
    let root_manifest = root.join("Cargo.toml");
    let mut manifests: Vec<PathBuf> = vec![root_manifest.clone()];
    if let Ok(text) = std::fs::read_to_string(&root_manifest) {
        for member in workspace_members(&text) {
            for dir in expand_member(root, &member) {
                manifests.push(dir.join("Cargo.toml"));
            }
        }
    }
    let mut packages = Vec::new();
    for manifest in manifests {
        if let Ok(text) = std::fs::read_to_string(&manifest)
            && let Ok(Some(package)) = parse_cargo_manifest(&text)
        {
            let dir = rel_dir(root, &manifest);
            packages.push(DiscoveredPackage {
                ecosystem: Ecosystem::Cargo,
                dir,
                name: package.name,
                version: package.version,
                deps: package
                    .dependencies
                    .iter()
                    .map(|d| DiscoveredDep {
                        name: d.name.clone(),
                        code_name: dep_code_name(d),
                    })
                    .collect(),
            });
        }
    }
    packages
}

/// .NET projects: every `.csproj` under the repo (skipping hidden dirs and the
/// build outputs `bin`/`obj`). Each publishes a NuGet package id and references
/// others via `<PackageReference>` — the producer/consumer sides of cross-repo
/// links among .NET repos sharing packages through a private NuGet feed.
fn discover_dotnet_packages(root: &Path) -> Vec<DiscoveredPackage> {
    let mut packages = Vec::new();
    for entry in WalkBuilder::new(root).hidden(true).build().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("csproj") {
            continue;
        }
        if path
            .components()
            .any(|c| matches!(c.as_os_str().to_str(), Some("bin") | Some("obj")))
        {
            continue; // build output, not a source project
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("project");
        let proj = parse_csproj(&text, stem);
        packages.push(DiscoveredPackage {
            ecosystem: Ecosystem::Dotnet,
            dir: rel_dir(root, path),
            name: proj.package_id,
            version: proj.version,
            deps: proj
                .package_refs
                .into_iter()
                .map(|r| DiscoveredDep {
                    name: r.clone(),
                    code_name: r,
                })
                .collect(),
        });
    }
    packages
}

/// A manifest's repo-root-relative, forward-slashed parent directory (`""` for
/// the repo root).
fn rel_dir(root: &Path, manifest: &Path) -> String {
    manifest
        .parent()
        .and_then(|p| p.strip_prefix(root).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

/// Whether a node kind is a definition another crate could import — the kinds
/// worth recording as exports. Structural and API-surface kinds are excluded.
fn is_exportable_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Function
            | NodeKind::Method
            | NodeKind::Class
            | NodeKind::Struct
            | NodeKind::Interface
            | NodeKind::Enum
            | NodeKind::Trait
            | NodeKind::Module
    )
}

/// Resolve each discovered package's export index from the freshly-built graph:
/// every exportable symbol whose file lives under the package's directory,
/// attributed to the most specific (longest-directory) package so a workspace's
/// crates don't claim each other's symbols. Read-only over `store`.
fn index_packages(
    store: &dyn GraphStore,
    discovered: &[DiscoveredPackage],
) -> Result<Vec<IndexedPackage>> {
    // Owning package for a file: the one whose dir is the longest matching
    // prefix. "" (a root package) matches anything but loses to any deeper dir.
    let owner = |file: &str| -> Option<usize> {
        discovered
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.dir.is_empty() || file == p.dir || file.starts_with(&format!("{}/", p.dir))
            })
            .max_by_key(|(_, p)| p.dir.len())
            .map(|(i, _)| i)
    };

    let mut exports: Vec<Vec<PackageExport>> = vec![Vec::new(); discovered.len()];
    for file in store.all_files()? {
        let Some(idx) = owner(&file) else {
            continue;
        };
        for node in store.nodes_in_file(&file)? {
            if is_exportable_kind(node.kind) {
                exports[idx].push(PackageExport {
                    name: node.name,
                    qualified_name: node.qualified_name,
                    kind: node.kind,
                    file: node.file,
                    node_id: node.id.0,
                });
            }
        }
    }

    // `pub use X as Y` re-export pass (#239): a renamed re-export makes `Y` a
    // public name for the item defined as `X`. Non-renamed re-exports and globs
    // already resolve, since matching is by name. Add an alias export entry so a
    // consumer importing `Y` matches symbol-level instead of falling back to
    // crate level. A private `use … as …` adds a harmless alias no consumer can
    // import, so visibility need not be checked (matching is name-based).
    for (file, raw, _lang) in store.all_imports()? {
        let Some(idx) = owner(&file) else {
            continue;
        };
        for (underlying, alias) in use_aliases(&raw) {
            let clones: Vec<PackageExport> = exports[idx]
                .iter()
                .filter(|e| e.name == underlying)
                .map(|e| PackageExport {
                    name: alias.clone(),
                    ..e.clone()
                })
                .collect();
            exports[idx].extend(clones);
        }
    }

    Ok(discovered
        .iter()
        .zip(exports)
        .map(|(d, mut exports)| {
            exports.sort_by(|a, b| (&a.node_id, &a.name).cmp(&(&b.node_id, &b.name)));
            exports.dedup_by(|a, b| a.node_id == b.node_id && a.name == b.name);
            IndexedPackage {
                ecosystem: d.ecosystem,
                name: d.name.clone(),
                version: d.version.clone(),
                dir: d.dir.clone(),
                exports,
            }
        })
        .collect())
}

/// Renamed re-exports in a use specifier: `(underlying, alias)` for each
/// `X as Y` (underlying is the path tail before `as`). Handles a simple
/// `foo::Bar as Baz` and brace groups `foo::{Bar as Baz, Qux}`; non-renamed
/// entries yield nothing.
fn use_aliases(raw: &str) -> Vec<(String, String)> {
    let items: Vec<&str> = match (raw.find('{'), raw.rfind('}')) {
        (Some(open), Some(close)) if close > open => raw[open + 1..close].split(',').collect(),
        _ => vec![raw],
    };
    items
        .into_iter()
        .filter_map(|item| {
            let (lhs, alias) = item.split_once(" as ")?;
            let underlying = lhs.trim().rsplit("::").next()?.trim();
            let alias = alias.trim();
            (!underlying.is_empty() && !alias.is_empty())
                .then(|| (underlying.to_string(), alias.to_string()))
        })
        .collect()
}

/// The name a dependency is referenced under in code: its rename alias when
/// present, otherwise the crate name, with `-` normalised to `_` (Cargo crate
/// names hyphenate; Rust paths use underscores).
fn dep_code_name(dep: &glyphtrail_core::CargoDependency) -> String {
    dep.alias.as_deref().unwrap_or(&dep.name).replace('-', "_")
}

/// The first path segment of an import specifier, ignoring a leading `::`.
fn import_root(path: &str) -> Option<&str> {
    path.split("::")
        .map(str::trim)
        .find(|s| !s.is_empty() && *s != "{")
}

/// Whether an import `raw` references the dependency whose code name is
/// `code_name`, per ecosystem. Cargo matches the import's root crate segment
/// (`foo::bar` → `foo`); .NET matches a `using` namespace against the NuGet
/// package id, exactly or as a namespace prefix (`using Acme.Core.Sub` uses
/// `Acme.Core`). Other ecosystems aren't produced yet.
fn dep_matches_import(ecosystem: Ecosystem, code_name: &str, raw: &str) -> bool {
    match ecosystem {
        Ecosystem::Cargo => import_root(raw) == Some(code_name),
        Ecosystem::Dotnet => {
            let ns = raw.trim();
            ns == code_name || ns.starts_with(&format!("{code_name}."))
        }
        Ecosystem::Npm | Ecosystem::Go | Ecosystem::Python => false,
    }
}

/// Identify the imports that reference an external crate, the consumer side of
/// cross-repo links (#220). For each import record `(file, raw, lang)`, find the
/// package owning `file` (longest-directory match) and match the import's root
/// segment against that package's declared dependencies. Read-only over `store`.
fn external_uses(
    store: &dyn GraphStore,
    discovered: &[DiscoveredPackage],
    root: &Path,
) -> Result<Vec<ExternalUse>> {
    let owner = |file: &str| -> Option<&DiscoveredPackage> {
        discovered
            .iter()
            .filter(|p| {
                p.dir.is_empty() || file == p.dir || file.starts_with(&format!("{}/", p.dir))
            })
            .max_by_key(|p| p.dir.len())
    };

    // A C# `using` names a namespace, not a symbol, so symbol-level matching
    // can't read the consumed types off the import path. Instead use the file's
    // *unresolved* references (pending links) as candidate symbols + their
    // anchor nodes as use-sites; the link step keeps only those that match a
    // producer's exports. Built once, keyed by file.
    let unresolved = unresolved_refs_by_file(store)?;

    // Cache file source bytes so a file imported by several deps is read once.
    let mut sources: HashMap<String, Option<Vec<u8>>> = HashMap::new();
    let mut uses = Vec::new();
    for (file, raw, _lang) in store.all_imports()? {
        let Some(pkg) = owner(&file) else { continue };
        if let Some(dep) = pkg
            .deps
            .iter()
            .find(|d| dep_matches_import(pkg.ecosystem, &d.code_name, &raw))
        {
            // Cargo names the symbol in the path; .NET supplies candidate symbols
            // (and their use-site nodes) from the file. Candidates are the
            // PascalCase identifiers the file references (C# types and methods,
            // covering type-only uses the call-based pending refs miss) plus the
            // unresolved-reference names; the link step keeps only those matching
            // a producer export. Use-sites are the unresolved-reference anchors
            // (precise where there are calls; file-level otherwise).
            let (from_nodes, symbols) = match pkg.ecosystem {
                Ecosystem::Dotnet => {
                    let (anchors, mut names) = unresolved.get(&file).cloned().unwrap_or_default();
                    let bytes = sources
                        .entry(file.clone())
                        .or_insert_with(|| std::fs::read(root.join(&file)).ok());
                    let mut symbols = pascal_case_idents(bytes.as_deref().unwrap_or(&[]));
                    symbols.append(&mut names);
                    symbols.sort();
                    symbols.dedup();
                    (anchors, symbols)
                }
                _ => (
                    use_site_nodes(store, root, &file, &raw, &mut sources)?,
                    Vec::new(),
                ),
            };
            uses.push(ExternalUse {
                ecosystem: pkg.ecosystem,
                from_package: pkg.name.clone(),
                from_file: file.clone(),
                package: dep.name.clone(),
                path: raw,
                from_nodes,
                symbols,
            });
        }
    }
    uses.sort_by(|a, b| {
        (&a.from_file, &a.path, &a.package).cmp(&(&b.from_file, &b.path, &b.package))
    });
    uses.dedup();
    Ok(uses)
}

/// Distinct PascalCase identifiers in `src` — a C# convention for type and
/// method names, so this captures the producer symbols a consumer references
/// (including type-only uses that aren't calls). Over-broad on purpose: the link
/// step keeps only those that match a referenced package's exports, and comment/
/// string noise simply matches nothing. Identifiers are `[A-Z][A-Za-z0-9_]+`.
fn pascal_case_idents(src: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(src);
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, set: &mut std::collections::BTreeSet<String>| {
        if cur.len() >= 2 && cur.starts_with(|c: char| c.is_ascii_uppercase()) {
            set.insert(std::mem::take(cur));
        } else {
            cur.clear();
        }
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else {
            flush(&mut cur, &mut set);
        }
    }
    flush(&mut cur, &mut set);
    set.into_iter().collect()
}

/// File → (use-site node ids, referenced symbol names) of its pending refs.
type RefsByFile = HashMap<String, (Vec<String>, Vec<String>)>;

/// Per file, the `(use-site node ids, referenced names)` of its pending
/// references — calls/constructions the parser recorded as candidates for
/// cross-file (and cross-repo) resolution. Used for the .NET symbol-level path:
/// a C# `using` names only a namespace, so these supply the consumed-type
/// candidates and their use-sites, which the link step then filters to those
/// that match a referenced package's exports. De-duplicated per file.
fn unresolved_refs_by_file(store: &dyn GraphStore) -> Result<RefsByFile> {
    let node_file: HashMap<String, String> = store.node_files()?.into_iter().collect();
    let mut by_file: RefsByFile = HashMap::new();
    for p in store.all_pending()? {
        let Some(file) = node_file.get(&p.anchor.0) else {
            continue;
        };
        let entry = by_file.entry(file.clone()).or_default();
        entry.0.push(p.anchor.0.clone());
        entry.1.push(p.name.clone());
    }
    for (anchors, names) in by_file.values_mut() {
        anchors.sort();
        anchors.dedup();
        names.sort();
        names.dedup();
    }
    Ok(by_file)
}

/// Symbols in `file` whose source span references one of the import's imported
/// names — the precise consumer use-sites (#236). Empty when the import names no
/// specific symbol (a glob or bare-crate `use`) or the source can't be read, in
/// which case the caller falls back to file-level landing. Definitions are
/// matched by whole-identifier occurrence within their byte span; `Module`-like
/// spans are excluded so a module doesn't claim all its children.
fn use_site_nodes(
    store: &dyn GraphStore,
    root: &Path,
    file: &str,
    path: &str,
    sources: &mut HashMap<String, Option<Vec<u8>>>,
) -> Result<Vec<String>> {
    let names = glyphtrail_core::imported_symbols(path);
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = sources
        .entry(file.to_string())
        .or_insert_with(|| std::fs::read(root.join(file)).ok());
    let Some(bytes) = bytes.as_ref() else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    for node in store.nodes_in_file(file)? {
        if !is_use_site_kind(node.kind) {
            continue;
        }
        if let Some(span) = node.span
            && let Some(slice) = bytes.get(span.start_byte..span.end_byte)
        {
            let text = String::from_utf8_lossy(slice);
            if names.iter().any(|n| contains_ident(&text, n)) {
                out.push(node.id.0);
            }
        }
    }
    Ok(out)
}

/// Definition kinds that can reference an import in their span: callable bodies
/// and type definitions (a field can name an imported type). `Module` is
/// excluded — its span subsumes all its children, which would re-introduce the
/// file-level coarseness this avoids.
fn is_use_site_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Function
            | NodeKind::Method
            | NodeKind::Class
            | NodeKind::Struct
            | NodeKind::Interface
            | NodeKind::Enum
            | NodeKind::Trait
    )
}

/// Whether `needle` occurs in `haystack` as a whole identifier (not as a
/// substring of a larger one), so `go` doesn't match inside `goldfish`.
fn contains_ident(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let mut search_from = 0;
    while let Some(rel) = haystack[search_from..].find(needle) {
        let start = search_from + rel;
        let end = start + needle.len();
        let boundary_before = start == 0 || !is_ident_byte(bytes[start - 1]);
        let boundary_after = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if boundary_before && boundary_after {
            return true;
        }
        search_from = start + 1;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// Expand one workspace `members` entry (a path or path glob relative to `root`)
/// into the directories it matches. Each path component is matched
/// independently: a literal component must name an existing directory, while a
/// component containing a glob metacharacter (`*`, `?`, `[`) is matched against
/// the directory entries at that level. Covers the common `crates/*` and exact
/// path forms.
fn expand_member(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut current = vec![root.to_path_buf()];
    for comp in pattern.split('/').filter(|c| !c.is_empty()) {
        let mut next = Vec::new();
        let matcher = comp
            .contains(['*', '?', '['])
            .then(|| globset::Glob::new(comp).ok().map(|g| g.compile_matcher()))
            .flatten();
        for base in &current {
            match &matcher {
                Some(m) => {
                    let Ok(entries) = std::fs::read_dir(base) else {
                        continue;
                    };
                    for entry in entries.flatten() {
                        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                            && m.is_match(entry.file_name().to_string_lossy().as_ref())
                        {
                            next.push(entry.path());
                        }
                    }
                }
                None => {
                    let joined = base.join(comp);
                    if joined.is_dir() {
                        next.push(joined);
                    }
                }
            }
        }
        current = next;
    }
    current
}

/// The result of an analysis run, returned rather than printed so callers (the
/// CLI, the MCP server) decide how to surface it. `up_to_date` is true when the
/// #110 fast path found nothing changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyzeOutcome {
    pub up_to_date: bool,
    /// The repo was skipped because its path is excluded by the user-wide
    /// ignore file (#269); nothing was analyzed.
    #[serde(default)]
    pub ignored: bool,
    pub files: usize,
    pub nodes: usize,
    pub edges: usize,
    /// Indexed file counts per language, descending by count.
    pub languages: Vec<(String, usize)>,
}

/// Crate version, recorded in index meta (informational).
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Manual analysis-logic revision (#251): bump when a change to the Rust
/// extraction logic (matchers, graph building, schema/identity passes) could
/// change the analyzed graph without touching the crate version or the query
/// files. Changes to the tree-sitter query sources are picked up automatically
/// by [`analysis_revision`], so this is only for logic the queries don't encode.
/// 2: .NET/NuGet package identity (`.csproj` publishes + `using`/PackageReference
/// consumes), so existing indexes re-analyze to gain cross-repo .NET links.
/// 3: .NET symbol-level candidates from referenced PascalCase identifiers (type
/// uses, not just calls), so cross-repo .NET links resolve to symbols and chain.
/// 4: assembly call attribution — `jsr`/`jmp` in a line-oriented language now
/// attribute to the nearest preceding label, so existing `.S` indexes rebuild
/// to gain the routine-level callgraph (#368).
/// 5: JS/TS client extraction gained `inject(HttpClient)`, `HttpClient.request`,
/// and same-file const/concatenation URL folding (#404/#405), so existing
/// frontend indexes rebuild to surface the additional client calls.
/// 6: client-call URLs built from an *imported* constant base are resolved
/// cross-file at analyze time (#405), so existing frontend indexes rebuild to
/// fold those paths to concrete, matchable routes.
/// 7: constant resolution follows the Angular `environment` chain — a member
/// alias of an imported config object's string property (#405) — so those
/// frontend indexes rebuild to fold the common environment-base URL pattern.
/// 8: SQL DDL extraction (#416 Phase A) — `.sql` files yield `Table`/`Column`
/// nodes, so existing indexes rebuild to surface the database schema.
/// 9: code↔DB `Reads`/`Writes` from sqlx queries (#416 Phase B), so indexes
/// rebuild to link functions to the tables they query.
/// 10: JPA/Hibernate — `@Entity` tables + repository/`@Query` access linking
/// (#416 Phase B, Java).
/// 11: JPA `@Table`-less entities default to the snake_case naming strategy
/// (#432), so an entity merges with the `.sql`/native table of that name.
/// 12: JPA relationship fields (@ManyToOne/…) → table `References` edges (#433).
/// 13: Rust raw DB drivers (rusqlite/tokio-postgres) `conn.execute`/`query_row`/…
/// query extraction (#434).
/// 14: EntityManager/JDBC query calls (`createQuery`/`createNativeQuery`/
/// `prepareStatement`) extracted from Java method bodies (#434).
/// 15: embedded Cypher (kuzu DDL labels + MATCH/MERGE access) in Rust strings
/// linked to graph-label tables (#416 Phase C, #428).
/// 16: `format!`-built sqlx query templates are read for table accesses (#446).
/// 17: client URL extraction follows a whole-argument local variable binding
/// (`const url = …; http.get(url)`), so those calls are linked (#443).
/// 18: Cypher `.cypher`/`.cql` files, const-named queries, and pattern-only
/// (Neo4j) labels (#444), so graph-DB schema + access surface more fully.
/// 19: JS/TS (`pg`/`mysql2`/knex) and Python (DB-API/SQLAlchemy `text`) raw-driver
/// SQL queries are extracted, so those repos link functions to their tables (#440).
/// 20: Diesel ORM (Rust) — `table!` macros yield `Table`/`Column` nodes and the
/// query DSL (`<t>::table.load`, `insert_into`/`update`/`delete`) links code to
/// those tables (#440), so Diesel repos rebuild to gain the schema + access edges.
const ANALYSIS_REVISION: u32 = 20;

/// Fingerprint of everything that determines analysis output: the crate
/// version, the manual revision counter, and the built-in tree-sitter query
/// sources. Stored in index meta; a mismatch busts the no-op fast path so an
/// extractor change re-indexes an unchanged tree (#251).
fn analysis_revision() -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(VERSION.as_bytes());
    hasher.update(&ANALYSIS_REVISION.to_le_bytes());
    for lang in Language::ALL {
        if let Some(src) = glyphtrail_parse::registry::query_source(&lang) {
            hasher.update(src.as_bytes());
            hasher.update(&[0]);
        }
    }
    hasher.finalize().to_hex()[..16].to_string()
}

/// Path of the optional user-wide ignore file (#269): `~/.glyphtrailignore`,
/// the home-level counterpart of a repo's `.glyphtrailignore`. It serves two
/// roles: gitignore-format patterns apply to every repo's file walk, and a line
/// that is an absolute (or `~`) path excludes that whole repo/tree from
/// analysis (see [`excluded_trees`]) — handy when bulk-indexing a work tree but
/// skipping a few giant repos.
fn user_ignore_path() -> Option<PathBuf> {
    // `~/.glyphtrail/registry.json` -> `~/.glyphtrailignore`.
    glyphtrail_core::default_registry_path()
        .and_then(|p| p.parent().map(|d| d.with_file_name(".glyphtrailignore")))
}

/// Absolute directory paths listed in the user-wide ignore file, each excluding
/// itself and everything under it from analysis (#269). Lines are gitignore
/// patterns *except* those starting with `/` or `~`, which are taken as
/// filesystem paths. Best-effort: canonicalized when they exist.
fn excluded_trees(user_ignore: Option<&Path>) -> Vec<PathBuf> {
    let Some(text) = user_ignore.and_then(|p| std::fs::read_to_string(p).ok()) else {
        return Vec::new();
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let path = if let Some(rest) = l.strip_prefix("~/") {
                home.as_ref()?.join(rest)
            } else if l.starts_with('/') {
                PathBuf::from(l)
            } else {
                return None; // a relative gitignore pattern, not a tree exclusion
            };
            Some(path.canonicalize().unwrap_or(path))
        })
        .collect()
}

/// Whether `root` is excluded by the user-wide ignore: equal to, or nested
/// under, one of its path entries (#269).
fn is_excluded(root: &Path, trees: &[PathBuf]) -> bool {
    trees.iter().any(|t| root == t || root.starts_with(t))
}

/// Whether `path` is excluded by `~/.glyphtrailignore` (#269): it is, or is
/// under, a path listed there. Lets `repo scan` skip an excluded repo without
/// analyzing it. Canonicalizes `path` to match the canonicalized exclusions.
pub fn is_path_excluded(path: &Path) -> bool {
    let canonical = path.canonicalize();
    let root = canonical.as_deref().unwrap_or(path);
    is_excluded(root, &excluded_trees(user_ignore_path().as_deref()))
}

/// An edge's identity in the graph model: one edge per `(src, dst, kind)`.
type EdgeKey = (String, String, EdgeKind);
fn edge_key(e: &Edge) -> EdgeKey {
    (e.src.0.clone(), e.dst.0.clone(), e.kind)
}

/// Persist `nodes`, bulk-loading on a full rebuild. `MERGE (n:Node {id})` from
/// UNWIND can't use the id index in this engine (O(nodes²)); a fresh build
/// bulk-loads via COPY, which needs a primary-key-unique set, so de-duplicate by
/// id against `seen` (recording the ids). An update keeps MERGE.
fn persist_nodes<S: GraphStore + ?Sized>(
    store: &mut S,
    seen: &mut std::collections::HashSet<String>,
    nodes: &[Node],
    fresh: bool,
) -> Result<()> {
    if fresh {
        let new: Vec<Node> = nodes
            .iter()
            .filter(|n| seen.insert(n.id.0.clone()))
            .cloned()
            .collect();
        store.insert_nodes(&new, true)
    } else {
        store.insert_nodes(nodes, false)
    }
}

/// Persist `edges`, avoiding MERGE's per-edge existence scan on a full rebuild —
/// that scan grows with a node's degree and stalls on a high-degree hub in a
/// large repo (#282, extended to the resolve-phase inserts here). On a `fresh`
/// build the store holds exactly the keys in `seen` (seeded from the extracted
/// edges), so CREATE only the keys not yet seen and record them; an edge whose
/// key already exists is a no-op (MERGE wouldn't have changed it, since the
/// resolve passes never raise an existing edge's confidence). An incremental
/// update keeps MERGE (the in-memory `seen` can't see edges already on disk).
fn persist_edges<S: GraphStore + ?Sized>(
    store: &mut S,
    seen: &mut std::collections::HashSet<EdgeKey>,
    edges: &[Edge],
    fresh: bool,
) -> Result<()> {
    if fresh {
        let new: Vec<Edge> = edges
            .iter()
            .filter(|e| seen.insert(edge_key(e)))
            .cloned()
            .collect();
        store.insert_edges(&new, true)
    } else {
        store.insert_edges(edges, false)
    }
}

/// The repo's current `HEAD` commit, or `None` when `root` isn't a git
/// checkout, git is unavailable, or there are no commits yet. Best-effort: any
/// failure just disables the commit short-circuit (#273), falling back to the
/// content-hash fast path.
fn git_head_commit(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!head.is_empty()).then_some(head)
}

/// Whether the git working tree at `root` is clean (no modified, staged, or
/// untracked-non-ignored files), **disregarding our own `.glyphtrail/` index
/// dir** — analyze creates it inside the repo, so it shows as untracked unless
/// the repo gitignores it, and it must not count as a change. Only a clean tree
/// lets the commit short-circuit trust that `HEAD` describes what's on disk.
/// Non-git or any failure reports `false` (don't trust).
fn git_tree_clean(root: &Path) -> bool {
    let Some(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
    else {
        return false;
    };
    // Each porcelain line is `XY <path>`; the path starts at column 3. The tree
    // is clean if every reported entry is within the index dir we manage.
    String::from_utf8_lossy(&out.stdout).lines().all(|line| {
        let path = line.get(3..).unwrap_or("").trim_start_matches('"');
        path.is_empty() || path == INDEX_DIR || path.starts_with(&format!("{INDEX_DIR}/"))
    })
}

/// Whether an on-disk index still reflects the working tree (#313). Best-effort
/// and cheap: it never walks or hashes the tree, so it is safe to call on every
/// read. When it cannot tell, it says [`Unknown`](Staleness::Unknown) rather
/// than guessing, so a hint never fires spuriously.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staleness {
    /// The index appears current.
    Fresh,
    /// The index is known to lag the repo; carries a short reason.
    Stale(String),
    /// Indeterminate (e.g. indexed from a dirty or non-git tree).
    Unknown,
}

impl Staleness {
    /// Whether the index is known to be stale.
    pub fn is_stale(&self) -> bool {
        matches!(self, Staleness::Stale(_))
    }

    /// The reason, when stale.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Staleness::Stale(why) => Some(why),
            _ => None,
        }
    }

    /// A one-line advisory to show the user/agent, when stale.
    pub fn hint(&self) -> Option<String> {
        self.reason().map(|why| {
            format!("note: index may be stale ({why}); run `glyphtrail analyze` to refresh")
        })
    }
}

/// Cheaply assess whether the index at `store` still matches `root`, reusing the
/// same signals as analyze's fast paths (#273/#110/#251): the analysis revision
/// (so an analyzer upgrade flags every index as stale) and, for an index built
/// from a clean git checkout, the recorded HEAD plus working-tree cleanliness.
pub fn index_staleness(root: &Path, store: &dyn glyphtrail_store::GraphStore) -> Staleness {
    decide_staleness(
        store
            .get_meta("analysis_revision")
            .ok()
            .flatten()
            .as_deref(),
        &analysis_revision(),
        store.get_meta("head_commit").ok().flatten().as_deref(),
        || git_op_in_progress(root),
        || git_head_commit(root),
        || git_tree_clean(root),
    )
}

/// The name of a git operation in progress at `root` (merge / rebase /
/// cherry-pick / revert), if any — the working tree is then a transient mix that
/// any index predates (#448). Best-effort: non-git or any git failure yields
/// `None`.
fn git_op_in_progress(root: &Path) -> Option<&'static str> {
    for (refname, label) in [
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
    ] {
        if git_ref_exists(root, refname) {
            return Some(label);
        }
    }
    if git_path_exists(root, "rebase-merge") || git_path_exists(root, "rebase-apply") {
        return Some("rebase");
    }
    None
}

/// Whether a pseudo-ref like `MERGE_HEAD` resolves (the operation is in flight).
fn git_ref_exists(root: &Path, refname: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "-q", "--verify", refname])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Whether a git-dir path (e.g. `rebase-merge`) exists — used for in-progress
/// rebases, which leave a state directory rather than a pseudo-ref.
fn git_path_exists(root: &Path, name: &str) -> bool {
    let Some(out) = std::process::Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--git-path", name])
        .output()
        .ok()
        .filter(|o| o.status.success())
    else {
        return false;
    };
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() {
        return false;
    }
    let path = std::path::Path::new(&p);
    // `--git-path` prints relative to `root` (our cwd) unless already absolute.
    if path.is_absolute() {
        path.exists()
    } else {
        root.join(path).exists()
    }
}

/// The pure decision behind [`index_staleness`], with the store and git probed
/// out so it is deterministically testable. The git probes are lazy: they only
/// run once the cheaper revision/HEAD-metadata checks haven't already decided.
fn decide_staleness(
    stored_revision: Option<&str>,
    current_revision: &str,
    stored_head: Option<&str>,
    op_in_progress: impl FnOnce() -> Option<&'static str>,
    head_now: impl FnOnce() -> Option<String>,
    tree_clean: impl FnOnce() -> bool,
) -> Staleness {
    // The extraction logic changed since this index was built (e.g. a new edge
    // rule): the graph is stale regardless of git. Git-free and high-value.
    match stored_revision {
        Some(rev) if rev != current_revision => {
            return Staleness::Stale("analyzer updated since last index".into());
        }
        Some(_) => {}
        // Pre-revision index or an unreadable store: don't guess.
        None => return Staleness::Unknown,
    }
    // A git operation mid-flight (merge/rebase/cherry-pick/revert) leaves a
    // transient working tree the index can't reflect — flag it loudly even when
    // no clean HEAD was recorded at index time, since that's exactly when the
    // HEAD-comparison below can't help (#448).
    if let Some(op) = op_in_progress() {
        return Staleness::Stale(format!("{op} in progress; index predates it"));
    }
    // The git signal is only trustworthy when the index was built from a clean
    // checkout — the write path records the HEAD then, else an empty string.
    let stored_head = match stored_head {
        Some(h) if !h.is_empty() => h,
        _ => return Staleness::Unknown,
    };
    match head_now() {
        Some(head) if head != stored_head => Staleness::Stale("repo is on a new commit".into()),
        Some(_) if !tree_clean() => Staleness::Stale("uncommitted changes since last index".into()),
        Some(_) => Staleness::Fresh,
        // No longer a git checkout: can't apply the git signal.
        None => Staleness::Unknown,
    }
}

#[cfg(test)]
mod staleness_tests {
    use super::*;
    use assert2::check;

    // No git operation in progress in these cases; `|| None` is the op probe.
    #[test]
    fn revision_mismatch_is_stale_regardless_of_git() {
        let s = decide_staleness(
            Some("old"),
            "new",
            Some("abc"),
            || None,
            || Some("abc".into()),
            || true,
        );
        check!(s == Staleness::Stale("analyzer updated since last index".into()));
    }

    #[test]
    fn missing_revision_is_unknown() {
        let s = decide_staleness(
            None,
            "rev",
            Some("abc"),
            || None,
            || Some("abc".into()),
            || true,
        );
        check!(s == Staleness::Unknown);
    }

    #[test]
    fn empty_or_missing_head_is_unknown() {
        // Indexed from a dirty/non-git tree: the write path stored "".
        check!(
            decide_staleness(Some("r"), "r", Some(""), || None, || None, || true)
                == Staleness::Unknown
        );
        check!(
            decide_staleness(Some("r"), "r", None, || None, || None, || true) == Staleness::Unknown
        );
    }

    #[test]
    fn moved_head_is_stale() {
        let s = decide_staleness(
            Some("r"),
            "r",
            Some("aaa"),
            || None,
            || Some("bbb".into()),
            || true,
        );
        check!(s == Staleness::Stale("repo is on a new commit".into()));
    }

    #[test]
    fn same_head_dirty_tree_is_stale() {
        let s = decide_staleness(
            Some("r"),
            "r",
            Some("aaa"),
            || None,
            || Some("aaa".into()),
            || false,
        );
        check!(s == Staleness::Stale("uncommitted changes since last index".into()));
    }

    #[test]
    fn same_head_clean_tree_is_fresh() {
        let s = decide_staleness(
            Some("r"),
            "r",
            Some("aaa"),
            || None,
            || Some("aaa".into()),
            || true,
        );
        check!(s == Staleness::Fresh);
    }

    #[test]
    fn no_longer_git_is_unknown() {
        let s = decide_staleness(Some("r"), "r", Some("aaa"), || None, || None, || true);
        check!(s == Staleness::Unknown);
    }

    // #448: a merge in progress is loudly stale even when no clean HEAD was
    // recorded at index time (the case that previously fell through to Unknown).
    #[test]
    fn merge_in_progress_is_stale_even_without_recorded_head() {
        let s = decide_staleness(Some("r"), "r", Some(""), || Some("merge"), || None, || true);
        check!(s == Staleness::Stale("merge in progress; index predates it".into()));
    }

    // The op probe short-circuits before the HEAD comparison.
    #[test]
    fn rebase_in_progress_outranks_a_matching_head() {
        let s = decide_staleness(
            Some("r"),
            "r",
            Some("aaa"),
            || Some("rebase"),
            || Some("aaa".into()),
            || true,
        );
        check!(s == Staleness::Stale("rebase in progress; index predates it".into()));
    }

    // #364: analyze from a subdir resolves to the enclosing git root, and a stray
    // index left in the subdir must not win over it.
    #[test]
    fn repo_root_for_walks_up_to_the_git_checkout() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("gt-root-{nanos}"));
        let sub = root.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(sub.join(INDEX_DIR).join("ladybug")).unwrap();
        let got = repo_root_for(&sub.canonicalize().unwrap());
        check!(got == root.canonicalize().unwrap());
        std::fs::remove_dir_all(&root).ok();
    }
}

/// The repository root to index for a target inside it: the nearest enclosing
/// git checkout (`.git` — the canonical repo boundary), else an ancestor that
/// already holds an index, else the target itself. Git wins over a stray subdir
/// index so `analyze` from a subdir targets the real repo root (#364).
fn repo_root_for(path: &Path) -> PathBuf {
    if let Some(git) = path.ancestors().find(|p| p.join(".git").exists()) {
        return git.to_path_buf();
    }
    if let Some(indexed) = path
        .ancestors()
        .find(|p| p.join(INDEX_DIR).join("ladybug").exists())
    {
        return indexed.to_path_buf();
    }
    path.to_path_buf()
}

pub fn run(path: &Path, update: bool) -> Result<AnalyzeOutcome> {
    let target = path
        .canonicalize()
        .with_context(|| format!("cannot resolve path {}", path.display()))?;
    // Run from a subdirectory? Index the whole repository, not a second index in
    // the subdir (#364). Walk up to the enclosing git checkout (its `.git` is the
    // repo boundary; a submodule's own `.git` stops the walk), else an ancestor
    // that already holds an index.
    let root = repo_root_for(&target);
    if root != target {
        tracing::info!("indexing repository root {}", root.display());
    }

    // User-wide exclusions (#269): skip a repo (or any path under it) listed in
    // ~/.glyphtrailignore before touching it — so analyzing a subfolder of an
    // excluded giant repo does nothing rather than indexing it.
    if is_excluded(&root, &excluded_trees(user_ignore_path().as_deref())) {
        tracing::info!(
            "{} is excluded by ~/.glyphtrailignore; skipping",
            root.display()
        );
        return Ok(AnalyzeOutcome {
            up_to_date: false,
            ignored: true,
            files: 0,
            nodes: 0,
            edges: 0,
            languages: Vec::new(),
        });
    }

    let paths = RepoPaths::new(&root);
    paths.ensure_index_dir()?;
    let mut store = backend::open(&paths)?;

    // Commit short-circuit (#273): when the repo is a clean git checkout sitting
    // on the same HEAD it was last analyzed at — and the analysis logic is
    // unchanged — nothing can have changed, so skip even discovery. This is
    // cheaper than the content-hash fast path below (two `git` calls vs walking
    // and hashing every file), which matters when bulk-scanning many repos. A
    // dirty tree or non-git repo falls through to the content-hash path. The
    // stored `head_commit` is only trusted because it's written solely after a
    // clean-tree analysis (see the write path below).
    let revision = analysis_revision();
    let head_commit = git_head_commit(&root);
    let tree_clean = head_commit.is_some() && git_tree_clean(&root);
    if let Some(head) = &head_commit
        && tree_clean
        && store.get_meta("head_commit")?.as_deref() == Some(head.as_str())
        && store.get_meta("analysis_revision")?.as_deref() == Some(revision.as_str())
    {
        tracing::info!(
            "HEAD {} unchanged since last analysis; skipping",
            &head[..8.min(head.len())]
        );
        let stats = store.stats()?;
        return Ok(AnalyzeOutcome {
            up_to_date: true,
            ignored: false,
            files: stats.files,
            nodes: stats.nodes,
            edges: stats.edges,
            languages: stats.languages,
        });
    }

    let cfg = Config::load(&root)?;
    let files = discover(
        &root,
        &cfg.languages,
        &cfg.ignore_dirs,
        cfg.security.record_sensitive_files,
        user_ignore_path().as_deref(),
    )?;
    tracing::info!("discovered {} source files", files.len());

    // Cargo package identity (#220): the crate(s) this repo publishes and the
    // dependencies it declares, persisted so the cross-repo link step (#221) can
    // match a consumer's dependency to the producer repo whose package name
    // equals it. A fingerprint over the discovered identity (name/version/deps +
    // directory) folds into the fast path below, so editing a `Cargo.toml` alone
    // (no source change) still refreshes. The per-package export index is
    // resolved from the built graph and persisted later in the write path.
    let discovered = discover_packages(&root);
    let packages_fingerprint = blake3::hash(
        serde_json::to_string(&discovered)
            .unwrap_or_default()
            .as_bytes(),
    )
    .to_hex()
    .to_string();

    // Fast path (#110): when every discovered file matches the stored
    // (path, hash) set, the package identity is unchanged, and the index was
    // produced by the same analysis revision, nothing has changed — skip
    // parsing, re-resolution and writes entirely. The analysis revision (#251)
    // captures the crate version, the query sources, and a manual revision
    // counter, so a change to the *extraction logic* — not just a release —
    // forces a rebuild even on an unchanged tree.
    let current_set: std::collections::BTreeSet<(String, String)> = files
        .iter()
        .map(|f| (f.rel_path.clone(), f.hash.clone()))
        .collect();
    let stored_set: std::collections::BTreeSet<(String, String)> =
        store.files_with_hashes()?.into_iter().collect();
    if !files.is_empty()
        && current_set == stored_set
        && store.get_meta("packages_fingerprint")?.as_deref() == Some(packages_fingerprint.as_str())
        && store.get_meta("analysis_revision")?.as_deref() == Some(revision.as_str())
    {
        let stats = store.stats()?;
        return Ok(AnalyzeOutcome {
            up_to_date: true,
            ignored: false,
            files: stats.files,
            nodes: stats.nodes,
            edges: stats.edges,
            languages: stats.languages,
        });
    }

    // Lazily-loaded dynamic grammars, keyed by language name. `None` records a
    // load failure so we warn once and skip that language's files thereafter.
    let mut dyn_grammars: HashMap<String, Option<glyphtrail_parse::DynamicGrammar>> =
        HashMap::new();

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
        signature: None,
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
    // (file, name, value) module-scope string constants for client-URL folding (#405).
    let mut string_consts: Vec<(String, String, String)> = Vec::new();
    // (file, name, referenced key) constant references for the same.
    let mut const_refs: Vec<(String, String, String)> = Vec::new();
    // (enclosing fn id, access, table name) embedded-query accesses (#416 B),
    // resolved to Table nodes by name after the parse.
    let mut db_accesses: Vec<(NodeId, glyphtrail_parse::DbAccess, String)> = Vec::new();
    // (entity name, table name) JPA mappings, so an entity ref resolves to its table.
    let mut entity_tables: Vec<(String, String)> = Vec::new();
    // (owning table id, related entity/table name) JPA relationship FKs (#433).
    let mut db_references: Vec<(NodeId, String)> = Vec::new();

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
    // input order, so node insertion below stays deterministic. Run on a pool
    // with a large worker stack: extraction recurses over the AST, and a deeply
    // nested file (generated code, minified blobs) overflows the default ~2MB
    // worker stack — observed as a "stack overflow" abort on a huge repo.
    let outputs: Vec<FileOutput> = parse_pool().install(|| {
        changed
            .par_iter()
            .filter_map(|f| {
                parse_progress.set_message(f.rel_path.clone());
                let out = parse_file(f, &repo_id, &dyn_grammars);
                parse_progress.inc(1);
                out
            })
            .collect()
    });
    parse_progress.finish_and_clear();

    // Merge the fragments into the global accumulators, in file order.
    for out in outputs {
        graph.extend(out.graph);
        operations.extend(out.operations);
        pending_handlers.extend(out.pending_handlers);
        pending.extend(out.pending);
        imports.extend(out.imports);
        import_symbols.extend(out.import_symbols);
        string_consts.extend(out.string_consts);
        const_refs.extend(out.const_refs);
        db_accesses.extend(out.db_accesses);
        entity_tables.extend(out.entity_tables);
        db_references.extend(out.db_references);
    }

    // Resolve embedded-query accesses (#416 Phase B): match each query's table
    // name to the `Table` node(s) parsed this pass and add a `Reads`/`Writes`
    // edge from the enclosing function. Matching is by normalized name, qualified
    // or bare (`users` matches `public.users`). A reference with no matching table
    // in this pass is dropped (best-effort; cross-pass resolution is a follow-up).
    let mut db_edges: Vec<Edge> = Vec::new();
    if !db_accesses.is_empty() || !db_references.is_empty() {
        // A JPA repository/JPQL access names an entity; map it to that entity's
        // table before matching (a native query / sqlx already names the table).
        // Two entities with the same simple name (different packages) mapping to
        // different tables is ambiguous — drop the mapping so we don't link to the
        // wrong one (the access then falls back to a literal table-name match).
        let mut entity_to_table: HashMap<String, Option<String>> = HashMap::new();
        for (entity, table) in entity_tables {
            entity_to_table
                .entry(entity)
                .and_modify(|v| {
                    if v.as_deref() != Some(table.as_str()) {
                        *v = None;
                    }
                })
                .or_insert(Some(table));
        }
        let mut tables_by_name: HashMap<String, Vec<NodeId>> = HashMap::new();
        // Owning-table name per node id, so a self-relation is recognised by name
        // even across file-scoped duplicate table nodes (JPA + .sql), #433 review.
        let mut table_name_by_id: HashMap<NodeId, String> = HashMap::new();
        for n in &graph.nodes {
            if n.kind == NodeKind::Table {
                table_name_by_id.insert(n.id.clone(), n.qualified_name.clone());
                tables_by_name
                    .entry(n.qualified_name.clone())
                    .or_default()
                    .push(n.id.clone());
                if let Some(bare) = n.qualified_name.rsplit('.').next()
                    && bare != n.qualified_name
                {
                    tables_by_name
                        .entry(bare.to_string())
                        .or_default()
                        .push(n.id.clone());
                }
            }
        }
        // On an incremental update, also seed from tables already persisted whose
        // `.sql`/entity file wasn't re-parsed this pass, so edges still resolve to
        // them (#435). A full build cleared the store, so this only runs on update.
        // Ids already added above (a table re-parsed this pass) are skipped.
        if update {
            for (id, qname) in store.tables_by_name()? {
                if table_name_by_id.contains_key(&id) {
                    continue;
                }
                table_name_by_id.insert(id.clone(), qname.clone());
                tables_by_name
                    .entry(qname.clone())
                    .or_default()
                    .push(id.clone());
                if let Some(bare) = qname.rsplit('.').next()
                    && bare != qname
                {
                    tables_by_name
                        .entry(bare.to_string())
                        .or_default()
                        .push(id.clone());
                }
            }
        }
        for (fn_id, access, ref_name) in &db_accesses {
            // Map an entity ref to its table (unique mappings only), else use the
            // name as-is (a native/sqlx table, or an ambiguous entity name).
            let table = match entity_to_table.get(ref_name) {
                Some(Some(t)) => t,
                _ => ref_name,
            };
            let targets = tables_by_name
                .get(table)
                .or_else(|| table.rsplit('.').next().and_then(|b| tables_by_name.get(b)));
            if let Some(ids) = targets {
                let kind = match access {
                    glyphtrail_parse::DbAccess::Read => EdgeKind::Reads,
                    glyphtrail_parse::DbAccess::Write => EdgeKind::Writes,
                };
                for tid in ids {
                    db_edges.push(Edge {
                        src: fn_id.clone(),
                        dst: tid.clone(),
                        kind,
                        confidence: Confidence::Inferred,
                    });
                }
            }
        }
        // JPA relationship FKs (#433): the owning entity's table references the
        // related entity's table (mapped from its entity name like an access).
        for (src_table, ref_name) in &db_references {
            let table = match entity_to_table.get(ref_name) {
                Some(Some(t)) => t,
                _ => ref_name,
            };
            // A self-relation (target resolves to the owning table's own name) is
            // not a cross-table reference, even across duplicate table nodes.
            if table_name_by_id.get(src_table).map(String::as_str) == Some(table.as_str()) {
                continue;
            }
            let targets = tables_by_name
                .get(table)
                .or_else(|| table.rsplit('.').next().and_then(|b| tables_by_name.get(b)));
            if let Some(ids) = targets {
                for tid in ids {
                    if tid != src_table {
                        db_edges.push(Edge {
                            src: src_table.clone(),
                            dst: tid.clone(),
                            kind: EdgeKind::References,
                            confidence: Confidence::Inferred,
                        });
                    }
                }
            }
        }
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

    // Persist nodes and high-confidence (extracted) edges first. Dedup edges on
    // (src, dst, kind) — the graph model is one edge per such triple — so a full
    // rebuild can bulk-load them directly (see below) without MERGE's per-edge
    // existence check.
    let mut seen = std::collections::HashSet::new();
    let extracted: Vec<Edge> = graph
        .edges
        .iter()
        .filter(|e| e.confidence == Confidence::Extracted)
        .filter(|e| seen.insert((e.src.0.clone(), e.dst.0.clone(), e.kind)))
        .cloned()
        .collect();
    stage("persisting nodes and edges");
    // Persist in batches so a large repo shows steady progress (and bounded
    // memory) instead of one opaque, seemingly-hung call. All nodes go in first;
    // edges then match the already-inserted endpoints. Empty slices are no-ops.
    // A full (non-update) build starts from a cleared store, so nodes and edges
    // bulk-load (COPY) instead of MERGE — MERGE from UNWIND can't use the id
    // index in this engine, so it scans per row and stalls a large repo.
    const PERSIST_BATCH: usize = 4096;
    let fresh = !update;
    // Ids already persisted, so resolve-phase nodes (e.g. module placeholders)
    // can be de-duplicated against them before their own bulk load.
    let mut node_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let (n_nodes, n_edges) = (graph.nodes.len(), extracted.len());
    let mut done = 0;
    for chunk in graph.nodes.chunks(PERSIST_BATCH) {
        persist_nodes(&mut *store, &mut node_seen, chunk, fresh)?;
        done += chunk.len();
        resolve_progress.set_message(format!("persisting nodes {done}/{n_nodes}"));
    }
    done = 0;
    for chunk in extracted.chunks(PERSIST_BATCH) {
        store.insert_edges(chunk, fresh)?;
        done += chunk.len();
        resolve_progress.set_message(format!("persisting edges {done}/{n_edges}"));
    }
    resolve_progress.inc(1);

    stage("recording API operations and imports");
    store.insert_operations(&operations)?;
    store.insert_imports(&imports)?;
    resolve_progress.inc(1);

    // Receiver/scope qualifier per pending call (#5), kept in memory for this
    // run's resolution below. Keyed like the persisted pending link
    // `(anchor, name, kind)`; the Pending node has no scope column, so on
    // `--update` unchanged-file pending simply fall back to the other tiers.
    let scope_map: HashMap<(String, String, String), String> = pending
        .iter()
        .filter_map(|p| {
            p.scope.clone().map(|sc| {
                (
                    (p.src.0.clone(), p.name.clone(), p.kind.as_str().to_string()),
                    sc,
                )
            })
        })
        .collect();

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

    // Resolve imported string constants in client-call URLs (#405): a path like
    // `${API_BASE}/x`, whose `API_BASE` is imported from a constants module,
    // folds to the concrete value and re-normalizes to a path that can match a
    // route. Same-file constants were already folded at parse time (#404).
    stage("resolving client URL constants");
    let strings_by_file = group_consts(&string_consts);
    let refs_by_file = group_consts(&const_refs);
    // Last-resort lookup for an object property (`OBJ.PROP`) whose own module is
    // not indexed — e.g. a gitignored Angular `environment.ts`, where the
    // committed `environment.prod.ts` carries the same object. Only when the
    // value is unambiguous across files: a key defined with conflicting values
    // (a dev vs prod env with different paths) stays unresolved rather than risk
    // folding to the wrong route. `None` marks a key seen with differing values.
    let mut prop_values: HashMap<&str, Option<&str>> = HashMap::new();
    for (_file, key, value) in &string_consts {
        if key.contains('.') {
            prop_values
                .entry(key.as_str())
                .and_modify(|e| {
                    if *e != Some(value.as_str()) {
                        *e = None;
                    }
                })
                .or_insert(Some(value.as_str()));
        }
    }
    let global_props: HashMap<&str, &str> = prop_values
        .into_iter()
        .filter_map(|(k, v)| Some((k, v?)))
        .collect();
    let mut rewritten: Vec<(NodeId, OperationKey)> = Vec::new();
    for (id, key) in store.operations_by_kind(NodeKind::ClientCall)? {
        if key.protocol != Protocol::Rest || !key.path.contains("${") {
            continue;
        }
        let Some(method) = key.method else { continue };
        let Some(file) = store.get_node(&id.0)?.map(|n| n.file) else {
            continue;
        };
        let resolved = resolve_path_constants(
            &key.path,
            &file,
            &symbol_file,
            &strings_by_file,
            &refs_by_file,
            &global_props,
        );
        if resolved != key.path {
            rewritten.push((id, OperationKey::rest(method, &resolved)));
        }
    }
    if !rewritten.is_empty() {
        store.insert_operations(&rewritten)?;
    }
    resolve_progress.inc(1);

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
    // A uniquely-named target resolves directly; an ambiguous name resolves by,
    // in order: a matching receiver/scope qualifier (#5), exactly one candidate
    // in a file the anchor imports (#19), or exactly one in the anchor's own
    // directory (same-package locality, #5).
    stage("inferring cross-file edges");
    store.delete_edges_by_confidence(Confidence::Inferred)?;
    let mut index: HashMap<String, Vec<NodeId>> = HashMap::new();
    for (name, id) in store.definition_index()? {
        index.entry(name).or_default().push(id);
    }
    let node_file: HashMap<String, String> = store.node_files()?.into_iter().collect();
    let node_qualified: HashMap<String, String> =
        store.node_qualified_names()?.into_iter().collect();
    let inferred: Vec<Edge> = store
        .all_pending()?
        .into_iter()
        .filter_map(|l| {
            let candidates = index.get(&l.name)?;
            let other = match candidates.as_slice() {
                [one] => one.clone(),
                _ => scope_map
                    .get(&(
                        l.anchor.0.clone(),
                        l.name.clone(),
                        l.kind.as_str().to_string(),
                    ))
                    .and_then(|q| disambiguate_qualifier(candidates, q, &l.name, &node_qualified))
                    .or_else(|| {
                        disambiguate_symbol_import(
                            &l.anchor,
                            &l.name,
                            candidates,
                            &node_file,
                            &symbol_file,
                        )
                    })
                    .or_else(|| disambiguate_import(&l.anchor, candidates, &node_file, &import_map))
                    .or_else(|| disambiguate_dir(&l.anchor, candidates, &node_file))?,
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
    persist_edges(&mut *store, &mut seen, &inferred, fresh)?;
    // Code↔DB Reads/Writes edges (#416 Phase B), resolved above to Table nodes.
    persist_edges(&mut *store, &mut seen, &db_edges, fresh)?;
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
                    signature: None,
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
    persist_nodes(&mut *store, &mut node_seen, &import_nodes, fresh)?;
    persist_edges(&mut *store, &mut seen, &import_edges, fresh)?;
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
    persist_edges(&mut *store, &mut seen, &edges, fresh)?;
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
    persist_edges(&mut *store, &mut seen, &router_edges, fresh)?;
    resolve_progress.inc(1);

    // Stamp the per-file hashes for the changed set in one batch so the next
    // `--update` run can skip them. One bulk write beats one fresh connection +
    // commit per file (the per-file loop was the phase's second-slowest stage).
    stage(&format!("recording {} file records", changed.len()));
    let file_records: Vec<(String, Option<String>, String)> = changed
        .iter()
        .map(|f| {
            // A `.sql` file carries no `Language` but is still tagged "sql" so the
            // language tally reports it rather than "(unknown)" (#416).
            let lang = match (&f.language, f.sql) {
                (Some(l), _) => Some(l.name().to_string()),
                (None, true) => Some("sql".to_string()),
                (None, false) => None,
            };
            (f.rel_path.clone(), lang, f.hash.clone())
        })
        .collect();
    store.set_files(&file_records)?;
    let pruned = store.prune_dangling_edges()?;
    if pruned > 0 {
        tracing::info!("pruned {pruned} dangling edges");
    }
    resolve_progress.inc(1);

    store.set_meta("tool_version", VERSION)?;
    store.set_meta("analysis_revision", &revision)?;
    // Record HEAD for the commit short-circuit (#273) only when the tree was
    // clean, so a later run at this commit can trust the index matches it. If
    // the tree was dirty (or non-git), store an empty sentinel so the
    // short-circuit can't fire against this dirty-or-unknown state.
    match (&head_commit, tree_clean) {
        (Some(head), true) => store.set_meta("head_commit", head)?,
        _ => store.set_meta("head_commit", "")?,
    }
    // Resolve the export index from the now-complete graph and persist the full
    // package identity (#220). Read-only borrow ends before the meta write.
    let indexed = index_packages(&*store, &discovered)?;
    let packages_json = serde_json::to_string(&indexed).unwrap_or_else(|_| "[]".to_string());
    // Consumer side of cross-repo links (#220): imports referencing a declared
    // dependency. Resolved from the same identity, persisted for #221.
    let uses = external_uses(&*store, &discovered, &root)?;
    let uses_json = serde_json::to_string(&uses).unwrap_or_else(|_| "[]".to_string());
    store.set_meta(META_PACKAGES, &packages_json)?;
    store.set_meta(META_EXTERNAL_USES, &uses_json)?;
    store.set_meta("packages_fingerprint", &packages_fingerprint)?;

    // Pingback (#292): cache this repo's cross-repo identity on its registry
    // entry so federated impact resolves links from the loaded registry and opens
    // only link-connected member stores, instead of opening every member just to
    // read its identity. Best-effort: a busy registry lock or an unregistered
    // repo must never fail an analyze — federation backfills anything missed.
    if let Some(reg_path) = glyphtrail_core::default_registry_path() {
        let identity = PackageIdentity {
            packages: indexed,
            external_uses: uses,
        };
        let _ = Registry::mutate(&reg_path, |r| r.set_identity_by_root(&root, identity));
    }
    resolve_progress.finish_and_clear();

    let stats = store.stats()?;
    Ok(AnalyzeOutcome {
        up_to_date: false,
        ignored: false,
        files: stats.files,
        nodes: stats.nodes,
        edges: stats.edges,
        languages: stats.languages,
    })
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

/// Index `(file, key, value)` tuples as `file -> {key -> value}` for constant
/// resolution.
fn group_consts(rows: &[(String, String, String)]) -> HashMap<&str, HashMap<&str, &str>> {
    let mut by_file: HashMap<&str, HashMap<&str, &str>> = HashMap::new();
    for (file, key, value) in rows {
        by_file
            .entry(file.as_str())
            .or_default()
            .insert(key.as_str(), value.as_str());
    }
    by_file
}

/// Substitute resolvable `${NAME}` constants in a client URL path (#405).
/// Unknown / non-identifier interpolations are left verbatim so they collapse to
/// a dynamic segment as before.
fn resolve_path_constants(
    path: &str,
    file: &str,
    symbol_file: &HashMap<(String, String), String>,
    strings: &HashMap<&str, HashMap<&str, &str>>,
    refs: &HashMap<&str, HashMap<&str, &str>>,
    global_props: &HashMap<&str, &str>,
) -> String {
    let mut out = String::new();
    let mut rest = path;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = matching_brace(after) else {
            out.push_str(&rest[start..]); // unterminated `${` — keep verbatim
            return out;
        };
        let name = &after[..end];
        let value = is_ident(name)
            .then(|| resolve_const(name, file, symbol_file, strings, refs, global_props, 0))
            .flatten();
        match value {
            Some(v) => out.push_str(&v),
            None => {
                out.push_str("${");
                out.push_str(name);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Resolve a constant `key` (a bare `NAME` or an `OBJ.PROP`) seen in `file` to a
/// string literal, following same-file references, imported constants, and the
/// Angular `environment` chain (`const X = environment.X` → an imported config
/// object's string property), with a depth cap (#405).
fn resolve_const(
    key: &str,
    file: &str,
    symbol_file: &HashMap<(String, String), String>,
    strings: &HashMap<&str, HashMap<&str, &str>>,
    refs: &HashMap<&str, HashMap<&str, &str>>,
    global_props: &HashMap<&str, &str>,
    depth: usize,
) -> Option<String> {
    if depth > 8 {
        return None;
    }
    // A literal here.
    if let Some(v) = strings.get(file).and_then(|m| m.get(key)) {
        return Some((*v).to_string());
    }
    // A reference here (`const NAME = OTHER` / `OBJ.PROP -> other key`).
    if let Some(target) = refs.get(file).and_then(|m| m.get(key)) {
        return resolve_const(
            target,
            file,
            symbol_file,
            strings,
            refs,
            global_props,
            depth + 1,
        );
    }
    // The base name imported into this file — resolve in its defining file. For a
    // bare name that's the name itself; for `OBJ.PROP` it's the object `OBJ`
    // (imported `{ environment }`), resolving the same `OBJ.PROP` key there.
    let base = key.split('.').next().unwrap_or(key);
    if let Some(def) = symbol_file.get(&(file.to_string(), base.to_string())) {
        return resolve_const(
            key,
            def,
            symbol_file,
            strings,
            refs,
            global_props,
            depth + 1,
        );
    }
    // Last resort: an object property whose own module isn't indexed (a
    // gitignored config object), matched globally by `OBJ.PROP`.
    if key.contains('.') {
        return global_props.get(key).map(|v| (*v).to_string());
    }
    None
}

/// Whether `s` is a single JS identifier (so `${expr}` with a member access or
/// call is left alone, only a bare constant name is substituted).
fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Byte index of the `}` that closes a `${`, balancing nested braces (so
/// `${foo({a: 1})}` is treated as one interpolation, not split at the inner `}`).
fn matching_brace(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' if depth == 0 => return Some(i),
            '}' => depth -= 1,
            _ => {}
        }
    }
    None
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

/// Resolve an unqualified call to a *symbol* imported by name — the file the
/// anchor imported `name` from (e.g. a Java `import static pkg.Class.method;`,
/// or a JS/Python named import). More precise than [`disambiguate_import`],
/// which only knows the anchor imports the file, not which symbol: it picks the
/// unique candidate in the exact file `name` was imported from (#395). `None`
/// when the symbol wasn't imported, or zero/many candidates live in that file.
fn disambiguate_symbol_import(
    anchor: &NodeId,
    name: &str,
    candidates: &[NodeId],
    node_file: &HashMap<String, String>,
    symbol_file: &HashMap<(String, String), String>,
) -> Option<NodeId> {
    let anchor_file = node_file.get(&anchor.0)?;
    let target = symbol_file.get(&(anchor_file.clone(), name.to_string()))?;
    let mut hit: Option<&NodeId> = None;
    for c in candidates {
        if node_file.get(&c.0) == Some(target) {
            if hit.is_some() {
                return None;
            }
            hit = Some(c);
        }
    }
    hit.cloned()
}

/// Resolve an ambiguous name to the unique candidate defined in the anchor's own
/// directory (same-package locality, #5), used when import-based disambiguation
/// found nothing — common for languages where same-package symbols are visible
/// without an explicit import (Java/Go/C#, Python relative packages). Returns a
/// target only when exactly one candidate lives in that directory, so it never
/// invents an edge between two equally-plausible same-named siblings.
fn disambiguate_dir(
    anchor: &NodeId,
    candidates: &[NodeId],
    node_file: &HashMap<String, String>,
) -> Option<NodeId> {
    let anchor_dir = dir_of(node_file.get(&anchor.0)?);
    let mut hit: Option<&NodeId> = None;
    for c in candidates {
        if let Some(f) = node_file.get(&c.0)
            && dir_of(f) == anchor_dir
        {
            if hit.is_some() {
                return None; // two candidates in the same directory — still ambiguous
            }
            hit = Some(c);
        }
    }
    hit.cloned()
}

/// Resolve an ambiguous name to the unique candidate whose immediate enclosing
/// scope equals the call's receiver `qualifier` (#5): a call `Foo::bar()` /
/// `Foo.bar()` prefers the `bar` defined in `Foo` over same-named `bar`s
/// elsewhere. Qualified names are `Scope::…::name`, so a match means the
/// candidate ends in `qualifier::name` (the segment before `name` is the
/// qualifier). Conservative: returns a target only on a unique match.
fn disambiguate_qualifier(
    candidates: &[NodeId],
    qualifier: &str,
    name: &str,
    node_qualified: &HashMap<String, String>,
) -> Option<NodeId> {
    let tail = format!("{qualifier}::{name}");
    let nested = format!("::{tail}");
    let mut hit: Option<&NodeId> = None;
    for c in candidates {
        if let Some(q) = node_qualified.get(&c.0)
            && (*q == tail || q.ends_with(&nested))
        {
            if hit.is_some() {
                return None; // two candidates share the qualifier — still ambiguous
            }
            hit = Some(c);
        }
    }
    hit.cloned()
}

/// The directory portion of a repo-relative path (`""` for a top-level file).
fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
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
                signature: None,
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

    // #405: imported (and same-file) string constants fold into a client URL,
    // and the result re-normalizes to a concrete, matchable path.
    #[test]
    fn resolve_path_constants_folds_imported_and_same_file() {
        let symbol_file = HashMap::from([(
            ("svc.ts".to_string(), "API_BASE".to_string()),
            "config.ts".to_string(),
        )]);
        let strings = HashMap::from([
            ("config.ts", HashMap::from([("API_BASE", "https://h/api")])),
            ("svc.ts", HashMap::from([("LOCAL", "/local")])),
        ]);
        let refs = HashMap::new();
        let global = HashMap::new();
        let r =
            |p: &str| resolve_path_constants(p, "svc.ts", &symbol_file, &strings, &refs, &global);
        // Imported base folds; same-file const folds.
        check!(r("/${API_BASE}/users") == "/https://h/api/users");
        check!(r("${LOCAL}/x") == "/local/x");
        // Unknown name and non-identifier interpolations stay verbatim.
        check!(r("/${UNKNOWN}/x") == "/${UNKNOWN}/x");
        check!(r("/${this.base}/x") == "/${this.base}/x");
        // A nested-brace interpolation is balanced, not split at the inner `}`.
        check!(r("/${foo({a:1})}/x") == "/${foo({a:1})}/x");
        // The folded full-URL value re-normalizes to a concrete route signature.
        let get = glyphtrail_core::HttpMethod::Get;
        let key = OperationKey::rest(get, &r("/${API_BASE}/users/${id}"));
        check!(key.path == "/api/users/${id}");
        check!(OperationKey::rest(get, "/api/users/{id}").signature() == key.signature());
    }

    // #405: the Angular `environment` chain — `const API_URL = environment.API_URL`
    // aliasing an imported config object's string property — folds, including a
    // property that itself names a constant imported from a third file.
    #[test]
    fn resolve_path_constants_follows_environment_chain() {
        let symbol_file = HashMap::from([
            (
                ("svc.ts".to_string(), "environment".to_string()),
                "env.ts".to_string(),
            ),
            (
                ("env.ts".to_string(), "MEDIAN_LOGIN".to_string()),
                "model.ts".to_string(),
            ),
        ]);
        let strings = HashMap::from([
            (
                "env.ts",
                HashMap::from([("environment.API_URL", "https://h")]),
            ),
            ("model.ts", HashMap::from([("MEDIAN_LOGIN", "/signin")])),
        ]);
        let refs = HashMap::from([
            (
                "svc.ts",
                HashMap::from([
                    // `const API_URL = environment.API_URL`
                    ("API_URL", "environment.API_URL"),
                    // `const LOGIN = environment.MEDIAN_LOGIN`
                    ("LOGIN", "environment.MEDIAN_LOGIN"),
                ]),
            ),
            // env.ts object prop `MEDIAN_LOGIN: MEDIAN_LOGIN` (names the import).
            (
                "env.ts",
                HashMap::from([("environment.MEDIAN_LOGIN", "MEDIAN_LOGIN")]),
            ),
        ]);
        let global = HashMap::new();
        let r =
            |p: &str| resolve_path_constants(p, "svc.ts", &symbol_file, &strings, &refs, &global);
        // alias -> object property literal; and a property that itself names a
        // constant imported from a third file (env.ts -> model.ts).
        check!(r("/${API_URL}${LOGIN}") == "/https://h/signin");
        let key = OperationKey::rest(glyphtrail_core::HttpMethod::Get, &r("/${API_URL}${LOGIN}"));
        check!(key.path == "/signin"); // scheme/host normalized away
    }

    // #405: when an object's own module isn't indexed (a gitignored Angular
    // `environment.ts`), an `OBJ.PROP` resolves via the global fallback — e.g.
    // the committed `environment.prod.ts` carrying the same paths.
    #[test]
    fn resolve_path_constants_global_fallback_for_unindexed_object() {
        let symbol_file = HashMap::new(); // the `environment` import did not resolve
        let strings: HashMap<&str, HashMap<&str, &str>> = HashMap::new();
        let refs = HashMap::from([(
            "svc.ts",
            HashMap::from([("API_URL", "environment.API_URL")]),
        )]);
        let global = HashMap::from([("environment.API_URL", "https://h/v2/")]);
        let r =
            |p: &str| resolve_path_constants(p, "svc.ts", &symbol_file, &strings, &refs, &global);
        let key = OperationKey::rest(glyphtrail_core::HttpMethod::Get, &r("/${API_URL}signin"));
        check!(key.path == "/v2/signin");
        // When the global map has no unambiguous value, the base stays verbatim.
        let empty = HashMap::new();
        let r2 =
            |p: &str| resolve_path_constants(p, "svc.ts", &symbol_file, &strings, &refs, &empty);
        check!(r2("/${API_URL}signin") == "/${API_URL}signin");
    }

    fn temp_repo(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("glyphtrail-it-{tag}-{nanos}"));
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
        let found = discover(&dir, &[], &["generated".to_string()], false, None).unwrap();
        let rels: Vec<&str> = found.iter().map(|f| f.rel_path.as_str()).collect();
        check!(rels == ["main.rs"], "expected only main.rs, got {rels:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    // #416: a `.sql` file is discovered as a SQL artifact (no `Language`, the
    // `sql` flag set) so the DDL extractor handles it; a `.rs` file does not.
    #[test]
    fn discovery_flags_sql_files() {
        let dir = temp_repo("sql-discovery");
        std::fs::write(dir.join("schema.sql"), "CREATE TABLE t (id int);\n").unwrap();
        std::fs::write(dir.join("main.rs"), "fn f() {}\n").unwrap();
        let found = discover(&dir, &[], &[], false, None).unwrap();
        let sql = found.iter().find(|f| f.rel_path == "schema.sql").unwrap();
        check!(sql.sql);
        check!(sql.language.is_none());
        let rs = found.iter().find(|f| f.rel_path == "main.rs").unwrap();
        check!(!rs.sql);
        std::fs::remove_dir_all(&dir).ok();
    }

    // #444: a `.cypher` file is discovered as a Cypher artifact (no `Language`, the
    // `cypher` flag set) so the Cypher extractor handles it.
    #[test]
    fn discovery_flags_cypher_files() {
        let dir = temp_repo("cypher-discovery");
        std::fs::write(dir.join("links.cypher"), "MATCH (n:Node) RETURN n\n").unwrap();
        std::fs::write(dir.join("schema.cql"), "MERGE (a:Account)\n").unwrap();
        let found = discover(&dir, &[], &[], false, None).unwrap();
        for rel in ["links.cypher", "schema.cql"] {
            let f = found.iter().find(|f| f.rel_path == rel).unwrap();
            check!(f.cypher);
            check!(f.language.is_none());
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // #269: a user-wide ignore file (gitignore-format) excludes matching files
    // across any repo.
    #[test]
    fn discovery_honors_a_user_wide_ignore_file() {
        let dir = temp_repo("user-ignore");
        std::fs::write(dir.join("main.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join("scratch.rs"), "fn b() {}\n").unwrap();
        let user_ignore = dir.join("user-ignore-patterns");
        std::fs::write(&user_ignore, "scratch.rs\n").unwrap();

        let found = discover(&dir, &[], &[], false, Some(&user_ignore)).unwrap();
        let rels: Vec<&str> = found.iter().map(|f| f.rel_path.as_str()).collect();
        check!(rels == ["main.rs"], "expected only main.rs, got {rels:?}");

        // Without it, both files are discovered.
        let found = discover(&dir, &[], &[], false, None).unwrap();
        check!(found.len() == 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    // #269: an absolute-path line excludes that tree (and everything under it);
    // gitignore patterns and comments are not tree exclusions.
    #[test]
    fn excluded_trees_takes_only_path_lines() {
        let dir = temp_repo("excl-trees");
        let f = dir.join("ignore");
        std::fs::write(&f, "# a comment\n/abs/big-repo\n*.lock\nrel/dir\n").unwrap();
        check!(excluded_trees(Some(&f)) == vec![PathBuf::from("/abs/big-repo")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_excluded_matches_path_and_descendants_only() {
        let trees = vec![PathBuf::from("/work/big")];
        check!(is_excluded(Path::new("/work/big"), &trees));
        check!(is_excluded(Path::new("/work/big/sub/dir"), &trees));
        check!(!is_excluded(Path::new("/work/other"), &trees));
        check!(!is_excluded(Path::new("/work/big-2"), &trees)); // not a component prefix
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed");
    }

    // #273: a clean git checkout on an unchanged HEAD short-circuits to
    // "up to date"; a new commit busts it; a dirty tree never short-circuits.
    #[test]
    fn commit_short_circuit_tracks_head_and_dirtiness() {
        let dir = temp_repo("commit-sc");
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "init"]);

        check!(!run(&dir, false).unwrap().up_to_date); // first build
        check!(run(&dir, false).unwrap().up_to_date); // clean + same HEAD -> skip

        // HEAD is recorded so the next run can trust it.
        let head = git_head_commit(&dir).unwrap();
        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        check!(store.get_meta("head_commit").unwrap().as_deref() == Some(head.as_str()));
        drop(store);

        // A new commit moves HEAD -> re-index, then stable again.
        std::fs::write(dir.join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "more"]);
        check!(!run(&dir, false).unwrap().up_to_date); // HEAD moved
        check!(run(&dir, false).unwrap().up_to_date); // stable at new HEAD

        // A dirty tree at the recorded HEAD must not short-circuit.
        std::fs::write(dir.join("a.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        check!(!run(&dir, false).unwrap().up_to_date); // dirty content -> re-index
        std::fs::remove_dir_all(&dir).ok();
    }

    // A nested repository (submodule / vendored checkout) is a boundary: its
    // code stays out of the parent's index so `repo scan --recursive` doesn't
    // index those files twice. The parent's own VCS dir does not prune the root.
    #[test]
    fn discovery_stops_at_nested_repo_boundaries() {
        let dir = temp_repo("nested-repo");
        std::fs::create_dir_all(dir.join(".git")).unwrap(); // parent is a repo
        std::fs::write(dir.join("main.rs"), "fn f() {}\n").unwrap();
        // A submodule under a non-ignored path, with its own `.git` and source.
        let sub = dir.join("libs").join("sub");
        std::fs::create_dir_all(sub.join(".git")).unwrap();
        std::fs::write(sub.join("lib.rs"), "fn g() {}\n").unwrap();

        let found = discover(&dir, &[], &[], false, None).unwrap();
        let rels: Vec<&str> = found.iter().map(|f| f.rel_path.as_str()).collect();
        check!(rels == ["main.rs"], "submodule code leaked in: {rels:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    // #130: a changed server handler propagates across the wire not just to the
    // client call site but to the function that makes the call and its callers.
    #[test]
    fn impact_propagates_through_client_call_to_callers() {
        use glyphtrail_core::{ImpactPolicy, compute_impact};
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
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
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

    // #251: an unchanged tree fast-paths, but a changed analysis revision (a
    // stand-in for an extractor-logic change) busts it and forces a re-index.
    #[test]
    fn fast_path_busts_on_analysis_revision_change() {
        let dir = temp_repo("rev-bust");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"r\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap();

        check!(!run(&dir, false).unwrap().up_to_date); // first build
        check!(run(&dir, false).unwrap().up_to_date); // unchanged + same revision -> fast path

        // Stale the stored revision, as an analyzer-logic change would.
        {
            let mut store =
                LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
            store.set_meta("analysis_revision", "stale").unwrap();
        }
        check!(!run(&dir, false).unwrap().up_to_date); // revision mismatch -> re-index

        std::fs::remove_dir_all(&dir).ok();
    }

    // #220: analyze records Cargo package identity (workspace members, deps and
    // their sources) into the index meta, so the cross-repo link step can match
    // consumers to producers.
    #[test]
    fn analyze_persists_cargo_package_identity() {
        let dir = temp_repo("pkg-identity");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        let member = dir.join("crates/widget");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"widget\"\nversion = \"0.3.0\"\n\n[dependencies]\nserde = \"1\"\nhelper = { path = \"../helper\" }\n",
        )
        .unwrap();
        std::fs::write(member.join("src/lib.rs"), "pub fn go() {}\n").unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let json = store
            .get_meta("packages")
            .unwrap()
            .expect("packages meta written");
        let packages: Vec<IndexedPackage> = serde_json::from_str(&json).unwrap();
        let widget = packages
            .iter()
            .find(|p| p.name == "widget")
            .expect("widget package recorded");
        check!(widget.ecosystem == Ecosystem::Cargo);
        check!(widget.version == Some("0.3.0".to_string()));
        check!(widget.dir == "crates/widget");
        // #220c: the member crate's pub fn is recorded as an export attributed
        // to the owning package (longest-dir match), not the virtual root.
        check!(
            widget.exports.iter().any(|e| e.name == "go"),
            "expected `go` export, got {:?}",
            widget.exports.iter().map(|e| &e.name).collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // #416 Phase B: a sqlx query in a function links to the SQL table it reads,
    // so a table change can reach the code that touches it.
    #[test]
    fn analyze_links_sqlx_query_to_its_table() {
        let dir = temp_repo("sqlx-link");
        std::fs::create_dir_all(dir.join("db")).unwrap();
        std::fs::write(
            dir.join("db/schema.sql"),
            "CREATE TABLE users (id int, email text);\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/repo.rs"),
            "pub async fn load_user(db: &Pool) { \
             sqlx::query_as!(User, \"SELECT id FROM users WHERE id = $1\", x).fetch_one(db).await.unwrap(); }\n",
        )
        .unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let f = store
            .find_by_name("load_user")
            .unwrap()
            .into_iter()
            .next()
            .expect("function indexed");
        let reads = store
            .neighbors(&f.id.0, Some(EdgeKind::Reads), true)
            .unwrap();
        check!(
            reads
                .iter()
                .any(|(n, _, _)| n.kind == NodeKind::Table && n.name == "users"),
            "expected a Reads edge to the users table, got {:?}",
            reads.iter().map(|(n, _, _)| &n.name).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // #444: a `.cypher` query file links to the label tables it reads/writes, so a
    // schema-free graph's labels and their file-level access are in the graph.
    #[test]
    fn analyze_links_cypher_file_to_its_labels() {
        let dir = temp_repo("cypher-file-link");
        std::fs::create_dir_all(dir.join("queries")).unwrap();
        std::fs::write(
            dir.join("queries/links.cypher"),
            "MATCH (a:Account) MERGE (a)-[:OWNS]->(c:Card);\n",
        )
        .unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let file = store
            .find_by_name("queries/links.cypher")
            .unwrap()
            .into_iter()
            .next()
            .expect("cypher file indexed");
        let reads = store
            .neighbors(&file.id.0, Some(EdgeKind::Reads), true)
            .unwrap();
        check!(
            reads
                .iter()
                .any(|(n, _, _)| n.kind == NodeKind::Table && n.name == "Account"),
            "expected a Reads edge to the Account label, got {:?}",
            reads.iter().map(|(n, _, _)| &n.name).collect::<Vec<_>>()
        );
        let writes = store
            .neighbors(&file.id.0, Some(EdgeKind::Writes), true)
            .unwrap();
        check!(
            writes
                .iter()
                .any(|(n, _, _)| n.kind == NodeKind::Table && n.name == "OWNS"),
            "expected a Writes edge to the OWNS label, got {:?}",
            writes.iter().map(|(n, _, _)| &n.name).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // #440: a JS `pg` driver query (`pool.query("SELECT … FROM users")`) links its
    // enclosing function to the `.sql` `users` table, like the Rust sqlx path.
    #[test]
    fn analyze_links_js_driver_query_to_its_table() {
        let dir = temp_repo("js-driver-link");
        std::fs::create_dir_all(dir.join("db")).unwrap();
        std::fs::write(
            dir.join("db/schema.sql"),
            "CREATE TABLE users (id int, email text);\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/repo.js"),
            "async function loadUser(pool) { return pool.query('SELECT id FROM users WHERE id = $1', [id]); }\n",
        )
        .unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let f = store
            .find_by_name("loadUser")
            .unwrap()
            .into_iter()
            .next()
            .expect("function indexed");
        let reads = store
            .neighbors(&f.id.0, Some(EdgeKind::Reads), true)
            .unwrap();
        check!(
            reads
                .iter()
                .any(|(n, _, _)| n.kind == NodeKind::Table && n.name == "users"),
            "expected a Reads edge to the users table, got {:?}",
            reads.iter().map(|(n, _, _)| &n.name).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // #440: a Diesel query DSL links its function to the `table!` schema, across
    // files (schema in one file, query in another).
    #[test]
    fn analyze_links_diesel_query_to_its_table() {
        let dir = temp_repo("diesel-link");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/schema.rs"),
            "diesel::table! { users (id) { id -> Int4, email -> Text, } }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/repo.rs"),
            "pub fn load_user(conn: &mut PgConnection) -> Vec<User> { \
             users::table.filter(users::id.eq(1)).load(conn).unwrap() }\n",
        )
        .unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let f = store
            .find_by_name("load_user")
            .unwrap()
            .into_iter()
            .next()
            .expect("function indexed");
        let reads = store
            .neighbors(&f.id.0, Some(EdgeKind::Reads), true)
            .unwrap();
        check!(
            reads
                .iter()
                .any(|(n, _, _)| n.kind == NodeKind::Table && n.name == "users"),
            "expected a Reads edge to the users table, got {:?}",
            reads.iter().map(|(n, _, _)| &n.name).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // #416 Phase B (Java): a Spring Data repository method links to the JPA
    // entity's table, across files (entity in one file, repo in another).
    #[test]
    fn analyze_links_jpa_repository_to_entity_table() {
        let dir = temp_repo("jpa-link");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/User.java"),
            "@jakarta.persistence.Entity @jakarta.persistence.Table(name = \"users\") class User { @jakarta.persistence.Id Long id; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/UserRepository.java"),
            "interface UserRepository extends org.springframework.data.jpa.repository.JpaRepository<User, Long> { void deleteByEmail(String e); }\n",
        )
        .unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let m = store
            .find_by_name("deleteByEmail")
            .unwrap()
            .into_iter()
            .next()
            .expect("repository method indexed");
        let writes = store
            .neighbors(&m.id.0, Some(EdgeKind::Writes), true)
            .unwrap();
        check!(
            writes
                .iter()
                .any(|(n, _, _)| n.kind == NodeKind::Table && n.name == "users"),
            "expected a Writes edge to the users table, got {:?}",
            writes.iter().map(|(n, _, _)| &n.name).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // #435: an incremental `--update` that re-parses only a code file still links
    // it to a table whose `.sql` file wasn't re-parsed (resolved from the store).
    #[test]
    fn incremental_update_relinks_code_to_unchanged_table() {
        let dir = temp_repo("incr-db-relink");
        std::fs::create_dir_all(dir.join("db")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("db/schema.sql"), "CREATE TABLE users (id int);\n").unwrap();
        std::fs::write(
            dir.join("src/repo.rs"),
            "pub async fn load(db: &Pool) { sqlx::query(\"SELECT id FROM users\").fetch_one(db).await.ok(); }\n",
        )
        .unwrap();
        run(&dir, false).unwrap(); // full build

        // Edit only the code file (the .sql is untouched), then incrementally update.
        std::fs::write(
            dir.join("src/repo.rs"),
            "pub fn other() {}\npub async fn load(db: &Pool) { sqlx::query(\"SELECT id FROM users\").fetch_one(db).await.ok(); }\n",
        )
        .unwrap();
        let outcome = run(&dir, true).unwrap();
        check!(!outcome.up_to_date); // the code file changed, so it re-parsed

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let f = store
            .find_by_name("load")
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let reads = store
            .neighbors(&f.id.0, Some(EdgeKind::Reads), true)
            .unwrap();
        check!(
            reads.iter().any(|(n, _, _)| n.name == "users"),
            "incremental update should keep the code→table link, got {:?}",
            reads.iter().map(|(n, _, _)| &n.name).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn import_root_takes_first_path_segment() {
        check!(import_root("widget::go") == Some("widget"));
        check!(import_root("::widget::go") == Some("widget"));
        check!(import_root("widget") == Some("widget"));
        check!(import_root("") == None);
    }

    #[test]
    fn dep_matches_import_per_ecosystem() {
        // Cargo: first crate segment.
        check!(dep_matches_import(Ecosystem::Cargo, "widget", "widget::go"));
        check!(!dep_matches_import(Ecosystem::Cargo, "widget", "other::go"));
        // .NET: exact namespace or a namespace prefix; not a sibling sharing a stem.
        check!(dep_matches_import(
            Ecosystem::Dotnet,
            "Acme.Core",
            "Acme.Core"
        ));
        check!(dep_matches_import(
            Ecosystem::Dotnet,
            "Acme.Core",
            "Acme.Core.Sub"
        ));
        check!(!dep_matches_import(
            Ecosystem::Dotnet,
            "Acme.Core",
            "Acme.CoreX"
        ));
        check!(!dep_matches_import(Ecosystem::Dotnet, "Acme.Core", "Other"));
    }

    // .NET cross-repo identity: a .csproj publishes a NuGet package id (export
    // = its public types) and a `using` of a referenced package is tagged as an
    // external use — the producer and consumer sides a NuGet link matches.
    #[test]
    fn analyze_persists_dotnet_package_identity() {
        let dir = temp_repo("dotnet-identity");
        let proj = dir.join("Acme.Models");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("Acme.Models.csproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup>\
             <PackageId>Acme.Models</PackageId><Version>1.2.0</Version></PropertyGroup>\
             <ItemGroup><PackageReference Include=\"Acme.Core\" Version=\"1.0.0\" /></ItemGroup></Project>",
        )
        .unwrap();
        std::fs::write(
            proj.join("Item.cs"),
            "using Acme.Core;\nnamespace Acme.Models {\n  public class Item {\n    public void Run() { var t = new CoreThing(); t.Process(); }\n  }\n}\n",
        )
        .unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let packages: Vec<IndexedPackage> =
            serde_json::from_str(&store.get_meta("packages").unwrap().unwrap()).unwrap();
        let models = packages
            .iter()
            .find(|p| p.name == "Acme.Models")
            .expect("Acme.Models package recorded");
        check!(models.ecosystem == Ecosystem::Dotnet);
        check!(models.version == Some("1.2.0".to_string()));
        check!(models.dir == "Acme.Models");
        check!(
            models.exports.iter().any(|e| e.name == "Item"),
            "expected Item export, got {:?}",
            models.exports.iter().map(|e| &e.name).collect::<Vec<_>>()
        );

        let uses: Vec<ExternalUse> =
            serde_json::from_str(&store.get_meta("external_uses").unwrap().unwrap()).unwrap();
        let core = uses
            .iter()
            .find(|u| u.ecosystem == Ecosystem::Dotnet && u.package == "Acme.Core")
            .unwrap_or_else(|| panic!("expected Acme.Core external use, got {uses:?}"));
        // Symbol-level: the referenced type `CoreThing` becomes a candidate
        // symbol (type uses, not just calls), so the link step can land on a
        // producer export rather than only the package.
        check!(
            core.symbols.contains(&"CoreThing".to_string()),
            "expected CoreThing candidate symbol, got {:?}",
            core.symbols
        );
        check!(!core.from_nodes.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    // #220: imports referencing a declared dependency are tagged as external
    // use-sites (the consumer side of a cross-repo link); std and unknown roots
    // are not.
    #[test]
    fn analyze_tags_external_crate_uses() {
        let dir = temp_repo("external-uses");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        let app = dir.join("crates/app");
        std::fs::create_dir_all(app.join("src")).unwrap();
        std::fs::write(
            app.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nwidget = \"1\"\nlocaldep = { path = \"../localdep\" }\n",
        )
        .unwrap();
        std::fs::write(
            app.join("src/lib.rs"),
            "use widget::go;\nuse localdep::helper;\nuse std::collections::HashMap;\nfn use_them() { let _: HashMap<u8, u8>; }\n",
        )
        .unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let json = store
            .get_meta("external_uses")
            .unwrap()
            .expect("external_uses meta written");
        let uses: Vec<ExternalUse> = serde_json::from_str(&json).unwrap();
        check!(
            uses.iter()
                .any(|u| u.package == "widget" && u.path == "widget::go"),
            "expected widget::go external use, got {uses:?}"
        );
        check!(
            uses.iter()
                .any(|u| u.package == "localdep" && u.path == "localdep::helper")
        );
        // std is not a declared dependency, so it is not an external use.
        check!(!uses.iter().any(|u| u.package == "std"));
        check!(uses.iter().all(|u| u.from_package == "app"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // #405: a client URL built from the Angular `environment` chain resolves
    // end-to-end through the full analyze pipeline (import -> alias -> object
    // property), producing a concrete, matchable op path.
    #[test]
    fn client_url_resolves_imported_environment_constant() {
        let dir = temp_repo("client-env-const");
        std::fs::create_dir_all(dir.join("src/environments")).unwrap();
        let svc = dir.join("src/app/core/services/authentication");
        std::fs::create_dir_all(&svc).unwrap();
        // Mirror the real structure: a deep relative import, and an env object
        // property that itself names a constant imported from a third file.
        std::fs::write(
            dir.join("src/environments/environment.model.ts"),
            "export const MEDIAN_LOGIN = '/signin';\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/environments/environment.ts"),
            "import { MEDIAN_LOGIN } from './environment.model';\n\
             export const environment = { IS_PRODUCTION: false, API_URL: 'https://api.example.test/v2/', MEDIAN_LOGIN: MEDIAN_LOGIN };\n",
        )
        .unwrap();
        std::fs::write(
            svc.join("auth.service.ts"),
            "import { HttpClient } from '@angular/common/http';\n\
             import { environment } from '../../../../environments/environment';\n\
             const API_URL = environment.API_URL;\n\
             const MEDIAN_LOGIN = environment.MEDIAN_LOGIN;\n\
             class S {\n  constructor(private http: HttpClient) {}\n  \
             login() { return this.http.post(`${API_URL}${MEDIAN_LOGIN}`, null); } }\n",
        )
        .unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let ops = store.operations_by_kind(NodeKind::ClientCall).unwrap();
        check!(
            ops.iter().any(|(_, k)| k.path == "/v2/signin"),
            "expected resolved /v2/signin, got {:?}",
            ops.iter().map(|(_, k)| k.path.clone()).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // #236: an external use records the precise consumer symbols that reference
    // the imported name, not every symbol in the file.
    #[test]
    fn external_use_records_referencing_symbols() {
        let dir = temp_repo("precise-use-sites");
        let app = dir.join("crates/app");
        std::fs::create_dir_all(app.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        std::fs::write(
            app.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nwidget = \"1\"\n",
        )
        .unwrap();
        // `caller` references the imported `go`; `unrelated` does not.
        std::fs::write(
            app.join("src/lib.rs"),
            "use widget::go;\nfn caller() { go(); }\nfn unrelated() { let goldfish = 1; }\n",
        )
        .unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let uses: Vec<ExternalUse> =
            serde_json::from_str(&store.get_meta("external_uses").unwrap().unwrap()).unwrap();
        let go_use = uses
            .iter()
            .find(|u| u.package == "widget" && u.path == "widget::go")
            .expect("widget::go external use");
        let names: Vec<String> = go_use
            .from_nodes
            .iter()
            .filter_map(|id| store.get_node(id).unwrap().map(|n| n.name))
            .collect();
        check!(names.contains(&"caller".to_string()), "got {names:?}");
        // `unrelated` only contains the substring "go" inside "goldfish".
        check!(!names.contains(&"unrelated".to_string()), "got {names:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn use_aliases_extracts_renames() {
        check!(use_aliases("foo::Bar as Baz") == vec![("Bar".to_string(), "Baz".to_string())]);
        check!(use_aliases("foo::{A as B, C}") == vec![("A".to_string(), "B".to_string())]);
        check!(use_aliases("foo::Bar").is_empty());
    }

    // #239: a `pub use X as Y` renamed re-export adds an alias export entry named
    // `Y` pointing at the item defined as `X`, so a consumer importing `Y`
    // matches symbol-level.
    #[test]
    fn pub_use_rename_adds_alias_export() {
        let dir = temp_repo("pub-use-rename");
        let lib = dir.join("crates/lib");
        std::fs::create_dir_all(lib.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        std::fs::write(
            lib.join("Cargo.toml"),
            "[package]\nname = \"lib\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            lib.join("src/lib.rs"),
            "pub struct Thing {}\npub use Thing as Widget;\n",
        )
        .unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let packages: Vec<IndexedPackage> =
            serde_json::from_str(&store.get_meta("packages").unwrap().unwrap()).unwrap();
        let lib_pkg = packages
            .iter()
            .find(|p| p.name == "lib")
            .expect("lib package");
        let thing = lib_pkg
            .exports
            .iter()
            .find(|e| e.name == "Thing")
            .expect("Thing export");
        let widget = lib_pkg
            .exports
            .iter()
            .find(|e| e.name == "Widget")
            .expect("Widget alias export");
        check!(widget.node_id == thing.node_id);

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
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
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
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
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
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
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
        let found = discover(&dir, &[], &[], false, None).unwrap();
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

        let found = discover(&dir, &[], &[], true, None).unwrap();
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
        std::fs::create_dir_all(dir.join(".glyphtrail")).unwrap();
        std::fs::write(
            dir.join(".glyphtrail/config.toml"),
            "[security]\nrecord_sensitive_files = true\n",
        )
        .unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let env = store
            .find_by_name(".env")
            .unwrap()
            .into_iter()
            .next()
            .expect(".env File node");
        check!(env.kind == NodeKind::File);
        check!(env.doc.as_deref() == Some("sensitive: contents excluded from the index"));
        // The secret value never entered the index (no node mentions it).
        check!(store.search("supersecret", 50, false).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    fn callers_of(dir: &Path, name: &str) -> Vec<String> {
        let store = LadybugStore::open(&RepoPaths::new(dir).index_dir.join("ladybug")).unwrap();
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
            let store =
                LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
            check!(store.find_by_name("foo").unwrap().is_empty());
        }

        // Add the definition; caller.rs is unchanged, so only a global
        // re-resolution of persisted pending edges can create the link.
        std::fs::write(dir.join("def.rs"), "fn foo() {}\n").unwrap();
        run(&dir, true).unwrap();
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
        run(&dir, false).unwrap();
        check!(callers_of(&dir, "foo").contains(&"use_it".to_string()));

        // Remove foo; the inferred use_it -> foo edge must disappear.
        std::fs::write(dir.join("def.rs"), "fn other() {}\n").unwrap();
        run(&dir, true).unwrap();
        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
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
        let store = LadybugStore::open(&RepoPaths::new(dir).index_dir.join("ladybug")).unwrap();
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
        run(&dir, false).unwrap();

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
        run(&dir, false).unwrap();
        // No util.ts yet: the import is a placeholder module.
        check!(
            import_targets(&dir, "web/app.ts")
                .iter()
                .any(|(n, k)| n == "./util" && k == "module"),
            "expected unresolved placeholder before the target exists"
        );

        // Add the target; app.ts is unchanged, so only the global rebuild links it.
        std::fs::write(dir.join("web/util.ts"), "export const x = 1;\n").unwrap();
        run(&dir, true).unwrap();
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
        let store = LadybugStore::open(&RepoPaths::new(dir).index_dir.join("ladybug")).unwrap();
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
        run(&dir, false).unwrap();

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

    // #395: a Java `import static pkg.Class.method;` then an unqualified
    // `method()` call must resolve to the static-imported method.
    #[test]
    fn java_static_import_unqualified_call_resolves() {
        let dir = temp_repo("java-static-import");
        // Two `square` definitions in package-aligned directories force the
        // ambiguous-name path (no `[one]` fast-path), so resolution must use the
        // static-import tier to pick the right one.
        std::fs::create_dir_all(dir.join("util")).unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        std::fs::create_dir_all(dir.join("app")).unwrap();
        std::fs::write(
            dir.join("util/MathUtil.java"),
            "package util;\npublic class MathUtil {\n  public static int square(int x) { return x * x; }\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("other/OtherUtil.java"),
            "package other;\npublic class OtherUtil {\n  public static int square(int x) { return x + x; }\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("app/Calc.java"),
            "package app;\nimport static util.MathUtil.square;\n\
             public class Calc {\n  public int run(int n) { return square(n); }\n}\n",
        )
        .unwrap();
        run(&dir, false).unwrap();

        // The call resolves to the statically-imported `util.MathUtil.square`,
        // not the same-named `other.OtherUtil.square`.
        check!(
            callers_of_def_in(&dir, "square", "util/MathUtil.java").contains(&"run".to_string()),
            "run() should resolve to the static-imported square in util/MathUtil.java"
        );
        check!(
            callers_of_def_in(&dir, "square", "other/OtherUtil.java").is_empty(),
            "the un-imported square in other/OtherUtil.java should have no caller"
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
    // temp dir so the run's `.glyphtrail/` index doesn't pollute the source tree.
    #[test]
    fn analyzes_fixture_repo_with_cross_file_links() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sample");
        let dir = temp_repo("fixture");
        copy_dir(&fixture, &dir);
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
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

    // #5: an ambiguous call (two same-named defs in different directories, no
    // import between them) resolves to the candidate in the caller's own
    // directory rather than being dropped.
    #[test]
    fn ambiguous_call_resolves_within_caller_directory() {
        let dir = temp_repo("samedir-resolve");
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        // Caller and its same-directory target, with no import linking them.
        std::fs::write(dir.join("pkg/a.py"), "def use():\n    return helper()\n").unwrap();
        std::fs::write(dir.join("pkg/b.py"), "def helper():\n    return 1\n").unwrap();
        // A second, unrelated `helper` elsewhere makes the bare name ambiguous.
        std::fs::write(dir.join("other/c.py"), "def helper():\n    return 2\n").unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let use_id = store
            .find_by_name("use")
            .unwrap()
            .into_iter()
            .next()
            .expect("`use` fn")
            .id;
        let callees = store
            .neighbors(&use_id.0, Some(EdgeKind::Calls), true)
            .unwrap();
        check!(
            callees.len() == 1,
            "expected one resolved call, got {:?}",
            callees
                .iter()
                .map(|(n, _, _)| (&n.name, &n.file))
                .collect::<Vec<_>>()
        );
        // Resolved to the helper in the caller's directory, not the other one.
        check!(callees[0].0.file == "pkg/b.py");

        std::fs::remove_dir_all(&dir).ok();
    }

    // #5: a qualified call (`User.save()`) resolves to the method on the named
    // type, not a same-named method on another type — even when both live in the
    // caller's own directory (so the same-directory tier can't decide).
    #[test]
    fn qualified_call_resolves_by_receiver_scope() {
        let dir = temp_repo("qualifier-resolve");
        std::fs::create_dir_all(dir.join("m")).unwrap();
        std::fs::write(
            dir.join("m/user.py"),
            "class User:\n    def save(self):\n        return 1\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("m/admin.py"),
            "class Admin:\n    def save(self):\n        return 2\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("m/caller.py"),
            "def go():\n    return User.save(None)\n",
        )
        .unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let go = store
            .find_by_name("go")
            .unwrap()
            .into_iter()
            .next()
            .expect("`go` fn")
            .id;
        let callees = store.neighbors(&go.0, Some(EdgeKind::Calls), true).unwrap();
        check!(
            callees.len() == 1,
            "expected one resolved call, got {:?}",
            callees
                .iter()
                .map(|(n, _, _)| (&n.qualified_name, &n.file))
                .collect::<Vec<_>>()
        );
        // Resolved to User's `save`, not Admin's.
        check!(callees[0].0.qualified_name == "User::save");
        check!(callees[0].0.file == "m/user.py");

        std::fs::remove_dir_all(&dir).ok();
    }

    // #5: an in-file qualified call resolves precisely (and at extracted
    // confidence) to the local method on the named type, not a same-named method
    // on another class in the same file.
    #[test]
    fn qualified_call_resolves_to_local_scope() {
        let dir = temp_repo("local-qualifier");
        std::fs::write(
            dir.join("m.py"),
            "class User:\n    def save(self):\n        return 1\n\
             class Admin:\n    def save(self):\n        return 2\n\
             def go():\n    return User.save(None)\n",
        )
        .unwrap();
        run(&dir, false).unwrap();

        let store = LadybugStore::open(&RepoPaths::new(&dir).index_dir.join("ladybug")).unwrap();
        let go = store
            .find_by_name("go")
            .unwrap()
            .into_iter()
            .next()
            .expect("`go` fn")
            .id;
        let callees = store.neighbors(&go.0, Some(EdgeKind::Calls), true).unwrap();
        check!(
            callees.len() == 1,
            "got {:?}",
            callees
                .iter()
                .map(|(n, _, c)| (&n.qualified_name, c))
                .collect::<Vec<_>>()
        );
        check!(callees[0].0.qualified_name == "User::save");
        // Resolved in-file, so it is extracted (not inferred via the global pass).
        check!(callees[0].2 == Confidence::Extracted);

        std::fs::remove_dir_all(&dir).ok();
    }
}
