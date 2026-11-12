//! WebSocket handler — versioned UI DTOs over the real subject inbox/outbox.
//! docs/specs/web-interface.md#spec-21: one subject timeline; transport admission is not durable acceptance.
//!
//! One socket serves four durable lanes, all cursor-replayable from the
//! ledger: committed timeline parts (post-S1), public-text stream parts
//! (pre-S1, only when a real producer is wired — never faked), validated
//! semantic documents derived from committed text, and web input
//! receipts/views (docs/specs/web-interface.md#spec-41-18). Capability facts are negotiated per connection.
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
use shenko_subject::outbox::OutboxMessage;
use shenko_types::ids::EntityId;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::input::{
    ActionSubmission, InputReceipt, SubmissionStage, SubmissionView, UserSubmission,
    WebInputService, WithdrawRequest,
};
use crate::presentation::{
    ClientSurfaceFacts, DegradedDocument, PresentationService, SurfaceCapabilities, degrade,
};
use crate::stream::StreamSink;

/// One committed part on the subject timeline. `observed_at` is the transport
/// observation time, not a fabricated durable-commit wall clock. `terminal`
/// marks the final chunk of the part (set on committed messages; stream
/// chunks carry their own `terminal` flag in the stream DTO).
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
    /// Receiving-ledger replay position; `commit_ref` may name a foreign source.
    pub commit_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
}

/// The text part identity shared by the committed projection and the stream
/// sink: `message_id` is the step id, `part_id` names the part within it.
pub use shenko_subject::outbox::TEXT_PART_ID;

fn committed_timeline_message(out: OutboxMessage) -> TimelineMessage {
    let step_id = out.step_id.to_string();
    let commit_ref = out.commit_ref.to_string();
    TimelineMessage {
        schema_version: 2,
        kind: "timeline_part".to_string(),
        role: "subject".to_string(),
        content: out.text,
        message_id: step_id.clone(),
        part_id: TEXT_PART_ID.to_owned(),
        part_revision: commit_ref.clone(),
        chunk_seq: 0,
        committed_only: true,
        observed_at: Utc::now().to_rfc3339(),
        step: out.step,
        step_id,
        commit_ref,
        commit_seq: out.replay_seq,
        terminal: Some(true),
        // The current OutboxMessage has no stream/request/attempt binding.
        // They remain absent rather than being inferred from the step number.
        stream_id: None,
        request_id: None,
        attempt_id: None,
    }
}

/// Build the committed-decision message(s) for one step.
///
/// The committed message always carries the FULL text at chunk_seq 0: every
/// client, live or replaying, ends with the whole committed truth through the
/// parts cursor alone. Clients that already received streamed chunks absorb
/// this message as the S1 binding (commit_ref/step/step_id) instead of
/// re-appending — chunk_seq 0 sits below their part cursor, so absorption is
/// idempotent in both delivery orders. No remainder arithmetic happens here;
/// the only reconciliation is divergence reporting: if the durably stored
/// streamed prefix does not match the committed text, a corrected part under
/// a distinct identity follows the base message so clients replace rather
/// than append.
fn committed_timeline_messages(state: &WsState, out: OutboxMessage) -> Vec<TimelineMessage> {
    let mut base = committed_timeline_message(out.clone());
    let Some(store) = &state.store else {
        return vec![base];
    };
    let Some(subject_id) = state.subject_id else {
        return vec![base];
    };
    match store.output_part_state(subject_id, out.step_id, TEXT_PART_ID) {
        Ok(Some(position)) if !out.text.starts_with(position.content.as_str()) => {
            // Streamed content diverges from the committed decision text —
            // a producer contract violation. The committed truth wins under
            // a corrected part identity so clients replace, not append.
            warn!(step = out.step, "streamed part diverges from committed text; emitting corrected part");
            base.part_id = format!("{TEXT_PART_ID}@{}", base.commit_ref);
            vec![base]
        }
        Ok(_) => vec![base],
        Err(error) => {
            warn!(%error, "output part state unreadable; committed message emitted without reconciliation");
            vec![base]
        }
    }
}

/// Documents derived from one committed output, degraded for this surface.
#[derive(Debug, Clone, Serialize)]
struct DocumentMessage {
    schema_version: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    source_step_id: String,
    source_commit: String,
    degraded: DegradedDocument,
}

/// Serialize one committed output's documents for a specific capability
/// surface. Used both for the live projection and for re-degrading the newest
/// surface after a `client_hello` renegotiation.
fn degraded_document_messages(
    documents: Vec<crate::presentation::ExtractedDocument>,
    caps: &SurfaceCapabilities,
) -> Vec<String> {
    documents
        .into_iter()
        .map(|extracted| {
            serde_json::to_string(&DocumentMessage {
                schema_version: 1,
                kind: "document",
                source_step_id: extracted.source_step_id.to_string(),
                source_commit: extracted.source_commit.to_string(),
                degraded: degrade(&extracted.document, caps),
            })
        })
        .filter_map(Result::ok)
        .collect()
}

#[derive(Debug, Clone, Serialize)]
struct DocumentErrorMessage {
    schema_version: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    source_step_id: String,
    source_commit: String,
    reason: String,
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
    pub outbox_tx: Option<std::sync::Arc<broadcast::Sender<OutboxMessage>>>,
    pub live: Arc<LiveStatus>,
    pub store: Option<Arc<shenko_store::Store>>,
    /// Real genesis subject identity shared with `/api/self`; never a browser-
    /// supplied actor or a hard-coded "primary" alias.
    pub subject_id: Option<EntityId>,
    /// Durable web input service (docs/specs/web-interface.md#spec-41-18). Present iff store + subject are.
    pub input: Option<Arc<WebInputService>>,
    /// Rich presentation service: document validation, degradation, caps.
    pub presentation: Option<Arc<PresentationService>>,
    /// Durable stream sink. Absent = no real producer wired; the UI then
    /// honestly reports streaming:false and never fabricates chunks.
    pub stream: Option<Arc<StreamSink>>,
}

impl WsState {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            subject_tx: std::sync::Arc::new(tx),
            outbox_tx: None,
            live: Arc::new(LiveStatus::new()),
            store: None,
            subject_id: None,
            input: None,
            presentation: None,
            stream: None,
        }
    }

    pub fn with_outbox(mut self, outbox_tx: broadcast::Sender<OutboxMessage>) -> Self {
        self.outbox_tx = Some(std::sync::Arc::new(outbox_tx));
        self
    }

    pub fn with_input(mut self, input: Arc<WebInputService>) -> Self {
        self.input = Some(input);
        self
    }

    pub fn with_presentation(mut self, presentation: Arc<PresentationService>) -> Self {
        self.presentation = Some(presentation);
        self
    }

    pub fn with_stream(mut self, stream: Arc<StreamSink>) -> Self {
        self.stream = Some(stream);
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmissionQuery {
    schema_version: u32,
    #[serde(rename = "type")]
    kind: String,
    submission_id: String,
    subject_id: String,
    // Client-scoped attempt counter; echoed back, never trusted as ordering
    // or identity on the server side.
    #[serde(default)]
    send_epoch: Option<u64>,
}

#[derive(Deserialize)]
struct ClientHello {
    schema_version: u32,
    #[serde(rename = "type")]
    kind: String,
    #[serde(flatten)]
    facts: ClientSurfaceFacts,
}

async fn handle_socket(mut socket: WebSocket, state: WsState) {
    info!("ws client connected");
    let _connection = state.live.connection_added();
    let mut rx = state.subject_tx.subscribe();
    let mut outbox_rx = state.outbox_tx.as_ref().map(|tx| tx.subscribe());
    let mut stream_rx = state.stream.as_ref().map(|sink| sink.subscribe());

    // Per-connection capability facts. Revision 0 = server baseline before
    // the client attests its display facts; a client_hello renegotiates.
    let mut caps = state.presentation.as_ref().map(|presentation| {
        presentation
            .capability_provider()
            .capabilities_for(&ClientSurfaceFacts::default(), 0)
    });
    let mut caps_revision = 0u64;

    let hello = serde_json::json!({
        "schema_version": 2,
        "type": "hello",
        "server_version": env!("CARGO_PKG_VERSION"),
        "capabilities": {
            "append": state.input.is_some(),
            "steer": state.input.is_some(),
            "withdraw": state.input.is_some(),
            "actions": state.input.is_some() && state.presentation.is_some(),
            "documents": state.presentation.is_some(),
            "attachments": false,
            "durable_input_receipts": state.input.is_some(),
            "committed_replay": state.store.is_some() && state.subject_id.is_some(),
            "streaming": state.stream.is_some(),
        },
        "surface": caps,
    });
    if socket.send(Message::Text(hello.to_string().into())).await.is_err() {
        return;
    }

    let mut identity_revision: Option<u64> = None;
    let mut committed_cursor = (0u64, None);
    let mut committed_history = std::collections::VecDeque::new();
    // Newest committed output observed by this connection. A later
    // `client_hello` re-degrades its documents for the renegotiated surface,
    // so a richer client is not left holding the baseline fallback.
    let mut last_committed: Option<OutboxMessage> = None;
    let mut stream_cursor = 0u64;
    let mut stream_history = std::collections::VecDeque::new();
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
            out = recv_committed(&state, &mut committed_cursor, &mut committed_history, &mut outbox_rx) => {
                match out {
                    Some(out) => {
                        if out.identity_revision.is_some() && out.identity_revision != identity_revision {
                            identity_revision = out.identity_revision;
                            let note = serde_json::json!({ "type": "identity", "revision": identity_revision });
                            if socket.send(Message::Text(note.to_string().into())).await.is_err() { break; }
                        }
                        state.live.record_outbox(out.step);
                        // Documents derive from the same committed text and
                        // follow the part message; degradation uses this
                        // connection's current capability facts.
                        last_committed = Some(out.clone());
                        let text = out.text.clone();
                        let step_id = out.step_id;
                        let commit_ref = out.commit_ref;
                        let document_payloads = match &state.presentation {
                            Some(presentation) => {
                                let (documents, errors) = presentation.project(&text, step_id, commit_ref);
                                let mut payloads = Vec::with_capacity(documents.len() + errors.len());
                                for error in errors {
                                    payloads.push(serde_json::to_string(&DocumentErrorMessage {
                                        schema_version: 1,
                                        kind: "document_error",
                                        source_step_id: error.source_step_id.to_string(),
                                        source_commit: error.source_commit.to_string(),
                                        reason: error.reason,
                                    }).expect("document error serialization"));
                                }
                                if let Some(caps) = &caps {
                                    payloads.extend(degraded_document_messages(documents, caps));
                                }
                                payloads
                            }
                            None => Vec::new(),
                        };
                        for message in committed_timeline_messages(&state, out) {
                            if let Ok(json) = serde_json::to_string(&message) {
                                if socket.send(Message::Text(json.into())).await.is_err() { break; }
                            }
                        }
                        for payload in document_payloads {
                            if socket.send(Message::Text(payload.into())).await.is_err() { break; }
                        }
                    }
                    None => break,
                }
            }
            chunk = recv_stream(&state, &mut stream_cursor, &mut stream_history, &mut stream_rx) => {
                match chunk {
                    Some(chunk) => {
                        if let Ok(json) = serde_json::to_string(&crate::stream::StreamChunkDto::from(chunk)) {
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
                        if let Ok(user_msg) = serde_json::from_str::<UserSubmission>(&text) {
                            let Some(input) = &state.input else {
                                let reply = InputReceipt {
                                    schema_version: 2,
                                    kind: "receipt",
                                    submission_id: user_msg.submission_id.clone(),
                                    stage: SubmissionStage::Unavailable,
                                    mode: Some(user_msg.mode),
                                    durability: None,
                                    event_id: None,
                                    accepted_commit: None,
                                    replayed: None,
                                    placement: None,
                                    detail: "持久输入服务不可用；未接纳，未生成新身份。".to_owned(),
                                };
                                if !send_json(&mut socket, &reply).await { break; }
                                continue;
                            };
                            let reply = input.submit(&user_msg);
                            if !send_json(&mut socket, &reply).await { break; }
                        } else if let Ok(query) = serde_json::from_str::<SubmissionQuery>(&text) {
                            let view = if query.schema_version == 2
                                && query.kind == "query_submission"
                                && !query.submission_id.is_empty()
                            {
                                match &state.input {
                                    Some(input) => input.get(&query.submission_id, &query.subject_id, query.send_epoch),
                                    None => SubmissionView {
                                        result: "temporarily_unavailable",
                                        ..SubmissionView::unavailable(&query.submission_id, query.send_epoch)
                                    },
                                }
                            } else {
                                SubmissionView {
                                    result: "not_admitted",
                                    ..SubmissionView::unavailable(&query.submission_id, query.send_epoch)
                                }
                            };
                            if !send_json(&mut socket, &view).await { break; }
                        } else if let Ok(withdraw) = serde_json::from_str::<WithdrawRequest>(&text) {
                            let view = match &state.input {
                                Some(input) => input.withdraw(&withdraw),
                                None => SubmissionView::unavailable(&withdraw.submission_id, None),
                            };
                            if !send_json(&mut socket, &view).await { break; }
                        } else if let Ok(action) = serde_json::from_str::<ActionSubmission>(&text) {
                            let reply = match (&state.input, &caps) {
                                (Some(input), Some(caps)) => input.submit_action(&action, caps),
                                _ => InputReceipt {
                                    schema_version: 2,
                                    kind: "receipt",
                                    submission_id: action.submission_id.clone(),
                                    stage: SubmissionStage::Rejected,
                                    mode: None,
                                    durability: None,
                                    event_id: None,
                                    accepted_commit: None,
                                    replayed: None,
                                    placement: None,
                                    detail: "动作服务不可用；未接纳。".to_owned(),
                                },
                            };
                            if !send_json(&mut socket, &reply).await { break; }
                        } else if let Ok(hello) = serde_json::from_str::<ClientHello>(&text) {
                            if hello.schema_version == 2 && hello.kind == "client_hello" {
                                if let Some(presentation) = &state.presentation {
                                    caps_revision += 1;
                                    let next = presentation
                                        .capability_provider()
                                        .capabilities_for(&hello.facts, caps_revision);
                                    caps = Some(next.clone());
                                    let note = serde_json::json!({
                                        "schema_version": 2,
                                        "type": "capabilities",
                                        "surface": next,
                                    });
                                    if socket.send(Message::Text(note.to_string().into())).await.is_err() { break; }
                                    // Renegotiation changes how documents may be
                                    // presented, so the newest committed surface
                                    // is re-degraded instead of leaving the client
                                    // with the baseline fallback it already got.
                                    // The client replaces by document_id; no
                                    // timeline part is resent and older surfaces
                                    // keep the degradation they were admitted with.
                                    if let Some(out) = &last_committed {
                                        let (documents, _) = presentation.project(
                                            &out.text,
                                            out.step_id,
                                            out.commit_ref,
                                        );
                                        for payload in degraded_document_messages(documents, &next) {
                                            if socket.send(Message::Text(payload.into())).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        } else {
                            let submission_id = serde_json::from_str::<serde_json::Value>(&text)
                                .ok()
                                .and_then(|value| {
                                    value.get("submission_id")?.as_str().map(ToOwned::to_owned)
                                })
                                .unwrap_or_else(|| Uuid::new_v4().to_string());
                            let reply = InputReceipt {
                                schema_version: 2,
                                kind: "receipt",
                                submission_id,
                                stage: SubmissionStage::Rejected,
                                mode: None,
                                durability: None,
                                event_id: None,
                                accepted_commit: None,
                                replayed: None,
                                placement: None,
                                detail: "无效或未知字段的输入协议；未接纳输入。".to_owned(),
                            };
                            if !send_json(&mut socket, &reply).await { break; }
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

async fn send_json(socket: &mut WebSocket, value: &impl Serialize) -> bool {
    match serde_json::to_string(value) {
        Ok(json) => socket.send(Message::Text(json.into())).await.is_ok(),
        Err(_) => true,
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
    let Some(subject_id) = state.subject_id else {
        // Missing or ambiguous local identity never selects another subject.
        return std::future::pending().await;
    };
    loop {
        if let Some(out) = history.pop_front() {
            *cursor = (out.replay_seq, Some(out.step_id));
            return Some(out);
        }
        match shenko_subject::outbox::committed_after(store, subject_id, cursor.0, cursor.1, 64) {
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

/// Durable stream-part receiver: the store cursor is authoritative; the
/// broadcast channel only shortens the wait. A lagged or lost wake re-reads
/// the ledger, so a reconnect never depends on volatile delivery.
async fn recv_stream(
    state: &WsState,
    cursor: &mut u64,
    history: &mut std::collections::VecDeque<shenko_store::output_parts::StoredOutputPart>,
    wake: &mut Option<broadcast::Receiver<crate::stream::StreamChunkDto>>,
) -> Option<shenko_store::output_parts::StoredOutputPart> {
    let Some(sink) = &state.stream else {
        return std::future::pending().await;
    };
    loop {
        if let Some(chunk) = history.pop_front() {
            *cursor = (*cursor).max(chunk.commit_seq);
            return Some(chunk);
        }
        match sink.parts_after(*cursor, 64) {
            Ok(parts) if !parts.is_empty() => history.extend(parts),
            Ok(_) => {
                tokio::select! {
                    msg = recv_wake(wake) => {
                        match msg {
                            Some(()) | None => {}
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(150)) => {}
                }
            }
            Err(e) => {
                tracing::error!(err=%e, "stream part replay failed; no guessed chunks");
                return None;
            }
        }
    }
}

/// One broadcast wake. `None` = channel closed; `Some(())` = message or lag,
/// in both cases the durable cursor re-read is the source of truth.
async fn recv_wake(rx: &mut Option<broadcast::Receiver<crate::stream::StreamChunkDto>>) -> Option<()> {
    match rx {
        Some(rx) => match rx.recv().await {
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => Some(()),
            Err(broadcast::error::RecvError::Closed) => None,
        },
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::SubmissionMode;
    use crate::presentation::SurfaceCapabilityProvider;
    use shenko_types::commit::CommitRef;
    use shenko_types::ids::LedgerSeq;

    fn presentation() -> Arc<PresentationService> {
        PresentationService::new(
            8,
            SurfaceCapabilityProvider::new(false, "test-runtime".to_owned()),
        )
    }

    fn service_state() -> (
        std::path::PathBuf,
        WsState,
        Arc<WebInputService>,
        tokio::sync::mpsc::Receiver<shenko_types::inbox::InboxEvent>,
    ) {
        let dir = std::env::temp_dir().join(format!("shenko-web-input-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(shenko_store::Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let subject_id = EntityId::new();
        let presentation = presentation();
        let input = WebInputService::new(
            Arc::clone(&store),
            subject_id,
            Some(Arc::new(tx)),
            Arc::clone(&presentation),
        );
        let mut state = WsState::new();
        state.store = Some(store);
        state.subject_id = Some(subject_id);
        state.input = Some(Arc::clone(&input));
        state.presentation = Some(presentation);
        (dir, state, input, rx)
    }

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
    fn append_and_steer_bind_real_subject_with_next_boundary_placement() {
        let (dir, state, input, _rx) = service_state();
        let subject = state.subject_id.unwrap();
        let mut append = submission(SubmissionMode::Append);
        append.subject_id = subject.to_string();
        let accepted = input.submit(&append);
        assert!(matches!(accepted.stage, SubmissionStage::Accepted));
        assert!(accepted.placement.is_some());
        assert_eq!(
            accepted.placement.as_ref().unwrap().actual,
            crate::input::PlacementActual::NextBoundary
        );
        // Steer is durably admitted like append; without a verified in-request
        // control capability its placement is honestly next_boundary (UI02).
        let mut steer = submission(SubmissionMode::Steer);
        steer.subject_id = subject.to_string();
        steer.submission_id = "submission-steer".into();
        steer.message_id = "message-steer".into();
        let steered = input.submit(&steer);
        assert!(matches!(steered.stage, SubmissionStage::Accepted));
        let placement = steered.placement.expect("steer placement recorded");
        assert_eq!(placement.requested, SubmissionMode::Steer);
        assert_eq!(placement.actual, crate::input::PlacementActual::NextBoundary);
        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wrong_subject_is_rejected_without_admission() {
        let (dir, _state, input, _rx) = service_state();
        let mut input_msg = submission(SubmissionMode::Append);
        input_msg.subject_id = EntityId::new().to_string();
        let reply = input.submit(&input_msg);
        assert!(matches!(reply.stage, SubmissionStage::Rejected));
        assert!(reply.event_id.is_none());
        drop(input);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn full_inbox_does_not_delay_durable_acceptance() {
        let (dir, state, input, _rx) = service_state();
        let mut first = submission(SubmissionMode::Append);
        first.subject_id = state.subject_id.unwrap().to_string();
        // The first wake fills the single-slot inbox while its consumer is
        // intentionally stopped (the same backpressure as a paused subject).
        input.submit(&first);
        let mut next = submission(SubmissionMode::Append);
        next.subject_id = first.subject_id.clone();
        next.submission_id = "submission-2".into();
        next.message_id = "message-2".into();
        let accepted = input.submit(&next);
        assert!(matches!(accepted.stage, SubmissionStage::Accepted));
        assert!(accepted.accepted_commit.is_some());
        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn closed_writer_reconciles_lost_ack_from_original_record() {
        let (dir, state, input, _rx) = service_state();
        let mut first = submission(SubmissionMode::Append);
        first.subject_id = state.subject_id.unwrap().to_string();
        let original = input.submit(&first);
        state.store.as_ref().unwrap().close_external_writer();
        let replay = input.submit(&first);
        assert!(matches!(replay.stage, SubmissionStage::Accepted));
        assert_eq!(replay.event_id, original.event_id);
        assert_eq!(replay.accepted_commit, original.accepted_commit);
        assert_eq!(replay.replayed, Some(true));
        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn conflicting_payload_cannot_rewrite_the_original_submission() {
        let (dir, state, input, _rx) = service_state();
        let mut first = submission(SubmissionMode::Append);
        first.subject_id = state.subject_id.unwrap().to_string();
        input.submit(&first);
        // The stored admission payload is the canonical dedup record.
        let payload = state.store.as_ref().unwrap()
            .web_submission(state.subject_id.unwrap(), &first.submission_id).unwrap().unwrap()
            .payload;
        first.content = "不能用原 ID 改写".into();
        assert!(matches!(input.submit(&first).stage, SubmissionStage::Conflict));
        let original = state.store.as_ref().unwrap()
            .web_submission(state.subject_id.unwrap(), &first.submission_id).unwrap().unwrap();
        assert_eq!(original.payload, payload);
        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn input_history_retains_admission_order_and_withdrawal_after_reopen() {
        let (dir, state, input, mut rx) = service_state();
        let subject = state.subject_id.unwrap();
        let store = state.store.as_ref().unwrap();
        let mut first_time = None;
        for number in 0..3 {
            let mut message = submission(SubmissionMode::Append);
            message.subject_id = subject.to_string();
            message.submission_id = format!("history-{number}");
            message.message_id = format!("message-{number}");
            message.content = format!("历史正文 {number}");
            assert!(matches!(input.submit(&message).stage, SubmissionStage::Accepted));
            if number == 0 {
                first_time = Some(rx.try_recv().unwrap().created_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
            }
            // Another subject's admission creates a real gap in ledger seq;
            // it must neither appear nor terminate this subject's page.
            if number == 1 {
                let other = EntityId::new();
                let foreign = WebInputService::new(Arc::clone(store), other, None, presentation());
                let mut message = submission(SubmissionMode::Append);
                message.subject_id = other.to_string();
                assert!(matches!(foreign.submit(&message).stage, SubmissionStage::Accepted));
            }
        }
        let withdrawal = input.withdraw(&WithdrawRequest {
            schema_version: 2, kind: "withdraw".into(), submission_id: "history-0".into(),
            subject_id: subject.to_string(), cancel_id: "cancel-history-0".into(),
        });
        assert_eq!(withdrawal.withdrawal.unwrap().result, "withdrawn_before_presentation");
        let page = WebInputService::history(store, subject, None, 2).unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.items.iter().map(|row| row.payload["submission_id"].as_str().unwrap())
            .collect::<Vec<_>>(), ["history-2", "history-1"]);
        assert!(page.has_older);
        let before = page.next_before.unwrap();
        let accepted = input.get("history-0", &subject.to_string(), None).accepted_commit.unwrap();
        drop(input);
        drop(state);
        let reopened = shenko_store::Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
        let older = WebInputService::history(&reopened, subject, Some(&before), 2).unwrap();
        assert_eq!(older.total, 3);
        assert!(!older.has_older);
        assert_eq!(older.next_before, None);
        assert_eq!(older.items[0].payload["content"], "历史正文 0");
        assert_eq!(older.items[0].created_at, first_time.unwrap());
        assert_eq!(older.items[0].view.accepted_commit.as_deref(), Some(accepted.as_str()));
        assert_eq!(older.items[0].view.withdrawal.as_ref().unwrap().cancel_id, "cancel-history-0");
        assert_eq!(WebInputService::history(&reopened, EntityId::new(), None, 2).unwrap().total, 0);
    }

    #[test]
    fn an_unreadable_submission_table_does_not_report_not_admitted() {
        let (dir, state, input, _rx) = service_state();
        let subject = state.subject_id.unwrap();
        let mut message = submission(SubmissionMode::Append);
        message.subject_id = subject.to_string();
        assert!(matches!(input.submit(&message).stage, SubmissionStage::Accepted));
        // Preserve the original rows while making the normal reader fail.
        let database = rusqlite::Connection::open(dir.join("store.db")).unwrap();
        database.execute("ALTER TABLE web_input_submissions RENAME TO retained_unavailable_submissions", []).unwrap();
        assert_eq!(input.get(&message.submission_id, &message.subject_id, None).result, "temporarily_unavailable");
        assert!(WebInputService::history(state.store.as_ref().unwrap(), subject, None, 10).is_err());
        assert_eq!(database.query_row("SELECT COUNT(*) FROM retained_unavailable_submissions", [],
            |row| row.get::<_, i64>(0)).unwrap(), 1);
    }

    #[test]
    fn committed_projection_reuses_message_part_identity() {
        let step_id = EntityId::new();
        let commit_ref = CommitRef::new(EntityId::new(), LedgerSeq(700));
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
        let state = WsState::new();
        let messages = committed_timeline_messages(&state, out);
        assert_eq!(messages.len(), 1);
        let first = &messages[0];
        assert_eq!(first.message_id, step_id.to_string());
        assert_eq!(first.commit_ref, commit_ref.to_string());
        assert_eq!(first.commit_seq, 7);
        assert_eq!(first.part_id, TEXT_PART_ID);
        assert_eq!(first.chunk_seq, 0);
        assert_eq!(first.terminal, Some(true));
        assert!(first.committed_only);
        assert!(first.request_id.is_none());
        assert!(first.attempt_id.is_none());
    }
}
