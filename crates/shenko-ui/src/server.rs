use anyhow::Result;
use axum::{Extension, Router, routing::get};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

use crate::api::{self, ApiState};
use crate::media::{self, MediaRouteState};
use crate::playground::{self, PlaygroundState};
use crate::routes::*;
use crate::ws::{WsState, ws_handler};

pub struct UiServer {
    pub bind: SocketAddr,
    pub state: WsState,
    /// Root under which Playground artifacts persist (SHENKO_DATA_DIR/playground).
    pub playground_root: PathBuf,
    /// Data directory reported by `/api/config`. None = unknown.
    data_dir: Option<PathBuf>,
    /// Durable task/input store. Without it task APIs and Playground writes
    /// report unavailable rather than admitting volatile effects.
    store: Option<std::sync::Arc<shenko_store::store::Store>>,
    native: Option<Arc<shenko_subject::NativeChildService>>,
    inference: Option<api::InferenceStatus>,
    context: Option<api::ContextFacts>,
    inbox_tx: Option<tokio::sync::mpsc::Sender<shenko_subject::inbox::InboxEvent>>,
    /// Live media ingress service for `/api/media/*`; `None` leaves those
    /// routes answering `available: false` instead of faking admission.
    media_ingress: Option<std::sync::Arc<shenko_media::MediaIngress>>,
}

impl UiServer {
    pub fn new(port: u16) -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], port)),
            state: WsState::new(),
            playground_root: default_playground_root(),
            data_dir: None,
            store: None,
            native: None,
            inference: None,
            context: None,
            inbox_tx: None,
            media_ingress: None,
        }
    }

    pub fn ws_state(&self) -> WsState {
        self.state.clone()
    }

    pub fn with_native(mut self, native: Arc<shenko_subject::NativeChildService>) -> Self {
        self.native = Some(native);
        self
    }

    /// Point the persistent Playground at `<data_dir>/playground`. SPEC §43.2.
    pub fn with_data_dir(mut self, data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        self.playground_root = data_dir.join("playground");
        self.data_dir = Some(data_dir);
        self
    }

    /// Attach the persistent store for the read-only WebUI task list.
    /// Without this, `/api/tasks` honestly serves `available: false`.
    pub fn with_store(mut self, store: std::sync::Arc<shenko_store::store::Store>) -> Self {
        self.state.subject_id = match store.get_existing_agent_id() {
            Ok(subject_id) => subject_id,
            Err(error) => {
                tracing::warn!(%error, "UI subject identity could not be read");
                None
            }
        };
        self.state.store = Some(Arc::clone(&store));
        self.store = Some(store);
        self
    }

    /// Report the same provider selection used by production bootstrap.
    /// Configured is not a successful model probe or a capacity guarantee.
    pub fn with_inference_status(mut self, model: Option<String>, configured: bool) -> Self {
        self.inference = Some(api::InferenceStatus::new(model, configured));
        self
    }

    /// Wire the subject's live context-policy registry and the advertised
    /// capabilities it binds, so `/api/context` reports real strategy and
    /// window facts. Omitting it reports `plugins_wired: false`.
    pub fn with_context(
        mut self,
        registry: std::sync::Arc<shenko_context_policy::PolicyRegistry>,
        capabilities: Option<shenko_broker::request::ProviderCapabilities>,
    ) -> Self {
        self.context = Some(api::ContextFacts { registry: Some(registry), capabilities });
        self
    }

    /// Wire the subject inbox so UI messages reach the loop. SPEC §5.1.
    pub fn with_inbox(
        mut self,
        inbox_tx: tokio::sync::mpsc::Sender<shenko_subject::inbox::InboxEvent>,
    ) -> Self {
        self.state = self.state.with_inbox(inbox_tx.clone());
        self.inbox_tx = Some(inbox_tx);
        self
    }

    /// Attach the live media ingress service behind `/api/media/*`. SPEC §20.
    pub fn with_media_ingress(
        mut self,
        ingress: std::sync::Arc<shenko_media::MediaIngress>,
    ) -> Self {
        self.media_ingress = Some(ingress);
        self
    }

    /// Wire the subject outbox so model output reaches the timeline. SPEC §21.12.
    pub fn with_outbox(
        mut self,
        outbox_tx: tokio::sync::broadcast::Sender<shenko_subject::outbox::OutboxMessage>,
    ) -> Self {
        self.state = self.state.with_outbox(outbox_tx);
        self
    }

    pub async fn run(self) -> Result<()> {
        // Playground shares the inbox the WS handler already writes to, so
        // artifact changes are perceived by the same subject loop. SPEC §43.2.
        let playground_root = self.playground_root;
        let playground_state = if let Some(store) = self.store.clone() {
            let mut pg = PlaygroundState::open(playground_root.clone(), store)?;
            if let Some(tx) = self.inbox_tx.clone() { pg = pg.with_inbox(tx); }
            if let Some(native) = self.native.clone() { pg = pg.with_native(native); }
            Some(pg)
        } else { None };

        // Observe the subject outbox independently of any browser client, so
        // `/api/self` knows the last committed thought step and whether the
        // loop is active. A broadcast subscriber never consumes messages meant
        // for WS clients. SPEC §21.12.
        if let Some(outbox_tx) = self.state.outbox_tx.as_ref() {
            self.state.live.set_wired(true);
            let live = Arc::clone(&self.state.live);
            let mut outbox_rx = outbox_tx.subscribe();
            tokio::spawn(async move {
                while let Ok(msg) = outbox_rx.recv().await {
                    live.record_outbox(msg.step);
                }
            });
        }

        let mut api_state = ApiState::new();
        if let Some(store) = self.store.clone() {
            api_state = api_state.with_store(store);
        }
        let mut media_state = MediaRouteState::default();
        if let Some(store) = self.store.clone() {
            media_state.store = Some(store);
        }
        media_state.ingress = self.media_ingress.clone();
        if let Some(data_dir) = self.data_dir.clone() {
            api_state = api_state.with_data_dir(data_dir);
        }
        if let Some(inference) = self.inference.clone() {
            api_state = api_state.with_inference(inference);
        }
        if let Some(context) = self.context.clone() {
            api_state = api_state.with_context(context);
        }
        // Shared Arc registries: `/api/self` reports the same live objects the
        // Playground routes and WS handler use, not a second copy.
        if let Some(pg) = playground_state.clone() { api_state = api_state.with_playground(pg); }
        api_state = api_state.with_live(Arc::clone(&self.state.live));

        let playground_routes = match playground_state {
            Some(pg) => playground::router(pg),
            None => Router::new()
                .route("/playground", axum::routing::any(playground_unavailable))
                .route("/playground/{*path}", axum::routing::any(playground_unavailable)),
        };
        let app = Router::new()
            .route("/", get(index_handler))
            .route("/assets/app.js", get(app_js_handler))
            .route("/assets/app.css", get(app_css_handler))
            .route("/assets/proto-ui-style.css", get(proto_style_handler))
            .route("/assets/proto-ui-tokens.generated.css", get(proto_tokens_handler))
            .route("/assets/shadcn-theme.css", get(shadcn_theme_handler))
            .route("/assets/build-manifest.json", get(build_manifest_handler))
            .route("/assets/licenses/PROTO-UI-LICENSE", get(proto_license_handler))
            .route("/assets/licenses/SHADCN-NOTICES.md", get(shadcn_notices_handler))
            .route("/assets/licenses/REACT-LICENSE", get(react_license_handler))
            .route("/assets/licenses/REACT-DOM-LICENSE", get(react_dom_license_handler))
            .route("/health", get(health_handler))
            .route("/ws", get(ws_handler))
            .merge(playground_routes)
            .merge(api::router(api_state))
            .merge(media::router(media_state))
            .layer(Extension(self.state));
        info!(addr=%self.bind, playground=%playground_root.display(), "UI server starting");
        let listener = tokio::net::TcpListener::bind(self.bind).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }
}

async fn playground_unavailable() -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    (axum::http::StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({"accepted":false,"error":"Playground requires a durable Store and subject"})))
}

/// Fallback used by `UiServer::new` when the caller does not pass a data dir.
fn default_playground_root() -> PathBuf {
    std::env::var_os("SHENKO_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("shenko"))
        .join("playground")
}
