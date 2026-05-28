use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::{
    extract::{Query, State},
    response::{Html, Json},
    routing::get,
    Router,
};
use meridian_store::SqliteStore;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<SqliteStore>>,
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
    Json(meridian_viz::to_elements(&nodes, &edges))
}

async fn api_search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Json<Value> {
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let nodes = store.search(&params.q, params.limit).unwrap_or_default();
    Json(json!(nodes))
}

pub async fn serve(db_path: PathBuf, port: u16) -> Result<()> {
    let store = SqliteStore::open(&db_path)?;
    let state = AppState {
        store: Arc::new(Mutex::new(store)),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/graph", get(api_graph))
        .route("/api/search", get(api_search))
        .with_state(state);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("meridian serving at http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
