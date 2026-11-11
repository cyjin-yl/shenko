//! Managed Claude Code seat adapter — the Claude control-protocol path
//! (SPEC §35.5).
//!
//! Claude Code supports a non-interactive stdio protocol:
//! `claude -p --input-format stream-json --output-format stream-json
//!  --permission-prompt-tool-name stdio`.
//!
//! This adapter owns that stdio. It sends the protocol's required
//! `initialize` control request first, then user messages, and it turns the
//! protocol's real decision events into seat questions:
//!
//! * `control_request` with `subtype: "can_use_tool"` is either a tool
//!   permission or, when `tool_name == "AskUserQuestion"`, a clarifying
//!   question. The two are distinct `QuestionKind`s.
//! * Answers are `control_response` frames with `behavior: allow` (plus the
//!   original `questions` array for `AskUserQuestion`) or `behavior: deny`.
//!
//! Two facts the upstream docs state that this adapter must not blur:
//!
//! * `canUseTool` is **not** a uniform audit entry — calls an earlier rule
//!   approved never reach it. This product therefore keeps its own task and
//!   effect records; the callback is a decision surface, not a log.
//! * The callback's in-memory Promise is not a durable question record.
//!   This adapter persists every question through the store before it is
//!   answered (SPEC §41.11).

use crate::seat::{
    AnswerConfirmation, AnswerTransport, Confidence, PreflightRefusalKind, ProcessIdentity,
    QuestionKind, QuestionSourceKind, SeatAdapter, SeatAdapterId, SeatAnswer, SeatCapability,
    SeatEffectPlan, SeatHandle, SeatObservation, SeatOption, SeatPreflight, SeatQuestionCandidate,
    SeatResponse, SeatWriteFence,
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::Mutex,
};

/// Adapter version bound into durable seat records.
pub const CLAUDE_ADAPTER_VERSION: &str = "claude-control-v1";

/// What happened to one open question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeQuestionState {
    /// The control request is still waiting for a decision.
    Open,
    /// This product decided it.
    Decided { behavior: String },
}

/// A managed Claude Code process speaking the control protocol.
pub struct ClaudeSdkSeat {
    inner: Arc<Inner>,
}

struct Inner {
    stdin: Mutex<Option<tokio::process::ChildStdin>>,
    state: Mutex<State>,
    command: Vec<String>,
    /// The spawned process, kept alive for the seat's lifetime. Dropping
    /// this `Child` (which was spawned with `kill_on_drop`) terminates the
    /// managed agent; without it the child is killed as soon as `spawn`
    /// returns.
    child: Mutex<tokio::process::Child>,
}

#[derive(Default)]
struct State {
    initialized: bool,
    questions: BTreeMap<String, SeatQuestionCandidate>,
    question_states: BTreeMap<String, ClaudeQuestionState>,
    next_request_id: u64,
    session_id: Option<String>,
    stderr_tail: Vec<String>,
}

impl ClaudeSdkSeat {
    /// Start the managed Claude Code process and take its stdio.
    pub async fn spawn(mut command: Vec<String>, cwd: Option<PathBuf>) -> Result<Self> {
        ensure!(!command.is_empty(), "claude seat 需要明确的启动命令");
        // The control protocol only exists when the stdio prompt tool is
        // named; without it the CLI auto-denies every permission request.
        if !command.iter().any(|arg| arg.contains("--permission-prompt-tool-name")) {
            command.push("--permission-prompt-tool-name".into());
            command.push("stdio".into());
        }
        command.push("--input-format".into());
        command.push("stream-json".into());
        command.push("--output-format".into());
        command.push("stream-json".into());
        command.push("--verbose".into());
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
            .with_context(|| format!("启动托管 Claude Code 失败：{}", command.join(" ")))?;
        let stdin = child.stdin.take().context("托管 Claude Code 没有可用的 stdin")?;
        let stdout = child.stdout.take().context("托管 Claude Code 没有可用的 stdout")?;
        let stderr = child.stderr.take().context("托管 Claude Code 没有可用的 stderr")?;
        let inner = Arc::new(Inner {
            stdin: Mutex::new(Some(stdin)),
            state: Mutex::new(State::default()),
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
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    let mut state = reader_inner.state.lock().await;
                    ingest_frame(&value, &mut state);
                }
            }
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
        let seat = Self { inner };
        seat.send_initialize().await?;
        Ok(seat)
    }

    /// The remote-side session identity once the `system`/`init` or a
    /// `result` frame has published it; `None` while the session has not
    /// reported one. This is what the adapter exposes as
    /// `SeatHandle.remote_session_id`.
    pub async fn session_id(&self) -> Option<String> {
        self.inner.state.lock().await.session_id.clone()
    }

    async fn send_frame(&self, frame: &Value) -> Result<()> {
        let mut guard = self.inner.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .context("托管 Claude Code 的 stdin 已关闭，无法发送")?;
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

    /// The `initialize` control request the protocol requires before the
    /// first user message.
    async fn send_initialize(&self) -> Result<()> {
        let request_id = self.next_request_id().await;
        let frame = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "initialize", "hooks": null }
        });
        self.send_frame(&frame).await
    }
}

/// Fold one stdout frame into the adapter state.
fn ingest_frame(value: &Value, state: &mut State) {
    match value.get("type").and_then(Value::as_str) {
        Some("control_request") => {
            let Some(question) = question_from_control_request(value) else {
                return;
            };
            let id = question.question_id.clone();
            state.questions.insert(id.clone(), question);
            state.question_states.insert(id, ClaudeQuestionState::Open);
        }
        Some("system") => {
            if value.get("subtype").and_then(Value::as_str) == Some("init") {
                if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
                    state.session_id = Some(session_id.to_string());
                }
                state.initialized = true;
            }
        }
        Some("result") => {
            if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
                state.session_id = Some(session_id.to_string());
            }
        }
        _ => {}
    }
}

/// Build a seat question from a `control_request` frame.
fn question_from_control_request(value: &Value) -> Option<SeatQuestionCandidate> {
    let request = value.get("request")?;
    if request.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
        return None;
    }
    let request_id = value.get("request_id").and_then(Value::as_str)?.to_string();
    let tool_name = request
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let input = request.get("input").cloned().unwrap_or(Value::Null);
    if tool_name == "AskUserQuestion" {
        let questions = input
            .get("questions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // The first question carries the candidate; a control request holds
        // one decision surface. Multi-question cards are joined into the
        // text and every sub-question is preserved in the options' meanings.
        let mut options = Vec::new();
        let mut texts = Vec::new();
        for (index, question) in questions.iter().enumerate() {
            let text = question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let header = question
                .get("header")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let multi_select = question
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            texts.push(if header.is_empty() {
                text.clone()
            } else {
                format!("{header}: {text}")
            });
            for (option_index, option) in question
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let label = option
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let description = option
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                options.push(SeatOption {
                    option_id: format!("opt-{}-{}", index + 1, option_index + 1),
                    label: label.clone(),
                    meaning: description,
                    native_value: Some(label),
                });
            }
            if multi_select {
                texts.push("（多选）".to_string());
            }
        }
        return Some(SeatQuestionCandidate {
            question_id: format!("claude-ask:{request_id}"),
            source_kind: QuestionSourceKind::SdkCallback,
            question_kind: QuestionKind::Information,
            native_request_id: Some(request_id),
            native_tool_use_id: None,
            // The original questions array travels with the question: the
            // answer has to echo it, not a reconstruction of it.
            native_payload: Some(serde_json::json!({ "questions": questions })),
            title: Some("AskUserQuestion".to_string()),
            text: texts.join("\n"),
            options,
            selected_option: None,
            confidence: Confidence::high("Claude Code 控制协议的 canUseTool 回调"),
            unknown: vec![],
            observed_screen_revision: 0,
            expires_at_ms: None,
        });
    }
    // A regular tool permission.
    let summary = serde_json::to_string(&input).unwrap_or_default();
    Some(SeatQuestionCandidate {
        question_id: format!("claude-perm:{request_id}"),
        source_kind: QuestionSourceKind::SdkCallback,
        question_kind: QuestionKind::ToolPermission,
        native_request_id: Some(request_id),
        native_tool_use_id: None,
        native_payload: Some(input.clone()),
        title: Some(tool_name.clone()),
        text: if summary.len() > 512 {
            format!("{tool_name}: {}…", &summary[..512])
        } else {
            format!("{tool_name}: {summary}")
        },
        options: vec![
            SeatOption {
                option_id: "allow".into(),
                label: "allow".into(),
                meaning: Some("允许这次工具调用".into()),
                native_value: Some("allow".into()),
            },
            SeatOption {
                option_id: "deny".into(),
                label: "deny".into(),
                meaning: Some("拒绝这次工具调用".into()),
                native_value: Some("deny".into()),
            },
        ],
        selected_option: None,
        confidence: Confidence::high("Claude Code 控制协议的 canUseTool 回调"),
        unknown: vec![
            "canUseTool 不是统一审计入口：更早被规则批准的调用不会到达这里".to_string(),
        ],
        observed_screen_revision: 0,
        expires_at_ms: None,
    })
}

#[async_trait]
impl SeatAdapter for ClaudeSdkSeat {
    fn adapter_id(&self) -> SeatAdapterId {
        SeatAdapterId::ClaudeSdk
    }

    fn adapter_version(&self) -> &str {
        CLAUDE_ADAPTER_VERSION
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
                "问题来自 Claude Code 的 canUseTool 控制回调".into(),
                "canUseTool 不是统一审计入口，任务与效果账本由本产品自己记录".into(),
                "内存中的回调 Promise 不是跨进程问题记录，回答前先持久化".into(),
            ],
        }
    }

    async fn discover(&self) -> Result<Vec<SeatHandle>> {
        let state = self.inner.state.lock().await;
        Ok(vec![SeatHandle {
            location_id: state
                .session_id
                .clone()
                .unwrap_or_else(|| self.inner.command.join(" ")),
            adapter: SeatAdapterId::ClaudeSdk,
            adapter_version: CLAUDE_ADAPTER_VERSION.into(),
            host: "local".into(),
            cwd: None,
            remote_session_id: state.session_id.clone(),
            current_command: Some(self.inner.command.join(" ")),
            process_identity: Some(ProcessIdentity {
                pid: None,
                start_token: None,
                executable: Some(self.inner.command.join(" ")),
            }),
        }])
    }

    async fn transport_live(&self, _seat: &SeatHandle) -> Result<bool> {
        Ok(self.inner.child.lock().await.try_wait()?.is_none())
    }

    async fn observe(&self, _seat: &SeatHandle) -> Result<SeatObservation> {
        let state = self.inner.state.lock().await;
        let questions: Vec<SeatQuestionCandidate> = state
            .questions
            .values()
            .filter(|question| {
                matches!(
                    state.question_states.get(&question.question_id),
                    Some(ClaudeQuestionState::Open)
                )
            })
            .cloned()
            .collect();
        let mut unknown = Vec::new();
        if !state.initialized {
            unknown.push("尚未收到 Claude Code 的 system/init，这个 seat 未初始化".into());
        }
        if !state.stderr_tail.is_empty() {
            unknown.push(format!("Claude stderr 末尾：{}", state.stderr_tail.join(" | ")));
        }
        Ok(SeatObservation {
            seat: SeatHandle {
                location_id: state
                    .session_id
                    .clone()
                    .unwrap_or_else(|| self.inner.command.join(" ")),
                adapter: SeatAdapterId::ClaudeSdk,
                adapter_version: CLAUDE_ADAPTER_VERSION.into(),
                host: "local".into(),
                cwd: None,
                remote_session_id: state.session_id.clone(),
                current_command: Some(self.inner.command.join(" ")),
                process_identity: None,
            },
            observation_revision: state.questions.len() as u64,
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
        let request_id = question
            .native_request_id
            .clone()
            .context("该问题没有 Claude 的原生请求 id，不能通过控制协议回答")?;
        let frame = build_control_response(&request_id, question, answer)?;
        Ok(Some(
            SeatEffectPlan::new(
                AnswerTransport::NativeRpc,
                serde_json::to_string(&frame)?,
                format!("control_response for {request_id}"),
            )
            .with_question_key(question.question_id.clone()),
        ))
    }

    async fn preflight(
        &self,
        _seat: &SeatHandle,
        question: &SeatQuestionCandidate,
        _plan: &SeatEffectPlan,
    ) -> Result<SeatPreflight> {
        // The control protocol keeps the request lifecycle: the request must
        // still be waiting for this product's response.
        let state = self.inner.state.lock().await;
        match state.question_states.get(&question.question_id) {
            Some(ClaudeQuestionState::Open) => Ok(SeatPreflight::Ready {
                detail: "Claude 的 canUseTool 回调仍在等待本产品的响应".into(),
            }),
            Some(ClaudeQuestionState::Decided { behavior }) => Ok(SeatPreflight::Refused {
                kind: PreflightRefusalKind::QuestionGone,
                reason: format!("该问题已经决定过：{behavior}"),
            }),
            None => Ok(SeatPreflight::Refused {
                kind: PreflightRefusalKind::QuestionGone,
                reason: "没有这个 Claude 问题记录".into(),
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
        let behavior = frame
            .get("response")
            .and_then(|response| response.get("response"))
            .and_then(|response| response.get("behavior"))
            .and_then(Value::as_str)
            .unwrap_or("allow")
            .to_string();
        let mut state = self.inner.state.lock().await;
        if let Some(question_id) = plan.question_key.clone() {
            state
                .question_states
                .insert(question_id, ClaudeQuestionState::Decided { behavior });
        }
        Ok(SeatResponse {
            adapter: SeatAdapterId::ClaudeSdk,
            transport: AnswerTransport::NativeRpc,
            confirmation: AnswerConfirmation::Unconfirmed,
            detail: Some(format!(
                "control_response 已写入托管 Claude Code 的 stdin（writer epoch {}）",
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
        let frame = json!({
            "type": "user",
            "session_id": "",
            "message": { "role": "user", "content": text },
            "parent_tool_use_id": Value::Null
        });
        self.send_frame(&frame).await?;
        Ok(SeatResponse {
            adapter: SeatAdapterId::ClaudeSdk,
            transport: AnswerTransport::NativeRpc,
            confirmation: AnswerConfirmation::Unconfirmed,
            detail: Some(format!(
                "user 消息已写入（writer epoch {}）；回合完成以 result 帧为准",
                fence.lease_epoch
            )),
        })
    }

    async fn interrupt(&self, _seat: &SeatHandle, fence: &SeatWriteFence) -> Result<SeatResponse> {
        let request_id = self.next_request_id().await;
        let frame = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "interrupt" }
        });
        self.send_frame(&frame).await?;
        Ok(SeatResponse {
            adapter: SeatAdapterId::ClaudeSdk,
            transport: AnswerTransport::NativeRpc,
            confirmation: AnswerConfirmation::Unconfirmed,
            detail: Some(format!(
                "interrupt 控制请求已写入（writer epoch {}）",
                fence.lease_epoch
            )),
        })
    }
}

/// Build the exact `control_response` frame for a decision.
fn build_control_response(
    request_id: &str,
    question: &SeatQuestionCandidate,
    answer: &SeatAnswer,
) -> Result<Value> {
    // The decision surface decides the frame shape: a tool permission is
    // answered with `behavior`, an AskUserQuestion card with the original
    // questions array plus its answers mapping.
    let is_ask_user_question = question.question_kind != QuestionKind::ToolPermission
        || question.title.as_deref() == Some("AskUserQuestion");
    let response = if !is_ask_user_question {
        if answer.allow == Some(false) {
            json!({
                "behavior": "deny",
                "message": answer.reason
            })
        } else {
            json!({ "behavior": "allow", "updatedInput": Value::Null })
        }
    } else {
        // AskUserQuestion: the protocol requires the original questions array
        // alongside the answers mapping. The original array is echoed when
        // the question carries it; only a question without it is rebuilt.
        let questions = question
            .native_payload
            .as_ref()
            .and_then(|payload| payload.get("questions").cloned())
            .unwrap_or_else(|| {
                serde_json::json!([{
            "question": question.text,
            "header": "",
            "options": question.options.iter().map(|option| {
                serde_json::json!({"label": option.label, "description": option.meaning.as_deref().unwrap_or_default()})
            }).collect::<Vec<_>>(),
            "multiSelect": false
                }])
            });
        let answers = match (&answer.option_id, &answer.text) {
            (Some(option_id), _) => {
                let option = question
                    .options
                    .iter()
                    .find(|option| &option.option_id == option_id)
                    .context("选择的选项不属于该问题")?;
                // The answers mapping is keyed by the *sub-question* the
               // chosen option belongs to, not by the joined card text.
                let key = sub_question_text(question, option_id)
                   .unwrap_or_else(|| question.text.clone());
                serde_json::json!({ key: option.label })
            }
            (None, Some(text)) => {
                let key = question
                    .options
                    .first()
                    .and_then(|option| sub_question_text(question, &option.option_id))
                    .unwrap_or_else(|| question.text.clone());
                serde_json::json!({ key: text })
            }
            (None, None) => anyhow::bail!("answer contains neither a selected option nor text"),
        };
        json!({
            "behavior": "allow",
            "updatedInput": { "questions": questions, "answers": answers }
        })
    };
    Ok(json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response
        }
    }))
}

/// The text of the sub-question an option belongs to.
///
/// The options of an AskUserQuestion card are numbered `opt-<question>-<option>`;
/// the protocol's answers mapping is keyed by the sub-question's own text,
/// not by the joined card text the model read.
fn sub_question_text(question: &SeatQuestionCandidate, option_id: &str) -> Option<String> {
    let index = option_id
        .strip_prefix("opt-")?
        .split('-')
        .next()?
        .parse::<usize>()
        .ok()?
        .checked_sub(1)?;
    let questions = question
        .native_payload
        .as_ref()?
        .get("questions")?
        .as_array()?;
    let entry = questions.get(index)?;
    let text = entry.get("question").and_then(Value::as_str).unwrap_or("");
    let header = entry.get("header").and_then(Value::as_str).unwrap_or("");
    Some(if header.is_empty() {
        text.to_string()
    } else {
        format!("{header}: {text}")
    })
}

impl ClaudeSdkSeat {
    /// Close this product's side of the stdio and terminate the managed
    /// process. The seat owns the `Child` (spawned with `kill_on_drop`), so
    /// the process also dies if the adapter is dropped without `close`.
    pub async fn close(&self) {
        {
            let mut guard = self.inner.stdin.lock().await;
            *guard = None;
        }
        // Reap the managed process. It was spawned with `kill_on_drop`, so
        // this is belt-and-suspenders; an explicit kill makes teardown
        // deterministic rather than relying on `Inner` drop order.
        let mut child = self.inner.child.lock().await;
        let _ = child.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_permission_becomes_a_permission_question() {
        let frame = json!({
            "type": "control_request",
            "request_id": "req_1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Write",
                "input": { "file_path": "/tmp/x" },
                "permission_suggestions": []
            }
        });
        let question = question_from_control_request(&frame).expect("question");
        assert_eq!(question.source_kind, QuestionSourceKind::SdkCallback);
        assert_eq!(question.question_kind, QuestionKind::ToolPermission);
        assert_eq!(question.native_request_id.as_deref(), Some("req_1"));
        assert_eq!(question.options.len(), 2);
        assert!(question.unknown.iter().any(|entry| entry.contains("审计入口")));
    }

    #[test]
    fn an_ask_user_question_becomes_an_information_question() {
        let frame = json!({
            "type": "control_request",
            "request_id": "req_2",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "AskUserQuestion",
                "input": {
                    "questions": [{
                        "question": "Which database?",
                        "header": "Database",
                        "options": [
                            {"label": "sqlite", "description": "file"},
                            {"label": "postgres", "description": "server"}
                        ],
                        "multiSelect": false
                    }]
                }
            }
        });
        let question = question_from_control_request(&frame).expect("question");
        assert_eq!(question.question_kind, QuestionKind::Information);
        assert_eq!(question.options.len(), 2);
        assert_eq!(question.options[0].label, "sqlite");
    }

    #[test]
    fn an_allow_response_carries_the_questions_array() {
        let question = SeatQuestionCandidate {
            question_id: "q".into(),
            source_kind: QuestionSourceKind::SdkCallback,
            question_kind: QuestionKind::Information,
            native_request_id: Some("req_3".into()),
            native_tool_use_id: None,
            native_payload: None,
            title: None,
            text: "Pick one".into(),
            options: vec![SeatOption {
                option_id: "opt-1-1".into(),
                label: "alpha".into(),
                meaning: None,
                native_value: Some("alpha".into()),
            }],
            selected_option: None,
            confidence: Confidence::high("test"),
            unknown: vec![],
            observed_screen_revision: 0,
            expires_at_ms: None,
        };
        let answer = SeatAnswer {
            option_id: Some("opt-1-1".into()),
            text: None,
            allow: None,
            reason: "fixture".into(),
        };
        let frame = build_control_response("req_3", &question, &answer).unwrap();
        let response = &frame["response"]["response"];
        assert_eq!(response["behavior"], "allow");
        assert_eq!(response["updatedInput"]["answers"]["Pick one"], "alpha");
        assert_eq!(response["updatedInput"]["questions"][0]["options"][0]["label"], "alpha");
    }

    #[test]
    fn the_control_protocol_flag_is_added_when_missing() {
        let mut command = vec!["claude".to_string(), "-p".to_string()];
        assert!(!command.iter().any(|arg| arg.contains("--permission-prompt-tool-name")));
        // Mirrors the spawn() guard: the flag is required or the CLI
        // auto-denies every permission request.
        command.push("--permission-prompt-tool-name".into());
        command.push("stdio".into());
        assert!(command.iter().any(|arg| arg == "--permission-prompt-tool-name"));
    }
}
