//! Real NapCat/OneBot v11 WebSocket client — SPEC §16, §35.6
//! Connects to NapCat WS endpoint, receives CQ events, sends messages.
//!
//! Outbound calls are correlated by `echo` so a caller learns the provider's
//! actual status/retcode. A missing response is an *unknown* outcome: the
//! request may or may not have been executed, so callers must reconcile
//! instead of blindly resending (SPEC §35.6, §41.4 X03).
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures::{SinkExt, StreamExt};
use tracing::{info, warn, error};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::NapCatConfig;
use crate::spool::IngressSpool;

/// How long an outbound action waits for its `echo` response before the
/// outcome is declared unknown. SPEC §35.6.
pub const SEND_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Pending outbound actions, keyed by echo token.
pub type PendingActions = Arc<parking_lot::Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>;

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

/// Provider response to one outbound action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendReceipt {
    pub echo: String,
    /// OneBot `status`: `ok`, `failed` or `async`.
    pub status: String,
    pub retcode: i64,
    #[serde(default)]
    pub data: serde_json::Value,
    #[serde(default)]
    pub message: Option<String>,
}

impl SendReceipt {
    pub fn is_success(&self) -> bool {
        self.retcode == 0 && self.status != "failed"
    }
}

/// Why an outbound call did not produce a definite answer.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("napcat action channel closed — connector not running")]
    Disconnected,
    /// The request may have reached the provider. Never auto-retry.
    #[error("unknown outcome: {0}")]
    Unknown(String),
}

#[derive(Clone)]
pub struct NapCatClient {
    pub config: NapCatConfig,
    event_tx: mpsc::Sender<OneBotEvent>,
    action_tx: mpsc::Sender<OneBotAction>,
    /// Echo token → waiter. Shared with `run_connection_loop`.
    pub pending_actions: PendingActions,
    pub spool: Arc<IngressSpool>,
}

impl std::fmt::Debug for NapCatClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NapCatClient")
            .field("config", &self.config)
            .field("spool_pending", &self.spool.pending_count())
            .field("pending_actions", &self.pending_actions.lock().len())
            .finish()
    }
}

impl NapCatClient {
    pub fn new(config: NapCatConfig) -> (Self, mpsc::Receiver<OneBotEvent>, mpsc::Receiver<OneBotAction>) {
        let (event_tx, event_rx) = mpsc::channel(256);
        let (action_tx, action_rx) = mpsc::channel(256);
        let spool = Arc::new(IngressSpool::new(config.bot_qq));
        (Self {
            config,
            event_tx,
            action_tx,
            pending_actions: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            spool,
        }, event_rx, action_rx)
    }

    /// Sender for inbound events; hand to `run_connection_loop`.
    pub fn event_sender(&self) -> mpsc::Sender<OneBotEvent> { self.event_tx.clone() }

    /// Send a private message via NapCat, generating a fresh echo token.
    pub async fn send_private_message(&self, user_id: u64, message: &str) -> Result<SendReceipt, SendError> {
        let echo = uuid::Uuid::new_v4().to_string();
        self.send_private_message_with_echo(user_id, message, echo).await
    }

    /// Send a group message via NapCat, generating a fresh echo token.
    pub async fn send_group_message(&self, group_id: u64, message: &str) -> Result<SendReceipt, SendError> {
        let echo = uuid::Uuid::new_v4().to_string();
        self.send_group_message_with_echo(group_id, message, echo).await
    }

    /// Send a private message using a caller-supplied echo token (the durable
    /// operation ledger keeps the same token so late echoes are attributable).
    pub async fn send_private_message_with_echo(
        &self,
        user_id: u64,
        message: &str,
        echo: String,
    ) -> Result<SendReceipt, SendError> {
        self.send_action(
            "send_private_msg",
            serde_json::json!({ "user_id": user_id, "message": message }),
            echo,
        ).await
    }

    /// Send a group message using a caller-supplied echo token.
    pub async fn send_group_message_with_echo(
        &self,
        group_id: u64,
        message: &str,
        echo: String,
    ) -> Result<SendReceipt, SendError> {
        self.send_action(
            "send_group_msg",
            serde_json::json!({ "group_id": group_id, "message": message }),
            echo,
        ).await
    }

    /// Enqueue one action and await its correlated response. SPEC §35.6.
    async fn send_action(
        &self,
        action: &str,
        params: serde_json::Value,
        echo: String,
    ) -> Result<SendReceipt, SendError> {
        let (tx, rx) = oneshot::channel();
        self.pending_actions.lock().insert(echo.clone(), tx);

        let action = OneBotAction {
            action: action.to_string(),
            params,
            echo: Some(echo.clone()),
        };
        if self.action_tx.send(action).await.is_err() {
            self.pending_actions.lock().remove(&echo);
            return Err(SendError::Disconnected);
        }

        match tokio::time::timeout(SEND_RESPONSE_TIMEOUT, rx).await {
            Ok(Ok(value)) => Ok(SendReceipt {
                echo,
                status: value.get("status").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
                retcode: value.get("retcode").and_then(|v| v.as_i64()).unwrap_or(-1),
                data: value.get("data").cloned().unwrap_or(serde_json::Value::Null),
                message: value.get("message").and_then(|v| v.as_str()).map(str::to_string),
            }),
            // The connector dropped the waiter (disconnect/cancel) — the action
            // may already have been written to the socket. Unknown, not failed.
            Ok(Err(_)) => Err(SendError::Unknown("connector dropped pending action".into())),
            Err(_) => {
                // Late responses for this echo are ignored by the reader loop.
                self.pending_actions.lock().remove(&echo);
                Err(SendError::Unknown(format!(
                    "no response within {}s",
                    SEND_RESPONSE_TIMEOUT.as_secs()
                )))
            }
        }
    }

    /// Run the WS connection loop — reconnects on disconnect
    pub async fn run_connection_loop(
        config: NapCatConfig,
        event_tx: mpsc::Sender<OneBotEvent>,
        mut action_rx: mpsc::Receiver<OneBotAction>,
        pending_actions: PendingActions,
        spool: Arc<IngressSpool>,
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
                    spool.record_reconnect();
                    let (mut write, mut read) = ws_stream.split();

                    loop {
                        tokio::select! {
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(Message::Text(text))) => {
                                        let value: serde_json::Value = match serde_json::from_str(&text) {
                                            Ok(v) => v,
                                            Err(e) => {
                                                warn!(err=%e, "NapCat: non-JSON message");
                                                continue;
                                            }
                                        };

                                        if value.get("post_type").is_some() {
                                            match serde_json::from_value::<OneBotEvent>(value.clone()) {
                                                Ok(event) => {
                                                    // Dedup via spool; duplicates are silently dropped
                                                    if spool.ingest(value).is_some() {
                                                        let _ = event_tx.send(event).await;
                                                    }
                                                }
                                                Err(e) => {
                                                    warn!(err=%e, "NapCat: failed to deserialize event");
                                                }
                                            }
                                        } else if let Some(echo) = value.get("echo").and_then(|v| v.as_str()) {
                                            // Action response — route to its waiter.
                                            let waiter = pending_actions.lock().remove(echo);
                                            match waiter {
                                                Some(tx) => { let _ = tx.send(value); }
                                                None => {
                                                    // E.g. a response that arrived after our timeout.
                                                    // The ledger marks it unknown and reconciles later.
                                                    tracing::debug!(echo, "NapCat: unmatched action response");
                                                }
                                            }
                                        } else {
                                            tracing::debug!("NapCat: unrecognized message");
                                        }
                                    }
                                    Some(Ok(Message::Close(_))) | None => {
                                        warn!("NapCat: connection closed");
                                        spool.record_disconnect();
                                        break;
                                    }
                                    Some(Err(e)) => {
                                        warn!(err=%e, "NapCat: WS error");
                                        spool.record_disconnect();
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
                                            spool.record_disconnect();
                                            break;
                                        }
                                    }
                                    None => break,
                                }
                            }
                            _ = cancel.cancelled() => { break; }
                        }
                    }

                    // This connection is gone. Every action still waiting may
                    // have reached NapCat before the socket dropped, so its
                    // outcome is unknown — wake the callers instead of letting
                    // them sit until timeout. Actions still sitting in the
                    // queue were never written; discard them so they are not
                    // silently transmitted on the next connection while their
                    // callers were told "unknown". SPEC §35.6: reconcile, no
                    // implicit retry.
                    while action_rx.try_recv().is_ok() {}
                    pending_actions.lock().clear();
                }
            }

            if cancel.is_cancelled() { break; }
            tokio::time::sleep(tokio::time::Duration::from_millis(config.reconnect_interval_ms)).await;
        }
        pending_actions.lock().clear();
        info!("NapCat: connection loop terminated");
    }
}
