//! Honest read-only JSON endpoints behind the WebUI 任务 / 记忆 / 配置 / 自身状态 pages.
//! SPEC §21 (single subject timeline / UI), §43 (playground & configuration).
//!
//! These handlers only report state that actually exists on this node. When the
//! store handle has not been wired (main.rs still calls `UiServer::new`), the
//! task endpoint answers `available: false` instead of inventing rows. Memory
//! intents are not persisted to the store yet, so that endpoint says so.
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

/// Actual provider selection supplied by the production bootstrap. Configured
/// means a provider can be constructed, not that a probe or capacity succeeded.
#[derive(Clone, Debug)]
pub struct InferenceStatus {
    pub model: Option<String>,
    pub configured: bool,
}

impl InferenceStatus {
    pub fn new(model: Option<String>, configured: bool) -> Self {
        Self { model, configured }
    }

    fn model_label(&self) -> &str {
        self.model.as_deref().unwrap_or("not_connected")
    }

    fn capacity(&self) -> &'static str {
        if self.configured {
            "unprobed"
        } else {
            "blocked"
        }
    }
}

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
    /// Provider binding from the same bootstrap that decided whether to start
    /// the subject loop. None means no runtime binding was supplied.
    pub inference: Option<InferenceStatus>,
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

    pub fn with_inference(mut self, inference: InferenceStatus) -> Self {
        self.inference = Some(inference);
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
                "note": format!("任务读取失败: {e}"),
            })),
        },
        None => Json(json!({
            "available": false,
            "items": [],
            "note": "store 尚未接入 UI server (UiServer::with_store)",
        })),
    }
}

/// `GET /api/memory` → honest empty result: memory intents are not persisted
/// into `memory_entries` yet, so there is no real data to show.
async fn memory_handler() -> Json<Value> {
    Json(json!({
        "items": [],
        "intents": [],
        "note": "memory intents 尚未持久化",
    }))
}

/// Resolved configuration shared by `/api/config` and `/api/self`.
struct ConfigValues {
    model: String,
    data_dir: Option<String>,
    api_key_configured: bool,
    generation: String,
    model_configured: Option<bool>,
    inference_capacity: &'static str,
}

fn config_values(state: &ApiState) -> ConfigValues {
    // Only presence is reported; the key value is never read into a response.
    let api_key_configured = std::env::var("ANTHROPIC_API_KEY").is_ok();
    // Only an explicit binding is evidence. An API key's presence does not
    // prove a model, provider, online state, or effective sampling profile.
    let inference = state.inference.as_ref();
    let data_dir = state
        .data_dir
        .clone()
        .or_else(|| std::env::var_os("SHENKO_DATA_DIR").map(PathBuf::from))
        .map(|p| p.display().to_string());
    let generation = std::env::var("SHENKO_GENERATION").unwrap_or_else(|_| "unknown".to_string());
    ConfigValues {
        model: inference.map(InferenceStatus::model_label).unwrap_or("unknown").into(),
        data_dir,
        api_key_configured,
        generation,
        model_configured: inference.map(|binding| binding.configured),
        inference_capacity: inference.map(InferenceStatus::capacity).unwrap_or("unknown"),
    }
}

/// `GET /api/config` reports the actual binding and distinguishes blocked from
/// configured-but-unprobed capacity. Secrets are reduced to a boolean.
async fn config_handler(State(state): State<ApiState>) -> Json<Value> {
    let values = config_values(&state);
    Json(json!({
        "model": values.model,
        "data_dir": values.data_dir,
        "generation": values.generation,
        "api_key_configured": values.api_key_configured,
        "model_configured": values.model_configured,
        "inference_capacity": values.inference_capacity,
    }))
}

/// `GET /api/self` → honest node/self status for the new WebUI.
///
/// Known today:
/// - `connected_clients`: counted live by the WS handler;
/// - `step`: the step number of the last committed thought observed on the
///   subject outbox (the outbox is published only after the S1 commit, so the
///   number is always a durably committed step). TODO: the newest *committed*
///   step is not exposed to the UI crate; when it is, report that instead.
/// - `artifacts`: the real Playground registry, if it was wired.
///
/// Unknown values are `null`, never invented. The local host is the only
/// device this node actually knows about; scheduled/interval activities do not
/// exist yet, so `activities` is empty.
async fn self_status_handler(State(state): State<ApiState>) -> Json<Value> {
    let values = config_values(&state);
    // Recent output is not evidence of current thinking or idleness.
    let status = if values.model_configured == Some(false) { "blocked" } else { "unknown" };
    let connected_clients = state.live.as_ref().map(|live| live.connections());
    let step = state.live.as_ref().and_then(|live| live.last_step());
    let last_outbox_observed_at = state
        .live
        .as_ref()
        .and_then(|live| live.last_outbox_observed_at());
    let outbox_wired = state.live.as_ref().is_some_and(|live| live.is_wired());

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

    // Instance identity (SPEC §6): the name is the instance's, not the brand.
    let subject_id = state
        .store
        .as_ref()
        .and_then(|store| store.get_existing_agent_id().ok().flatten());
    let identity = state.store.as_ref().and_then(|store| {
        let agent_id = subject_id?;
        store.latest_identity(agent_id).ok()?
    });

    Json(json!({
        "status": status,
        "status_source": "no live subject-state binding; recent outbox is not a thinking signal",
        "subject_id": subject_id.map(|id| id.to_string()),
        "outbox_wired": outbox_wired,
        "last_outbox_observed_at": last_outbox_observed_at,
        "identity": identity.map(|p| json!({
            "agent_id": p.agent_id.to_string(),
            "revision": p.revision,
            "display_name": p.display_name,
            "naming": p.naming,
            "soul": p.soul,
        })),
        "connected_clients": connected_clients,
        "step": step,
        "model": values.model,
        "model_configured": values.model_configured,
        "inference_capacity": values.inference_capacity,
        "data_dir": values.data_dir,
        // Device discovery is not wired. The UI must not turn loopback HTTP
        // reachability into a fabricated green device presence signal.
        "devices": [],
        "device_discovery": "not_wired",
        "activities": [],
        "artifacts": artifacts,
    }))
}
