use anyhow::Result;
use axum::{Extension, Router, routing::get};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

use crate::api::{self, ApiState};
use crate::input::WebInputService;
use crate::config_service::ConfigService;
use crate::media::{self, MediaRouteState};
use crate::owner::OwnerState;
use crate::playground::{self, PlaygroundState};
use crate::presentation::{PresentationService, SurfaceCapabilityProvider};
use crate::playground_backend::ArtifactServiceController;
use crate::routes::*;
use crate::stream::StreamSink;
use crate::ws::{WsState, ws_handler};

pub struct UiServer {
    pub bind: SocketAddr,
    pub state: WsState,
    /// Root under which Playground artifacts persist (SHENKO_DATA_DIR/playground).
    pub playground_root: PathBuf,
    /// The shared Playground instance. The subject's own `playground.manage`
    /// handler and this server must write the SAME lifecycle, so one instance
    /// is built by the composition root and handed to both (docs/specs/configuration-playground.md#spec-57-6).
    playground_state: Option<Arc<crate::playground::PlaygroundState>>,
    /// Data directory reported by `/api/config`. None = unknown.
    data_dir: Option<PathBuf>,
    /// Durable task/input store. Without it task APIs and Playground writes
    /// report unavailable rather than admitting volatile effects.
    store: Option<std::sync::Arc<shenko_store::store::Store>>,
    native: Option<Arc<shenko_subject::NativeChildService>>,
    inference: Option<api::InferenceStatus>,
    context: Option<api::ContextFacts>,
    inbox_tx: Option<tokio::sync::mpsc::Sender<shenko_types::inbox::InboxEvent>>,
    /// Rich presentation service: document projection, degradation, caps.
    presentation: Option<Arc<PresentationService>>,
    /// Durable stream sink; absent = no real producer wired (UI reports
    /// streaming:false rather than faking chunks).
    stream_sink: Option<Arc<StreamSink>>,
    /// Live media ingress service for `/api/media/*`; `None` leaves those
    /// routes answering `available: false` instead of faking admission.
    media_ingress: Option<std::sync::Arc<shenko_media::MediaIngress>>,
    media_runtime: Option<std::sync::Arc<shenko_media::runtime::MediaRuntime>>,
    /// The configuration service the API exposes. When omitted but a store
    /// is present, `run` constructs one so `/api/config/*` is live.
    config: Option<std::sync::Arc<ConfigService>>,
    /// Ownership state for the claim and authenticated-actor routes.
    owner: Option<OwnerState>,
    /// Launcher/owner of long-lived artifact backend services. The runtime
    /// reconciles it before admitting work; the UI and handoff seam share
    /// that already-owned controller, never repeat boot recovery here.
    artifact_services: Option<std::sync::Arc<ArtifactServiceController>>,
}

impl UiServer {
    pub fn new(port: u16) -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], port)),
            state: WsState::new(),
            playground_root: default_playground_root(),
            playground_state: None,
            data_dir: None,
            store: None,
            native: None,
            inference: None,
            context: None,
            inbox_tx: None,
            presentation: None,
            stream_sink: None,
            media_ingress: None,
            media_runtime: None,
            config: None,
            owner: None,
            artifact_services: None,
        }
    }

    pub fn ws_state(&self) -> WsState {
        self.state.clone()
    }

    pub fn with_native(mut self, native: Arc<shenko_subject::NativeChildService>) -> Self {
        self.native = Some(native);
        self
    }

    /// Point the persistent Playground at `<data_dir>/playground`. docs/specs/configuration-playground.md#spec-43-2.
    pub fn with_data_dir(mut self, data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        self.playground_root = data_dir.join("playground");
        self.data_dir = Some(data_dir);
        self
    }

    /// Attach the shared Playground instance. The subject's own
    /// `playground.manage` handler and this server must write the same
    /// lifecycle, so the composition root builds one instance and hands it to
    /// both (docs/specs/configuration-playground.md#spec-57-6).
    pub fn with_playground_state(
        mut self,
        playground: Arc<crate::playground::PlaygroundState>,
    ) -> Self {
        self.playground_state = Some(playground);
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
        reserved_output_tokens: u64,
    ) -> Self {
        self.context = Some(api::ContextFacts {
            registry: Some(registry), capabilities, reserved_output_tokens,
        });
        self
    }

    /// Wire the subject inbox so UI messages reach the loop. docs/specs/subject-runtime.md#spec-5-1.
    pub fn with_inbox(
        mut self,
        inbox_tx: tokio::sync::mpsc::Sender<shenko_types::inbox::InboxEvent>,
    ) -> Self {
        self.inbox_tx = Some(inbox_tx);
        self
    }

    /// Attach the rich presentation service (document projection + caps).
    /// When omitted, `run` builds one with server-side facts (streaming on
    /// iff a real stream sink is attached).
    pub fn with_presentation(mut self, presentation: Arc<PresentationService>) -> Self {
        self.presentation = Some(presentation);
        self
    }

    /// Attach the durable stream sink (real producer required upstream).
    pub fn with_stream_sink(mut self, sink: Arc<StreamSink>) -> Self {
        self.stream_sink = Some(sink);
        self
    }

    /// Attach the live media ingress service behind `/api/media/*`. docs/specs/media.md#spec-20.
    pub fn with_media_ingress(
        mut self,
        ingress: std::sync::Arc<shenko_media::MediaIngress>,
    ) -> Self {
        self.media_ingress = Some(ingress);
        self
    }

    /// Attach the media runtime (realtime sessions, recording, barge-in,
    /// speech playback) behind `/api/media/*`. docs/specs/media.md#spec-20.
    pub fn with_media_runtime(
        mut self,
        runtime: std::sync::Arc<shenko_media::runtime::MediaRuntime>,
    ) -> Self {
        self.media_runtime = Some(runtime);
        self
    }

    /// Wire the subject outbox so model output reaches the timeline. docs/specs/web-interface.md#spec-21-12.
    pub fn with_outbox(
        mut self,
        outbox_tx: tokio::sync::broadcast::Sender<shenko_subject::outbox::OutboxMessage>,
    ) -> Self {
        self.state = self.state.with_outbox(outbox_tx);
        self
    }

    pub fn with_config(mut self, config: std::sync::Arc<ConfigService>) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_owner(mut self, owner: OwnerState) -> Self {
        self.owner = Some(owner);
        self
    }

    pub fn with_artifact_services(mut self, services: std::sync::Arc<ArtifactServiceController>) -> Self {
        self.artifact_services = Some(services);
        self
    }

    /// The handle the update seam uses to stop and restart long-lived
    /// artifact services across a generation hand-off.
    pub fn artifact_services(&self) -> Option<std::sync::Arc<ArtifactServiceController>> {
        self.artifact_services.clone()
    }

    pub async fn run(self) -> Result<()> {
        let mut state = self.state;
        // Build the durable input service when the real store, genesis
        // subject, and live inbox are all present (docs/specs/web-interface.md#spec-41-18). Without all
        // three the UI honestly reports input:false instead of faking
        // acceptance. Presentation defaults to server facts when the caller
        // did not inject one.
        if state.input.is_none() {
            if let (Some(store), Some(subject_id), Some(inbox_tx)) =
                (self.store.clone(), state.subject_id, self.inbox_tx.clone())
            {
                let presentation = self.presentation.clone().unwrap_or_else(|| {
                    PresentationService::new(
                        8,
                        SurfaceCapabilityProvider::new(
                            self.stream_sink.is_some(),
                            "shenko-ui".to_owned(),
                        ),
                    )
                });
                state.presentation = Some(Arc::clone(&presentation));
                state.input = Some(WebInputService::new(
                    store,
                    subject_id,
                    Some(Arc::new(inbox_tx)),
                    presentation,
                ));
            }
        }
        state.stream = state.stream.or_else(|| self.stream_sink.clone());

        // Playground shares the inbox the WS handler already writes to, so
        // artifact changes are perceived by the same subject loop. docs/specs/configuration-playground.md#spec-43-2.
        let playground_root = self.playground_root;
        let playground_state = match self.playground_state.clone() {
            Some(shared) => Some(shared),
            None => {
                if let Some(store) = self.store.clone() {
                    let mut pg = PlaygroundState::open(playground_root.clone(), store)?;
                    if let Some(tx) = self.inbox_tx.clone() { pg = pg.with_inbox(tx); }
                    if let Some(native) = self.native.clone() { pg = pg.with_native(native); }
                    Some(Arc::new(pg))
                } else { None }
            }
        };

        // Observe the subject outbox independently of any browser client, so
        // `/api/self` knows the last committed thought step and whether the
        // loop is active. A broadcast subscriber never consumes messages meant
        // for WS clients. docs/specs/web-interface.md#spec-21-12.
        if let Some(outbox_tx) = state.outbox_tx.as_ref() {
            state.live.set_wired(true);
            let live = Arc::clone(&state.live);
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
        media_state.runtime = self.media_runtime.clone();
        if let Some(data_dir) = self.data_dir.clone() {
            api_state = api_state.with_data_dir(data_dir);
        }
        if let Some(inference) = self.inference.clone() {
            api_state = api_state.with_inference(inference);
        }
        if let Some(context) = self.context.clone() {
            api_state = api_state.with_context(context);
        }
        if let Some(config) = self.config.clone() {
            api_state = api_state.with_config(config);
        } else if let Some(store) = self.store.clone() {
            // A store without an explicit service still exposes discover/
            // read of whatever namespaces were registered on this process.
            api_state = api_state.with_config(std::sync::Arc::new(ConfigService::new(store)));
        }
        if let Some(owner) = self.owner.clone() {
            api_state = api_state.with_owner(owner);
        }
        // Shared Arc registries: `/api/self` reports the same live objects the
        // Playground routes and WS handler use, not a second copy.
        if let Some(pg) = playground_state.clone() {
            api_state = api_state.with_playground((*pg).clone());
        }
        api_state = api_state.with_live(Arc::clone(&state.live));

        let playground_routes = match playground_state {
            Some(pg) => playground::router((*pg).clone()),
            None => Router::new()
                .route("/playground", axum::routing::any(playground_unavailable))
                .route("/playground/{*path}", axum::routing::any(playground_unavailable)),
        };
        let app = Router::new()
            .route("/", get(index_handler))
            .route("/assets/theme-bootstrap.js", get(theme_bootstrap_handler))
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
            .layer(Extension(state));
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
