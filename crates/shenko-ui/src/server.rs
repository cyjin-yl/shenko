use std::net::SocketAddr;
use axum::{Router, routing::get, Extension};
use tower_http::cors::CorsLayer;
use tracing::info;
use anyhow::Result;

use crate::routes::*;
use crate::ws::{ws_handler, WsState};

pub struct UiServer {
    pub bind: SocketAddr,
    pub state: WsState,
}

impl UiServer {
    pub fn new(port: u16) -> Self {
        Self { bind: SocketAddr::from(([0, 0, 0, 0], port)), state: WsState::new() }
    }

    pub fn ws_state(&self) -> WsState { self.state.clone() }

    /// Wire the subject inbox so UI messages reach the loop. SPEC §5.1.
    pub fn with_inbox(
        mut self,
        inbox_tx: tokio::sync::mpsc::Sender<shenko_subject::inbox::InboxEvent>,
    ) -> Self {
        self.state = self.state.with_inbox(inbox_tx);
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
        let app = Router::new()
            .route("/", get(index_handler))
            .route("/health", get(health_handler))
            .route("/ws", get(ws_handler))
            .layer(Extension(self.state))
            .layer(CorsLayer::permissive());
        info!(addr=%self.bind, "UI server starting");
        let listener = tokio::net::TcpListener::bind(self.bind).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }
}
