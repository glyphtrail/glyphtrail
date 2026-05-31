#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
};
use glyphtrail_core::{Edge, NodeId};
use glyphtrail_store::GraphStore;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<Box<dyn GraphStore + Send>>>,
    /// Index anchor path (`.glyphtrail/graph.db`) the `/mcp` endpoint uses to
    /// locate the graph store beside it.
    mcp_db: PathBuf,
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    50
}

/// Filters for `/api/graph` (#194): `kinds`/`edges` are comma-separated node and
/// edge kinds to include (absent = all); `limit` caps the node count so large
/// graphs are trimmed server-side before they reach the browser.
#[derive(Deserialize)]
struct GraphParams {
    kinds: Option<String>,
    edges: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct NeighborParams {
    id: String,
}

/// Parse a comma-separated query value into a list of kinds. An **absent** param
/// (`None`) means "no filter, keep all"; a **present** one (even empty, e.g.
/// `kinds=`) is an explicit selection — so unchecking every box yields an empty
/// list that matches nothing, rather than silently falling back to everything.
fn csv_vec(value: &Option<String>) -> Option<Vec<String>> {
    value.as_ref().map(|v| {
        v.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    })
}

async fn index() -> Html<&'static str> {
    Html(glyphtrail_viz::TEMPLATE)
}

async fn api_graph(State(state): State<AppState>, Query(p): Query<GraphParams>) -> Json<Value> {
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let node_kinds = csv_vec(&p.kinds);
    let edge_kinds = csv_vec(&p.edges);
    let limit = p.limit.unwrap_or(2000);
    let (nodes, edges) = store
        .export_filtered(node_kinds.as_deref(), edge_kinds.as_deref(), limit)
        .unwrap_or_default();
    let ops = store.all_operations().unwrap_or_default();
    // Kinds were filtered in the query; `select_graph` only enforces the node
    // cap and prunes edges to any node the cap dropped.
    let (nodes, edges) = glyphtrail_viz::select_graph(
        &nodes,
        &edges,
        &glyphtrail_viz::Selection {
            kinds: None,
            edge_kinds: None,
            limit,
        },
    );
    Json(glyphtrail_viz::to_elements(&nodes, &edges, &ops, None))
}

/// Neighbors of one node, for click-to-expand lazy loading (#194): returns the
/// node plus its direct graph neighbours and the edges connecting them, in the
/// same Cytoscape element format the frontend merges into the current graph.
async fn api_neighbors(
    State(state): State<AppState>,
    Query(p): Query<NeighborParams>,
) -> Json<Value> {
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let ops = store.all_operations().unwrap_or_default();
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    if let Ok(Some(center)) = store.get_node(&p.id) {
        nodes.push(center);
    }
    let center = NodeId(p.id.clone());
    for (n, kind, confidence) in store.neighbors(&p.id, None, true).unwrap_or_default() {
        edges.push(Edge {
            src: center.clone(),
            dst: n.id.clone(),
            kind,
            confidence,
        });
        nodes.push(n);
    }
    for (n, kind, confidence) in store.neighbors(&p.id, None, false).unwrap_or_default() {
        edges.push(Edge {
            src: n.id.clone(),
            dst: center.clone(),
            kind,
            confidence,
        });
        nodes.push(n);
    }
    Json(glyphtrail_viz::to_elements(&nodes, &edges, &ops, None))
}

async fn api_search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Json<Value> {
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let nodes = store.search(&params.q, params.limit).unwrap_or_default();
    Json(json!(nodes))
}

/// MCP endpoint: accept a single JSON-RPC message and return its response.
/// Notifications (no `id`) yield `204 No Content`. Each call queries the graph
/// through the shared MCP dispatch, so the tool surface matches `glyphtrail mcp`.
async fn mcp(State(state): State<AppState>, Json(msg): Json<Value>) -> Response {
    match glyphtrail_mcp::handle_request(&state.mcp_db, &msg) {
        Some(resp) => Json(resp).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

/// Serve the web explorer and the `/mcp` endpoint over HTTP, backed by an
/// already-opened graph store.
pub async fn serve(store: Box<dyn GraphStore + Send>, mcp_db: PathBuf, port: u16) -> Result<()> {
    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        mcp_db,
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/graph", get(api_graph))
        .route("/api/neighbors", get(api_neighbors))
        .route("/api/search", get(api_search))
        .route("/mcp", post(mcp))
        .with_state(state);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("glyphtrail serving at http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Serve a generated wiki directory over HTTP, rendering each Markdown page to
/// HTML on the fly (#52). `/` serves `index.md`; `/<slug>` serves `<slug>.md`.
/// Page slugs are restricted to a single safe filename segment (no path
/// traversal). Other assets are not served — the wiki output is flat Markdown.
pub async fn serve_wiki(dir: PathBuf, port: u16) -> Result<()> {
    let app = Router::new()
        .route("/", get(wiki_page))
        .route("/{slug}", get(wiki_page))
        .with_state(dir);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("glyphtrail wiki serving at http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn wiki_page(
    State(dir): State<PathBuf>,
    slug: Option<axum::extract::Path<String>>,
) -> Response {
    let raw = slug.map(|s| s.0).unwrap_or_default();
    let slug = if raw.is_empty() {
        "index"
    } else {
        raw.trim_end_matches(".md")
    };
    // Reject anything that isn't a plain single-segment page name.
    if slug.is_empty() || slug.contains('/') || slug.contains("..") {
        return (StatusCode::BAD_REQUEST, "invalid page").into_response();
    }
    match std::fs::read_to_string(dir.join(format!("{slug}.md"))) {
        Ok(md) => Html(render_markdown(slug, &md)).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, format!("no wiki page '{slug}'")).into_response(),
    }
}

/// Render a Markdown page to a minimal standalone HTML document.
fn render_markdown(title: &str, markdown: &str) -> String {
    let mut body = String::new();
    pulldown_cmark::html::push_html(&mut body, pulldown_cmark::Parser::new(markdown));
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <title>{title}</title></head><body>{body}</body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::render_markdown;
    use assert2::check;

    #[test]
    fn renders_markdown_to_html_document() {
        let html = render_markdown("Page", "# Title\n\nA **bold** word.\n");
        check!(html.contains("<title>Page</title>"));
        check!(html.contains("<h1>Title</h1>"));
        check!(html.contains("<strong>bold</strong>"));
        check!(html.starts_with("<!DOCTYPE html>"));
    }
}
