//! MCP tool definitions and handlers over the Glyphtrail graph store.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use glyphtrail_core::config::{INDEX_DIR, RepoPaths};
use glyphtrail_core::{
    Confidence, Detail, EdgeKind, Groups, HttpMethod, ImpactPolicy, ImpactReport, Node, NodeId,
    NodeKind, OperationKey, Protocol, Registry, RepoHealth, default_groups_path,
    default_registry_path, is_outline_kind, operations_matching, outline_symbol,
};
use glyphtrail_store::{
    ChangeSpec, FederationScope, GraphStore, LadybugStore, SeedSpec, changed_files,
    federated_impact, seed_nodes,
};
use serde_json::{Value, json};

/// The advertised tool set (`tools/list`).
///
/// `has_default_repo` reflects whether the server was launched with a `--repo`.
/// When it was not (the global Claude Desktop bundle, say), the `repo` argument
/// is promoted to *required* on every repo-scoped tool — both in the JSON schema
/// (`required`) and in its description — so the agent is steered to name a target
/// up front rather than relying on a default the server does not have.
pub fn definitions(has_default_repo: bool) -> Vec<Value> {
    let name_arg = json!({ "name": { "type": "string", "description": "Symbol name." } });
    let proto_arg =
        json!({ "protocol": { "type": "string", "enum": ["rest", "grpc", "graphql"] } });
    let path_method = json!({
        "path": { "type": "string", "description": "Request path, e.g. /users/{id} or /users/123." },
        "method": { "type": "string", "description": "Optional HTTP method (GET, POST, …)." }
    });
    let mut defs = vec![
        tool(
            "search",
            "Substring search over symbol names, qualified names, and doc \
             comments. Case-insensitive by default.",
            json!({
                "query": { "type": "string" },
                "limit": { "type": "integer" },
                "case_sensitive": { "type": "boolean", "description": "Match case exactly (default: case-insensitive)." }
            }),
            &["query"],
        ),
        tool(
            "definition",
            "Locate definition(s) matching a name.",
            name_arg.clone(),
            &["name"],
        ),
        tool(
            "callers",
            "Symbols that call the named symbol.",
            name_arg.clone(),
            &["name"],
        ),
        tool(
            "callees",
            "Symbols the named symbol calls.",
            name_arg.clone(),
            &["name"],
        ),
        tool(
            "neighbors",
            "Direct graph neighbours of the named symbol, any direction.",
            name_arg.clone(),
            &["name"],
        ),
        tool(
            "endpoints",
            "List server-side API endpoints (routes) with their handlers.",
            proto_arg.clone(),
            &[],
        ),
        tool(
            "clients",
            "List client-side API call sites.",
            proto_arg,
            &[],
        ),
        tool(
            "serves",
            "Endpoint(s) serving a path (template- and value-aware).",
            path_method.clone(),
            &["path"],
        ),
        tool(
            "who_calls",
            "Client call sites that invoke an endpoint (cross-boundary INVOKES).",
            path_method.clone(),
            &["path"],
        ),
        tool(
            "api_impact",
            "Cross-boundary view of an endpoint: invoking clients, handler(s), schema op(s).",
            path_method,
            &["path"],
        ),
        tool(
            "impact",
            "Blast radius of a change: seed from a symbol, a file, or a git \
             change set, and report classified, confidence-aware impacted nodes \
             (tests, API surface, cross-boundary consumers, internal). The \
             summary carries a risk level (none/low/medium/high/critical).",
            json!({
                "name": { "type": "string", "description": "Seed symbol name." },
                "file": { "type": "string", "description": "Seed every symbol in this repo-relative file." },
                "files": { "type": "array", "items": { "type": "string" }, "description": "Seed every symbol in these files." },
                "since": { "type": "string", "description": "Seed symbols changed since a git rev/range (e.g. main..HEAD)." },
                "staged": { "type": "boolean", "description": "Seed from staged changes." },
                "diff": { "type": "boolean", "description": "Seed from unstaged working-tree changes." },
                "edges": { "type": "array", "items": { "type": "string" }, "description": "Edge sets: calls, refs (type usages), imports, impl, api." },
                "depth": { "type": "integer", "description": "Max hops from a seed (default 5)." },
                "min_confidence": { "type": "string", "enum": ["extracted", "inferred"] },
                "cross_boundary": { "type": "boolean", "description": "Include HANDLES/INVOKES/EXPOSES/MOUNTS consumers." },
                "downstream": { "type": "boolean", "description": "Extend the blast radius into OTHER indexed repos that depend on this one (federate over the registry). Reports which repos break and where." },
                "group": { "type": "string", "description": "Like downstream, but scope the federation to a named group." },
                "deep": { "type": "boolean", "description": "Thorough federated scan: re-read each member's identity from its store instead of the registry cache, and include indexed repos beside this one that were never registered. Slower; use when the registry shortcut may be stale or incomplete." }
            }),
            &[],
        ),
        tool(
            "status",
            "Index statistics for the repository, plus a `freshness` field \
             (fresh/stale/unknown) and `stale`/`stale_reason`: whether the index \
             still reflects the working tree. If stale, call `analyze` before \
             trusting query/impact results. `links` lists OTHER repos this one \
             declares cross-repo links to (e.g. a vendored/submodule repo) — \
             pass one as the `repo` argument to search it.",
            json!({}),
            &[],
        ),
        tool(
            "outline",
            "Symbol outline of a file or directory: each definition (functions, \
             types, etc.) with its signature, at adjustable detail. The compact \
             'shape of the code' for getting oriented before deeper queries.",
            json!({
                "path": { "type": "string", "description": "File or directory (repo-relative); '.' for the whole repo." },
                "detail": { "type": "string", "enum": ["minimal", "standard", "full"], "description": "minimal=names, standard=signatures, full=+doc (default standard)." }
            }),
            &["path"],
        ),
        tool(
            "list_repos",
            "List OTHER repositories in the global registry (on-disk health, \
             groups, stable forge ids) for cross-repo work. You do NOT need this \
             for the repo you are already in — target that directly by its path. \
             The registry may be large, so narrow the result: `summary` for \
             counts, `path_hint` for the repo(s) at a path, or \
             `detail`/`limit`/filters — no shell required.",
            json!({
                "summary": { "type": "boolean", "description": "Return only counts {total, by_health, by_group} instead of the repo list. Cheapest way to see what is indexed." },
                "detail": { "type": "string", "enum": ["minimal", "standard", "full"], "description": "Per-repo fields: minimal=name+health, standard=+root+groups, full=+alt_roots+ids (default full)." },
                "path_hint": { "type": "string", "description": "Absolute path: return only the repo(s) at or containing it (e.g. the directory you are working in)." },
                "health": { "type": "string", "enum": ["indexed", "unindexed", "missing"], "description": "Filter by on-disk health." },
                "group": { "type": "string", "description": "Filter to repos in this named group." },
                "name": { "type": "string", "description": "Filter to repos whose name contains this (case-insensitive)." },
                "limit": { "type": "integer", "description": "Max repos to return." },
                "offset": { "type": "integer", "description": "Skip this many repos before returning (pagination)." }
            }),
            &[],
        ),
        tool(
            "analyze",
            "(Re)index a repository: walk it, parse sources, resolve links, and \
             persist the graph. Targets the `repo` (registered name or path) or \
             the server's launch repo. The target must be a git repository (the \
             repo root is used) or an already-indexed or registered directory; a \
             bare path that is neither is refused, so pass `repo` explicitly when \
             unsure. Run this after code changes, or to index a repo the server \
             has never seen, so queries reflect the latest state — no shell required.",
            json!({
                "update": { "type": "boolean", "description": "Only reparse files changed since the last index (incremental)." }
            }),
            &[],
        ),
    ];
    // analyze is the only tool that writes (it rebuilds the index). Mark it so
    // the agent leaves the re-index decision to the caller rather than the model
    // assuming every tool is side-effect-free (#362).
    if let Some(analyze) = defs.iter_mut().find(|d| d["name"] == json!("analyze")) {
        analyze["annotations"]["readOnlyHint"] = json!(false);
    }
    if !has_default_repo {
        require_repo_argument(&mut defs);
    }
    // Opt-in project-reporting tool (#370), added last so it never gains a
    // repo argument (it is not repo-scoped).
    if file_issue_enabled() {
        defs.push(file_issue_tool());
    }
    defs
}

/// Promote `repo` to a required argument on every repo-scoped tool, for a server
/// with no launch repo. Adds `repo` to each tool's `required` list and rewrites
/// its description to say so. `list_repos` is skipped — it spans repos and takes
/// no `repo` — so the agent always has a way to discover a target.
fn require_repo_argument(defs: &mut [Value]) {
    const REQUIRED_DESC: &str = "REQUIRED — the server has no default repo and \
        could not detect one from its working directory. Pass the absolute path \
        of the directory you are working in, or a registered repo name (you \
        already know your path — no lookup needed). `list_repos` is only for \
        discovering OTHER repos.";
    for def in defs {
        if def["name"] == json!("list_repos") {
            continue;
        }
        if let Some(req) = def["inputSchema"]["required"].as_array_mut()
            && !req.iter().any(|v| v == "repo")
        {
            req.push(json!("repo"));
        }
        def["inputSchema"]["properties"]["repo"]["description"] = json!(REQUIRED_DESC);
    }
}

/// Execute a tool call, returning a `tools/call` result object. Tool-level
/// failures are reported as `isError` results rather than protocol errors, per
/// the MCP convention.
pub fn call(default_db: Option<&Path>, name: &str, args: &Value) -> Value {
    let mut warn = None;
    match dispatch(default_db, name, args, &mut warn) {
        Ok(value) => {
            let mut result = text_result(&value, false);
            // A stale-index warning rides along as a second text block, so the
            // data block stays at content[0] and the agent still sees the
            // re-analyze nudge (#345).
            if let Some(text) = warn
                && let Some(content) = result["content"].as_array_mut()
            {
                content.push(json!({ "type": "text", "text": text }));
            }
            result
        }
        Err(message) => text_result(&json!({ "error": message }), true),
    }
}

fn dispatch(
    default_db: Option<&Path>,
    name: &str,
    args: &Value,
    warn: &mut Option<String>,
) -> Result<Value, String> {
    // Registry-level tools span repos and need no per-repo store.
    if name == "list_repos" {
        return list_repos(args);
    }
    // Opt-in, project-level, store-free (#370). Refuse unless enabled, even if a
    // client somehow calls it off a stale schema.
    if name == "file_issue" {
        if !file_issue_enabled() {
            return Err("file_issue is disabled; set GLYPHTRAIL_MCP_FILE_ISSUE=1 to enable".into());
        }
        return Ok(file_issue_guidance(args));
    }
    // Resolve the per-call repo selector (#240): a registered name or a path
    // overrides the server's launch repo; absent, the launch repo is used — and
    // if the server has no launch repo, the call is rejected (the agent must
    // name a repo). See `target_db`.
    let db = target_db(default_db, args)?;
    // analyze writes a fresh index for the target repo; it opens its own store,
    // so it runs before (and instead of) the read-path store open.
    if name == "analyze" {
        return analyze_tool(&db, args);
    }
    // Cross-repo impact opens its own member stores (incl. the current repo), so
    // it must not be given the pre-opened per-repo store (no double-open).
    if name == "impact" && is_federated(args) {
        return federated_impact_tool(&db, args);
    }
    let store = open(&db)?;
    // Whatever the tool returns, flag a stale index so the agent re-analyzes
    // before trusting results (#345). Computed once here, for every store tool.
    *warn = stale_warning_for(&db, &*store);
    match name {
        "search" => {
            let limit = opt_usize(args, "limit").unwrap_or(30);
            let case_sensitive = args
                .get("case_sensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let nodes = store
                .search(req_str(args, "query")?, limit, case_sensitive)
                .map_err(err)?;
            Ok(nodes_json(&nodes))
        }
        "definition" => {
            let nodes = store.find_by_name(req_str(args, "name")?).map_err(err)?;
            Ok(nodes_json(&nodes))
        }
        "callers" => neighbors_of(&*store, args, EdgeKind::Calls, false),
        "callees" => neighbors_of(&*store, args, EdgeKind::Calls, true),
        "neighbors" => {
            let node = resolve_one(&*store, req_str(args, "name")?)?;
            let mut items = store.neighbors(&node.id.0, None, true).map_err(err)?;
            items.extend(store.neighbors(&node.id.0, None, false).map_err(err)?);
            Ok(neighbors_json(&items))
        }
        "endpoints" => operations_list(&*store, NodeKind::Endpoint, args, true),
        "clients" => operations_list(&*store, NodeKind::ClientCall, args, false),
        "serves" => {
            let matched = matched_endpoints(&*store, args)?;
            let mut out = Vec::new();
            for (id, key) in matched {
                let node = store.get_node(&id.0).map_err(err)?;
                out.push(operation_json(
                    &key,
                    node.as_ref(),
                    endpoint_handler(&*store, &id)?,
                ));
            }
            Ok(Value::Array(out))
        }
        "who_calls" => {
            let mut items = Vec::new();
            for (id, _) in matched_endpoints(&*store, args)? {
                items.extend(
                    store
                        .neighbors(&id.0, Some(EdgeKind::Invokes), false)
                        .map_err(err)?,
                );
            }
            Ok(neighbors_json(&items))
        }
        "api_impact" => {
            let mut report = Vec::new();
            for (id, key) in matched_endpoints(&*store, args)? {
                let node = store.get_node(&id.0).map_err(err)?;
                report.push(json!({
                    "endpoint": operation_json(&key, node.as_ref(), None),
                    "handlers": neighbor_nodes(&*store, &id, EdgeKind::Handles, false)?,
                    "callers": neighbor_nodes(&*store, &id, EdgeKind::Invokes, false)?,
                    "schema_ops": neighbor_nodes(&*store, &id, EdgeKind::Exposes, true)?,
                }));
            }
            Ok(Value::Array(report))
        }
        "impact" => impact_tool(&db, &*store, args),
        "outline" => outline_tool(&db, &*store, args),
        "status" => {
            let s = store.stats().map_err(err)?;
            let languages: serde_json::Map<String, Value> = s
                .languages
                .into_iter()
                .map(|(lang, n)| (lang, json!(n)))
                .collect();
            let root = db.parent().and_then(Path::parent);
            // Freshness (#313): whether the index still reflects the working
            // tree, so an agent knows to call `analyze` before trusting results.
            let staleness = root
                .map(|root| glyphtrail_analyze::index_staleness(root, &*store))
                .unwrap_or(glyphtrail_analyze::Staleness::Unknown);
            // Declared cross-repo links (#365): the OTHER repos this one links to
            // via [[links]] (e.g. a vendored/submodule repo). Surfacing them tells
            // an agent to target those repos in its searches instead of missing
            // code that lives outside this one.
            let links = declared_links(root);
            let (freshness, reason) = match &staleness {
                glyphtrail_analyze::Staleness::Fresh => ("fresh", Value::Null),
                glyphtrail_analyze::Staleness::Stale(why) => ("stale", json!(why)),
                glyphtrail_analyze::Staleness::Unknown => ("unknown", Value::Null),
            };
            Ok(json!({
                "nodes": s.nodes,
                "edges": s.edges,
                "files": s.files,
                "languages": languages,
                "freshness": freshness,
                "stale": staleness.is_stale(),
                "stale_reason": reason,
                // Which build is answering (#351): the semver alone (a static
                // 0.1.0) can't distinguish builds, so carry the commit + build time.
                "version": env!("CARGO_PKG_VERSION"),
                "commit": env!("GLYPHTRAIL_GIT_COMMIT"),
                "built": env!("GLYPHTRAIL_BUILD_TIMESTAMP"),
                // OTHER repos this one declares links to (#365), so the agent can
                // target them; empty when none are declared.
                "links": links,
            }))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Enumerate the global registry: every indexed repo with its on-disk health
/// and group membership. Reads the registry and groups files directly (no
/// per-repo store), so an agent can discover what is indexed without a shell.
fn list_repos(args: &Value) -> Result<Value, String> {
    let registry = match default_registry_path() {
        Some(p) => Registry::load(&p).map_err(err)?,
        None => Registry::default(),
    };
    let groups = match default_groups_path() {
        Some(p) => Groups::load(&p).map_err(err)?,
        None => Groups::default(),
    };

    // Filters. `path_hint` is the headway against "list the whole registry just
    // to find the repo I'm already in" (#347): pass an absolute path, get back
    // only the repo(s) at or containing it.
    let path_hint = opt_str(args, "path_hint").and_then(|p| Path::new(p).canonicalize().ok());
    let want_health = opt_str(args, "health");
    let want_group = opt_str(args, "group");
    let want_name = opt_str(args, "name").map(str::to_ascii_lowercase);

    // (entry, health, groups) for every repo passing the filters.
    let rows: Vec<(&_, &'static str, Vec<&str>)> = registry
        .repos
        .iter()
        .filter_map(|e| {
            let health = match e.health() {
                RepoHealth::Indexed => "indexed",
                RepoHealth::Unindexed => "unindexed",
                RepoHealth::Missing => "missing",
            };
            if let Some(h) = want_health
                && !h.eq_ignore_ascii_case(health)
            {
                return None;
            }
            if let Some(n) = &want_name
                && !e.name.to_ascii_lowercase().contains(n)
            {
                return None;
            }
            let member_of: Vec<&str> = groups
                .groups
                .iter()
                .filter(|g| g.repos.iter().any(|r| r == &e.name))
                .map(|g| g.name.as_str())
                .collect();
            if let Some(g) = want_group
                && !member_of.contains(&g)
            {
                return None;
            }
            if let Some(p) = &path_hint
                && !std::iter::once(&e.root)
                    .chain(e.alt_roots.iter())
                    .filter_map(|r| r.canonicalize().ok())
                    .any(|root| p.starts_with(&root) || root.starts_with(p))
            {
                return None;
            }
            Some((e, health, member_of))
        })
        .collect();

    // Summary: counts only — the cheapest "what's indexed?" answer (#343).
    if args
        .get("summary")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let mut by_health: BTreeMap<&str, usize> = BTreeMap::new();
        let mut by_group: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, health, member_of) in &rows {
            *by_health.entry(*health).or_default() += 1;
            for g in member_of {
                *by_group.entry(*g).or_default() += 1;
            }
        }
        return Ok(json!({
            "total": rows.len(),
            "by_health": serde_json::to_value(&by_health).unwrap_or_default(),
            "by_group": serde_json::to_value(&by_group).unwrap_or_default(),
        }));
    }

    // Projection + pagination. Default `full` preserves the historical shape.
    let detail = opt_str(args, "detail").unwrap_or("full");
    let offset = opt_usize(args, "offset").unwrap_or(0);
    let limit = opt_usize(args, "limit").unwrap_or(usize::MAX);
    let repos: Vec<Value> = rows
        .iter()
        .skip(offset)
        .take(limit)
        .map(|&(e, health, ref member_of)| match detail {
            "minimal" => json!({ "name": e.name, "health": health }),
            "standard" => json!({
                "name": e.name,
                "root": e.root,
                "health": health,
                "groups": member_of.clone(),
            }),
            // "full" (default): every field, including #272 alt_roots and #233 ids.
            _ => json!({
                "name": e.name,
                "root": e.root,
                "alt_roots": e.alt_roots,
                "health": health,
                "groups": member_of.clone(),
                "ids": serde_json::to_value(&e.ids).unwrap_or_default(),
            }),
        })
        .collect();
    Ok(Value::Array(repos))
}

/// Resolve the index anchor path a call targets (#240). A `repo` argument is
/// first matched against the global registry by name; if no registered repo
/// matches, it is treated as a filesystem path to a repository root. With no
/// `repo` argument, the server's launch `default_db` is used — but a server
/// started without a launch repo (e.g. the globally-installed Claude Desktop
/// bundle, whose working directory is undefined) has no default, so the call is
/// rejected and the agent is told to name a repo. Either way the returned path
/// is the `.glyphtrail/graph.db` anchor beside which the index lives.
fn target_db(default_db: Option<&Path>, args: &Value) -> Result<PathBuf, String> {
    if let Some(selector) = opt_str(args, "repo") {
        if let Some(path) = default_registry_path()
            && let Ok(registry) = Registry::load(&path)
            && let Some(entry) = registry.get(selector)
        {
            return Ok(RepoPaths::new(entry.active_root()).db_path);
        }
        return Ok(RepoPaths::new(Path::new(selector)).db_path);
    }
    default_db.map(Path::to_path_buf).ok_or_else(|| {
        "no repository selected: this server has no default repo and could not \
         detect one from its working directory. Pass `repo` as the absolute path \
         of the directory you are working in, or a registered repo name; use \
         `list_repos` only to discover other repos."
            .to_string()
    })
}

/// Open the repo's LadybugDB graph store. `db` is the index anchor path
/// (`.glyphtrail/graph.db`); the LadybugDB index lives beside it at
/// `.glyphtrail/ladybug`. Returns a trait object so every tool stays decoupled
/// from the concrete store type.
fn open(db: &Path) -> Result<Box<dyn GraphStore>, String> {
    let dir = db
        .parent()
        .ok_or_else(|| format!("invalid index path {}", db.display()))?;
    let ladybug = dir.join("ladybug");
    if !ladybug.exists() {
        return Err(format!(
            "no index at {} — run `glyphtrail analyze` first",
            ladybug.display()
        ));
    }
    Ok(Box::new(LadybugStore::open(&ladybug).map_err(err)?))
}

fn neighbors_of(
    store: &dyn GraphStore,
    args: &Value,
    kind: EdgeKind,
    outgoing: bool,
) -> Result<Value, String> {
    let node = resolve_one(store, req_str(args, "name")?)?;
    let items = store
        .neighbors(&node.id.0, Some(kind), outgoing)
        .map_err(err)?;
    Ok(neighbors_json(&items))
}

fn operations_list(
    store: &dyn GraphStore,
    kind: NodeKind,
    args: &Value,
    with_handler: bool,
) -> Result<Value, String> {
    let proto = match opt_str(args, "protocol") {
        Some(p) => Some(Protocol::parse(p).ok_or_else(|| format!("unknown protocol '{p}'"))?),
        None => None,
    };
    let mut out = Vec::new();
    for (id, key) in store.operations_by_kind(kind).map_err(err)? {
        if proto.is_some_and(|p| key.protocol != p) {
            continue;
        }
        let node = store.get_node(&id.0).map_err(err)?;
        let handler = if with_handler {
            endpoint_handler(store, &id)?
        } else {
            None
        };
        out.push(operation_json(&key, node.as_ref(), handler));
    }
    Ok(Value::Array(out))
}

/// Impact-analysis tool: resolve seeds (symbol / file / change set), run the
/// shared engine via `store.classify_impact`, and return the `ImpactReport`
/// JSON — the same report the `glyphtrail impact` CLI emits.
/// Symbol outline of a file or directory (#293-adjacent): each definition with
/// its signature, sliced from source on demand. Returns per-file symbol arrays.
fn outline_tool(db: &Path, store: &dyn GraphStore, args: &Value) -> Result<Value, String> {
    // The DB lives at <repo>/.glyphtrail/graph.db; signatures are read from source.
    let root = db
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));
    let detail = opt_str(args, "detail")
        .map(Detail::parse)
        .unwrap_or(Detail::Standard);
    let prefix = opt_str(args, "path")
        .unwrap_or(".")
        .trim_start_matches("./")
        .trim_end_matches('/')
        .replace('\\', "/");
    let whole_repo = prefix.is_empty() || prefix == ".";

    let mut files: Vec<String> = store
        .all_files()
        .map_err(err)?
        .into_iter()
        .filter(|f| whole_repo || *f == prefix || f.starts_with(&format!("{prefix}/")))
        .collect();
    files.sort();

    let mut out = Vec::new();
    for file in files {
        let mut nodes: Vec<Node> = store
            .nodes_in_file(&file)
            .map_err(err)?
            .into_iter()
            .filter(|n| is_outline_kind(n.kind))
            .collect();
        nodes.sort_by_key(|n| n.span.map(|s| s.start_line).unwrap_or(0));
        let source = (detail != Detail::Minimal)
            .then(|| std::fs::read_to_string(root.join(&file)).ok())
            .flatten();
        let symbols: Vec<_> = nodes
            .iter()
            .map(|n| outline_symbol(n, source.as_deref(), detail))
            .collect();
        if !symbols.is_empty() {
            out.push(json!({ "file": file, "symbols": symbols }));
        }
    }
    Ok(Value::Array(out))
}

fn impact_tool(db: &Path, store: &dyn GraphStore, args: &Value) -> Result<Value, String> {
    // The DB lives at <repo>/.glyphtrail/graph.db; git seed modes need the repo root.
    let repo = db
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));

    let (seeds, removed, unresolved) = if let Some(name) = opt_str(args, "name") {
        let ids: Vec<NodeId> = store
            .find_by_name(name)
            .map_err(err)?
            .into_iter()
            .map(|n| n.id)
            .collect();
        if ids.is_empty() {
            return Err(format!("no symbol named '{name}' in the index"));
        }
        (ids, Vec::new(), Vec::new())
    } else {
        let spec = if let Some(f) = opt_str(args, "file") {
            ChangeSpec::Files(vec![f.to_string()])
        } else if let Some(fs) = str_array(args, "files") {
            ChangeSpec::Files(fs)
        } else if let Some(rev) = opt_str(args, "since") {
            ChangeSpec::Since(rev.to_string())
        } else if args.get("staged").and_then(Value::as_bool).unwrap_or(false) {
            ChangeSpec::Staged
        } else if args.get("diff").and_then(Value::as_bool).unwrap_or(false) {
            ChangeSpec::WorkingTree
        } else {
            return Err("provide 'name' or one of file/files/since/staged/diff".into());
        };
        let files = changed_files(repo, &spec).map_err(err)?;
        let set = seed_nodes(store, &files).map_err(err)?;
        (set.seeds, set.removed_files, set.unresolved_files)
    };

    let policy = policy_from_args(args)?;
    let items = store.classify_impact(&seeds, &policy).map_err(err)?;
    let report = ImpactReport::new(items, removed, unresolved);
    serde_json::to_value(&report).map_err(err)
}

/// Whether the `impact` arguments request a cross-repo (federated) blast radius.
fn is_federated(args: &Value) -> bool {
    args.get("downstream")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || opt_str(args, "group").is_some()
}

/// Build the traversal policy from the shared impact arguments.
fn policy_from_args(args: &Value) -> Result<ImpactPolicy, String> {
    let depth = opt_usize(args, "depth").unwrap_or(5);
    let cross_boundary = args
        .get("cross_boundary")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut policy = if cross_boundary {
        ImpactPolicy::cross_boundary(depth)
    } else {
        ImpactPolicy::in_process(depth)
    };
    if let Some(tokens) = str_array(args, "edges") {
        let refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
        policy.edges = glyphtrail_core::edge_rules(&refs)?;
    }
    if let Some(mc) = opt_str(args, "min_confidence") {
        policy.min_confidence = glyphtrail_core::parse_confidence(mc)?;
    }
    Ok(policy)
}

/// Analyze (index) a repository on demand (#240): resolve and vet the repo root
/// from the (already repo-resolved) index anchor and run the analysis pipeline,
/// so an agent can point the server at any repo and build/refresh its index
/// without a shell. Returns the `AnalyzeOutcome` JSON.
fn analyze_tool(db: &Path, args: &Value) -> Result<Value, String> {
    let root = resolve_analyze_root(db)?;
    let update = args.get("update").and_then(Value::as_bool).unwrap_or(false);
    let outcome = glyphtrail_analyze::run(&root, update).map_err(err)?;
    let mut value = serde_json::to_value(&outcome).map_err(err)?;
    // Render `languages` as a {lang: count} map, matching the `status` tool
    // instead of a YAML-awkward array of pairs (#346). The descending-by-count
    // order survives because serde_json's `preserve_order` is enabled in this
    // workspace (it pulls indexmap), so the Map iterates in insertion order.
    let languages: serde_json::Map<String, Value> = outcome
        .languages
        .into_iter()
        .map(|(lang, n)| (lang, json!(n)))
        .collect();
    value["languages"] = Value::Object(languages);
    Ok(value)
}

/// Resolve and vet the repository root an `analyze` call will index.
///
/// `analyze` is the only MCP tool that writes to disk — it creates (and walks
/// to build) `<root>/.glyphtrail/ladybug`. Left unconstrained, a call against
/// the server's ambient default repo (`--repo .`, i.e. whatever directory the
/// host launched the server in, which for a globally-registered server is
/// undefined and may be `$HOME` or `/`) would drop an index in, and crawl, an
/// arbitrary tree. So vet the target:
///
/// - a repo already in the global registry is allowed as-is (vetted, and may be
///   a deliberately-indexed non-git directory);
/// - otherwise, if the target sits inside a git repository, index that repo's
///   root — so a subdirectory target (or an ambient CWD deep in a tree) still
///   indexes the right thing;
/// - otherwise, if the target already holds a `.glyphtrail` index, keep using it;
/// - otherwise refuse, and tell the agent to name a target explicitly.
fn resolve_analyze_root(target_db: &Path) -> Result<PathBuf, String> {
    let root = target_db
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| format!("invalid repo path {}", target_db.display()))?;
    let canonical = root
        .canonicalize()
        .map_err(|e| format!("repo path {} is not accessible: {e}", root.display()))?;

    if is_registered_root(&canonical) {
        return Ok(canonical);
    }
    if let Some(git_root) = enclosing_git_root(&canonical) {
        return Ok(git_root);
    }
    if canonical.join(INDEX_DIR).join("ladybug").exists() {
        return Ok(canonical);
    }
    Err(format!(
        "refusing to index {} — it is not inside a git repository and has no \
         existing index, so it is probably not a repository root. Pass `repo` \
         with a registered name (see the `list_repos` tool) or an explicit \
         repository path.",
        canonical.display()
    ))
}

/// Whether `canonical` is the active root of a repo in the global registry.
fn is_registered_root(canonical: &Path) -> bool {
    default_registry_path()
        .and_then(|p| Registry::load(&p).ok())
        .is_some_and(|reg| {
            reg.repos
                .iter()
                .any(|e| e.active_root().canonicalize().is_ok_and(|r| r == canonical))
        })
}

/// The nearest ancestor of `path` (inclusive) that contains a `.git` entry — a
/// directory for a normal checkout, a file for a worktree/submodule.
fn enclosing_git_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| p.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Best-effort: the glyphtrail repo the server is *running in*, used as the
/// launch default when started without an explicit `--repo` (#347). Resolves the
/// process working directory (or its enclosing git root) to a registered or
/// already-indexed repo, so an agent working inside a repo never has to discover
/// or name the path it is already in. `None` when the working directory is not a
/// glyphtrail repo (e.g. the Claude Desktop bundle's undefined CWD), so callers
/// fall back to requiring an explicit `repo`.
pub(crate) fn infer_cwd_repo() -> Option<PathBuf> {
    let usable = |p: &Path| is_registered_root(p) || p.join(INDEX_DIR).join("ladybug").exists();
    let cwd = std::env::current_dir().ok()?.canonicalize().ok()?;
    if usable(&cwd) {
        return Some(cwd);
    }
    let root = enclosing_git_root(&cwd)?;
    usable(&root).then_some(root)
}

/// Cross-repo impact (#222/#223): seed in the current repo and traverse into
/// downstream repos across the package boundary, returning the per-repo
/// `FederatedReport`. Opens its own member stores via the registry.
fn federated_impact_tool(db: &Path, args: &Value) -> Result<Value, String> {
    let repo = db
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));
    let scope = match opt_str(args, "group") {
        Some(g) => FederationScope::Group(g.to_string()),
        None => FederationScope::Registry,
    };
    let seeds = seed_spec_from_args(args)?;
    let policy = policy_from_args(args)?;
    let deep = args.get("deep").and_then(Value::as_bool).unwrap_or(false);
    let report = federated_impact(repo, &scope, seeds, &policy, deep).map_err(err)?;
    serde_json::to_value(&report).map_err(err)
}

/// Build the seed spec from the impact arguments (a symbol name, else a git
/// change set), matching the single-repo tool's precedence.
fn seed_spec_from_args(args: &Value) -> Result<SeedSpec, String> {
    if let Some(name) = opt_str(args, "name") {
        return Ok(SeedSpec::Name(name.to_string()));
    }
    let spec = if let Some(f) = opt_str(args, "file") {
        ChangeSpec::Files(vec![f.to_string()])
    } else if let Some(fs) = str_array(args, "files") {
        ChangeSpec::Files(fs)
    } else if let Some(rev) = opt_str(args, "since") {
        ChangeSpec::Since(rev.to_string())
    } else if args.get("staged").and_then(Value::as_bool).unwrap_or(false) {
        ChangeSpec::Staged
    } else if args.get("diff").and_then(Value::as_bool).unwrap_or(false) {
        ChangeSpec::WorkingTree
    } else {
        return Err("provide 'name' or one of file/files/since/staged/diff".into());
    };
    Ok(SeedSpec::Change(spec))
}

/// Endpoints matching the `path` (+ optional `method`) arguments.
fn matched_endpoints(
    store: &dyn GraphStore,
    args: &Value,
) -> Result<Vec<(NodeId, OperationKey)>, String> {
    let path = req_str(args, "path")?;
    let method = match opt_str(args, "method") {
        Some(m) => Some(HttpMethod::parse(m).ok_or_else(|| format!("unknown HTTP method '{m}'"))?),
        None => None,
    };
    let endpoints = store.operations_by_kind(NodeKind::Endpoint).map_err(err)?;
    let matched = operations_matching(&endpoints, method, path);
    if matched.is_empty() {
        return Err(format!("no endpoint matching '{path}'"));
    }
    Ok(matched)
}

fn endpoint_handler(store: &dyn GraphStore, id: &NodeId) -> Result<Option<String>, String> {
    Ok(store
        .neighbors(&id.0, Some(EdgeKind::Handles), false)
        .map_err(err)?
        .into_iter()
        .next()
        .map(|(n, _, _)| n.qualified_name))
}

fn neighbor_nodes(
    store: &dyn GraphStore,
    id: &NodeId,
    kind: EdgeKind,
    outgoing: bool,
) -> Result<Value, String> {
    let nodes: Vec<Node> = store
        .neighbors(&id.0, Some(kind), outgoing)
        .map_err(err)?
        .into_iter()
        .map(|(n, _, _)| n)
        .collect();
    Ok(nodes_json(&nodes))
}

fn operation_json(key: &OperationKey, node: Option<&Node>, handler: Option<String>) -> Value {
    json!({
        "protocol": key.protocol.as_str(),
        "method": key.method.map(|m| m.as_str()),
        "path": key.path,
        "file": node.map(|n| n.file.clone()),
        "line": node.and_then(|n| n.span.map(|s| s.start_line)),
        "handler": handler,
    })
}

fn nodes_json(nodes: &[Node]) -> Value {
    Value::Array(nodes.iter().map(node_json).collect())
}

fn node_json(n: &Node) -> Value {
    serde_json::to_value(n).unwrap_or(Value::Null)
}

fn neighbors_json(items: &[(Node, EdgeKind, Confidence)]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|(n, e, c)| {
                json!({ "node": node_json(n), "edge": e.as_str(), "confidence": c.as_str() })
            })
            .collect(),
    )
}

fn resolve_one(store: &dyn GraphStore, name: &str) -> Result<Node, String> {
    store
        .find_by_name(name)
        .map_err(err)?
        .into_iter()
        .next()
        .ok_or_else(|| format!("no symbol named '{name}' in the index"))
}

fn tool(name: &str, description: &str, mut properties: Value, required: &[&str]) -> Value {
    // Every tool accepts an optional `repo` selector so a single MCP server can
    // be pointed at any repository per call, not just the one it launched in
    // (#240). list_repos ignores it harmlessly.
    if let Some(obj) = properties.as_object_mut() {
        obj.insert(
            "repo".to_string(),
            json!({
                "type": "string",
                "description": "Repository to target: the absolute path of the directory you are working in, or a registered repo name. Defaults to the repo the server was launched in (or inferred from its working directory), so you usually omit it. `list_repos` is only for discovering OTHER repos — you do not need it for the repo you are already in."
            }),
        );
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
        },
        // Default to read-only; `definitions` flips the one writer (analyze) so an
        // agent can tell a mutating tool from a query without running it (#362).
        "annotations": {
            "readOnlyHint": true,
            "idempotentHint": true,
            "destructiveHint": false,
        },
    })
}

/// Render a tool result's text payload.
///
/// Agent-facing output defaults to YAML: lower-boilerplate than JSON, so it costs
/// fewer tokens for the model to read. Set `GLYPHTRAIL_MCP_FORMAT=json` to restore
/// pretty-JSON for clients that expect it. (See #109.)
fn text_result(value: &Value, is_error: bool) -> Value {
    let as_json = std::env::var("GLYPHTRAIL_MCP_FORMAT")
        .map(|f| f.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    let text = if as_json {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    } else {
        serde_norway::to_string(value).unwrap_or_else(|_| value.to_string())
    };
    json!({ "content": [ { "type": "text", "text": text } ], "isError": is_error })
}

/// Whether the opt-in project-reporting tool is advertised + allowed (#370). Off
/// by default; set `GLYPHTRAIL_MCP_FILE_ISSUE` to a truthy value to enable.
fn file_issue_enabled() -> bool {
    std::env::var("GLYPHTRAIL_MCP_FILE_ISSUE")
        .map(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// The `file_issue` tool definition (#370): guidance for reporting a glyphtrail
/// bug/idea, deliberately doing nothing itself. Built directly (not via `tool()`)
/// because it is project-level, not repo-scoped, so it carries no `repo` arg.
fn file_issue_tool() -> Value {
    json!({
        "name": "file_issue",
        "description": "Guidance for reporting a glyphtrail bug or idea to its \
             GitHub project. It does NOT file anything — it returns instructions \
             and a provenance line; you act with your own tools. ALWAYS search \
             existing OPEN and CLOSED issues first and prefer commenting on a \
             match over opening a duplicate.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Proposed issue title." },
                "body": { "type": "string", "description": "Proposed issue body (Markdown)." }
            },
            "required": ["title"],
        },
        // Returns text only; no mutation, no network.
        "annotations": { "readOnlyHint": true, "idempotentHint": true, "destructiveHint": false },
    })
}

/// Build the `file_issue` response: where and how to report, plus the proposed
/// title/body with an agent-provenance footer. Performs no network action (#370).
fn file_issue_guidance(args: &Value) -> Value {
    const REPO: &str = env!("CARGO_PKG_REPOSITORY");
    let title = opt_str(args, "title").unwrap_or_default();
    let body = opt_str(args, "body").unwrap_or_default();
    let provenance = "_Reported via `glyphtrail-mcp` by an automated agent (not a human); review before acting._";
    let q = url_query(title);
    json!({
        "project": REPO,
        "files_nothing": true,
        "steps": [
            "This returns guidance only — use your own tools (e.g. the `gh` CLI) to act.",
            format!("Search existing issues, OPEN and CLOSED: {REPO}/issues?q=is%3Aissue+{q}"),
            "If a matching issue exists, add a comment there instead of opening a duplicate.",
            "Only file when the information is accurate and not already covered.",
            "Include the `provenance` line so the report is identifiable as agent-filed.",
        ],
        "new_issue_url": format!("{REPO}/issues/new"),
        "provenance": provenance,
        "title": title,
        "body": if body.is_empty() {
            provenance.to_string()
        } else {
            format!("{body}\n\n{provenance}")
        },
    })
}

/// Minimal percent-encoding for a GitHub issue-search query string (#370).
fn url_query(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b' ' => out.push('+'),
            b if b.is_ascii_alphanumeric() => out.push(b as char),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The OTHER repos this one declares cross-repo links to (`[[links]]`), so the
/// `status` tool can point an agent at code that lives in a linked/submodule
/// repo (#365). Deduplicated; empty when there are none or no config.
fn declared_links(root: Option<&Path>) -> Vec<String> {
    let mut repos: Vec<String> = root
        .and_then(|r| glyphtrail_core::Config::load(r).ok())
        .map(|cfg| {
            cfg.links
                .iter()
                .flat_map(|l| [l.from.repo.clone(), l.to.repo.clone()])
                .flatten()
                .filter(|r| r != "." && !r.trim().is_empty())
                .collect()
        })
        .unwrap_or_default();
    repos.sort();
    repos.dedup();
    repos
}

/// A stale-index warning for the repo behind `db`, or `None` when the index
/// still matches the working tree. Reuses the same `index_staleness` check the
/// `status` tool reports (#313), so every store-based tool carries the signal.
fn stale_warning_for(db: &Path, store: &dyn GraphStore) -> Option<String> {
    let root = db.parent().and_then(Path::parent)?;
    stale_warning(&glyphtrail_analyze::index_staleness(root, store))
}

/// A strong, agent-facing nudge to re-`analyze` — only when the index is
/// actually stale (#345). `Fresh`/`Unknown` produce no noise.
fn stale_warning(staleness: &glyphtrail_analyze::Staleness) -> Option<String> {
    match staleness {
        glyphtrail_analyze::Staleness::Stale(why) => Some(format!(
            "⚠ STALE INDEX: {why}. These results may be wrong — run `analyze` \
             (the analyze tool, or `glyphtrail analyze`) for this repo before \
             trusting them."
        )),
        _ => None,
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required string argument '{key}'"))
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn opt_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|v| v as usize)
}

/// A non-empty array-of-strings argument, e.g. `files` or `edges`.
fn str_array(args: &Value, key: &str) -> Option<Vec<String>> {
    let arr = args.get(key)?.as_array()?;
    let out: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    (!out.is_empty()).then_some(out)
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use glyphtrail_core::{Confidence, Edge, Span};

    // Build a tiny graph in a temp index dir; return the `graph.db` anchor path
    // (the LadybugDB index sits beside it at `<dir>/ladybug`).
    fn build_db(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("glyphtrail-mcp-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = LadybugStore::open(&dir.join("ladybug")).unwrap();
        let mk = |id: &str, name: &str, file: &str, kind: NodeKind| Node {
            id: NodeId(id.into()),
            kind,
            name: name.into(),
            qualified_name: name.into(),
            file: file.into(),
            language: Some("rust".into()),
            span: Some(Span {
                start_byte: 0,
                end_byte: 1,
                start_line: 3,
                end_line: 4,
            }),
            doc: None,
            signature: None,
        };
        store
            .insert_graph(
                &[
                    mk("a", "caller", "a.rs", NodeKind::Function),
                    mk("b", "callee", "b.rs", NodeKind::Function),
                ],
                &[Edge {
                    src: NodeId("a".into()),
                    dst: NodeId("b".into()),
                    kind: EdgeKind::Calls,
                    confidence: Confidence::Extracted,
                }],
            )
            .unwrap();
        dir.join("graph.db")
    }

    #[test]
    fn missing_index_is_a_tool_error() {
        let res = call(
            Some(Path::new("/nonexistent/glyphtrail.db")),
            "status",
            &json!({}),
        );
        check!(res["isError"] == json!(true));
    }

    // #165: the MCP tools open the LadybugDB index that sits beside the
    // `graph.db` anchor path.
    #[test]
    fn opens_ladybug_index_beside_graph_db() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let index_dir = std::env::temp_dir().join(format!("glyphtrail-mcp-lb-{nanos}/.glyphtrail"));
        std::fs::create_dir_all(&index_dir).unwrap();
        {
            let mut store = LadybugStore::open(&index_dir.join("ladybug")).unwrap();
            store
                .insert_graph(
                    &[Node {
                        id: NodeId("a".into()),
                        kind: NodeKind::Function,
                        name: "lonely".into(),
                        qualified_name: "lonely".into(),
                        file: "a.rs".into(),
                        language: Some("rust".into()),
                        span: None,
                        doc: None,
                        signature: None,
                    }],
                    &[],
                )
                .unwrap();
        }
        // graph.db itself does not exist; open() must fall through to ladybug.
        let res = call(
            Some(index_dir.join("graph.db").as_path()),
            "definition",
            &json!({ "name": "lonely" }),
        );
        check!(res["isError"] == json!(false));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_norway::from_str(text).unwrap();
        check!(parsed[0]["name"] == json!("lonely"));
        std::fs::remove_dir_all(index_dir.parent().unwrap()).ok();
    }

    #[test]
    fn callers_tool_returns_the_caller() {
        let db = build_db("callers");
        let res = call(Some(&db), "callers", &json!({ "name": "callee" }));
        check!(res["isError"] == json!(false));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_norway::from_str(text).unwrap();
        check!(parsed[0]["node"]["name"] == json!("caller"));
        check!(parsed[0]["edge"] == json!("calls"));
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
    }

    // search is case-insensitive by default; `case_sensitive: true` is exact (#367).
    #[test]
    fn search_tool_is_case_insensitive_by_default() {
        let db = build_db("search-case");
        let hits = |query: &str, case_sensitive: bool| -> usize {
            let res = call(
                Some(&db),
                "search",
                &json!({ "query": query, "case_sensitive": case_sensitive }),
            );
            let text = res["content"][0]["text"].as_str().unwrap();
            serde_norway::from_str::<Value>(text)
                .unwrap()
                .as_array()
                .unwrap()
                .len()
        };
        check!(hits("CALLER", false) >= 1);
        check!(hits("CALLER", true) == 0);
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
    }

    #[test]
    fn status_tool_reports_counts() {
        let db = build_db("status");
        let res = call(Some(&db), "status", &json!({}));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_norway::from_str(text).unwrap();
        check!(parsed["nodes"] == json!(2));
        check!(parsed["edges"] == json!(1));
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
    }

    // status reports which build is answering (#351): version + commit + built.
    #[test]
    fn status_reports_build_provenance() {
        let db = build_db("provenance");
        let res = call(Some(&db), "status", &json!({}));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_norway::from_str(text).unwrap();
        check!(parsed["version"] == json!(env!("CARGO_PKG_VERSION")));
        check!(!parsed["commit"].as_str().unwrap().is_empty());
        check!(parsed["built"].as_str().unwrap().contains('T'));
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
    }

    // #365: status surfaces the OTHER repos this one links to via [[links]].
    #[test]
    fn declared_links_lists_link_targets() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gt-links-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("glyphtrail.toml"),
            "[[links]]\nto = { repo = \"vendor/pop-apple2/\" }\n",
        )
        .unwrap();
        check!(declared_links(Some(&dir)) == vec!["vendor/pop-apple2/".to_string()]);
        check!(declared_links(None).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    // #370: the opt-in file_issue tool returns guidance and files nothing.
    #[test]
    fn file_issue_guidance_reports_without_acting() {
        let g = file_issue_guidance(&json!({ "title": "outline is slow", "body": "Repro: ..." }));
        check!(g["files_nothing"] == json!(true));
        check!(g["project"].as_str().unwrap().contains("glyphtrail"));
        check!(
            g["new_issue_url"]
                .as_str()
                .unwrap()
                .ends_with("/issues/new")
        );
        check!(g["title"] == json!("outline is slow"));
        check!(g["body"].as_str().unwrap().contains("glyphtrail-mcp"));
        let steps = g["steps"].as_array().unwrap();
        check!(steps.iter().any(|s| s.as_str().unwrap().contains("CLOSED")));
    }

    // file_issue is advertised exactly when enabled (off by default). Asserting
    // the iff is deterministic regardless of the ambient env var, with no env
    // mutation (the workspace forbids `unsafe`).
    #[test]
    fn file_issue_tool_advertised_only_when_enabled() {
        let advertised = definitions(true)
            .iter()
            .any(|d| d["name"] == json!("file_issue"));
        check!(advertised == file_issue_enabled());
    }

    // #223: a registry-level tool needs no per-repo index. It reads the global
    // registry (empty in CI) and returns an array, even with a bogus db path.
    #[test]
    fn list_repos_needs_no_store_and_returns_an_array() {
        let res = call(None, "list_repos", &json!({}));
        check!(res["isError"] == json!(false));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_norway::from_str(text).unwrap();
        check!(parsed.is_array());
    }

    // `summary` collapses the (potentially huge) registry to counts (#343).
    #[test]
    fn list_repos_summary_returns_counts_not_a_list() {
        let res = call(None, "list_repos", &json!({ "summary": true }));
        check!(res["isError"] == json!(false));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_norway::from_str(text).unwrap();
        check!(parsed["total"].is_number());
        check!(parsed["by_health"].is_object());
    }

    // `detail: minimal` drops everything but name + health, whatever is indexed.
    #[test]
    fn list_repos_detail_minimal_omits_root() {
        let res = call(None, "list_repos", &json!({ "detail": "minimal" }));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_norway::from_str(text).unwrap();
        check!(parsed.is_array());
        check!(
            parsed
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r.get("root").is_none() && r["name"].is_string())
        );
    }

    // `path_hint` at a directory that is no registered repo lists nothing —
    // the basis for resolving "the repo I'm in" without dumping the registry.
    #[test]
    fn list_repos_path_hint_unrelated_dir_is_empty() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("gt-listrepos-{nanos}"));
        std::fs::create_dir_all(&tmp).unwrap();
        let res = call(
            None,
            "list_repos",
            &json!({ "path_hint": tmp.to_str().unwrap() }),
        );
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_norway::from_str(text).unwrap();
        check!(parsed.as_array().unwrap().is_empty());
        std::fs::remove_dir_all(&tmp).ok();
    }

    // #240: a `repo` argument redirects a call to another repository by path,
    // overriding the server's launch repo (which here points nowhere).
    #[test]
    fn repo_argument_targets_a_different_repo_by_path() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("glyphtrail-mcp-target-{nanos}"));
        let index_dir = root.join(".glyphtrail");
        std::fs::create_dir_all(&index_dir).unwrap();
        {
            let mut store = LadybugStore::open(&index_dir.join("ladybug")).unwrap();
            store
                .insert_graph(
                    &[Node {
                        id: NodeId("z".into()),
                        kind: NodeKind::Function,
                        name: "zonk".into(),
                        qualified_name: "zonk".into(),
                        file: "z.rs".into(),
                        language: Some("rust".into()),
                        span: None,
                        doc: None,
                        signature: None,
                    }],
                    &[],
                )
                .unwrap();
        }
        // The launch db points nowhere; the `repo` arg redirects to `root`.
        let res = call(
            Some(Path::new("/nonexistent/graph.db")),
            "definition",
            &json!({ "name": "zonk", "repo": root.to_str().unwrap() }),
        );
        check!(res["isError"] == json!(false));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_norway::from_str(text).unwrap();
        check!(parsed[0]["name"] == json!("zonk"));
        std::fs::remove_dir_all(&root).ok();
    }

    // #240: the analyze tool indexes a repo at an arbitrary path on demand,
    // returning the outcome — no shell, no server restart. The target is a git
    // checkout, so the write-path guard lets it through (and a subdirectory
    // target resolves up to this root).
    #[test]
    fn analyze_tool_indexes_a_repo() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("glyphtrail-mcp-analyze-{nanos}"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap(); // make it a repo
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();

        let res = call(
            Some(Path::new("/nonexistent/graph.db")),
            "analyze",
            &json!({ "repo": root.to_str().unwrap() }),
        );
        check!(res["isError"] == json!(false));
        let text = res["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_norway::from_str(text).unwrap();
        check!(parsed["files"].as_u64().unwrap() >= 1);
        check!(parsed["nodes"].as_u64().unwrap() >= 1);
        // languages is a {lang: count} map (#346), not an array of pairs.
        check!(parsed["languages"].is_object());
        check!(parsed["languages"]["rust"].as_u64().unwrap() >= 1);
        std::fs::remove_dir_all(&root).ok();
    }

    // The write-path guard refuses to index a directory that is neither a git
    // repository nor already indexed — so a globally-launched server with an
    // ambient default repo can't crawl and litter `$HOME` or `/`.
    #[test]
    fn analyze_tool_refuses_non_repo_location() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("glyphtrail-mcp-norepo-{nanos}"));
        std::fs::create_dir_all(&root).unwrap(); // no .git, no .glyphtrail index

        let res = call(
            Some(Path::new("/nonexistent/graph.db")),
            "analyze",
            &json!({ "repo": root.to_str().unwrap() }),
        );
        check!(res["isError"] == json!(true));
        let text = res["content"][0]["text"].as_str().unwrap();
        check!(text.contains("refusing to index"));
        // Nothing was written into the unvetted directory.
        check!(!root.join(".glyphtrail").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unknown_tool_errors() {
        let db = build_db("unknown");
        let res = call(Some(&db), "nope", &json!({}));
        check!(res["isError"] == json!(true));
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
    }

    // The stale-index nudge (#345) fires only when stale, and names the fix.
    #[test]
    fn stale_warning_urges_reanalyze_only_when_stale() {
        use glyphtrail_analyze::Staleness;
        check!(stale_warning(&Staleness::Fresh).is_none());
        check!(stale_warning(&Staleness::Unknown).is_none());
        let w = stale_warning(&Staleness::Stale("repo is on a new commit".into())).unwrap();
        check!(w.contains("STALE"));
        check!(w.contains("analyze"));
        check!(w.contains("repo is on a new commit"));
    }

    // A non-stale index adds no second content block — data stays at content[0].
    #[test]
    fn non_stale_index_adds_no_warning_block() {
        let db = build_db("nonstale-warn");
        let res = call(Some(&db), "status", &json!({}));
        check!(res["content"].as_array().unwrap().len() == 1);
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
    }

    // The advertised schema adapts to the launch mode: with a default repo,
    // `repo` is optional; without one, it is required on every repo-scoped tool
    // (but never on list_repos, the discovery escape hatch).
    #[test]
    fn analyze_is_the_only_writing_tool() {
        // Every tool is read-only except analyze, which rebuilds the index (#362).
        // Iterate all of them so a stray write hint can't slip through.
        for def in definitions(true) {
            let name = def["name"].as_str().unwrap().to_string();
            let read_only = def["annotations"]["readOnlyHint"] == json!(true);
            check!(
                read_only == (name != "analyze"),
                "{name} has the wrong readOnlyHint"
            );
        }
    }

    #[test]
    fn schema_marks_repo_required_only_without_a_default_repo() {
        let required_of = |defs: &[Value], name: &str| -> Vec<String> {
            let tool = defs.iter().find(|d| d["name"] == json!(name)).unwrap();
            tool["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };

        let with_default = definitions(true);
        check!(!required_of(&with_default, "search").contains(&"repo".to_string()));
        check!(!required_of(&with_default, "status").contains(&"repo".to_string()));

        let no_default = definitions(false);
        check!(required_of(&no_default, "search").contains(&"repo".to_string()));
        check!(required_of(&no_default, "status").contains(&"repo".to_string()));
        // The seed arg stays required alongside the promoted repo arg.
        check!(required_of(&no_default, "search").contains(&"query".to_string()));
        // Discovery tool never requires a repo.
        check!(!required_of(&no_default, "list_repos").contains(&"repo".to_string()));
        // The repo description is tightened to flag the requirement.
        let search = no_default
            .iter()
            .find(|d| d["name"] == json!("search"))
            .unwrap();
        check!(
            search["inputSchema"]["properties"]["repo"]["description"]
                .as_str()
                .unwrap()
                .contains("REQUIRED")
        );
    }

    // A server started without a launch repo (no `--repo`, as the Claude Desktop
    // bundle runs) has no default: a tool call that names no `repo` is rejected,
    // steering the agent to provide one rather than guessing a directory.
    #[test]
    fn no_launch_repo_requires_repo_on_the_call() {
        let res = call(None, "status", &json!({}));
        check!(res["isError"] == json!(true));
        let text = res["content"][0]["text"].as_str().unwrap();
        check!(text.contains("no repository selected"));

        // list_repos still works with no repo, so the agent can discover one.
        let repos = call(None, "list_repos", &json!({}));
        check!(repos["isError"] == json!(false));
    }
}
