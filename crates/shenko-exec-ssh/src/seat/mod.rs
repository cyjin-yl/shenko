//! Seat adapters: one contract over three very different remote agents.
//!
//! SPEC §17.1 requires a `RemoteAgentProvider` that speaks to an already
//! running Claude Code, OMP or tmux pane. SPEC §17.5 and §35.5 name three
//! concrete paths, and this module is the seam between them:
//!
//! | path | adapter | how a question arrives | how an answer is delivered |
//! | --- | --- | --- | --- |
//! | existing tmux foreground TUI | [`tmux`](crate::seat::tmux) | terminal inference from the styled screen | verified `send-keys` into the pane |
//! | managed OMP | [`omp_rpc`](crate::seat::omp_rpc) | the OMP JSONL RPC `extension_ui_request` frame | the `extension_ui_response` frame |
//! | managed Claude Code | [`claude_sdk`](crate::seat::claude_sdk) | the Claude control protocol `control_request` frame | the `control_response` frame |
//! | remote SSH PTY | [`ssh_pty`](crate::seat::ssh_pty) | terminal inference from the live PTY screen | typed bytes over the PTY channel |
//!
//! Three rules hold for every adapter:
//!
//! * **A structured event beats a screen.** When the remote agent offers a
//!   real question event, that event is the question. Terminal inference is
//!   the compatibility path and always says so.
//! * **Unknown stays unknown.** Terminal inference reports its confidence
//!   and names what it could not see; it never guesses the selected option
//!   from the option order.
//! * **A written answer is not an applied answer.** Writing to a pane or a
//!   stream proves transport, not that the remote agent accepted it. The
//!   adapter reports which of the two it observed.

use crate::terminal::ScreenGrid;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_types::ids::{EntityId, RecordRevision, SeatIncarnation, TaskRevision};
use shenko_types::Digest;
use std::hash::Hash;

pub mod claude_sdk;
pub mod omp_rpc;
pub mod runtime;
pub mod ssh_pty;
pub mod tmux;

/// Which concrete adapter produced a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatAdapterId {
    /// An existing tmux pane running a foreground TUI.
    Tmux,
    /// A managed OMP process speaking its JSONL RPC protocol.
    OmpRpc,
    /// A managed Claude Code process speaking the control protocol.
    ClaudeSdk,
    /// A managed remote agent over a native SSH PTY channel.
    SshPty,
}

impl SeatAdapterId {
    /// Stable identifier used in durable records and receipts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
            Self::OmpRpc => "omp_rpc",
            Self::ClaudeSdk => "claude_sdk",
            Self::SshPty => "ssh_pty",
        }
    }

    /// Parse the stable identifier.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "tmux" => Some(Self::Tmux),
            "omp_rpc" => Some(Self::OmpRpc),
            "claude_sdk" => Some(Self::ClaudeSdk),
            "ssh_pty" => Some(Self::SshPty),
            _ => None,
        }
    }
}

/// Where a question came from. This is the distinction SPEC §41.11 draws
/// between a native request and a screen we inferred a question from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionSourceKind {
    /// The remote agent sent a structured question event.
    NativeRpc,
    /// An SDK callback raised the question.
    SdkCallback,
    /// The question was inferred from the terminal screen.
    TerminalInference,
}

impl QuestionSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NativeRpc => "native_rpc",
            Self::SdkCallback => "sdk_callback",
            Self::TerminalInference => "terminal_inference",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "native_rpc" => Some(Self::NativeRpc),
            "sdk_callback" => Some(Self::SdkCallback),
            "terminal_inference" => Some(Self::TerminalInference),
            _ => None,
        }
    }
}

/// What the remote agent is actually asking for. SPEC §17.2 requires these
/// to stay distinguishable: a planning choice, a request for information, a
/// tool permission and a genuinely new authority are not the same decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    Planning,
    Information,
    ToolPermission,
    NewAuthority,
}

impl QuestionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::Information => "information",
            Self::ToolPermission => "tool_permission",
            Self::NewAuthority => "new_authority",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "planning" => Some(Self::Planning),
            "information" => Some(Self::Information),
            "tool_permission" => Some(Self::ToolPermission),
            "new_authority" => Some(Self::NewAuthority),
            _ => None,
        }
    }
}

/// How much the observation supports the claim it makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceLevel {
    /// A structured event carried the question verbatim.
    High,
    /// The screen showed the question and its options, but some state was
    /// not visible.
    Medium,
    /// Only a prompt-shaped fragment was visible.
    Low,
}

impl ConfidenceLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            _ => None,
        }
    }
}

/// Confidence with the reason it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confidence {
    pub level: ConfidenceLevel,
    /// Why the level is what it is — never a bare number.
    pub basis: String,
}

impl Confidence {
    pub fn high(basis: impl Into<String>) -> Self {
        Self {
            level: ConfidenceLevel::High,
            basis: basis.into(),
        }
    }

    pub fn medium(basis: impl Into<String>) -> Self {
        Self {
            level: ConfidenceLevel::Medium,
            basis: basis.into(),
        }
    }

    pub fn low(basis: impl Into<String>) -> Self {
        Self {
            level: ConfidenceLevel::Low,
            basis: basis.into(),
        }
    }
}

/// One selectable option of a question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatOption {
    /// Stable within one question.
    pub option_id: String,
    /// What the remote agent calls this option.
    pub label: String,
    /// What the option means, when the remote agent said so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meaning: Option<String>,
    /// The exact value to send back for this option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_value: Option<String>,
}

/// A question the adapter believes is waiting for an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatQuestionCandidate {
    /// Stable identity of the question. A native request keeps the remote
    /// agent's own id; an inferred question is identified by its semantic
    /// content, never by a hash of the whole screen.
    pub question_id: String,
    pub source_kind: QuestionSourceKind,
    pub question_kind: QuestionKind,
    /// The remote agent's own request id, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_request_id: Option<String>,
    /// The remote agent's tool-use id, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_tool_use_id: Option<String>,
    /// The remote agent's own request payload, kept verbatim.
    ///
    /// SPEC §41.11 requires the original action and parameters to survive:
    /// a native answer has to echo what the remote side asked for, not a
    /// reconstruction of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_payload: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub text: String,
    #[serde(default)]
    pub options: Vec<SeatOption>,
    /// The option the remote agent had selected — only set when the
    /// observation actually showed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_option: Option<String>,
    pub confidence: Confidence,
    /// What the observation could not establish. Never empty for an
    /// inferred question.
    #[serde(default)]
    pub unknown: Vec<String>,
    /// The screen revision this question was read at.
    pub observed_screen_revision: u64,
    /// When the remote agent's own deadline is, if it published one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

/// The answer a decider chose for a question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatAnswer {
    /// The chosen option's id, when the question offered options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
    /// Free text answer, when the question accepted text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Allow/deny for a tool permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<bool>,
    /// Why this answer was chosen. Part of the durable decision.
    pub reason: String,
}

/// How the answer reached the remote agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerTransport {
    /// A structured native response frame.
    NativeRpc,
    /// Keystrokes written into a terminal.
    TerminalInput,
    /// Nothing was written: the decision was refused before any effect.
    None,
}

impl AnswerTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NativeRpc => "native_rpc",
            Self::TerminalInput => "terminal_input",
            Self::None => "none",
        }
    }
}

/// What the adapter actually observed after writing an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnswerConfirmation {
    /// The remote agent acknowledged the answer.
    Confirmed,
    /// The bytes were written; no acknowledgement was observed. The answer
    /// may or may not have been applied.
    Unconfirmed,
    /// The transport outcome itself is unknown: the write may have reached
    /// the remote side, or may not have. The answer is never replayed.
    Unknown { reason: String },
    /// The remote agent explicitly refused the answer.
    Rejected { reason: String },
    /// Nothing was sent (the answer was refused before the write).
    NotSent { reason: String },
}

/// The outcome of one adapter write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatResponse {
    pub adapter: SeatAdapterId,
    pub transport: AnswerTransport,
    pub confirmation: AnswerConfirmation,
    /// Adapter-specific detail about what happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Identity of the remote position a seat occupies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatHandle {
    /// Adapter-local identity: a tmux pane id, an OMP session id, a Claude
    /// session id. This is a *location*, never the durable seat identity.
    pub location_id: String,
    pub adapter: SeatAdapterId,
    pub adapter_version: String,
    /// "local" for this host, otherwise the adapter's remote target label.
    pub host: String,
    /// The working directory the remote agent was started in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The remote agent's own session identity, when it publishes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_session_id: Option<String>,
    /// The foreground program observed for this seat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_command: Option<String>,
    /// The process identity observed for this seat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_identity: Option<ProcessIdentity>,
}

/// Identity of the process behind a seat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// A token that changes when the process is replaced behind the same
    /// numeric pid. It is an observation, never an authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
}

/// What one adapter can actually do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatCapability {
    /// The remote agent sends structured question events.
    pub structured_questions: bool,
    /// Answers are delivered as a structured native response.
    pub structured_answers: bool,
    /// The screen can be observed with its attributes.
    pub screen_observation: bool,
    /// Free text can be steered into a running turn.
    pub steer: bool,
    /// The running turn can be interrupted.
    pub interrupt: bool,
    /// The remote session can be resumed.
    pub resume: bool,
    /// Anything the caller must know that the flags do not say.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// One observation of a seat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatObservation {
    pub seat: SeatHandle,
    /// Monotonic per seat; moves on any screen change.
    pub observation_revision: u64,
    pub observed_at: chrono::DateTime<chrono::Utc>,
    /// The styled screen, when the adapter can produce one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<ScreenGrid>,
    /// The screen as plain text, for the model's context.
    pub screen_text: String,
    /// The questions the adapter believes are open.
    #[serde(default)]
    pub questions: Vec<SeatQuestionCandidate>,
    /// What this observation could not establish.
    #[serde(default)]
    pub unknown: Vec<String>,
    /// Raw evidence bytes, bounded by the adapter.
    #[serde(default)]
    pub raw_evidence: Vec<u8>,
}

/// The writer authority a write travels with — SPEC §17.1, §35.5, §41.11.
///
/// The fence is not a local pre-check: it is the authority the adapter
/// carries into the transport, so the write stays bound to the exact
/// question, seat incarnation, task revision and writer epoch it was
/// decided against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatWriteFence {
    /// The lease that carries the write authority.
    pub lease_id: EntityId,
    /// The writer epoch of that lease. A newer writer displaces it.
    pub lease_epoch: u64,
    /// The exact input capability the holder verified for this seat.
    pub fence_capability: String,
    /// The durable seat record the write targets.
    pub seat_id: EntityId,
    /// The seat incarnation the writer verified.
    pub seat_incarnation: SeatIncarnation,
    /// The owning task.
    pub task_id: EntityId,
    /// The task revision the writer verified.
    pub task_revision: TaskRevision,
    /// The durable question record the write answers.
    pub question_id: EntityId,
    /// The semantic question revision the decision was made against.
    pub question_revision: u64,
    /// The row revision of the question record the decision was made against.
    pub question_record_revision: RecordRevision,
}

/// The exact bytes one answer will write — SPEC §41.11 `exact_effect_input_digest`.
///
/// The plan is produced *before* the effect is armed, so the durable
/// decision can name the exact input that was intended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatEffectPlan {
    /// How the answer reaches the remote agent.
    pub transport: AnswerTransport,
    /// The exact bytes to write. For terminal input this is the literal
    /// text; for a native protocol it is the exact frame.
    pub payload: String,
    /// Digest of `payload`.
    pub digest: String,
    /// What the effect does, for the durable record.
    pub summary: String,
    /// The adapter-local key of the question this plan answers, so the
    /// adapter can move its own protocol state when the effect is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_key: Option<String>,
}

impl SeatEffectPlan {
    /// Build a plan and its digest from the exact bytes.
    pub fn new(
        transport: AnswerTransport,
        payload: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        let payload = payload.into();
        Self {
            transport,
            digest: Digest::blake3(payload.as_bytes()).0,
            payload,
            summary: summary.into(),
            question_key: None,
        }
    }

    /// Bind the adapter-local question key to this plan.
    pub fn with_question_key(mut self, key: impl Into<String>) -> Self {
        self.question_key = Some(key.into());
        self
    }
}

/// The result of re-checking the live state right before a write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SeatPreflight {
    /// The live state still matches the observation: the write may proceed.
    Ready { detail: String },
    /// The live state no longer matches. Nothing may be written, and the
    /// caller must re-observe instead of sending the old answer.
    Refused {
        #[serde(rename = "refusal_kind")]
        kind: PreflightRefusalKind,
        reason: String,
    },
}

/// Why a preflight refused the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightRefusalKind {
    /// The question is no longer live on the remote side: it was resolved
    /// by the remote, answered by somebody else, expired or withdrawn.
    QuestionGone,
    /// The seat is still there, but the live state no longer matches the
    /// observation (another program, a reused pane, a different session).
    StateChanged,
}

/// One seat adapter. The trait is the plugin surface; the three concrete
/// adapters implement it against their real protocols.
#[async_trait]
pub trait SeatAdapter: Send + Sync + std::any::Any {
    fn adapter_id(&self) -> SeatAdapterId;
    /// The adapter implementation version, bound into durable records.
    fn adapter_version(&self) -> &str;
    fn capability(&self) -> SeatCapability;
    /// Find the seats this adapter can currently see.
    async fn discover(&self) -> Result<Vec<SeatHandle>>;
    /// Observe whether the owned process/channel/pane still exists. This is
    /// a transport fact, not protocol readiness or completion of remote work.
    async fn transport_live(&self, seat: &SeatHandle) -> Result<bool>;
    /// Read the current state of one seat.
    async fn observe(&self, seat: &SeatHandle) -> Result<SeatObservation>;
    /// Map a decision onto the exact bytes that will be written.
    ///
    /// `Ok(None)` means the mapping is not justified by what was observed:
    /// nothing may be written, and the caller records the refusal instead of
    /// guessing. The plan is produced before the effect is armed, so the
    /// durable decision can name the exact input.
    async fn plan_effect(
        &self,
        seat: &SeatHandle,
        question: &SeatQuestionCandidate,
        answer: &SeatAnswer,
    ) -> Result<Option<SeatEffectPlan>>;
    /// Re-check the live state immediately before the write.
    ///
    /// The adapter reads the *current* protocol or screen state — never the
    /// observation the decision was made from — and refuses when the
    /// question is gone, was answered by somebody else, or the program
    /// behind the seat changed.
    async fn preflight(
        &self,
        seat: &SeatHandle,
        question: &SeatQuestionCandidate,
        plan: &SeatEffectPlan,
    ) -> Result<SeatPreflight>;
    /// Write the planned effect. The fence carries the writer authority, so
    /// the write is bound to the epoch it was decided under.
    async fn execute_effect(
        &self,
        seat: &SeatHandle,
        plan: &SeatEffectPlan,
        fence: &SeatWriteFence,
    ) -> Result<SeatResponse>;
    /// Send free text to the seat.
    async fn submit(
        &self,
        seat: &SeatHandle,
        text: &str,
        fence: &SeatWriteFence,
    ) -> Result<SeatResponse>;
    /// Interrupt the running turn.
    async fn interrupt(
        &self,
        seat: &SeatHandle,
        fence: &SeatWriteFence,
    ) -> Result<SeatResponse>;

}

/// Stable identity of an inferred question.
///
/// The identity is the *semantic* content of the question — its text and
/// its options — and not the screen it happened to be read at. An unrelated
/// log line therefore does not create a new question, while a changed
/// option set does.
pub fn inferred_question_id(
    seat: &SeatHandle,
    text: &str,
    options: &[SeatOption],
) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    seat.adapter.as_str().hash(&mut hasher);
    seat.host.hash(&mut hasher);
    seat.location_id.hash(&mut hasher);
    text.trim().hash(&mut hasher);
    for option in options {
        option.option_id.hash(&mut hasher);
        option.label.hash(&mut hasher);
    }
    format!(
        "terminal:{:016x}",
        std::hash::Hasher::finish(&hasher)
    )
}

/// The semantic fingerprint of a question. Two observations with the same
/// fingerprint describe the same question; a different fingerprint is a
/// different question revision.
pub fn question_fingerprint(question: &SeatQuestionCandidate) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    question.question_kind_str().hash(&mut hasher);
    question.text.trim().hash(&mut hasher);
    question.title.hash(&mut hasher);
    for option in &question.options {
        option.option_id.hash(&mut hasher);
        option.label.hash(&mut hasher);
        option.meaning.hash(&mut hasher);
        option.native_value.hash(&mut hasher);
    }
    question.native_request_id.hash(&mut hasher);
    question.native_tool_use_id.hash(&mut hasher);
    format!("{:016x}", std::hash::Hasher::finish(&hasher))
}

impl SeatQuestionCandidate {
    fn question_kind_str(&self) -> &'static str {
        self.question_kind.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> SeatHandle {
        SeatHandle {
            location_id: "%3".into(),
            adapter: SeatAdapterId::Tmux,
            adapter_version: "tmux-capture-v1".into(),
            host: "local".into(),
            cwd: Some("/project".into()),
            remote_session_id: None,
            current_command: Some("claude".into()),
            process_identity: None,
        }
    }

    fn option(id: &str) -> SeatOption {
        SeatOption {
            option_id: id.into(),
            label: id.into(),
            meaning: None,
            native_value: None,
        }
    }

    #[test]
    fn inferred_identity_is_stable_across_unrelated_screen_changes() {
        let first = inferred_question_id(&handle(), "Continue?", &[option("yes")]);
        let second = inferred_question_id(&handle(), "Continue?  ", &[option("yes")]);
        assert_eq!(first, second);
        let third = inferred_question_id(&handle(), "Continue?", &[option("yes"), option("no")]);
        assert_ne!(first, third);
    }

    #[test]
    fn fingerprint_changes_when_the_option_set_changes() {
        let mut question = SeatQuestionCandidate {
            question_id: "q".into(),
            source_kind: QuestionSourceKind::TerminalInference,
            question_kind: QuestionKind::Planning,
            native_request_id: None,
            native_tool_use_id: None,
            native_payload: None,
            title: None,
            text: "Continue?".into(),
            options: vec![option("yes")],
            selected_option: None,
            confidence: Confidence::medium("observed"),
            unknown: vec!["选中项未知".into()],
            observed_screen_revision: 1,
            expires_at_ms: None,
        };
        let first = question_fingerprint(&question);
        question.options.push(option("no"));
        assert_ne!(first, question_fingerprint(&question));
    }
}
