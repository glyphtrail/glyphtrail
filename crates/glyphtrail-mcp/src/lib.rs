#![forbid(unsafe_code)]

//! Model Context Protocol server for Glyphtrail, over stdio.
//!
//! Speaks JSON-RPC 2.0 with newline-delimited framing (one JSON object per
//! line, no embedded newlines), which is the MCP stdio transport. It exposes
//! the graph query surface as MCP tools so agents can explore a repository's
//! structure and API boundaries. The transport is hand-rolled on `serde_json`
//! to keep the dependency surface small.

mod tools;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use glyphtrail_core::config::RepoPaths;
use serde_json::{Value, json};

/// MCP protocol revision this server implements.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Serve the MCP protocol over stdin/stdout for the repository at `repo` until
/// stdin reaches EOF. Each query opens the repo's graph fresh, so results track
/// re-indexing without restarting the server.
pub fn serve_stdio(repo: PathBuf) -> Result<()> {
    let db = RepoPaths::new(&repo).db_path;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut out = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break; // EOF: client closed the pipe.
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(trimmed) {
            Ok(msg) => handle_request(&db, &msg),
            // Malformed JSON: reply with a parse error and a null id (JSON-RPC).
            Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(resp) = response {
            writeln!(out, "{}", serde_json::to_string(&resp)?)?;
            out.flush()?;
        }
    }
    Ok(())
}

/// Dispatch a single JSON-RPC message against the graph database at `db`.
/// Returns `Some(response)` for requests (those carrying an `id`) and `None`
/// for notifications. Shared by the stdio loop and the HTTP `/mcp` endpoint.
pub fn handle_request(db: &Path, msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str)?;
    let id = msg.get("id").cloned();
    match method {
        "initialize" => Some(success(id?, initialize_result())),
        "ping" => Some(success(id?, json!({}))),
        "tools/list" => Some(success(id?, json!({ "tools": tools::definitions() }))),
        "tools/call" => {
            let id = id?;
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            Some(success(id, tools::call(db, name, &args)))
        }
        // Notifications (initialized, cancelled, …) carry no id and need no reply.
        _ => id.map(|id| error(id, -32601, &format!("method not found: {method}"))),
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "glyphtrail", "version": env!("CARGO_PKG_VERSION") },
    })
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn initialize_advertises_tools_capability() {
        let req = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
        let resp = handle_request(Path::new("."), &req).unwrap();
        check!(resp["id"] == json!(1));
        check!(resp["result"]["protocolVersion"] == json!(PROTOCOL_VERSION));
        check!(resp["result"]["capabilities"]["tools"].is_object());
        check!(resp["result"]["serverInfo"]["name"] == json!("glyphtrail"));
    }

    #[test]
    fn notifications_get_no_response() {
        let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        check!(handle_request(Path::new("."), &note).is_none());
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let req = json!({"jsonrpc":"2.0","id":7,"method":"frobnicate"});
        let resp = handle_request(Path::new("."), &req).unwrap();
        check!(resp["error"]["code"] == json!(-32601));
        check!(resp["id"] == json!(7));
    }

    #[test]
    fn tools_list_returns_named_tools_with_schemas() {
        let req = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
        let resp = handle_request(Path::new("."), &req).unwrap();
        let list = resp["result"]["tools"].as_array().unwrap();
        check!(list.len() >= 5);
        for t in list {
            check!(t["name"].is_string());
            check!(t["description"].is_string());
            check!(t["inputSchema"]["type"] == json!("object"));
        }
    }
}
