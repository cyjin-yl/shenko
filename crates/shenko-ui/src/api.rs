//! Honest read-only JSON endpoints behind the WebUI 任务 / 记忆 / 配置 / 自身状态 pages.
//! SPEC §21 (single subject timeline / UI), §43 (playground & configuration).
//!
//! 任务与记忆接口在未接入 store 时返回 `available: false`，不虚构内容。
//!
//! No secrets are ever returned: `/api/config` reports only
//! `api_key_configured: true|false`.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

use shenko_store::store::Store;

use crate::playground::PlaygroundState;
use crate::ws::LiveStatus;

/// Shared state for the `/api/*` routes. Every field is optional so the router
/// stays usable in tests and in early deployments with no store attached.
#[derive(Clone, Default)]
pub struct ApiState {
    /// Persistent store; `None` means the task list cannot be read yet.
    pub store: Option<Arc<Store>>,
    /// Resolved data directory for `/api/config`. Never includes credentials.
    pub data_dir: Option<PathBuf>,
    /// Live Playground registry (shared Arc), so `/api/self` can list the real
    /// artifacts without a second in-memory index. None = not wired yet.
    pub playground: Option<PlaygroundState>,
    /// Real observations about the subject loop and WebSocket clients.
    pub live: Option<Arc<LiveStatus>>,
}

impl ApiState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_store(mut self, store: Arc<Store>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_data_dir(mut self, data_dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(data_dir.into());
        self
    }

    /// Share the Playground registry created in `UiServer::run`.
    pub fn with_playground(mut self, playground: PlaygroundState) -> Self {
        self.playground = Some(playground);
        self
    }

    /// Share the live subject/client facts tracked by the WS layer.
    pub fn with_live(mut self, live: Arc<LiveStatus>) -> Self {
        self.live = Some(live);
        self
    }
}

/// Mount the JSON API. Registered from [`crate::server::UiServer::run`] so
/// `bin/shenko/src/main.rs` needs no change for the routes themselves.
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/tasks", get(tasks_handler))
        .route("/api/memory", get(memory_handler))
        .route("/api/config", get(config_handler))
        .route("/api/self", get(self_status_handler))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct TasksQuery {
    limit: Option<usize>,
}

/// `GET /api/tasks` → `{ items: [{task_id, kind, state, created_at}] }`.
/// Empty table → `items: []`; no store handle → `available: false`.
async fn tasks_handler(
    State(state): State<ApiState>,
    Query(query): Query<TasksQuery>,
) -> Json<Value> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    match state.store.as_ref() {
        Some(store) => match store.list_recent_tasks(limit) {
            Ok(items) => Json(json!({ "available": true, "items": items })),
            Err(e) => Json(json!({
                "available": false,
                "items": [],
                "error": "STORE_READ_FAILED",
                "note": format!("任务读取失败: {e}"),
            })),
        },
        None => Json(json!({
            "available": false,
            "items": [],
            "error": "STORE_UNAVAILABLE",
            "note": "store 尚未接入 UI server (UiServer::with_store)",
        })),
    }
}

#[derive(Debug, Deserialize)]
struct MemoryQuery {
    limit: Option<usize>,
    q: Option<String>,
}

/// 读取已有持久记忆；空表、未接线与读取失败是三种不同事实。
async fn memory_handler(
    State(state): State<ApiState>,
    Query(query): Query<MemoryQuery>,
) -> Json<Value> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let Some(store) = state.store.as_ref() else {
        return Json(json!({
            "available": false, "items": [], "error": "STORE_UNAVAILABLE",
            "note": "store 尚未接入 UI server (UiServer::with_store)",
        }));
    };
    let result = match query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        Some(q) => store.search_memory(q, limit),
        None => store.list_memory(limit),
    };
    match result {
        Ok(items) => Json(json!({ "available": true, "items": items })),
        Err(e) => Json(json!({
            "available": false, "items": [], "error": "STORE_READ_FAILED",
            "note": format!("记忆读取失败: {e}"),
        })),
    }
}

/// Resolved configuration shared by `/api/config` and `/api/self`.
/// Returns `(model, data_dir, api_key_configured, generation)`.
fn config_values(state: &ApiState) -> (String, Option<String>, bool, String) {
    // Only presence is reported; the key value is never read into a response.
    let api_key_configured = std::env::var("ANTHROPIC_API_KEY").is_ok();
    // Mirrors the model label main.rs prints for this deployment. There is no
    // model negotiator wired into the UI yet, so this is the known binding.
    let model = std::env::var("SHENKO_MODEL").unwrap_or_else(|_| {
        if api_key_configured {
            "claude-opus-5-5".to_string()
        } else {
            "stub".to_string()
        }
    });
    let data_dir = state
        .data_dir
        .clone()
        .or_else(|| std::env::var_os("SHENKO_DATA_DIR").map(PathBuf::from))
        .map(|p| p.display().to_string());
    // RuntimeGeneration is a label whose documented default is "seed"
    // (shenko-types::ids::RuntimeGeneration::default).
    let generation = std::env::var("SHENKO_GENERATION").unwrap_or_else(|_| "seed".to_string());
    (model, data_dir, api_key_configured, generation)
}

/// `GET /api/config` → `{ model, data_dir, generation, api_key_configured }`.
/// Secrets are reduced to a boolean; the key itself is never echoed.
async fn config_handler(State(state): State<ApiState>) -> Json<Value> {
    let (model, data_dir, api_key_configured, generation) = config_values(&state);

    Json(json!({
        "model": model,
        "data_dir": data_dir,
        "generation": generation,
        "api_key_configured": api_key_configured,
    }))
}

/// 只返回最后观测的输出；连接与输出都不能证明主体此刻正在执行。
async fn self_status_handler(State(state): State<ApiState>) -> Json<Value> {
    let (model, data_dir, _api_key_configured, _generation) = config_values(&state);

    let connected_clients = state.live.as_ref().map(|live| live.connections());
    let step = state.live.as_ref().and_then(|live| live.last_step());

    let artifacts: Vec<Value> = match state.playground.as_ref() {
        Some(playground) => playground
            .list()
            .into_iter()
            .map(|record| {
                json!({
                    "id": record.artifact_id.to_string(),
                    "title": record.title,
                    "kind": "file",
                })
            })
            .collect(),
        None => Vec::new(),
    };

    Json(json!({
        "status": null,
        "execution_status": null,
        "outbox_observer_connected": state.live.as_ref().map(|live| live.is_wired()),
        "last_output_observed_at": state.live.as_ref().and_then(|live| live.last_output_observed_at()),
        "seconds_since_last_output": state.live.as_ref().and_then(|live| live.seconds_since_activity()),
        "committed_at": null,
        "last_observed_step": step,
        "connected_clients": connected_clients,
        "step": step,
        "model": model,
        "data_dir": data_dir,
        // 仅知道本节点存在，未接入设备存活探测；浏览器不是这个 host。
        "devices": [
            { "id": "local", "label": "服务端节点", "online": null, "kind": "host" }
        ],
        "activities": [],
        "artifacts": artifacts,
    }))
}
