//! Honest read-only JSON endpoints behind the WebUI 任务 / 记忆 / 配置 / 自身状态 pages.
//! docs/specs/web-interface.md#spec-21 (single subject timeline / UI), docs/specs/configuration-playground.md#spec-43 (playground & configuration).
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
    extract::{Path as AxumPath, Query, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use shenko_store::store::Store;

use crate::playground::PlaygroundState;
use crate::config_service::{ConfigApplyRequest, ConfigService};
use crate::owner::{OwnerState, claim_installation};
use crate::ws::LiveStatus;

/// Live context-policy facts supplied by the same bootstrap that installs the
/// strategies. `capabilities` is the advertised binding, never inferred.
#[derive(Clone, Default)]
pub struct ContextFacts {
    /// Registry the subject loop actually selects and commits windows through.
    /// None means no live registry was supplied to this reporter.
    pub registry: Option<Arc<shenko_context_policy::PolicyRegistry>>,
    /// Advertised capabilities of the bound model, before output reservation.
    pub capabilities: Option<shenko_broker::request::ProviderCapabilities>,
    /// The same output reservation the subject subtracts from context limits.
    pub reserved_output_tokens: u64,
}

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
    /// Live context-policy wiring for `/api/context`. None means installation
    /// state is not observed rather than fabricated strategies.
    pub context: Option<ContextFacts>,
    /// The configuration service (discover/read/preview/apply). None means
    /// the installation exposes no configuration namespaces yet.
    pub config: Option<Arc<ConfigService>>,
    /// Ownership state (claim + authenticated actor). None means the routes
    /// report that ownership is not installed rather than fabricating one.
    pub owner: Option<OwnerState>,
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

    /// Wire the same registry and advertised capabilities the subject loop
    /// uses, so `/api/context` can report real installed strategies.
    pub fn with_context(mut self, facts: ContextFacts) -> Self {
        self.context = Some(facts);
        self
    }

    pub fn with_config(mut self, config: Arc<ConfigService>) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_owner(mut self, owner: OwnerState) -> Self {
        self.owner = Some(owner);
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
        .route("/api/context", get(context_handler))
        .route("/api/config/namespaces", get(config_namespaces_handler))
        .route("/api/config/entries", get(config_entries_handler))
        .route("/api/config/entries/{namespace}/{key}", get(config_entry_handler))
        .route("/api/config/preview", post(config_preview_handler))
        .route("/api/config/apply", post(config_apply_handler))
        .route("/api/config/adoptions/{namespace}/{key}", get(config_adoptions_handler))
        .route("/api/identity/name", post(identity_name_handler))
        .route("/api/owner/state", get(owner_state_handler))
        .route("/api/owner/claim", post(owner_claim_handler))
        .route("/api/playground", get(playground_surface_handler))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Configuration and ownership — docs/specs/configuration-playground.md#spec-43-1, docs/specs/plugins.md#spec-44-4.
//
// Discover/read are open to every session. Preview is open: looking at a
// change is never privileged. Apply requires an authenticated actor; whether
// the actor may change a namespace is the handler's decision, not the
// route's. There is no routine approval gate: a durable change receipt is
// the record, adoption by consumers is separately observed.
// ---------------------------------------------------------------------------

/// `GET /api/config/namespaces` → the installed namespaces and their keys.
async fn config_namespaces_handler(State(state): State<ApiState>) -> Json<Value> {
    match state.config.as_ref() {
        Some(config) => Json(json!({
            "available": true,
            "namespaces": config.discover(),
        })),
        None => Json(json!({ "available": false, "namespaces": [] })),
    }
}

/// `GET /api/config/entries` — every registered key with its saved desired
/// revision, the value each consumer actually runs, and its adoption rows.
///
/// The panel reads this projection; it holds the same facts the conversation
/// path gets from `config.manage`, not a second configuration source. A
/// consumer that cannot report its value is `null`, never the requested value.
async fn config_entries_handler(State(state): State<ApiState>) -> Json<Value> {
    let Some(config) = state.config.as_ref() else {
        return Json(json!({ "available": false, "entries": [] }));
    };
    let mut entries = Vec::new();
    for namespace in config.discover() {
        for key in &namespace.logical_keys {
            match config.read(&namespace.namespace, key) {
                Ok(view) => entries.push(json!({
                    "namespace": &namespace.namespace,
                    "logical_key": key,
                    "application": namespace.application,
                    "entry": view.entry,
                    "desired_value": view.desired_value,
                    "current_value": view.current_value,
                    "current_value_error": view.current_value_error,
                    "adoptions": view.adoptions,
                    "interrupted_change": view.interrupted_change,
                })),
                Err(error) => entries.push(json!({
                    "namespace": &namespace.namespace,
                    "logical_key": key,
                    "application": namespace.application,
                    "code": error.code(),
                    "error": error.to_string(),
                })),
            }
        }
    }
    Json(json!({ "available": true, "entries": entries }))
}

async fn config_entry_handler(
    State(state): State<ApiState>,
    AxumPath((namespace, key)): AxumPath<(String, String)>,
) -> Json<Value> {
    let Some(config) = state.config.as_ref() else {
        return Json(json!({ "available": false, "error": "CONFIG_INTERNAL: configuration service not installed" }));
    };
    match config.read(&namespace, &key) {
        Ok(view) => Json(json!({ "available": true, "entry": view })),
        Err(error) => Json(json!({ "available": true, "accepted": false, "code": error.code(), "error": error.to_string() })),
    }
}

/// The body a panel may send. It carries no actor: the actor is derived from
/// the request's credentials, because a client that names its own actor is
/// not authenticated as it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigPanelRequest {
    namespace: String,
    logical_key: String,
    desired: Value,
    /// The panel's own intent identity. Re-submitting the same intent after a
    /// lost response resolves the original change instead of a second one.
    origin_identity: String,
    expected_base_revision: Option<u64>,
    coordinating_task_id: Option<shenko_types::ids::EntityId>,
}

async fn config_preview_handler(
    State(state): State<ApiState>,
    Json(request): Json<ConfigPanelRequest>,
) -> Json<Value> {
    let Some(config) = state.config.as_ref() else {
        return Json(json!({ "available": false, "error": "CONFIG_INTERNAL: configuration service not installed" }));
    };
    match config.preview(&request.namespace, &request.logical_key, request.desired, request.expected_base_revision) {
        Ok(preview) => Json(json!({ "available": true, "accepted": true, "preview": preview })),
        Err(error) => Json(json!({ "available": true, "accepted": false, "code": error.code(), "error": error.to_string() })),
    }
}

async fn config_apply_handler(
    State(state): State<ApiState>,
    headers: axum::http::HeaderMap,
    Json(request): Json<ConfigPanelRequest>,
) -> Json<Value> {
    let Some(config) = state.config.as_ref() else {
        return Json(json!({ "available": false, "error": "CONFIG_INTERNAL: configuration service not installed" }));
    };
    // The actor is resolved from the request's credentials, never from a body
    // field: only an authenticated owner principal may carry a panel change.
    // The subject's own changes arrive through its delegated `config.manage`
    // task, which derives the subject actor from that task.
    let Some(owner) = state.owner.as_ref() else {
        return Json(json!({ "available": true, "accepted": false, "code": "CONFIG_DENIED",
            "error": "CONFIG_DENIED: ownership is not installed, so no panel change can be authenticated" }));
    };
    let actor = match owner.actor(&headers) {
        Ok(crate::owner::AuthenticatedActor::Owner { principal_ref }) =>
            crate::config_service::ConfigActor::Owner { principal_ref },
        Ok(crate::owner::AuthenticatedActor::Participant) =>
            return Json(json!({ "available": true, "accepted": false, "code": "CONFIG_DENIED",
                "error": "CONFIG_DENIED: a panel configuration change requires owner authentication" })),
        Err(error) => return Json(json!({ "available": true, "accepted": false, "code": "CONFIG_DENIED", "error": error.to_string() })),
    };
    let request = ConfigApplyRequest {
        namespace: request.namespace,
        logical_key: request.logical_key,
        origin_identity: request.origin_identity,
        actor,
        desired: request.desired,
        expected_base_revision: request.expected_base_revision,
        coordinating_task_id: request.coordinating_task_id,
    };
    match config.apply(request) {
        Ok(result) => Json(json!({
            "available": true,
            "accepted": true,
            "replayed": result.replayed,
            "change": result.change,
        })),
        Err(error) => Json(json!({ "available": true, "accepted": false, "code": error.code(), "error": error.to_string() })),
    }
}

async fn config_adoptions_handler(
    State(state): State<ApiState>,
    AxumPath((namespace, key)): AxumPath<(String, String)>,
) -> Json<Value> {
    let Some(config) = state.config.as_ref() else {
        return Json(json!({ "available": false, "error": "CONFIG_INTERNAL: configuration service not installed" }));
    };
    match config.adoption_state(&namespace, &key) {
        Ok(adoptions) => Json(json!({ "available": true, "adoptions": adoptions })),
        Err(error) => Json(json!({ "available": true, "accepted": false, "code": error.code(), "error": error.to_string() })),
    }
}

/// `GET /api/owner/state` → whether the installation has an owner and
/// whether a claim capability is still pending. No credential material is
/// ever included.
/// `POST /api/identity/name` — the owner names the instance (docs/specs/web-interface.md#spec-21-1).
///
/// The name is the instance's, not the brand's, and the subject may also
/// name itself (docs/specs/web-interface.md#spec-21-1: "可由用户取名，或让 Agent 自主取名"). The change goes
/// through the SAME identity policy the subject's own naming uses, so the
/// rules about what a name may be are one policy, not two.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityNameRequest {
    name: String,
}

async fn identity_name_handler(
    State(state): State<ApiState>,
    headers: axum::http::HeaderMap,
    Json(request): Json<IdentityNameRequest>,
) -> Json<Value> {
    let Some(store) = state.store.as_ref() else {
        return Json(json!({ "available": false, "error": "IDENTITY_UNAVAILABLE: no store is attached" }));
    };
    let Some(owner) = state.owner.as_ref() else {
        return Json(json!({ "available": true, "accepted": false, "code": "IDENTITY_DENIED",
            "error": "IDENTITY_DENIED: ownership is not installed, so no name can be authenticated" }));
    };
    // The actor is resolved from the request's credentials, never from a body
    // field: only the authenticated owner may name the instance.
    let principal_ref = match owner.actor(&headers) {
        Ok(crate::owner::AuthenticatedActor::Owner { principal_ref }) => principal_ref,
        Ok(crate::owner::AuthenticatedActor::Participant) =>
            return Json(json!({ "available": true, "accepted": false, "code": "IDENTITY_DENIED",
                "error": "IDENTITY_DENIED: naming the instance requires owner authentication" })),
        Err(error) => return Json(json!({ "available": true, "accepted": false, "code": "IDENTITY_DENIED", "error": error.to_string() })),
    };
    let _ = principal_ref;
    let agent = match store.get_existing_agent_id() {
        Ok(Some(agent)) => agent,
        _ => return Json(json!({ "available": true, "accepted": false, "code": "IDENTITY_UNKNOWN",
            "error": "IDENTITY_UNKNOWN: this installation has no subject to name" })),
    };
    let Some(current) = store.latest_identity(agent).ok().flatten() else {
        return Json(json!({ "available": true, "accepted": false, "code": "IDENTITY_UNKNOWN",
            "error": "IDENTITY_UNKNOWN: this installation has no identity to name" }));
    };
    let change = shenko_types::identity::IdentityChange::Name(request.name);
    let Some(next) = shenko_identity::DefaultIdentityPolicy::successor(
        &current,
        &change,
        shenko_types::identity::IdentitySource::Operator,
    ) else {
        // An empty or unchanged name is not a new revision: the answer is the
        // name the instance already has.
        return Json(json!({ "available": true, "accepted": true, "changed": false, "identity": current }));
    };
    match store.append_identity(&next, current.revision) {
        Ok(commit) => Json(json!({ "available": true, "accepted": true, "changed": true,
            "identity": next, "commit": commit })),
        Err(error) => Json(json!({ "available": true, "accepted": false, "code": "IDENTITY_CONFLICT", "error": error.to_string() })),
    }
}

async fn owner_state_handler(State(state): State<ApiState>) -> Json<Value> {
    let Some(owner) = state.owner.as_ref() else {
        return Json(json!({ "available": false }));
    };
    let status = (|| -> anyhow::Result<(bool, bool)> {
        let claimed = owner.store.owner_grant(owner.installation_id)?.is_some();
        let claim_pending = !claimed
            && crate::owner::load_claim_capability(&owner.data_dir, owner.installation_id)?.is_some();
        Ok((claimed, claim_pending))
    })();
    let (claimed, claim_pending) = match status {
        Ok(status) => status,
        Err(error) => return Json(json!({ "available": true, "error": error.to_string() })),
    };
    Json(json!({
        "available": true,
        "installation_id": owner.installation_id,
        "claimed": claimed,
        "claim_pending": claim_pending,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimRequest {
    capability: String,
}

/// `POST /api/owner/claim` → the first recognized human presents the install
/// capability; the response carries the admin bearer exactly once.
async fn owner_claim_handler(State(state): State<ApiState>, Json(request): Json<ClaimRequest>) -> Json<Value> {
    let Some(owner) = state.owner.as_ref() else {
        return Json(json!({ "available": false, "error": "ownership is not installed" }));
    };
    match claim_installation(&owner.store, &owner.data_dir, owner.installation_id, &request.capability) {
        Ok(response) => {
            let status = if response.outcome == "granted" { "granted" } else { &response.outcome };
            Json(json!({ "available": true, "outcome": status, "response": response }))
        }
        Err(error) => Json(json!({ "available": true, "outcome": "error", "error": error.to_string() })),
    }
}

/// `GET /api/playground` → the Proto-UI playground surface projection: the
/// enumerable roots, every artifact with its durable record and its committed
/// head. The frontend renders this as a Proto-UI view, not a second page.
async fn playground_surface_handler(State(state): State<ApiState>) -> Json<Value> {
    let Some(playground) = state.playground.as_ref() else {
        return Json(json!({ "available": false }));
    };
    let artifacts: Vec<Value> = playground.list().into_iter().map(|version| {
        let durable = playground.durable_record(version.artifact_id).ok().flatten();
        // docs/specs/cognition-units.md#spec-51-1: the default projection of a revision chain is a fact
        // pair — how many committed versions exist and when the newest was
        // written. How many *older* versions the default view shows is the
        // installed taste macro's decision, so this surface does not derive
        // it: the projection is reported as omitted until a taste disclosure
        // (`Disclosure::project_json`) is supplied by its owner. The bytes of
        // every version stay behind the explicit `/playground/{id}/versions`
        // read.
        let committed_version_count = playground
            .revisions(version.artifact_id)
            .map(|revisions| revisions.len())
            .ok();
        json!({
            "artifact_id": version.artifact_id,
            "title": version.title,
            "mime": version.mime,
            "revision": version.revision,
            "parent_revision": version.parent_revision,
            "updated_at": version.updated_at,
            "latest_committed_at": version.updated_at,
            "committed_version_count": committed_version_count,
            "revision_projection": {
                "state": "omitted",
                "reason": "no installed taste disclosure projection was applied to this surface",
            },
            "backend": version.backend.is_some(),
            "lifecycle": durable.as_ref().map(|record| record.lifecycle.clone()),
            "record": durable,
        })
    }).collect();
    // The playground surface also lists the registered configuration
    // namespaces: an artifact's settings are managed through the same
    // configuration service as every other setting, not through a private
    // per-artifact panel. docs/specs/configuration-playground.md#spec-43-2.
    let namespaces = state
        .config
        .as_ref()
        .map(|config| config.discover())
        .unwrap_or_default();
    Json(json!({
        "available": true,
        "roots": playground.roots(),
        "artifacts": artifacts,
        "namespaces": namespaces,
    }))
}

#[derive(Debug, Deserialize)]
struct TasksQuery {
    limit: Option<usize>,
}

/// `/api/context` page bounds: `limit` bounds the metadata page (default 50,
/// clamped 1–200); `before` requests windows strictly below that generation.
#[derive(Debug, Deserialize)]
struct ContextQuery {
    limit: Option<usize>,
    before: Option<u64>,
}

/// `GET /api/tasks` → `{ items: [{task_id, kind, state, created_at}] }`.
/// Empty table → `items: []`; no store handle → `available: false`.
async fn tasks_handler(
    State(state): State<ApiState>,
    Query(query): Query<TasksQuery>,
) -> Json<Value> {
    // The default page bound is a live setting of this consumer
    // (`shenko.ui/page_limit`); an explicit `?limit=` still wins.
    let limit = query
        .limit
        .unwrap_or_else(|| crate::config_namespaces::page_limit(state.config.as_ref()))
        .clamp(1, crate::config_namespaces::MAX_PAGE_LIMIT);
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

    // Instance identity (docs/specs/subject-runtime.md#spec-6): the name is the instance's, not the brand.
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

fn availability_json(availability: shenko_context_policy::Availability) -> Value {
    use shenko_context_policy::Availability::*;
    match availability {
        Available => json!({"state": "available"}),
        Degraded(reason) => json!({"state": "degraded", "reason": reason}),
        Unavailable(reason) => json!({"state": "unavailable", "reason": reason}),
    }
}

/// Window report from the committed record plus stored derivative identity;
/// `derivatives[].bytes` is the durable stored length, never payload bytes.
fn context_window_json(meta: &shenko_context_policy::CommittedWindowMeta) -> Value {
    let window = &meta.window;
    let kind = |entry: &shenko_context_policy::WindowEntry| match entry {
        shenko_context_policy::WindowEntry::Verbatim { .. } => "verbatim",
        shenko_context_policy::WindowEntry::Pointer { .. } => "pointer",
        shenko_context_policy::WindowEntry::Text { .. } => "text",
        shenko_context_policy::WindowEntry::Image { .. } => "image",
        shenko_context_policy::WindowEntry::Note { .. } => "note",
    };
    let mut counts = serde_json::Map::new();
    // The notebook is working state, not a payload: the report names the
    // revision and provenance the window carries and never the note text.
    let mut notes = Vec::new();
    for entry in &window.entries {
        let count = counts.entry(kind(entry).to_string()).or_insert(json!(0));
        *count = json!(count.as_u64().unwrap_or(0) + 1);
        if let shenko_context_policy::WindowEntry::Note { revision, source, .. } = entry {
            notes.push(json!({"revision": revision, "source": source}));
        }
    }
    json!({
        "id": window.id.to_string(),
        "generation": window.generation,
        "base": window.base,
        "policy_id": window.policy_id,
        "policy_version": window.policy_version,
        "model_generation": window.model_generation,
        "covers_through": window.covers_through,
        "entry_kinds": Value::Object(counts),
        "notes": notes,
        "call_states": window.call_states,
        "derivatives": meta
            .derivatives
            .iter()
            .map(|derivative| json!({
                "media_type": derivative.media_type,
                "blake3": derivative.blake3,
                "bytes": derivative.bytes,
            }))
            .collect::<Vec<_>>(),
        // Reported cost, including the conservative reusable-prefix bound;
        // never a backend KV cache-hit claim (docs/specs/context-policies.md#spec-12-3-2).
        "cost": window.cost,
    })
}

/// `GET /api/context` → the subject scope's committed window chain, installed
/// context-policy strategies and their availability against the observed
/// model capabilities. Every field is a real record: committed windows from
/// the durable chain, strategies/epochs/preference from the live registry.
/// `plugins_wired: false` means no live registry was supplied to this reporter
/// (as in deterministic no-model mode); it never fabricates an install.
/// `windows` is a bounded metadata page, newest generation first:
/// `?limit=` (default 50, clamped 1–200) bounds the page and `?before=<generation>`
/// pages strictly below that generation. `window_total` counts every committed
/// window under the scope; `has_older`/`next_before` carry the cursor while
/// older windows remain. Derivative payloads are never read or returned.
async fn context_handler(
    State(state): State<ApiState>,
    Query(query): Query<ContextQuery>,
) -> Json<Value> {
    // The default page bound is the same live setting `/api/tasks` applies.
    let limit = query
        .limit
        .unwrap_or_else(|| crate::config_namespaces::page_limit(state.config.as_ref()))
        .clamp(1, crate::config_namespaces::MAX_PAGE_LIMIT) as u64;
    let mut report = json!({
        "scope": Value::Null,
        "plugins_wired": false,
        "preference": [],
        "strategies": [],
        "capabilities": Value::Null,
        "availability": [],
        "windows": [],
    });
    if let Some(context) = &state.context {
        report["capabilities"] = context
            .capabilities
            .as_ref()
            .map(|caps| {
                json!({
                    "advertised": true,
                    "model": caps.binding.model(),
                    "max_context_tokens": caps.max_context_tokens,
                    "vision": caps.vision,
                })
            })
            .unwrap_or(Value::Null);
        if let Some(registry) = &context.registry {
            report["plugins_wired"] = json!(true);
            report["preference"] = json!(registry.preference());
            report["strategies"] = registry
                .installed()
                .iter()
                .map(|(manifest, epoch)| json!({
                    "id": manifest.id,
                    "version": manifest.version,
                    "contract": manifest.contract,
                    "requires": manifest.requires,
                    "epoch": epoch,
                }))
                .collect();
            let caps = u32::try_from(context.reserved_output_tokens).map(|reserved| {
                context.capabilities.as_ref()
                    .and_then(|caps| shenko_subject::input_model_caps(caps, reserved))
            });
            report["availability"] = match caps {
                Ok(Some(caps)) if caps.max_context > 0 => {
                    registry
                        .availability(&caps)
                        .into_iter()
                        .map(|(id, availability)| json!({
                            "id": id,
                            "availability": availability_json(availability),
                        }))
                        .collect()
                }
                Ok(None) => {
                    report["availability_note"] = json!(
                        "provider capabilities not observed; strategy availability not evaluated"
                    );
                    json!([])
                }
                Ok(Some(_)) => {
                    report["availability_note"] = json!(
                        "output reservation leaves no input context; strategy availability not evaluated"
                    );
                    json!([])
                }
                Err(_) => {
                    report["availability_note"] = json!(
                        "output reservation exceeds token budget type; strategy availability not evaluated"
                    );
                    json!([])
                }
            };
        }
    }
    let Some(store) = state.store.as_ref() else {
        report["available"] = json!(false);
        report["note"] = json!("store 尚未接入 UI server");
        return Json(report);
    };
    let Some(agent_id) = (match store.get_existing_agent_id() {
        Ok(agent_id) => agent_id,
        Err(error) => {
            report["available"] = json!(false);
            report["note"] = json!(format!("subject identity unreadable: {error}"));
            return Json(report);
        }
    }) else {
        report["available"] = json!(false);
        report["note"] = json!("subject 尚未初始化");
        return Json(report);
    };
    let (head, _) = match store.load_subject(agent_id) {
        Ok(subject) => subject,
        Err(error) => {
            report["available"] = json!(false);
            report["scope"] = json!({"agent_id": agent_id.to_string()});
            report["note"] = json!(format!("subject head unreadable: {error}"));
            return Json(report);
        }
    };
    let scope_id = shenko_types::ids::EntityId(head.branch_id.0);
    report["scope"] = json!({
        "agent_id": agent_id.to_string(),
        "branch_id": scope_id.to_string(),
    });
    let log = shenko_context_policy::StoreWindowLog::new(Arc::clone(store), scope_id);
    let scope_facts = (|| -> Result<Value, String> {
        // Bounded metadata page: bodies stay text, derivative payloads are
        // never materialized or hashed; `bytes` is the recorded length.
        let page = log.meta_page(query.before, limit).map_err(|e| e.to_string())?;
        let next_before = page
            .windows
            .last()
            .filter(|_| page.has_older)
            .map(|meta| meta.window.generation);
        Ok(json!({
            "model_generation": store.context_model_generation(scope_id).map_err(|e| e.to_string())?,
            "profile_digest": store.context_profile(scope_id).map_err(|e| e.to_string())?.map(|(_, digest)| digest),
            "window_total": page.total,
            "has_older": page.has_older,
            "next_before": next_before,
            "windows": page
                .windows
                .iter()
                .map(context_window_json)
                .collect::<Vec<_>>(),
        }))
    })();
    match scope_facts {
        Ok(facts) => {
            if let (Some(report), Some(facts)) = (report.as_object_mut(), facts.as_object()) {
                report.extend(facts.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            report["available"] = json!(true);
        }
        Err(error) => {
            report["available"] = json!(false);
            report["note"] = json!(format!("context window chain unreadable: {error}"));
        }
    }
    Json(report)
}

#[cfg(test)]
mod context_tests {
    use super::*;

    #[tokio::test]
    async fn unwired_context_does_not_invent_plugins_or_windows() {
        let Json(report) = context_handler(
            State(ApiState::new()),
            Query(ContextQuery { limit: None, before: None }),
        ).await;
        assert_eq!(report["available"], false);
        assert_eq!(report["plugins_wired"], false);
        assert_eq!(report["strategies"], json!([]));
        assert_eq!(report["windows"], json!([]));
        assert_eq!(report["capabilities"], Value::Null);
    }

    #[test]
    fn window_report_retains_policy_cost_and_derivative_identity_not_payload() {
        use shenko_context_policy::{
            CommittedWindow, CommittedWindowMeta, Derivative, DerivativeMeta, WindowEntry,
        };
        let mut window = CommittedWindow::genesis();
        window.policy_id = Some("snapcompact".into());
        window.policy_version = Some("test-decoder".into());
        window.entries.push(WindowEntry::Image { from: 1, to: 9, derivative: 0, tokens: 7 });
        window.entries.push(WindowEntry::Note {
            revision: 3,
            source: shenko_context_policy::NoteSource::Tool,
            text: "working state".into(),
            tokens: 4,
        });
        // The stored frame's real identity; the report must show it without
        // the payload ever reaching the reader.
        let stored = Derivative::new("image/png", vec![1, 2, 3]);
        window.cost.window_tokens = 7;
        window.cost.reusable_prefix_tokens = 0;
        window.cost.recompute_tokens = 7;
        let meta = CommittedWindowMeta {
            window,
            derivatives: vec![DerivativeMeta {
                media_type: stored.media_type.clone(),
                blake3: stored.blake3.clone(),
                bytes: stored.bytes.len() as u64,
            }],
        };
        let window = &meta.window;
        let report = context_window_json(&meta);
        assert_eq!(report["id"], window.id.to_string());
        assert_eq!(report["policy_id"], "snapcompact");
        assert_eq!(report["policy_version"], "test-decoder");
        assert_eq!(report["entry_kinds"]["image"], 1);
        assert_eq!(report["entry_kinds"]["note"], 1);
        // Revision and provenance are reported; the authored text is not a
        // payload the report hands out.
        assert_eq!(report["notes"], json!([{"revision": 3, "source": "tool"}]));
        assert!(!report.to_string().contains("working state"));
        assert_eq!(report["cost"], json!(window.cost));
        assert_eq!(report["derivatives"][0]["bytes"], 3);
        assert_eq!(report["derivatives"][0]["blake3"], meta.derivatives[0].blake3);
        assert!(report["derivatives"][0].get("payload").is_none());
    }
}

#[cfg(test)]
mod naming_tests {
    //! The owner's naming (docs/specs/web-interface.md#spec-21-1) is a durable identity revision, not a
    //! label in the browser: the name goes through the same policy the
    //! subject's own naming uses, and only an authenticated owner may set it.

    use super::*;
    use shenko_types::ids::EntityId;
    use shenko_types::object::Digest;

    /// A store with a genesis, a subject head and an unnamed instance: the
    /// state the naming endpoint acts on.
    fn store_with_identity(root: &std::path::Path) -> (Arc<Store>, EntityId) {
        use shenko_types::commit::CommitRef;
        use shenko_types::ids::*;
        use shenko_types::ids::LedgerSeq;
        use shenko_types::subject::*;
        
        let store = Arc::new(
            Store::open(root.join("shenko.db"), root.join("cas")).expect("the store opens"),
        );
        let agent = EntityId::new();
        let branch = BranchId::new();
        let first = EntityId::new();
        let genesis_id = EntityId::new();
        let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
        let object = store
            .cas
            .seal_object(b"seed", "application/json", "fixture", "seed", 1)
            .expect("the seed is sealed")
            .object;
        let genesis = SubjectGenesis {
            genesis_id,
            subject_id: agent,
            bootstrap_id: EntityId::new(),
            launch_id: EntityId::new(),
            seed_manifest_ref: object.clone(),
            initial_profile_ref: object.clone(),
            home_manifest_ref: object.clone(),
            bootstrap_event_ref: object,
            created_commit: zero,
        };
        let head = SubjectBranchHead {
            agent_id: agent,
            branch_id: branch.clone(),
            subject_root_task_id: EntityId::new(),
            controller_epoch: ControllerEpoch(1),
            software_generation: RuntimeGeneration::default(),
            genesis_ref: GenesisRef { ledger_id: store.ledger.ledger_id(), genesis_id },
            committed_step_no: 0,
            committed_step_id: None,
            committed_at: zero,
            next_step_id: first,
            mode: SubjectMode::Active,
            mode_reason_ref: None,
            record_revision: RecordRevision(0),
        };
        let step = SubjectStep {
            step_id: first,
            branch_id: branch,
            step_no: 1,
            expected_parent_commit: zero,
            controller_epoch: ControllerEpoch(1),
            software_generation: RuntimeGeneration::default(),
            state: SubjectStepState::Planned,
            input_plan_ref: None,
            context_revision: None,
            active_request_id: None,
            active_attempt_id: None,
            model_result_ref: None,
            model_outcome_ref: None,
            decision_ref: None,
            commit_ref: None,
            retry_reason_ref: None,
            record_revision: RecordRevision(0),
            created_by_step_id: None,
        };
        let root_task = shenko_types::task::TaskRecord::subject_root(
            head.subject_root_task_id,
            agent,
            genesis.seed_manifest_ref.clone(),
            zero,
        );
        store
            .initialize_genesis(&genesis, &head, &step, &root_task)
            .expect("the genesis is installed");
        let profile = shenko_identity::DefaultIdentityPolicy::genesis(agent, None);
        store
            .initialize_identity(&profile)
            .expect("the identity is born");
        (store, agent)
    }

    /// The installation is claimed by this reader, and the reader's admin
    /// token is the one the store knows.
    fn claim(store: &Store, installation_id: EntityId) -> axum::http::HeaderMap {
        let token = "owner-token-for-the-test";
        let object = store
            .cas
            .seal_object(b"principal", "application/json", "principal", "principal", 1)
            .expect("the principal is sealed")
            .object;
        store
            .claim_owner(
                installation_id,
                EntityId::new(),
                object.clone(),
                object,
                Digest::blake3(token.as_bytes()),
            )
            .expect("the installation is claimed");
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "x-shenko-admin",
            axum::http::HeaderValue::from_static("owner-token-for-the-test"),
        );
        headers
    }

    #[tokio::test]
    async fn the_owner_names_the_instance_and_the_name_becomes_a_revision() {
        let root = std::env::temp_dir().join(format!("shenko-naming-{}", EntityId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let (store, agent) = store_with_identity(&root);
        let installation_id = EntityId::new();
        let state = ApiState::new()
            .with_store(Arc::clone(&store))
            .with_owner(crate::owner::OwnerState {
                store: Arc::clone(&store),
                installation_id,
                data_dir: root.clone(),
            });
        let headers = claim(&store, installation_id);
        let answer = identity_name_handler(
            State(state),
            headers,
            Json(IdentityNameRequest { name: "小灯".into() }),
        )
        .await
        .0;
        assert_eq!(answer["accepted"], serde_json::json!(true));
        assert_eq!(answer["changed"], serde_json::json!(true));
        let profile = store.latest_identity(agent).unwrap().unwrap();
        assert_eq!(profile.display_name.as_deref(), Some("小灯"));
        assert_eq!(profile.revision, 2, "the name is a new revision");
        assert_eq!(
            profile.naming,
            shenko_types::identity::NamingOrigin::UserGiven,
            "a user-given name says so"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_participant_cannot_name_the_instance() {
        let root = std::env::temp_dir().join(format!("shenko-naming-{}", EntityId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let (store, agent) = store_with_identity(&root);
        let installation_id = EntityId::new();
        let state = ApiState::new()
            .with_store(Arc::clone(&store))
            .with_owner(crate::owner::OwnerState {
                store: Arc::clone(&store),
                installation_id,
                data_dir: root.clone(),
            });
        let answer = identity_name_handler(
            State(state),
            axum::http::HeaderMap::new(),
            Json(IdentityNameRequest { name: "不请自来".into() }),
        )
        .await
        .0;
        assert_eq!(answer["accepted"], serde_json::json!(false));
        assert_eq!(answer["code"], serde_json::json!("IDENTITY_DENIED"));
        let profile = store.latest_identity(agent).unwrap().unwrap();
        assert!(
            profile.display_name.is_none(),
            "the name was refused, so the instance keeps the name it had"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
