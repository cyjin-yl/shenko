//! Real NapCat/OneBot v11 WebSocket client — SPEC §16
//! Connects to NapCat WS endpoint, receives CQ events, sends messages.
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures::{SinkExt, StreamExt};
use tracing::{info, warn, error};
use serde::{Deserialize, Serialize};
use anyhow::Result;
use url::Url;

use crate::config::NapCatConfig;

/// OneBot v11 event from NapCat
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OneBotEvent {
    pub post_type: String,
    pub time: Option<i64>,
    pub self_id: Option<u64>,
    #[serde(flatten)]
    pub data: serde_json::Value,
}

/// Outgoing message action
#[derive(Debug, Clone, Serialize)]
pub struct OneBotAction {
    pub action: String,
    pub params: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub echo: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NapCatClient {
    pub config: NapCatConfig,
    event_tx: mpsc::Sender<OneBotEvent>,
    action_tx: mpsc::Sender<OneBotAction>,
}

impl NapCatClient {
    pub fn new(config: NapCatConfig) -> (Self, mpsc::Receiver<OneBotEvent>, mpsc::Receiver<OneBotAction>) {
        let (event_tx, event_rx) = mpsc::channel(256);
        let (action_tx, action_rx) = mpsc::channel(256);
        (Self { config, event_tx, action_tx }, event_rx, action_rx)
    }

    /// Send a private message via NapCat
    pub async fn send_private_message(&self, user_id: u64, message: &str) -> Result<()> {
        let action = OneBotAction {
            action: "send_private_msg".to_string(),
            params: serde_json::json!({ "user_id": user_id, "message": message }),
            echo: Some(format!("pm_{user_id}")),
        };
        self.action_tx.send(action).await?;
        Ok(())
    }

    /// Send a group message via NapCat
    pub async fn send_group_message(&self, group_id: u64, message: &str) -> Result<()> {
        let action = OneBotAction {
            action: "send_group_msg".to_string(),
            params: serde_json::json!({ "group_id": group_id, "message": message }),
            echo: Some(format!("gm_{group_id}")),
        };
        self.action_tx.send(action).await?;
        Ok(())
    }

    /// Run the WS connection loop — reconnects on disconnect
    pub async fn run_connection_loop(
        config: NapCatConfig,
        event_tx: mpsc::Sender<OneBotEvent>,
        mut action_rx: mpsc::Receiver<OneBotAction>,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        loop {
            if cancel.is_cancelled() { break; }

            let url = match Url::parse(&config.ws_url) {
                Ok(u) => u,
                Err(e) => { error!(url=%config.ws_url, err=%e, "invalid WS URL"); break; }
            };

            info!(url=%config.ws_url, "NapCat: connecting");

            match connect_async(url.as_str()).await {
                Err(e) => {
                    warn!(err=%e, "NapCat: connection failed, retrying in {}ms", config.reconnect_interval_ms);
                    tokio::time::sleep(tokio::time::Duration::from_millis(config.reconnect_interval_ms)).await;
                    continue;
                }
                Ok((ws_stream, _)) => {
                    info!("NapCat: connected");
                    let (mut write, mut read) = ws_stream.split();

                    loop {
                        tokio::select! {
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(Message::Text(text))) => {
                                        match serde_json::from_str::<OneBotEvent>(&text) {
                                            Ok(event) => {
                                                let _ = event_tx.send(event).await;
                                            }
                                            Err(e) => {
                                                // Not all messages are events (some are API responses)
                                                tracing::debug!("NapCat: non-event msg: {}", &text[..text.len().min(100)]);
                                            }
                                        }
                                    }
                                    Some(Ok(Message::Close(_))) | None => {
                                        warn!("NapCat: connection closed");
                                        break;
                                    }
                                    Some(Err(e)) => {
                                        warn!(err=%e, "NapCat: WS error");
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                            action = action_rx.recv() => {
                                match action {
                                    Some(a) => {
                                        let json = serde_json::to_string(&a).unwrap_or_default();
                                        if let Err(e) = write.send(Message::Text(json.into())).await {
                                            warn!(err=%e, "NapCat: send failed");
                                            break;
                                        }
                                    }
                                    None => break,
                                }
                            }
                            _ = cancel.cancelled() => { break; }
                        }
                    }
                }
            }

            if cancel.is_cancelled() { break; }
            tokio::time::sleep(tokio::time::Duration::from_millis(config.reconnect_interval_ms)).await;
        }
        info!("NapCat: connection loop terminated");
    }
}
