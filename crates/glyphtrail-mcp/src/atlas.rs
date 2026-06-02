//! Atlas MCP tools (#335) — recall over the private, local-only archaeology
//! index (`glyphtrail atlas mcp`). Mirrors the per-repo MCP wiring but on the
//! atlas `LadybugStore`: a visibility-gated `atlas_timeline`, a cheap
//! `atlas_status`, and a `atlas_resolve` bridge that walks an atlas hit (repo +
//! file) through the registry into that repo's own graph, so an agent can chain
//! recall -> resolve -> detail in one session.

use std::path::Path;

use glyphtrail_core::config::RepoPaths;
use glyphtrail_core::{
    AtlasConfig, Registry, TimelineQuery, Window, author_scope_label, default_registry_path,
    filter_timeline, timeline_value,
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
    let cfg = AtlasConfig::load(atlas_dir).map_err(|e| e.to_string())?;
    Ok(json!({
        "enabled": true,
        "nodes": stats.nodes,
        "edges": stats.edges,
        "commits": commits,
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
        public_only: false, // local recall: private shows, only proprietary hidden
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
        check!(defs.len() == 4);
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
}
