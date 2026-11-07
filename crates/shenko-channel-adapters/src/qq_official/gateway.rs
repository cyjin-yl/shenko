//! The official QQ bot gateway session — SPEC §16, §15.3, §35.6.
//!
//! The official platform pushes events over a WebSocket that behaves like a
//! versioned session:
//!
//! | opcode | meaning |
//! | --- | --- |
//! | 0 | Dispatch — one event |
//! | 1 | Heartbeat — the client answers with the newest sequence |
//! | 2 | Identify — the client authenticates and subscribes |
//! | 6 | Resume — the client restores a session and asks for the gap |
//! | 7 | Reconnect — the server asks the client to reconnect |
//! | 9 | Invalid Session — the session cannot be resumed |
//! | 10 | Hello — the server's first frame, carries the heartbeat interval |
//! | 11 | Heartbeat ACK |
//!
//! The session id and the newest sequence form the CURSOR. They are written
//! to disk before they are used, so a restart resumes instead of silently
//! skipping the events that arrived while the process was down. The cursor is
//! per account: it never mixes two bots.
//!
//! One gateway connection belongs to one bot account. This module never
//! starts a second personality and never answers an event on its own.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::common::{HealthState, ReplyContext, ReplyContextCache, endpoint_provenance};
use crate::ingress::SharedAdapterIngress;

use super::api::QqOfficialApi;
use super::config::QqOfficialConfig;
use super::events::{QQ_OFFICIAL_SOURCE, classify};

/// The durable cursor of one gateway session.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GatewayCursor {
    /// The session id the platform issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The newest sequence the client has seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
}

/// Where one account's gateway cursor is stored.
pub fn cursor_path(data_dir: &Path, account: &str) -> PathBuf {
    data_dir.join(format!("qq-official-gateway-{account}.json"))
}

/// Read a stored cursor; a missing or unreadable file means "no cursor".
pub async fn load_cursor(path: &Path) -> GatewayCursor {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => GatewayCursor::default(),
    }
}

/// Write the cursor so a restart can resume.
pub async fn store_cursor(path: &Path, cursor: &GatewayCursor) -> Result<(), String> {
    let text = serde_json::to_string_pretty(cursor)
        .map_err(|error| format!("gateway cursor encode failed: {error}"))?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| format!("gateway cursor directory failed: {error}"))?;
    }
    tokio::fs::write(path, text)
        .await
        .map_err(|error| format!("gateway cursor write failed: {error}"))
}

/// Why a gateway session stopped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GatewayError {
    /// The gateway address could not be obtained.
    #[error("qq official gateway unavailable: {0}")]
    Unavailable(String),
    /// The transport failed.
    #[error("qq official gateway transport failed: {0}")]
    Transport(String),
    /// The platform closed the session.
    #[error("qq official gateway closed: {0}")]
    Closed(String),
    /// The event could not be made durable, so the cursor must not move.
    #[error("qq official ingress refused the event: {0}")]
    Ingress(String),
}

/// One official QQ gateway connection.
pub struct QqGatewaySession {
    config: Arc<QqOfficialConfig>,
    api: QqOfficialApi,
    health: HealthState,
    replies: ReplyContextCache,
    cursor_path: PathBuf,
    ledger: SharedAdapterIngress,
}

impl QqGatewaySession {
    /// Build a session for one bot account.
    pub fn new(
        config: Arc<QqOfficialConfig>,
        api: QqOfficialApi,
        health: HealthState,
        replies: ReplyContextCache,
        cursor_path: PathBuf,
        ledger: SharedAdapterIngress,
    ) -> Self {
        Self {
            config,
            api,
            health,
            replies,
            cursor_path,
            ledger,
        }
    }

    /// Run the gateway until the runtime asks it to stop.
    ///
    /// A dropped connection is retried with a bounded backoff. The platform's
    /// "reconnect" and "invalid session" opcodes decide whether the client
    /// resumes the old session or identifies a new one.
    pub async fn run(
        &self,
        cancel: CancellationToken,
    ) -> Result<(), GatewayError> {
        let mut cursor = load_cursor(&self.cursor_path).await;
        let mut backoff = Duration::from_secs(1);
        loop {
            if cancel.is_cancelled() {
                self.health
                    .transition("stopped", Some("runtime asked the connector to stop".into()))
                    .await;
                return Ok(());
            }
            self.health
                .transition("connecting", Some("opening the gateway connection".into()))
                .await;
            let outcome = self
                .run_once(&cancel, &mut cursor)
                .await;
            match outcome {
                Ok(()) => {
                    // A clean end still means the connection is gone.
                    self.health
                        .transition("degraded", Some("gateway connection ended".into()))
                        .await;
                }
                Err(error) => {
                    self.health
                        .transition("degraded", Some(error.to_string()))
                        .await;
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

    /// One connection attempt.
    async fn run_once(
        &self,
        cancel: &CancellationToken,
        cursor: &mut GatewayCursor,
    ) -> Result<(), GatewayError> {
        let gateway = self
            .api
            .gateway_url()
            .await
            .map_err(|error| GatewayError::Unavailable(error.to_string()))?;
        tracing::info!(gateway=%endpoint_provenance(&gateway), "qq official gateway connecting");
        let (mut socket, _) = tokio_tungstenite::connect_async(&gateway)
            .await
            .map_err(|error| GatewayError::Transport(error.to_string()))?;

        let token = self
            .api
            .access_token()
            .await
            .map_err(|error| GatewayError::Unavailable(error.to_string()))?;
        let heartbeat_interval = self
            .handshake(&mut socket, &token, cursor)
            .await
            .map_err(|error| GatewayError::Transport(error.to_string()))?;
        self.health
            .transition("ready", Some("gateway session established".into()))
            .await;

        let mut heartbeat = tokio::time::interval(heartbeat_interval);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = socket.close(None).await;
                    return Ok(());
                }
                _ = heartbeat.tick() => {
                    let payload = json!({
                        "op": 1,
                        "d": cursor.seq,
                    });
                    socket
                        .send(Message::Text(payload.to_string().into()))
                        .await
                        .map_err(|error| GatewayError::Transport(error.to_string()))?;
                }
                message = socket.next() => {
                    let Some(message) = message else {
                        return Err(GatewayError::Closed("gateway closed the connection".into()));
                    };
                    let message = message.map_err(|error| GatewayError::Transport(error.to_string()))?;
                    let Message::Text(text) = message else {
                        continue;
                    };
                    let frame: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    let opcode = frame.get("op").and_then(Value::as_i64).unwrap_or(-1);
                    match opcode {
                        0 => {
                            if let Some(seq) = frame.get("s").and_then(Value::as_i64) {
                                cursor.seq = Some(seq);
                            }
                            // The event must be durable before the cursor that
                            // claims it is advanced: a crash in between then
                            // replays the window instead of losing the message.
                            self.handle_dispatch(&frame)
                                .await
                                .map_err(GatewayError::Ingress)?;
                            let _ = store_cursor(&self.cursor_path, cursor).await;
                        }
                        7 => {
                            // The server asks for a reconnect; resume keeps
                            // the session so the gap is replayed.
                            return Ok(());
                        }
                        9 => {
                            // The session cannot be resumed; identify again.
                            cursor.session_id = None;
                            cursor.seq = None;
                            let _ = store_cursor(&self.cursor_path, cursor).await;
                            return Ok(());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Identify or resume, and read the heartbeat interval from the Hello.
    async fn handshake(
        &self,
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        token: &str,
        cursor: &mut GatewayCursor,
    ) -> Result<Duration, String> {
        let hello = socket
            .next()
            .await
            .ok_or_else(|| "gateway closed before the hello frame".to_string())?
            .map_err(|error| format!("gateway hello receive failed: {error}"))?;
        let Message::Text(text) = hello else {
            return Err("gateway did not send a text hello frame".to_string());
        };
        let frame: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let interval = frame
            .get("d")
            .and_then(|data| data.get("heartbeat_interval"))
            .and_then(Value::as_u64)
            .unwrap_or(41_250);
        let interval = Duration::from_millis(interval.max(1_000));

        let payload = match (&cursor.session_id, cursor.seq) {
            (Some(session_id), Some(seq)) => json!({
                "op": 6,
                "d": {
                    "token": format!("QQBot {token}"),
                    "session_id": session_id,
                    "seq": seq,
                }
            }),
            _ => json!({
                "op": 2,
                "d": {
                    "token": format!("QQBot {token}"),
                    "intents": self.config.intents,
                    "shard": self.config.shard,
                    "properties": {
                        "$os": "linux",
                        "$browser": "shenko",
                        "$device": "shenko",
                    }
                }
            }),
        };
        socket
            .send(Message::Text(payload.to_string().into()))
            .await
            .map_err(|error| format!("identify failed: {error}"))?;

        // The platform answers the identify with a Ready frame, or with an
        // Invalid Session frame when the resume was refused.
        let ready = socket
            .next()
            .await
            .ok_or_else(|| "gateway closed before the ready frame".to_string())?
            .map_err(|error| format!("gateway ready receive failed: {error}"))?;
        let Message::Text(text) = ready else {
            return Err("gateway did not send a text ready frame".to_string());
        };
        let frame: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let opcode = frame.get("op").and_then(Value::as_i64).unwrap_or(-1);
        if opcode == 9 {
            cursor.session_id = None;
            cursor.seq = None;
            return Err("the platform refused the session".to_string());
        }
        if opcode == 0 {
            if let Some(session_id) = frame
                .get("d")
                .and_then(|data| data.get("session_id"))
                .and_then(Value::as_str)
            {
                cursor.session_id = Some(session_id.to_string());
                let _ = store_cursor(&self.cursor_path, cursor).await;
            }
        }
        Ok(interval)
    }

    /// Classify one dispatch frame and make it durable.
    ///
    /// The classified envelope and the platform's own frame are written to the
    /// account's ingress ledger before the cursor advances. An error here is
    /// returned to `run_once`, which ends the connection without storing the
    /// new sequence: the platform replays the window.
    async fn handle_dispatch(&self, frame: &Value) -> Result<(), String> {
        let Some(envelope) = classify(&self.config, frame) else {
            return Ok(());
        };
        self.health.observe_event().await;
        self.ledger.record(QQ_OFFICIAL_SOURCE, &envelope, frame)?;
        if let Some(target) = &envelope.reply_target {
            let conversation = target.conversation.as_ref().unwrap_or(&target.id);
            if !conversation.is_empty() {
                self.replies.record(
                    conversation,
                    ReplyContext {
                        message_id: envelope.message_id.clone(),
                        context_token: None,
                        thread_id: None,
                        at: chrono::Utc::now(),
                    },
                );
            }
        }
        Ok(())
    }
}
