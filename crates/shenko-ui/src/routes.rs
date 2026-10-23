use axum::response::{Html, Json};
use serde_json::json;

pub async fn index_handler() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub async fn health_handler() -> Json<serde_json::Value> {
    Json(json!({"status": "ok", "product": "Shenko", "version": "0.1.0"}))
}

/// The one served UI. `src/index.html` is the canonical source so the design
/// can be opened directly in a browser and included into the binary unchanged.
const INDEX_HTML: &str = include_str!("index.html");
