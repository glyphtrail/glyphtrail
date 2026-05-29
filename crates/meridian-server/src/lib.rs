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
use meridian_store::GraphStore;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<Box<dyn GraphStore + Send>>>,
    /// SQLite index path for the `/mcp` endpoint; `None` for backends the MCP
    /// layer can't open yet (e.g. LadybugDB), where `/mcp` returns 501.
    mcp_db: Option<PathBuf>,
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

async fn index() -> Html<&'static str> {
    Html(meridian_viz::TEMPLATE)
}

async fn api_graph(State(state): State<AppState>) -> Json<Value> {
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let (nodes, edges) = store.export_graph(5000).unwrap_or_default();
    let ops = store.all_operations().unwrap_or_default();
    Json(meridian_viz::to_elements(&nodes, &edges, &ops, None))
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
/// through the shared MCP dispatch, so the tool surface matches `meridian mcp`.
async fn mcp(State(state): State<AppState>, Json(msg): Json<Value>) -> Response {
    let Some(db) = &state.mcp_db else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "the /mcp endpoint requires the sqlite backend (serve with --backend sqlite)",
        )
            .into_response();
    };
    match meridian_mcp::handle_request(db, &msg) {
        Some(resp) => Json(resp).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

/// Serve the web explorer (and, for the sqlite backend, the `/mcp` endpoint)
/// over HTTP, backed by an already-opened graph store.
pub async fn serve(
    store: Box<dyn GraphStore + Send>,
    mcp_db: Option<PathBuf>,
    port: u16,
) -> Result<()> {
    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        mcp_db,
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/graph", get(api_graph))
        .route("/api/search", get(api_search))
        .route("/mcp", post(mcp))
        .with_state(state);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("meridian serving at http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
