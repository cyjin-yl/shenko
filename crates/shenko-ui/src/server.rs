use std::net::SocketAddr;
use axum::{Router, routing::get};
use tower_http::cors::CorsLayer;
use tracing::info;
use anyhow::Result;

use crate::routes::*;

pub struct UiServer {
    pub bind: SocketAddr,
}

impl UiServer {
    pub fn new(port: u16) -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], port)),
        }
    }

    pub async fn run(self) -> Result<()> {
        let app = Router::new()
            .route("/", get(index_handler))
            .route("/health", get(health_handler))
            .route("/ws", get(crate::ws::ws_handler))
            .layer(CorsLayer::permissive());

        info!(addr = %self.bind, "UI server starting");
        let listener = tokio::net::TcpListener::bind(self.bind).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }
}
