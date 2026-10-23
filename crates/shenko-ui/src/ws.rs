//! WebSocket handler — routes UI messages to subject inbox, streams subject output.
//! SPEC §21: single subject timeline, real-time streaming.
use axum::{
    extract::{ws::{WebSocket, WebSocketUpgrade, Message}, Extension},
    response::Response,
};
use tracing::{info, warn, debug};
use tokio::sync::{broadcast, mpsc::Sender};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::Utc;
use shenko_subject::inbox::{InboxEvent, InboxEventKind};
use shenko_types::ids::EntityId;

/// Message on the subject timeline
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineMessage {
    pub role: String,
    pub content: String,
    pub message_id: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
}

/// Shared broadcast state
#[derive(Clone)]
pub struct WsState {
    pub subject_tx: std::sync::Arc<broadcast::Sender<TimelineMessage>>,
    /// Forward UI messages into the subject inbox.
    /// TODO(main.rs): wire by passing UiServer::new_with_inbox(inbox.sender()) after S2 subject loop init.
    pub inbox_tx: Option<std::sync::Arc<Sender<InboxEvent>>>,
}

impl WsState {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self { subject_tx: std::sync::Arc::new(tx), inbox_tx: None }
    }

    pub fn new_with_inbox(inbox_tx: Sender<InboxEvent>) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            subject_tx: std::sync::Arc::new(tx),
            inbox_tx: Some(std::sync::Arc::new(inbox_tx)),
        }
    }

    pub fn broadcast(&self, msg: TimelineMessage) {
        let _ = self.subject_tx.send(msg);
    }
}

impl Default for WsState {
    fn default() -> Self { Self::new() }
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Extension(state): Extension<WsState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: WsState) {
    info!("ws client connected");
    let mut rx = state.subject_tx.subscribe();

    // Send welcome
    let welcome = TimelineMessage {
        role: "system".to_string(),
        content: "已连接到 Shenko（深刻）。持续思考中。".to_string(),
        message_id: Uuid::new_v4().to_string(),
        timestamp: Utc::now().to_rfc3339(),
        stream: None,
    };
    if let Ok(json) = serde_json::to_string(&welcome) {
        let _ = socket.send(Message::Text(json.into())).await;
    }

    loop {
        tokio::select! {
            // Subject broadcasts → all WS clients
            msg = rx.recv() => {
                match msg {
                    Ok(m) => {
                        if let Ok(json) = serde_json::to_string(&m) {
                            if socket.send(Message::Text(json.into())).await.is_err() { break; }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // client fell behind; skip old messages
                    }
                    Err(_) => break,
                }
            }
            // Client → subject inbox
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        debug!("ws recv {} bytes", text.len());
                        if let Ok(user_msg) = serde_json::from_str::<TimelineMessage>(&text) {
                            // Route the user message into the subject inbox (SPEC §5.1).
                            let event = InboxEvent {
                                event_id: EntityId::new(),
                                kind: InboxEventKind::Message,
                                payload: serde_json::json!({
                                    "content": user_msg.content,
                                    "message_id": user_msg.message_id,
                                    "role": user_msg.role,
                                }),
                                created_at: Utc::now(),
                                source: "web_ui".to_string(),
                            };
                            if let Some(tx) = &state.inbox_tx {
                                let _ = tx.try_send(event);
                            }

                            // Brief echo back to the sender only.
                            let reply = TimelineMessage {
                                role: "subject".to_string(),
                                content: format!("收到。"),
                                message_id: Uuid::new_v4().to_string(),
                                timestamp: Utc::now().to_rfc3339(),
                                stream: None,
                            };
                            if let Ok(json) = serde_json::to_string(&reply) {
                                let _ = socket.send(Message::Text(json.into())).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => { info!("ws disconnected"); break; }
                    Some(Err(e)) => { warn!(err=%e, "ws error"); break; }
                    _ => {}
                }
            }
        }
    }
}
