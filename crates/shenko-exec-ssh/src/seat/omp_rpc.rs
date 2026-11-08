//! Managed OMP seat adapter — the OMP JSONL RPC path (SPEC §35.5).
//!
//! OMP exposes a newline-delimited JSON protocol on stdio
//! (`omp --mode rpc`). This adapter owns that stdio: it starts the process,
//! keeps the single reader of its stdout, negotiates the optional lossless
//! v2 framing, and turns the protocol's real question events into seat
//! questions.
//!
//! The protocol facts this adapter relies on:
//!
//! * The first frame is `{"type":"ready", ...}`. It advertises the
//!   supported protocol versions and the frame size limits.
//! * Questions arrive as `{"type":"extension_ui_request","id":...,
//!   "method":"select"|"confirm"|"input"|"editor", ...}`. A `select` carries
//!   `options: string[]` and an optional positional `optionDetails`.
//! * Answers are `{"type":"extension_ui_response","id":...}` with either
//!   `value`, `confirmed` or `cancelled`.
//! * A dialog with a timeout is resolved by the remote side when the
//!   timeout fires. That resolution is **not** a decision by this product,
//!   and a late answer to it is ignored by the remote. Both facts are
//!   recorded here rather than being folded into "the agent answered".
//!
//! Nothing in this module spawns a process on its own: the seat runtime
//! creates the adapter explicitly.

use crate::seat::{
    AnswerConfirmation, AnswerTransport, Confidence, PreflightRefusalKind, ProcessIdentity,
    QuestionKind, QuestionSourceKind, SeatAdapter, SeatAdapterId, SeatAnswer, SeatCapability,
    SeatEffectPlan, SeatHandle, SeatObservation, SeatOption, SeatPreflight, SeatQuestionCandidate,
    SeatResponse, SeatWriteFence,
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    process::Stdio,
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::Mutex,
};

/// Adapter version bound into durable seat records.
pub const OMP_ADAPTER_VERSION: &str = "omp-rpc-v1";

/// One frame the remote side published that the subject may want to read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OmpEvent {
    /// The ready frame; carries the negotiated protocol version.
    Ready { protocol_version: u32 },
    /// A turn ended. `terminal` is false when the runtime says more work is
    /// scheduled — a prompt result is not a settled session.
    TurnEnded { terminal: bool },
    /// The session reported that it settled.
    SessionSettled,
    /// A command failed.
    CommandFailed { command: String, error: String },
}

/// What happened to one open question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OmpQuestionState {
    /// The remote side is waiting for an answer.
    Open,
    /// This product answered it.
    Answered { value: String },
    /// The remote side resolved it without this product — most likely the
    /// dialog's timeout picked a default. That is *not* a decision by the
    /// agent.
    ExternallyResolved { reason: String },
    /// This product's answer arrived after the remote side had already
    /// resolved the question; the remote ignores such answers.
    LateAnswerIgnored { reason: String },
}

/// A managed OMP process speaking its JSONL RPC protocol.
pub struct OmpRpcSeat {
    inner: Arc<Inner>,
}

struct Inner {
    stdin: Mutex<Option<ChildStdin>>,
    state: Mutex<State>,
    session_id: String,
    command: Vec<String>,
    /// The spawned process, kept alive for the seat's lifetime. Dropping this
    /// `Child` (which was spawned with `kill_on_drop`) terminates the managed
    /// OMP; without it the child is killed as soon as `spawn` returns.
    child: Mutex<Child>,
}

#[derive(Default)]
struct State {
    ready: bool,
    protocol_version: u32,
    questions: BTreeMap<String, SeatQuestionCandidate>,
    question_states: BTreeMap<String, OmpQuestionState>,
    events: VecDeque<OmpEvent>,
    chunks: BTreeMap<String, ChunkAssembly>,
    stderr_tail: Vec<String>,
    next_request_id: u64,
}

#[derive(Default)]
struct ChunkAssembly {
    count: usize,
    byte_length: usize,
    parts: BTreeMap<usize, Vec<u8>>,
}

impl OmpRpcSeat {
    /// Start the managed OMP process and take ownership of its stdio.
    pub async fn spawn(command: Vec<String>, cwd: Option<PathBuf>) -> Result<Self> {
        ensure!(!command.is_empty(), "omp seat 需要明确的启动命令");
        let mut builder = Command::new(&command[0]);
        builder
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            builder.current_dir(cwd);
        }
        let mut child = builder
            .spawn()
            .with_context(|| format!("启动托管 OMP 失败：{}", command.join(" ")))?;
        let stdin = child.stdin.take().context("托管 OMP 没有可用的 stdin")?;
        let stdout = child.stdout.take().context("托管 OMP 没有可用的 stdout")?;
        let stderr = child.stderr.take().context("托管 OMP 没有可用的 stderr")?;
        let session_id = command.join(" ");
        let inner = Arc::new(Inner {
            stdin: Mutex::new(Some(stdin)),
            state: Mutex::new(State::default()),
            session_id,
            command: command.clone(),
            child: Mutex::new(child),
        });
        let reader_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let mut state = reader_inner.state.lock().await;
                ingest_line(&line, &mut state);
            }
            let mut state = reader_inner.state.lock().await;
            state.ready = false;
        });
        let stderr_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut state = stderr_inner.state.lock().await;
                if state.stderr_tail.len() >= 32 {
                    state.stderr_tail.remove(0);
                }
                state.stderr_tail.push(line);
            }
        });
        Ok(Self { inner })
    }

    /// The exact command this seat was started with.
    pub fn command(&self) -> &[String] {
        &self.inner.command
    }

    async fn send_frame(&self, frame: &Value) -> Result<()> {
        let mut guard = self.inner.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .context("托管 OMP 的 stdin 已关闭，无法发送")?;
        let mut line = serde_json::to_vec(frame)?;
        line.push(b'\n');
        stdin.write_all(&line).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn next_request_id(&self) -> String {
        let mut state = self.inner.state.lock().await;
        state.next_request_id += 1;
        format!("shenko-{}", state.next_request_id)
    }
}

/// Parse one stdout line and fold it into the adapter state.
fn ingest_line(line: &str, state: &mut State) {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return;
    };
    if let Some(frame) = take_chunk(&value, state) {
        ingest_frame(&frame, state);
        return;
    }
    ingest_frame(&value, state);
}

/// Handle the v2 lossless framing: a large object is emitted as an
/// uninterrupted sequence of `rpc_chunk` frames that must be validated and
/// reassembled in index order.
fn take_chunk(value: &Value, state: &mut State) -> Option<Value> {
    if value.get("type").and_then(Value::as_str) != Some("rpc_chunk") {
        return None;
    }
    let chunk_id = value.get("chunkId").and_then(Value::as_str)?;
    let index = value.get("index").and_then(Value::as_u64)? as usize;
    let count = value.get("count").and_then(Value::as_u64)? as usize;
    let byte_length = value.get("byteLength").and_then(Value::as_u64)? as usize;
    let data = value.get("data").and_then(Value::as_str)?;
    let bytes = B64.decode(data).ok()?;
    let entry = state.chunks.entry(chunk_id.to_string()).or_insert_with(|| ChunkAssembly {
        count, byte_length, parts: BTreeMap::new(),
    });
    if entry.count != count || entry.byte_length != byte_length {
        // An inconsistent header invalidates the whole sequence.
        state.chunks.remove(chunk_id);
        return None;
    }
    if index >= count || entry.parts.contains_key(&index) {
        state.chunks.remove(chunk_id);
        return None;
    }
    entry.parts.insert(index, bytes);
    if entry.parts.len() != count {
        return None;
    }
    let assembly = state.chunks.remove(chunk_id)?;
    let actual_bytes = assembly.parts.values().try_fold(0usize, |sum, part| sum.checked_add(part.len()))?;
    if actual_bytes != assembly.byte_length {
        return None;
    }
    let mut merged = Vec::with_capacity(actual_bytes);
    for index in 0..assembly.count {
        let Some(part) = assembly.parts.get(&index) else {
            return None;
        };
        merged.extend_from_slice(part);
    }
    serde_json::from_slice::<Value>(&merged).ok()
}

/// Fold one protocol frame into the state.
fn ingest_frame(value: &Value, state: &mut State) {
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return;
    };
    match kind {
        "ready" => {
            state.ready = true;
            state.protocol_version = value
                .get("protocolVersion")
                .and_then(Value::as_u64)
                .unwrap_or(1) as u32;
            push_event(
                state,
                OmpEvent::Ready {
                    protocol_version: state.protocol_version,
                },
            );
        }
        "extension_ui_request" => {
            let Some(question) = question_from_ui_request(value) else {
                return;
            };
            let id = question.question_id.clone();
            state.questions.insert(id.clone(), question);
            state
                .question_states
                .insert(id, OmpQuestionState::Open);
        }
        "response" => {
            if value.get("success").and_then(Value::as_bool) == Some(false) {
                push_event(
                    state,
                    OmpEvent::CommandFailed {
                        command: value
                            .get("command")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        error: value
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    },
                );
            }
        }
        "agent_end" => {
            let terminal = value.get("isTerminal").and_then(Value::as_bool).unwrap_or(true);
            push_event(state, OmpEvent::TurnEnded { terminal });
            resolve_open_questions(state, "远端回合结束，问题不再等待回答");
        }
        "session_settled" => {
            push_event(state, OmpEvent::SessionSettled);
            resolve_open_questions(state, "远端 session 已结算，问题不再等待回答");
        }
        "prompt_result" => {
            // A prompt result says the prompt resolved locally; it is not a
            // settled session.
            let agent_invoked = value.get("agentInvoked").and_then(Value::as_bool);
            if agent_invoked == Some(false) {
                resolve_open_questions(state, "远端 prompt 已本地结算");
            }
        }
        _ => {}
    }
}

fn push_event(state: &mut State, event: OmpEvent) {
    if state.events.len() >= 256 {
        state.events.pop_front();
    }
    state.events.push_back(event);
}

/// Mark every still-open question as resolved by the remote side.
fn resolve_open_questions(state: &mut State, reason: &str) {
    let ids: Vec<String> = state
        .question_states
        .iter()
        .filter(|(_, value)| matches!(value, OmpQuestionState::Open))
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        state
            .question_states
            .insert(id, OmpQuestionState::ExternallyResolved { reason: reason.into() });
    }
}

/// Build a seat question from an `extension_ui_request` frame.
fn question_from_ui_request(value: &Value) -> Option<SeatQuestionCandidate> {
    let id = value.get("id").and_then(Value::as_str)?.to_string();
    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string);
    let timeout_ms = value.get("timeout").and_then(Value::as_i64);
    let mut unknown = Vec::new();
    if timeout_ms.is_some() {
        unknown.push(
            "该对话框带超时；远端在超时后可能自行采用默认值，那不是本主体的决定".to_string(),
        );
    }
    let (question_kind, options) = match method {
        "select" => {
            let labels: Vec<String> = value
                .get("options")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|item| match item {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let details: Vec<Option<String>> = value
                .get("optionDetails")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            item.get("description")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let options = labels
                .iter()
                .enumerate()
                .map(|(index, label)| SeatOption {
                    option_id: format!("opt-{}", index + 1),
                    label: label.clone(),
                    meaning: details.get(index).cloned().flatten(),
                    native_value: Some(label.clone()),
                })
                .collect();
            (QuestionKind::Planning, options)
        }
        "confirm" => (QuestionKind::ToolPermission, Vec::new()),
        "input" | "editor" => (QuestionKind::Information, Vec::new()),
        _ => return None,
    };
    Some(SeatQuestionCandidate {
        question_id: format!("omp-ui:{id}"),
        source_kind: QuestionSourceKind::NativeRpc,
        question_kind,
        native_request_id: Some(id),
        native_tool_use_id: None,
        // The original request frame travels with the question, so the
        // answer can be compared against what the remote side asked for.
        native_payload: Some(value.clone()),
        title,
        text: message,
        options,
        selected_option: None,
        confidence: Confidence::high("OMP RPC 的 extension_ui_request 结构化事件"),
        unknown,
        observed_screen_revision: 0,
        expires_at_ms: timeout_ms.map(|timeout| {
            chrono::Utc::now().timestamp_millis().saturating_add(timeout)
        }),
    })
}

#[async_trait]
impl SeatAdapter for OmpRpcSeat {
    fn adapter_id(&self) -> SeatAdapterId {
        SeatAdapterId::OmpRpc
    }

    fn adapter_version(&self) -> &str {
        OMP_ADAPTER_VERSION
    }

    fn capability(&self) -> SeatCapability {
        SeatCapability {
            structured_questions: true,
            structured_answers: true,
            screen_observation: false,
            steer: true,
            interrupt: true,
            resume: true,
            notes: vec![
                "问题来自 OMP RPC 的 extension_ui_request 结构化事件".into(),
                "带超时的对话框由远端在超时后自行结算，本产品不会把默认值记成主体决定".into(),
                "prompt_result 不是 session_settled：后台工作可能仍在继续".into(),
            ],
        }
    }

    async fn discover(&self) -> Result<Vec<SeatHandle>> {
        Ok(vec![SeatHandle {
            location_id: self.inner.session_id.clone(),
            adapter: SeatAdapterId::OmpRpc,
            adapter_version: OMP_ADAPTER_VERSION.into(),
            host: "local".into(),
            cwd: None,
            remote_session_id: Some(self.inner.session_id.clone()),
            current_command: Some(self.inner.command.join(" ")),
            process_identity: Some(ProcessIdentity {
                pid: None,
                start_token: None,
                executable: Some(self.inner.command.join(" ")),
            }),
        }])
    }

    async fn observe(&self, _seat: &SeatHandle) -> Result<SeatObservation> {
        let state = self.inner.state.lock().await;
        let questions: Vec<SeatQuestionCandidate> = state
            .questions
            .values()
            .filter(|question| {
                matches!(
                    state.question_states.get(&question.question_id),
                    Some(OmpQuestionState::Open)
                )
            })
            .cloned()
            .collect();
        let events: Vec<OmpEvent> = state.events.iter().cloned().collect();
        let mut unknown = Vec::new();
        if !state.ready {
            unknown.push("尚未收到 OMP 的 ready 帧，这个 seat 的协议状态未知".into());
        }
        if !state.stderr_tail.is_empty() {
            unknown.push(format!(
                "OMP stderr 末尾：{}",
                state.stderr_tail.join(" | ")
            ));
        }
        Ok(SeatObservation {
            seat: SeatHandle {
                location_id: self.inner.session_id.clone(),
                adapter: SeatAdapterId::OmpRpc,
                adapter_version: OMP_ADAPTER_VERSION.into(),
                host: "local".into(),
                cwd: None,
                remote_session_id: Some(self.inner.session_id.clone()),
                current_command: Some(self.inner.command.join(" ")),
                process_identity: None,
            },
            observation_revision: events.len() as u64,
            observed_at: chrono::Utc::now(),
            screen: None,
            screen_text: String::new(),
            questions,
            unknown,
            raw_evidence: Vec::new(),
        })
    }

    async fn plan_effect(
        &self,
        _seat: &SeatHandle,
        question: &SeatQuestionCandidate,
        answer: &SeatAnswer,
    ) -> Result<Option<SeatEffectPlan>> {
        let native_id = question
            .native_request_id
            .clone()
            .context("该问题没有 OMP 的原生请求 id，不能通过 RPC 回答")?;
        let frame = build_ui_response(&native_id, question, answer)?;
        Ok(Some(SeatEffectPlan::new(
            AnswerTransport::NativeRpc,
            serde_json::to_string(&frame)?,
            format!("extension_ui_response for {native_id}"),
        )
        .with_question_key(question.question_id.clone())))
    }

    async fn preflight(
        &self,
        _seat: &SeatHandle,
        question: &SeatQuestionCandidate,
        _plan: &SeatEffectPlan,
    ) -> Result<SeatPreflight> {
        // The protocol state is the live truth here: OMP publishes a real
        // request lifecycle, so the request must still be open.
        let state = self.inner.state.lock().await;
        match state.question_states.get(&question.question_id) {
            Some(OmpQuestionState::Open) => Ok(SeatPreflight::Ready {
                detail: "OMP 的 extension_ui_request 仍是 open 状态".into(),
            }),
            Some(OmpQuestionState::ExternallyResolved { reason }) => Ok(SeatPreflight::Refused {
                kind: PreflightRefusalKind::QuestionGone,
                reason: format!("远端已自行结算该问题（{reason}），迟到的答案会被忽略"),
            }),
            Some(OmpQuestionState::Answered { value }) => Ok(SeatPreflight::Refused {
                kind: PreflightRefusalKind::QuestionGone,
                reason: format!("该问题已经回答过：{value}"),
            }),
            Some(OmpQuestionState::LateAnswerIgnored { reason }) => Ok(SeatPreflight::Refused {
                kind: PreflightRefusalKind::QuestionGone,
                reason: format!("该问题的迟到答案已被远端忽略：{reason}"),
            }),
            None => Ok(SeatPreflight::Refused {
                kind: PreflightRefusalKind::QuestionGone,
                reason: "没有这个 OMP 问题记录".into(),
            }),
        }
    }

    async fn execute_effect(
        &self,
        _seat: &SeatHandle,
        plan: &SeatEffectPlan,
        fence: &SeatWriteFence,
    ) -> Result<SeatResponse> {
        let frame: Value = serde_json::from_str(&plan.payload)?;
        self.send_frame(&frame).await?;
        let mut state = self.inner.state.lock().await;
        if let Some(question_id) = plan.question_key.clone() {
            state.question_states.insert(
                question_id,
                OmpQuestionState::Answered {
                    value: frame
                        .get("value")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| {
                            frame
                                .get("confirmed")
                                .and_then(Value::as_bool)
                                .map(|confirmed| confirmed.to_string())
                        })
                        .unwrap_or_default(),
                },
            );
        }
        Ok(SeatResponse {
            adapter: SeatAdapterId::OmpRpc,
            transport: AnswerTransport::NativeRpc,
            // The response frame was written; the remote side confirms by
            // consuming it, which this adapter does not observe.
            confirmation: AnswerConfirmation::Unconfirmed,
            detail: Some(format!(
                "extension_ui_response 已写入托管 OMP 的 stdin（writer epoch {}）",
                fence.lease_epoch
            )),
        })
    }

    async fn submit(
        &self,
        _seat: &SeatHandle,
        text: &str,
        fence: &SeatWriteFence,
    ) -> Result<SeatResponse> {
        let id = self.next_request_id().await;
        let frame = json!({"id": id, "type": "prompt", "message": text});
        self.send_frame(&frame).await?;
        Ok(SeatResponse {
            adapter: SeatAdapterId::OmpRpc,
            transport: AnswerTransport::NativeRpc,
            confirmation: AnswerConfirmation::Unconfirmed,
            detail: Some(format!(
                "prompt 命令已写入（writer epoch {}）；命令被接受不等于回合完成",
                fence.lease_epoch
            )),
        })
    }

    async fn interrupt(&self, _seat: &SeatHandle, fence: &SeatWriteFence) -> Result<SeatResponse> {
        let id = self.next_request_id().await;
        let frame = json!({"id": id, "type": "abort"});
        self.send_frame(&frame).await?;
        Ok(SeatResponse {
            adapter: SeatAdapterId::OmpRpc,
            transport: AnswerTransport::NativeRpc,
            confirmation: AnswerConfirmation::Unconfirmed,
            detail: Some(format!(
                "abort 命令已写入（writer epoch {}）；远端是否停止尚未确认",
                fence.lease_epoch
            )),
        })
    }
}

/// Build the exact `extension_ui_response` frame for a decision.
fn build_ui_response(
    native_id: &str,
    question: &SeatQuestionCandidate,
    answer: &SeatAnswer,
) -> Result<Value> {
    let mut frame = serde_json::Map::new();
    frame.insert("type".into(), Value::String("extension_ui_response".into()));
    frame.insert("id".into(), Value::String(native_id.to_string()));
    match question.question_kind {
        QuestionKind::ToolPermission => {
            let allow = answer
                .allow
                .context("许可类问题必须给出 allow/deny 决定")?;
            frame.insert("confirmed".into(), Value::Bool(allow));
        }
        _ => {
            if let Some(option_id) = &answer.option_id {
                let option = question
                    .options
                    .iter()
                    .find(|option| &option.option_id == option_id)
                    .context("选择的选项不属于该问题")?;
                let value = option
                    .native_value
                    .clone()
                    .unwrap_or_else(|| option.label.clone());
                frame.insert("value".into(), Value::String(value));
            } else if let Some(text) = &answer.text {
                frame.insert("value".into(), Value::String(text.clone()));
            } else {
                return Err(anyhow::anyhow!("该问题需要选项或文本回答"));
            }
        }
    }
    Ok(Value::Object(frame))
}

impl OmpRpcSeat {
    /// Close this product's side of the stdio and terminate the managed
    /// process. The seat owns the `Child` (spawned with `kill_on_drop`), so the
    /// process also dies if the adapter is dropped without `close`.
    pub async fn close(&self) {
        {
            let mut guard = self.inner.stdin.lock().await;
            *guard = None;
        }
        // Reap the managed process. It was spawned with `kill_on_drop`, so this
        // is belt-and-suspenders; an explicit kill makes teardown deterministic
        // rather than relying on `Inner` drop order.
        let mut child = self.inner.child.lock().await;
        let _ = child.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_select_request_becomes_a_structured_question() {
        let frame = json!({
            "type": "extension_ui_request",
            "id": "ui_7",
            "method": "select",
            "title": "Pick",
            "message": "Which database?",
            "options": ["sqlite", "postgres"],
            "optionDetails": [{"description": "file"}, {"description": "server"}],
            "timeout": 30000
        });
        let question = question_from_ui_request(&frame).expect("question");
        assert_eq!(question.source_kind, QuestionSourceKind::NativeRpc);
        assert_eq!(question.question_kind, QuestionKind::Planning);
        assert_eq!(question.options.len(), 2);
        assert_eq!(question.options[1].meaning.as_deref(), Some("server"));
        assert_eq!(question.native_request_id.as_deref(), Some("ui_7"));
        assert!(question.unknown.iter().any(|entry| entry.contains("超时")));
    }

    #[test]
    fn a_confirm_request_is_a_permission_question() {
        let frame = json!({
            "type": "extension_ui_request",
            "id": "ui_9",
            "method": "confirm",
            "message": "Continue?"
        });
        let question = question_from_ui_request(&frame).expect("question");
        assert_eq!(question.question_kind, QuestionKind::ToolPermission);
    }

    #[test]
    fn a_remote_timeout_is_not_recorded_as_our_decision() {
        let mut state = State::default();
        let frame = json!({
            "type": "extension_ui_request",
            "id": "ui_1",
            "method": "select",
            "message": "Pick",
            "options": ["a"]
        });
        ingest_frame(&frame, &mut state);
        assert!(matches!(
            state.question_states.get("omp-ui:ui_1"),
            Some(OmpQuestionState::Open)
        ));
        ingest_frame(&json!({"type": "session_settled"}), &mut state);
        assert!(matches!(
            state.question_states.get("omp-ui:ui_1"),
            Some(OmpQuestionState::ExternallyResolved { .. })
        ));
    }

    #[test]
    fn chunk_frames_reassemble_in_index_order() {
        let mut state = State::default();
        let payload = json!({"type": "extension_ui_request", "id": "ui_2", "method": "confirm", "message": "m"});
        let bytes = serde_json::to_vec(&payload).unwrap();
        let half = bytes.len() / 2;
        let first = B64.encode(&bytes[..half]);
        let second = B64.encode(&bytes[half..]);
        ingest_line(
            &json!({"type":"rpc_chunk","chunkId":"c1","index":0,"count":2,"byteLength":bytes.len(),"data":first}).to_string(),
            &mut state,
        );
        assert!(state.questions.is_empty());
        ingest_line(
            &json!({"type":"rpc_chunk","chunkId":"c1","index":1,"count":2,"byteLength":bytes.len(),"data":second}).to_string(),
            &mut state,
        );
        assert_eq!(state.questions.get("omp-ui:ui_2").map(|question| &question.question_kind),
            Some(&QuestionKind::ToolPermission));
        assert!(matches!(state.question_states.get("omp-ui:ui_2"), Some(OmpQuestionState::Open)));
    }

    #[test]
    fn chunk_length_mismatch_does_not_adopt_a_valid_json_prefix() {
        let mut state = State::default();
        let payload = json!({"type":"extension_ui_request","id":"truncated","method":"confirm","message":"m"});
        let bytes = serde_json::to_vec(&payload).unwrap();
        ingest_line(&json!({"type":"rpc_chunk","chunkId":"bad","index":0,"count":1,
            "byteLength":bytes.len()+1,"data":B64.encode(&bytes)}).to_string(), &mut state);
        assert!(!state.questions.contains_key("omp-ui:truncated"));
        assert!(!state.question_states.contains_key("omp-ui:truncated"));
        assert!(!state.chunks.contains_key("bad"));
    }
}
