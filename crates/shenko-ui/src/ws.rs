//! 单主体时间线的传输层；内存入队不等于 §41.18 的持久接纳。
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axum::{
    extract::{
        Extension,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use chrono::Utc;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc::Sender};
use tracing::{debug, info, warn};
use uuid::Uuid;

use shenko_subject::inbox::{InboxEvent, InboxEventKind};
use shenko_subject::outbox::OutboxMessage;
use shenko_types::{ids::EntityId, object::Digest};

/// 兼容原有客户端的封套。新增字段缺失时保留未知，不推导主体轮次。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineMessage {
    pub role: String,
    pub content: String,
    pub message_id: String,
    /// 旧协议字段：没有源时间时为空字符串，不能填转发时间冒充提交时间。
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default)]
    pub step: Option<u64>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub committed_at: Option<String>,
    #[serde(default)]
    pub observed_at: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub submission_id: Option<String>,
}

impl From<OutboxMessage> for TimelineMessage {
    fn from(out: OutboxMessage) -> Self {
        // 当前 outbox 没有持久事件 ID/提交时间。内容寻址投影在各客户端一致，
        // 但不是全局主体事件身份，也不提供持久 cursor/replay 保证。
        let identity = serde_json::to_vec(&(out.step, &out.kind, &out.content))
            .expect("outbox 基本字段可序列化");
        Self {
            role: "subject".into(),
            content: out.content,
            message_id: format!("outbox:{}", Digest::blake3(&identity)),
            timestamp: String::new(),
            stream: None,
            step: Some(out.step),
            kind: Some(out.kind),
            committed_at: None,
            observed_at: Some(Utc::now().to_rfc3339()),
            mode: None,
            submission_id: None,
        }
    }
}

/// 最后观测的输出与连接数；不声称知道主体当前是否执行或等待。
#[derive(Debug)]
pub struct LiveStatus {
    connections: AtomicUsize,
    wired: AtomicBool,
    last_output: RwLock<Option<(u64, chrono::DateTime<Utc>)>>,
}

impl LiveStatus {
    pub fn new() -> Self {
        Self {
            connections: AtomicUsize::new(0),
            wired: AtomicBool::new(false),
            last_output: RwLock::new(None),
        }
    }

    pub fn set_wired(&self, wired: bool) {
        self.wired.store(wired, Ordering::Relaxed);
    }

    pub fn is_wired(&self) -> bool {
        self.wired.load(Ordering::Relaxed)
    }

    pub fn connection_added(self: &Arc<Self>) -> ConnectionGuard {
        self.connections.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard {
            live: Arc::clone(self),
        }
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }

    pub fn record_outbox(&self, step: u64) {
        *self.last_output.write() = Some((step, Utc::now()));
    }

    pub fn last_step(&self) -> Option<u64> {
        self.last_output.read().as_ref().map(|(step, _)| *step)
    }

    pub fn last_output_observed_at(&self) -> Option<String> {
        self.last_output
            .read()
            .as_ref()
            .map(|(_, at)| at.to_rfc3339())
    }

    pub fn seconds_since_activity(&self) -> Option<i64> {
        self.last_output
            .read()
            .as_ref()
            .map(|(_, at)| Utc::now().signed_duration_since(*at).num_seconds().max(0))
    }
}

impl Default for LiveStatus {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct ConnectionGuard {
    live: Arc<LiveStatus>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.live.connections.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct WsState {
    pub subject_tx: Arc<broadcast::Sender<TimelineMessage>>,
    pub inbox_tx: Option<Arc<Sender<InboxEvent>>>,
    pub outbox_tx: Option<Arc<broadcast::Sender<OutboxMessage>>>,
    pub live: Arc<LiveStatus>,
}

impl WsState {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            subject_tx: Arc::new(tx),
            inbox_tx: None,
            outbox_tx: None,
            live: Arc::new(LiveStatus::new()),
        }
    }

    pub fn new_with_inbox(inbox_tx: Sender<InboxEvent>) -> Self {
        Self::new().with_inbox(inbox_tx)
    }

    pub fn with_inbox(mut self, inbox_tx: Sender<InboxEvent>) -> Self {
        self.inbox_tx = Some(Arc::new(inbox_tx));
        self
    }

    pub fn with_outbox(mut self, outbox_tx: broadcast::Sender<OutboxMessage>) -> Self {
        self.outbox_tx = Some(Arc::new(outbox_tx));
        self
    }

    pub fn broadcast(&self, msg: TimelineMessage) {
        let _ = self.subject_tx.send(msg);
    }
}

impl Default for WsState {
    fn default() -> Self {
        Self::new()
    }
}

/// 只报告实际 try_send 结果；没有持久查重、accepted、selected 或 committed。
fn enqueue_input(state: &WsState, msg: TimelineMessage) -> Value {
    let mode = msg.mode.as_deref().unwrap_or("append");
    let event_id = EntityId::new();
    let error = if mode != "append" {
        Some("STEER_NOT_IMPLEMENTED")
    } else if msg.content.trim().is_empty() {
        Some("EMPTY_INPUT")
    } else if let Some(tx) = &state.inbox_tx {
        let event = InboxEvent {
            event_id,
            kind: InboxEventKind::Message,
            payload: json!({
                "content": msg.content,
                "message_id": msg.message_id,
                "role": "user",
                "mode": mode,
                "submission_id": msg.submission_id,
                "placement": { "requested": mode, "actual": "next_boundary" },
            }),
            created_at: Utc::now(),
            source: "web_ui".into(),
        };
        match tx.try_send(event) {
            Ok(()) => None,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Some("INBOX_FULL"),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Some("INBOX_CLOSED"),
        }
    } else {
        Some("INBOX_UNAVAILABLE")
    };
    json!({
        "type": "receipt", "role": "system",
        "content": if error.is_none() { "已进入内存队列；尚无持久接纳或主体处置证据" } else { "未进入队列" },
        "message_id": msg.message_id, "submission_id": msg.submission_id,
        "timestamp": Utc::now().to_rfc3339(),
        "stage": if error.is_none() { "queued" } else { "rejected" },
        "event_id": if error.is_none() { Some(event_id.to_string()) } else { None },
        "error": error,
        "durable": false, "deduplication": false, "steer_supported": false,
        "placement": { "requested": mode, "actual": if error.is_none() { Some("next_boundary") } else { None } },
    })
}

pub async fn ws_handler(ws: WebSocketUpgrade, Extension(state): Extension<WsState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: WsState) {
    info!("ws client connected");
    let _connection = state.live.connection_added();
    let mut rx = state.subject_tx.subscribe();
    let mut outbox_rx = state.outbox_tx.as_ref().map(|tx| tx.subscribe());
    let welcome = json!({
        "role": "system", "content": "WebSocket 已连接；主体当前执行状态未知。仅支持追加到下一边界，steer 与持久输入回执尚未实现。",
        "message_id": Uuid::new_v4().to_string(), "timestamp": Utc::now().to_rfc3339(),
        "capabilities": { "append": true, "steer": false, "durable_input": false, "deduplication": false },
    });
    if socket
        .send(Message::Text(welcome.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(m) => {
                        if let Ok(json) = serde_json::to_string(&m) {
                            if socket.send(Message::Text(json.into())).await.is_err() { break; }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
            out = recv_outbox(&mut outbox_rx) => {
                match out {
                    Some(out) => {
                        let msg = TimelineMessage::from(out);
                        if let Ok(json) = serde_json::to_string(&msg) {
                            if socket.send(Message::Text(json.into())).await.is_err() { break; }
                        }
                    }
                    None => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        debug!("ws recv {} bytes", text.len());
                        let receipt = match serde_json::from_str::<TimelineMessage>(&text) {
                            Ok(msg) => enqueue_input(&state, msg),
                            Err(_) => json!({
                                "type": "receipt", "role": "system", "stage": "rejected",
                                "error": "INVALID_INPUT", "durable": false,
                            }),
                        };
                        if socket.send(Message::Text(receipt.to_string().into())).await.is_err() { break; }
                    }
                    Some(Ok(Message::Close(_))) | None => { info!("ws disconnected"); break; }
                    Some(Err(e)) => { warn!(err=%e, "ws error"); break; }
                    _ => {}
                }
            }
        }
    }
}

async fn recv_outbox(rx: &mut Option<broadcast::Receiver<OutboxMessage>>) -> Option<OutboxMessage> {
    match rx {
        Some(rx) => loop {
            match rx.recv().await {
                Ok(m) => return Some(m),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        },
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn outbox_identity_and_queue_receipts_reflect_only_real_facts() {
        let out = OutboxMessage {
            kind: "thought".into(),
            step: 812,
            content: "真实输出".into(),
        };
        let a = TimelineMessage::from(out.clone());
        let b = TimelineMessage::from(out);
        let wire = serde_json::to_value(&a).unwrap();
        assert_eq!(wire["step"], 812);
        assert_eq!(a.message_id, b.message_id);
        assert_eq!(wire["committed_at"], Value::Null);
        assert_eq!(wire["timestamp"], "");
        assert!(wire["observed_at"].is_string());

        let input = || {
            serde_json::from_value::<TimelineMessage>(json!({
                "role": "user", "content": "追加内容", "message_id": "m1", "timestamp": "",
                "mode": "append", "submission_id": "s1",
            }))
            .unwrap()
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let state = WsState::new_with_inbox(tx);
        let queued = enqueue_input(&state, input());
        assert_eq!(queued["stage"], "queued");
        assert_eq!(queued["durable"], false);
        assert_eq!(queued["placement"]["actual"], "next_boundary");
        let full = enqueue_input(&state, input());
        assert_eq!(full["stage"], "rejected");
        assert_eq!(full["error"], "INBOX_FULL");
        let event = rx.recv().await.unwrap();
        assert_eq!(event.payload["submission_id"], "s1");
        assert_eq!(event.payload["mode"], "append");
        drop(rx);
        assert_eq!(enqueue_input(&state, input())["error"], "INBOX_CLOSED");
        assert_eq!(
            enqueue_input(&WsState::new(), input())["error"],
            "INBOX_UNAVAILABLE"
        );
        let mut steer = input();
        steer.mode = Some("steer".into());
        assert_eq!(
            enqueue_input(&state, steer)["error"],
            "STEER_NOT_IMPLEMENTED"
        );
        let legacy: TimelineMessage = serde_json::from_value(json!({
            "role": "user", "content": "旧客户端", "message_id": "old", "timestamp": "",
        }))
        .unwrap();
        assert!(legacy.mode.is_none());
    }
}
