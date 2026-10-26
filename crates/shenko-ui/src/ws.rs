//! WebSocket handler — versioned UI DTOs over the real subject inbox/outbox.
//! SPEC §21: one subject timeline; transport admission is not durable acceptance.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

use axum::{
    extract::{
        Extension,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::{IntoResponse, Response},
    http::{HeaderMap, StatusCode, header},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use shenko_subject::inbox::{InboxEvent, InboxEventKind};
use shenko_subject::outbox::OutboxMessage;
use shenko_types::ids::EntityId;
use tokio::sync::{broadcast, mpsc::Sender};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// One committed part on the subject timeline. `observed_at` is the transport
/// observation time, not a fabricated durable-commit wall clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineMessage {
    pub schema_version: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub role: String,
    pub content: String,
    pub message_id: String,
    pub part_id: String,
    pub part_revision: String,
    pub chunk_seq: u64,
    pub committed_only: bool,
    pub observed_at: String,
    pub step: u64,
    pub step_id: String,
    pub commit_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
}

fn committed_timeline_message(out: OutboxMessage) -> TimelineMessage {
    let step_id = out.step_id.to_string();
    let commit_ref = out.commit_ref.to_string();
    TimelineMessage {
        schema_version: 2,
        kind: "timeline_part".to_string(),
        role: "subject".to_string(),
        content: out.text,
        message_id: step_id.clone(),
        part_id: format!("{step_id}/text/0"),
        part_revision: commit_ref.clone(),
        chunk_seq: 0,
        committed_only: true,
        observed_at: Utc::now().to_rfc3339(),
        step: out.step,
        step_id,
        commit_ref,
        // The current OutboxMessage has no stream/request/attempt binding.
        // They remain absent rather than being inferred from the step number.
        stream_id: None,
        request_id: None,
        attempt_id: None,
    }
}

/// Live facts shared by the WS layer and the read-only `/api/self` endpoint.
#[derive(Debug)]
pub struct LiveStatus {
    connections: AtomicUsize,
    wired: AtomicBool,
    /// Unix seconds of the last observed subject outbox message; -1 = never.
    last_outbox_unix: AtomicI64,
    /// Step number carried by the last observed outbox message; -1 = unknown.
    last_step: AtomicI64,
}

impl LiveStatus {
    pub fn new() -> Self {
        Self {
            connections: AtomicUsize::new(0),
            wired: AtomicBool::new(false),
            last_outbox_unix: AtomicI64::new(-1),
            last_step: AtomicI64::new(-1),
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
        ConnectionGuard { live: Arc::clone(self) }
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }

    /// Record observation of a committed thought, not a claim that the subject
    /// is currently thinking, idle, online, or holding an inference slot.
    pub fn record_outbox(&self, step: u64) {
        self.last_step.store(step as i64, Ordering::Relaxed);
        self.last_outbox_unix.store(Utc::now().timestamp(), Ordering::Relaxed);
    }

    pub fn last_step(&self) -> Option<u64> {
        match self.last_step.load(Ordering::Relaxed) {
            s if s < 0 => None,
            s => Some(s as u64),
        }
    }

    pub fn last_outbox_observed_at(&self) -> Option<chrono::DateTime<Utc>> {
        match self.last_outbox_unix.load(Ordering::Relaxed) {
            seconds if seconds < 0 => None,
            seconds => chrono::DateTime::from_timestamp(seconds, 0),
        }
    }

    pub fn seconds_since_activity(&self) -> Option<i64> {
        match self.last_outbox_unix.load(Ordering::Relaxed) {
            t if t < 0 => None,
            t => Some(Utc::now().timestamp().saturating_sub(t)),
        }
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
    pub subject_tx: std::sync::Arc<broadcast::Sender<TimelineMessage>>,
    pub inbox_tx: Option<std::sync::Arc<Sender<InboxEvent>>>,
    pub outbox_tx: Option<std::sync::Arc<broadcast::Sender<OutboxMessage>>>,
    pub live: Arc<LiveStatus>,
    pub store: Option<Arc<shenko_store::Store>>,
    /// Real genesis subject identity shared with `/api/self`; never a browser-
    /// supplied actor or a hard-coded "primary" alias.
    pub subject_id: Option<EntityId>,
}

impl WsState {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            subject_tx: std::sync::Arc::new(tx),
            inbox_tx: None,
            outbox_tx: None,
            live: Arc::new(LiveStatus::new()),
            store: None,
            subject_id: None,
        }
    }

    pub fn new_with_inbox(inbox_tx: Sender<InboxEvent>) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            subject_tx: std::sync::Arc::new(tx),
            inbox_tx: Some(std::sync::Arc::new(inbox_tx)),
            outbox_tx: None,
            live: Arc::new(LiveStatus::new()),
            store: None,
            subject_id: None,
        }
    }

    pub fn with_inbox(mut self, inbox_tx: Sender<InboxEvent>) -> Self {
        self.inbox_tx = Some(std::sync::Arc::new(inbox_tx));
        self
    }

    pub fn with_outbox(mut self, outbox_tx: broadcast::Sender<OutboxMessage>) -> Self {
        self.outbox_tx = Some(std::sync::Arc::new(outbox_tx));
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

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Extension(state): Extension<WsState>,
    headers: HeaderMap,
) -> Response {
    if !browser_origin_matches_host(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

fn browser_origin_matches_host(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        // Non-browser fixture clients do not necessarily send Origin. Browser
        // cross-origin WebSocket requests always carry it and fail closed here.
        return true;
    };
    let Some(host) = headers.get(header::HOST).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    origin
        .to_str()
        .ok()
        .and_then(|value| value.parse::<axum::http::Uri>().ok())
        .and_then(|uri| uri.authority().map(|authority| authority.as_str().eq_ignore_ascii_case(host)))
        .unwrap_or(false)
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum SubmissionStage {
    Queued,
    Rejected,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SubmissionMode {
    Append,
    Steer,
}

#[derive(Serialize)]
struct SubmissionReceipt<'a> {
    schema_version: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    submission_id: &'a str,
    stage: SubmissionStage,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<SubmissionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    durability: Option<&'static str>,
    detail: &'static str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserSubmission {
    schema_version: u32,
    #[serde(rename = "type")]
    kind: String,
    submission_id: String,
    message_id: String,
    subject_id: String,
    mode: SubmissionMode,
    content: String,
    #[serde(default)]
    attachment_refs: Vec<serde_json::Value>,
    #[serde(default)]
    rich_content_ref: Option<serde_json::Value>,
    #[serde(default)]
    observed_step_id: Option<String>,
    #[serde(default)]
    observed_request_id: Option<String>,
}

fn validate_submission(
    submission: &UserSubmission,
    expected_subject: Option<EntityId>,
) -> Result<(), &'static str> {
    if submission.schema_version != 2 || submission.kind != "submit" {
        return Err("不支持的输入协议版本。");
    }
    if submission.submission_id.is_empty()
        || submission.message_id.is_empty()
        || submission.content.trim().is_empty()
    {
        return Err("输入身份或内容为空。");
    }
    if submission.mode == SubmissionMode::Steer {
        return Err("Steer 尚未接入真实 request/placement；未接纳输入。");
    }
    if !submission.attachment_refs.is_empty() || submission.rich_content_ref.is_some() {
        return Err("附件引用解析尚未接入；未接纳输入。");
    }
    match expected_subject {
        Some(expected) if expected.to_string() == submission.subject_id => Ok(()),
        _ => Err("真实 subject 绑定不匹配或不可用；未接纳输入。"),
    }
}

async fn handle_socket(mut socket: WebSocket, state: WsState) {
    info!("ws client connected");
    let _connection = state.live.connection_added();
    let mut rx = state.subject_tx.subscribe();
    let mut outbox_rx = state.outbox_tx.as_ref().map(|tx| tx.subscribe());

    let hello = serde_json::json!({
        "schema_version": 2,
        "type": "hello",
        "server_version": env!("CARGO_PKG_VERSION"),
        "capabilities": {
            "append": state.inbox_tx.is_some() && state.subject_id.is_some(),
            "steer": false,
            "attachments": false,
            "durable_input_receipts": false,
            "committed_replay": state.store.is_some(),
        }
    });
    if socket.send(Message::Text(hello.to_string().into())).await.is_err() {
        return;
    }

    let mut identity_revision: Option<u64> = None;
    let mut committed_cursor = (0u64, None);
    let mut history = std::collections::VecDeque::new();
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(m) => {
                        if let Ok(json) = serde_json::to_string(&m) {
                            if socket.send(Message::Text(json.into())).await.is_err() { break; }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Committed store replay below is authoritative. A UI-
                        // private guessed snapshot is not synthesized here.
                    }
                    Err(_) => break,
                }
            }
            out = recv_committed(&state, &mut committed_cursor, &mut history, &mut outbox_rx) => {
                match out {
                    Some(out) => {
                        if out.identity_revision.is_some() && out.identity_revision != identity_revision {
                            identity_revision = out.identity_revision;
                            let note = serde_json::json!({ "type": "identity", "revision": identity_revision });
                            if socket.send(Message::Text(note.to_string().into())).await.is_err() { break; }
                        }
                        let msg = committed_timeline_message(out);
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
                        let parsed = serde_json::from_str::<UserSubmission>(&text);
                        let (submission_id, mode, stage, durability, detail) = match parsed {
                            Ok(user_msg) => {
                                if let Err(detail) = validate_submission(&user_msg, state.subject_id) {
                                    (user_msg.submission_id, Some(user_msg.mode), SubmissionStage::Rejected, None, detail)
                                } else {
                                    let event = InboxEvent {
                                        event_id: EntityId::new(),
                                        kind: InboxEventKind::Message,
                                        payload: serde_json::json!({
                                            "content": user_msg.content,
                                            "message_id": user_msg.message_id,
                                            "submission_id": user_msg.submission_id,
                                            "subject_id": user_msg.subject_id,
                                            "mode": "append",
                                            "attachment_refs": user_msg.attachment_refs,
                                            "observed_step_id": user_msg.observed_step_id,
                                            "observed_request_id": user_msg.observed_request_id,
                                            "role": "user",
                                        }),
                                        created_at: Utc::now(),
                                        source: "web_ui".to_string(),
                                    };
                                    // try_send proves only volatile queue admission. It
                                    // is not durable accepted/selected/committed and no
                                    // placement is invented for the browser.
                                    let (stage, detail) = match &state.inbox_tx {
                                        Some(tx) => match tx.try_send(event) {
                                            Ok(()) => (SubmissionStage::Queued, "已进入内存队列；尚未持久化、选入或处理。"),
                                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => (SubmissionStage::Rejected, "主体输入队列已满，请稍后重试。"),
                                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => (SubmissionStage::Rejected, "主体已停止，需要恢复。"),
                                        },
                                        None => (SubmissionStage::Rejected, "主体输入未连接。"),
                                    };
                                    let durability = matches!(stage, SubmissionStage::Queued).then_some("volatile");
                                    (user_msg.submission_id, Some(user_msg.mode), stage, durability, detail)
                                }
                            }
                            Err(_) => {
                                let submission_id = serde_json::from_str::<serde_json::Value>(&text)
                                    .ok()
                                    .and_then(|value| value.get("submission_id")?.as_str().map(ToOwned::to_owned))
                                    .unwrap_or_else(|| Uuid::new_v4().to_string());
                                (submission_id, None, SubmissionStage::Rejected, None, "无效或未知字段的输入协议；未接纳输入。")
                            }
                        };
                        let reply = SubmissionReceipt {
                            schema_version: 2,
                            kind: "receipt",
                            submission_id: &submission_id,
                            stage,
                            mode,
                            durability,
                            detail,
                        };
                        let json = serde_json::to_string(&reply).expect("receipt serialization");
                        if socket.send(Message::Text(json.into())).await.is_err() { break; }
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

async fn recv_committed(
    state: &WsState,
    cursor: &mut (u64, Option<EntityId>),
    history: &mut std::collections::VecDeque<OutboxMessage>,
    wake: &mut Option<broadcast::Receiver<OutboxMessage>>,
) -> Option<OutboxMessage> {
    let Some(store) = &state.store else {
        return recv_outbox(wake).await;
    };
    loop {
        if let Some(out) = history.pop_front() {
            *cursor = (out.replay_seq, Some(out.step_id));
            state.live.record_outbox(out.step);
            return Some(out);
        }
        match shenko_subject::outbox::committed_after(store, cursor.0, cursor.1, 64) {
            Ok(messages) => history.extend(messages),
            Err(e) => {
                tracing::error!(err=%e,"committed timeline read failed; no guessed replay");
                return None;
            }
        }
        if history.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_types::commit::CommitRef;
    use shenko_types::ids::LedgerSeq;

    fn submission(mode: SubmissionMode) -> UserSubmission {
        UserSubmission {
            schema_version: 2,
            kind: "submit".into(),
            submission_id: "submission-1".into(),
            message_id: "message-1".into(),
            subject_id: EntityId::new().to_string(),
            mode,
            content: "继续".into(),
            attachment_refs: vec![],
            rich_content_ref: None,
            observed_step_id: None,
            observed_request_id: None,
        }
    }

    #[test]
    fn append_binds_real_subject_and_pseudo_steer_is_rejected() {
        let subject = EntityId::new();
        let mut input = submission(SubmissionMode::Append);
        input.subject_id = subject.to_string();
        assert!(validate_submission(&input, Some(subject)).is_ok());
        assert!(validate_submission(&input, Some(EntityId::new())).is_err());
        input.mode = SubmissionMode::Steer;
        assert!(validate_submission(&input, Some(subject)).is_err());
    }

    #[test]
    fn committed_projection_reuses_message_part_identity() {
        let step_id = EntityId::new();
        let commit_ref = CommitRef::new(EntityId::new(), LedgerSeq(7));
        let out = OutboxMessage {
            kind: "thought".into(),
            step: 3,
            step_id,
            commit_ref,
            replay_seq: 7,
            text: "已提交正文".into(),
            publications: vec![],
            identity_revision: None,
        };
        let first = committed_timeline_message(out.clone());
        let replay = committed_timeline_message(out);
        assert_eq!(first.message_id, step_id.to_string());
        assert_eq!(first.part_id, format!("{step_id}/text/0"));
        assert_eq!(first.part_id, replay.part_id);
        assert_eq!(first.part_revision, commit_ref.to_string());
        assert_eq!(first.chunk_seq, 0);
        assert!(first.committed_only);
        assert!(first.request_id.is_none());
        assert!(first.attempt_id.is_none());
    }
}
