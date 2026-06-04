//! Atlas MCP tools (#335) — recall over the private, local-only archaeology
//! index (`glyphtrail atlas mcp`). Mirrors the per-repo MCP wiring but on the
//! atlas `LadybugStore`: a visibility-gated `atlas_timeline`, a cheap
//! `atlas_status`, and a `atlas_resolve` bridge that walks an atlas hit (repo +
//! file) through the registry into that repo's own graph, so an agent can chain
//! recall -> resolve -> detail in one session.

use std::path::Path;

use glyphtrail_core::config::RepoPaths;
use glyphtrail_core::{
    AtlasConfig, Embedder, HashingEmbedder, NodeId, Registry, RegistryEntry, TimelineQuery, Window,
    author_scope_label, cosine, default_registry_path, filter_timeline, timeline_value, vec_table,
};
use glyphtrail_store::{GraphStore, LadybugStore};
use serde_json::{Value, json};

use crate::tools::text_result;

/// The atlas tool set advertised by `glyphtrail atlas mcp`.
pub fn definitions() -> Vec<Value> {
    vec![
        status_tool(),
        timeline_tool(),
        topics_tool(),
        similar_tool(),
        resolve_tool(),
    ]
}

/// Run an atlas tool, formatting the result like the per-repo tools (YAML by
/// default, JSON under `GLYPHTRAIL_MCP_FORMAT=json`).
pub fn call(atlas_dir: &Path, name: &str, args: &Value) -> Value {
    match dispatch(atlas_dir, name, args) {
        Ok(value) => text_result(&value, false),
        Err(message) => text_result(&json!({ "error": message }), true),
    }
}

fn dispatch(atlas_dir: &Path, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "atlas_status" => status(atlas_dir),
        "atlas_timeline" => timeline(atlas_dir, args),
        "atlas_topics" => topics(atlas_dir),
        "atlas_similar" => similar(atlas_dir, args),
        "atlas_resolve" => resolve(args),
        other => Err(format!("unknown atlas tool: {other}")),
    }
}

/// Open the atlas store, or explain that atlas is disabled.
fn atlas_store(atlas_dir: &Path) -> Result<LadybugStore, String> {
    let lb = atlas_dir.join("ladybug");
    if !lb.exists() {
        return Err("atlas is disabled; run `glyphtrail atlas init` first".into());
    }
    LadybugStore::open(&lb).map_err(|e| e.to_string())
}

/// The registry, or an empty one when it can't be located.
fn registry() -> Registry {
    default_registry_path()
        .and_then(|p| Registry::load(&p).ok())
        .unwrap_or_default()
}

fn status(atlas_dir: &Path) -> Result<Value, String> {
    let store = atlas_store(atlas_dir)?;
    let stats = store.stats().map_err(|e| e.to_string())?;
    let commits = store.commit_count().map_err(|e| e.to_string())?;
    let embeds = store.embedding_index().map_err(|e| e.to_string())?;
    let cfg = AtlasConfig::load(atlas_dir).map_err(|e| e.to_string())?;
    Ok(json!({
        "enabled": true,
        "nodes": stats.nodes,
        "edges": stats.edges,
        "commits": commits,
        "embeddings": embeds
            .iter()
            .map(|(space, model, count, dim)| {
                json!({ "space": space, "model": model, "count": count, "dim": dim })
            })
            .collect::<Vec<_>>(),
        "window": cfg.window.label(),
    }))
}

fn timeline(atlas_dir: &Path, args: &Value) -> Result<Value, String> {
    let cfg = AtlasConfig::load(atlas_dir).map_err(|e| e.to_string())?;
    // Effective window: tool args override the config window.
    let window = Window {
        earliest: opt_str(args, "since").or_else(|| cfg.window.earliest.clone()),
        latest: opt_str(args, "until").or_else(|| cfg.window.latest.clone()),
    };
    let (since, until) = window
        .epoch_bounds()
        .map_err(|d| format!("invalid date: {d}"))?;
    let query = TimelineQuery {
        repo: opt_str(args, "repo"),
        author: opt_str(args, "author"),
        // Scope to me, matching the CLI: the configured [me], else git's
        // user.email — so an unset [me] doesn't silently widen to everyone.
        me: resolve_me(&cfg.me),
        public_only: false, // local recall: private shows; proprietary + unregistered hidden
        include_restricted: args
            .get("include_proprietary")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        limit: opt_usize(args, "limit").unwrap_or(50),
    };
    let store = atlas_store(atlas_dir)?;
    let topic = opt_str(args, "topic");
    let rows = store
        .atlas_timeline(since, until, topic.as_deref())
        .map_err(|e| e.to_string())?;
    let tl = filter_timeline(rows, &registry(), &query);
    Ok(timeline_value(
        &tl,
        &window.label(),
        &author_scope_label(&query),
    ))
}

fn topics(atlas_dir: &Path) -> Result<Value, String> {
    let store = atlas_store(atlas_dir)?;
    let topics = store.atlas_topics().map_err(|e| e.to_string())?;
    Ok(Value::Array(
        topics
            .iter()
            .map(|(name, count)| json!({ "topic": name, "commits": count }))
            .collect(),
    ))
}

/// Rank repos similar to a repo name (or, for text, a free-text query) by cosine
/// over the stored embeddings, gating restricted repos out of the result. The MCP
/// server makes no network calls, so a free-text query only works over a locally
/// (lexical) embedded index; repo↔repo similarity always works.
fn similar(atlas_dir: &Path, args: &Value) -> Result<Value, String> {
    let store = atlas_store(atlas_dir)?;
    let query = req_str(args, "query")?;
    let graph = args.get("graph").and_then(Value::as_bool).unwrap_or(false);
    let limit = opt_usize(args, "limit").unwrap_or(10);
    let include_restricted = args
        .get("include_restricted")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Pick the (space, model) namespace: graph vs text, model from `model` arg else
    // the active/default one. Read just that namespace — models never mix.
    let reg = registry();
    let space = if graph { "graph" } else { "text" };
    let model = resolve_model(&store, space, args.get("model").and_then(Value::as_str))?;
    let id_of = |name: &str| -> NodeId {
        if graph {
            NodeId::derive(&["repo_graph", name])
        } else {
            NodeId::derive(&["repo", name])
        }
    };
    let by_id: std::collections::HashMap<String, &RegistryEntry> =
        reg.repos.iter().map(|e| (id_of(&e.name).0, e)).collect();
    let space_vecs = store
        .embeddings_for(space, &model)
        .map_err(|e| e.to_string())?;
    if space_vecs.is_empty() {
        return Err(format!("no {space} embeddings for model '{model}'"));
    }
    let dim = space_vecs[0].vector.len();

    let self_id = id_of(query).0;
    let (qvec, self_id) = if let Some(e) = space_vecs.iter().find(|e| e.node_id.0 == self_id) {
        (e.vector.clone(), Some(self_id))
    } else if graph {
        return Err("graph similarity compares repositories; pass a registered repo name".into());
    } else if model == "lexical-hash-v1" {
        (HashingEmbedder::new(dim).embed(query), None)
    } else {
        return Err(
            "free-text similarity isn't available over an API-embedded index here (the MCP \
             server makes no network calls); pass a registered repo name instead"
                .into(),
        );
    };

    // Prefer this (space,model)'s HNSW index; fall back to an exact in-Rust scan.
    let table = vec_table(space, &model);
    let candidates: Vec<(String, f32)> = store
        .load_vector_ext()
        .then(|| store.vector_knn(&table, &qvec, space_vecs.len()).ok())
        .flatten()
        .filter(|h| !h.is_empty())
        .map(|h| h.into_iter().map(|(id, sim)| (id.0, sim)).collect())
        .unwrap_or_else(|| {
            space_vecs
                .iter()
                .map(|e| (e.node_id.0.clone(), cosine(&qvec, &e.vector)))
                .collect()
        });

    let mut hidden = 0usize;
    let mut scored: Vec<(f32, String, &'static str)> = Vec::new();
    for (id, sim) in &candidates {
        if Some(id) == self_id.as_ref() {
            continue;
        }
        // Only return repos the registry can name — the MCP server can't recover a
        // name from the one-way node id, so a deregistered/stray row is skipped.
        let Some(entry) = by_id.get(id) else {
            continue;
        };
        if entry.visibility.is_restricted() && !include_restricted {
            hidden += 1;
            continue;
        }
        scored.push((*sim, entry.name.clone(), entry.visibility.as_str()));
    }
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    scored.truncate(limit);

    Ok(json!({
        "query": query,
        "mode": space,
        "model": model,
        "hidden_restricted": hidden,
        "matches": scored
            .iter()
            .map(|(score, name, vis)| json!({ "repo": name, "score": score, "visibility": vis }))
            .collect::<Vec<_>>(),
    }))
}

/// Resolve which embedding model to search in `space`: the explicit one, else the
/// active (last-embedded) one, else the sole stored model — erroring with choices.
fn resolve_model(
    store: &LadybugStore,
    space: &str,
    explicit: Option<&str>,
) -> Result<String, String> {
    let models: Vec<String> = store
        .embedding_index()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|(sp, ..)| sp == space)
        .map(|(_, m, ..)| m)
        .collect();
    if models.is_empty() {
        return Err(format!(
            "no {space} embeddings yet; run the matching embed command"
        ));
    }
    if let Some(m) = explicit {
        return if models.iter().any(|x| x == m) {
            Ok(m.to_string())
        } else {
            Err(format!(
                "no {space} embeddings for model '{m}'; available: {}",
                models.join(", ")
            ))
        };
    }
    if let Some(active) = store
        .get_meta(&format!("active_model_{space}"))
        .map_err(|e| e.to_string())?
        && models.iter().any(|x| x == &active)
    {
        return Ok(active);
    }
    if models.len() == 1 {
        return Ok(models[0].clone());
    }
    Err(format!(
        "multiple {space} embedding models stored; choose one with `model`: {}",
        models.join(", ")
    ))
}

/// Bridge an atlas hit (repo + file) into that repo's own graph: resolve the
/// registry name to its active root, open its index, and return the file's
/// symbols so the agent can chain straight into per-repo detail.
fn resolve(args: &Value) -> Result<Value, String> {
    let repo = req_str(args, "repo")?;
    let file = req_str(args, "file")?;
    let registry = registry();
    let entry = registry
        .get(repo)
        .ok_or_else(|| format!("no repository named '{repo}' in the registry"))?;
    let root = entry.active_root();
    let ladybug = RepoPaths::new(root).index_dir.join("ladybug");
    if !ladybug.exists() {
        return Err(format!(
            "'{repo}' is not indexed — run `glyphtrail analyze` on it first"
        ));
    }
    let store = LadybugStore::open(&ladybug).map_err(|e| e.to_string())?;
    let nodes = store.nodes_in_file(file).map_err(|e| e.to_string())?;
    // Deliberately no absolute `root` in the payload — the agent has repo+file;
    // the local path would only leak the host's directory layout.
    Ok(json!({
        "repo": repo,
        "file": file,
        "symbols": serde_json::to_value(&nodes).unwrap_or(Value::Null),
    }))
}

fn status_tool() -> Value {
    atlas_tool(
        "atlas_status",
        "Atlas index health: whether it is enabled, node/edge/commit counts, and \
         the active date window. Cheap; call it first to see if the atlas has \
         anything to recall.",
        json!({}),
        &[],
    )
}

fn timeline_tool() -> Value {
    atlas_tool(
        "atlas_timeline",
        "Chronological commits across all your repos (newest first), each with \
         repo, date, subject and touched-file count. Scoped to you by default \
         (the configured [me]); proprietary and unregistered repos are hidden \
         unless include_proprietary is set.",
        json!({
            "repo": { "type": "string", "description": "Restrict to one registered repo name." },
            "author": { "type": "string", "description": "Substring the author email must contain (default: you)." },
            "topic": { "type": "string", "description": "Only commits tagged with this topic (see atlas_topics)." },
            "since": { "type": "string", "description": "Earliest commit date, YYYY-MM-DD (overrides the config window)." },
            "until": { "type": "string", "description": "Latest commit date, YYYY-MM-DD (overrides the config window)." },
            "include_proprietary": { "type": "boolean", "description": "Include proprietary/unregistered repos (default false)." },
            "limit": { "type": "integer", "description": "Cap how many commits to return (default 50)." }
        }),
        &[],
    )
}

fn topics_tool() -> Value {
    atlas_tool(
        "atlas_topics",
        "The heuristic topics derived across your commits (keywords, areas, \
         languages) with how many commits each tags, most-tagged first. Use a \
         topic name as the `topic` argument to `atlas_timeline`.",
        json!({}),
        &[],
    )
}

fn similar_tool() -> Value {
    atlas_tool(
        "atlas_similar",
        "Find repos similar to a given one (or, for text, a free-text query). \
         Default mode compares commit-text embeddings; set graph=true to compare \
         code-graph structure instead (repo name required). Returns repos ranked by \
         similarity with a score and visibility; private/proprietary/unregistered \
         repos are hidden unless include_restricted. Run `glyphtrail atlas embed` / \
         `graph-embed` first. The server makes no network calls, so a free-text \
         query needs a locally (lexical) embedded index; repo↔repo always works.",
        json!({
            "query": { "type": "string", "description": "A registered repo name, or free text (text mode only)." },
            "graph": { "type": "boolean", "description": "Compare code-graph structure instead of commit text (default false)." },
            "model": { "type": "string", "description": "Embedding model to search (default: the most-recently-embedded one for this space; see atlas_status)." },
            "include_restricted": { "type": "boolean", "description": "Include private/proprietary/unregistered repos (default false)." },
            "limit": { "type": "integer", "description": "How many matches to return (default 10)." }
        }),
        &["query"],
    )
}

fn resolve_tool() -> Value {
    atlas_tool(
        "atlas_resolve",
        "Bridge an atlas hit into a repo's own graph: given a repo name and a \
         file path (as `atlas_timeline` reports them), return that file's symbols \
         from the repo's index, so you can chain recall -> detail. Requires the \
         repo to be indexed (`glyphtrail analyze`).",
        json!({
            "repo": { "type": "string", "description": "Registered repo name (from atlas_timeline)." },
            "file": { "type": "string", "description": "Repo-relative file path (from atlas_timeline)." }
        }),
        &["repo", "file"],
    )
}

/// An atlas tool definition. Built directly (not the per-repo `tool()` builder)
/// because atlas tools are not repo-scoped — they take explicit args, no implicit
/// `repo` selector. All are read-only.
fn atlas_tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
        },
        "annotations": { "readOnlyHint": true, "idempotentHint": true, "destructiveHint": false },
    })
}

/// Resolve "me" like the CLI: the configured `[me]`, else seeded from `git
/// config user.email`, so the timeline scopes to you even when `atlas.toml` has
/// no `[me]` rather than silently widening to every author.
fn resolve_me(configured: &glyphtrail_core::MeConfig) -> glyphtrail_core::MeConfig {
    if configured.is_set() {
        return configured.clone();
    }
    let mut me = glyphtrail_core::MeConfig::default();
    if let Some(email) = git_user_email() {
        me.emails.push(email);
    }
    me
}

/// The user's configured git email (`git config --get user.email`), or `None`.
fn git_user_email() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["config", "--get", "user.email"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let email = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!email.is_empty()).then_some(email)
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing required argument '{key}'"))
}

fn opt_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|n| n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn definitions_are_read_only_atlas_tools() {
        let defs = definitions();
        check!(defs.len() == 5);
        for d in &defs {
            let name = d["name"].as_str().unwrap();
            check!(name.starts_with("atlas_"));
            check!(d["annotations"]["readOnlyHint"] == json!(true));
            check!(d["inputSchema"]["type"] == json!("object"));
        }
    }

    #[test]
    fn unknown_tool_and_disabled_atlas_are_errors() {
        let missing = std::env::temp_dir().join("glyphtrail-atlas-mcp-absent");
        // A disabled atlas reports an error result, not a panic.
        let r = call(&missing, "atlas_status", &json!({}));
        check!(r["isError"] == json!(true));
        // Unknown tool name.
        let r = call(&missing, "nope", &json!({}));
        check!(r["isError"] == json!(true));
    }

    #[test]
    fn atlas_similar_is_advertised_and_fails_gracefully() {
        // Advertised with a required `query`.
        let def = definitions()
            .into_iter()
            .find(|d| d["name"] == json!("atlas_similar"))
            .expect("atlas_similar tool");
        check!(def["inputSchema"]["required"] == json!(["query"]));
        // On a disabled atlas it errors (no panic), like the other tools.
        let missing = std::env::temp_dir().join("glyphtrail-atlas-mcp-absent");
        let r = call(&missing, "atlas_similar", &json!({ "query": "x" }));
        check!(r["isError"] == json!(true));
    }
}
