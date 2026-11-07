//! The Feishu/Lark event-dispatcher monitor — SPEC §16, §15.3, §35.6.
//!
//! Ingress is a WebSocket long connection to the platform's callback
//! dispatcher. The connection authenticates with the app ticket
//! (`app_access_token`); every event frame is the platform's own event JSON,
//! which the adapter classifies and forwards. A dropped connection is retried
//! with a bounded backoff, and the platform replays what a gap missed, so the
//! monitor itself keeps no cursor.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::common::{HealthState, ReplyContextCache};
use crate::ingress::SharedAdapterIngress;

use super::auth::TenantAuth;
use super::config::FeishuConfig;
use super::events::{FEISHU_SOURCE, classify};

/// The keepalive interval of the long connection.
const HEARTBEAT: Duration = Duration::from_secs(30);

/// Why one monitor connection attempt ended.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MonitorError {
    /// The app ticket could not be obtained.
    #[error("feishu monitor could not authenticate: {0}")]
    Unavailable(String),
    /// The WebSocket transport failed.
    #[error("feishu monitor transport failed: {0}")]
    Transport(String),
    /// The dispatcher closed the connection.
    #[error("feishu monitor connection closed: {0}")]
    Closed(String),
    /// The event could not be made durable.
    #[error("feishu ingress refused the event: {0}")]
    Ingress(String),
}

/// The Feishu event-dispatcher monitor.
pub struct FeishuMonitor {
    config: Arc<FeishuConfig>,
    auth: Arc<TenantAuth>,
    health: HealthState,
    replies: ReplyContextCache,
    ledger: SharedAdapterIngress,
}

impl FeishuMonitor {
    /// Build a monitor for one app.
    pub fn new(
        config: Arc<FeishuConfig>,
        auth: Arc<TenantAuth>,
        health: HealthState,
        replies: ReplyContextCache,
        ledger: SharedAdapterIngress,
    ) -> Self {
        Self {
            config,
            auth,
            health,
            replies,
            ledger,
        }
    }

    /// Run the monitor until the runtime asks it to stop.
    pub async fn run(&self, cancel: CancellationToken) -> Result<(), MonitorError> {
        let mut backoff = Duration::from_secs(1);
        loop {
            if cancel.is_cancelled() {
                self.health.transition("stopped", None).await;
                return Ok(());
            }
            self.health
                .transition("connecting", Some("opening the dispatcher connection".into()))
                .await;
            match self.run_once(&cancel).await {
                Ok(()) => {
                    self.health
                        .transition("degraded", Some("dispatcher connection ended".into()))
                        .await;
                }
                Err(error) => {
                    self.health.transition("degraded", Some(error.to_string())).await;
                }
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    self.health.transition("stopped", None).await;
                    return Ok(());
                }
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    /// One dispatcher connection.
    async fn run_once(&self, cancel: &CancellationToken) -> Result<(), MonitorError> {
        let token = self
            .auth
            .app_token()
            .await
            .map_err(|error| MonitorError::Unavailable(error.to_string()))?;
        let url = format!("{}/callback/ws", self.config.base_url);
        let request = tokio_tungstenite::tungstenite::handshake::client::Request::builder()
            .uri(url.as_str())
            .header("Authorization", format!("Bearer {token}"))
            .body(())
            .map_err(|error| MonitorError::Transport(format!("request build failed: {error}")))?;
        let (mut socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|error| MonitorError::Transport(error.to_string()))?;
        self.health
            .transition("ready", Some("dispatcher session established".into()))
            .await;

        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = socket.close(None).await;
                    return Ok(());
                }
                _ = heartbeat.tick() => {
                    if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                        return Err(MonitorError::Closed("dispatcher did not take the keepalive".into()));
                    }
                }
                message = socket.next() => {
                    let Some(message) = message else {
                        return Err(MonitorError::Closed("dispatcher closed the connection".into()));
                    };
                    let message = match message {
                        Ok(message) => message,
                        Err(error) => {
                            return Err(MonitorError::Transport(error.to_string()));
                        }
                    };
                    match message {
                        Message::Text(text) => {
                            let frame: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                            // Durable first: an event the ledger cannot take
                            // ends this connection, and the platform replays
                            // the window on the next one.
                            self.handle(&frame)
                                .await
                                .map_err(MonitorError::Ingress)?;
                        }
                        Message::Ping(payload) => {
                            if socket.send(Message::Pong(payload)).await.is_err() {
                                return Err(MonitorError::Closed("dispatcher did not take the pong".into()));
                            }
                        }
                        Message::Close(_) => {
                            return Err(MonitorError::Closed("dispatcher sent a close frame".into()));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Classify one event frame and make it durable before the connection is
    /// allowed to move on.
    async fn handle(&self, frame: &Value) -> Result<(), String> {
        let Some(envelope) = classify(&self.config, frame) else {
            return Ok(());
        };
        self.health.observe_event().await;
        self.ledger.record(FEISHU_SOURCE, &envelope, frame)?;
        Ok(())
    }
}
