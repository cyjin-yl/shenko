//! Durable seat records and single-writer lease admission.
//!
//! A `TaskSeat` is a stable product identity for one remote-agent work
//! position; a tmux pane id is only its current *location*, never its durable
//! identity. When the same work moves to a different pane/host the old record
//! keeps its history and the new record names it via `supersedes`; when the
//! position is gone the record is retired, not rewritten in place.
//! At most one active record may claim a `(host, location key)` location —
//! a pane id for tmux seats, a durable session anchor for managed RPC seats;
//! the invariant is enforced inside the admitting transaction and backed by a
//! partial unique index.
//!
//! `check_seat_write` exposes durable lease/seat facts to trusted executors;
//! it neither arms an operation nor sends input. The current runtime does not
//! yet have a seat-child handler consuming this interface. A successful read
//! is not an atomic check-and-send fence, and native tmux/human input remains
//! outside this database's control.

use crate::Store;
use anyhow::{ensure, Result};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use shenko_types::{
    commit::CommitRef,
    error::{ErrorScope, ProtocolError, RetryStrategy, ShenkoError},
    ids::*,
    task::TaskRecord,
};

pub(crate) const SEAT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS seat_records(
    seat_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    status TEXT NOT NULL,
    supersedes TEXT,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_seat_task ON seat_records(task_id, updated_commit_seq);
CREATE UNIQUE INDEX IF NOT EXISTS idx_seat_successor ON seat_records(supersedes) WHERE supersedes IS NOT NULL;
-- A pane id or session anchor is a physical location, and one location hosts
-- at most one live work position (docs/specs/remote-seats.md#spec-17-1): SQLite enforces it on the
-- stored location facts, so no trusted caller can admit a second active seat
-- for the same location.
CREATE UNIQUE INDEX IF NOT EXISTS idx_seat_active_location ON seat_records(
    json_extract(data,'$.location.host'),
    COALESCE(json_extract(data,'$.location.pane_id'),
             json_extract(data,'$.location.remote_session_id'))
) WHERE status='active';
CREATE TABLE IF NOT EXISTS seat_leases(
    lease_id TEXT PRIMARY KEY,
    seat_id TEXT NOT NULL,
    holder_task_id TEXT NOT NULL,
    state TEXT NOT NULL,
    end_reason TEXT,
    superseded_by TEXT,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL
);
-- The durable uniqueness rule behind docs/specs/remote-seats.md#spec-17-1: at most one lease may be current
-- for one seat at a time, enforced by SQLite rather than by callers.
CREATE UNIQUE INDEX IF NOT EXISTS idx_seat_current_lease
    ON seat_leases(seat_id) WHERE state='current';
CREATE INDEX IF NOT EXISTS idx_lease_holder ON seat_leases(seat_id, holder_task_id);
-- Durable seat questions (docs/specs/remote-seats.md#spec-41-11): the in-memory callback Promise is
-- not a question record. Every question is persisted here before it can be
-- answered, with the lease that observed it.
CREATE TABLE IF NOT EXISTS seat_questions(
    question_id TEXT PRIMARY KEY,
    seat_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    state TEXT NOT NULL,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_seat_questions_seat
    ON seat_questions(seat_id, updated_commit_seq);
CREATE INDEX IF NOT EXISTS idx_seat_questions_task
    ON seat_questions(task_id, updated_commit_seq);
-- The durable answer decision, one per question (docs/specs/remote-seats.md#spec-41-11). The unique
-- question binding makes a second decision for the same question a typed
-- conflict inside the admitting transaction, never a silent overwrite.
-- The durable answer decision (docs/specs/remote-seats.md#spec-41-11). A question carries at most
-- one *live* decision: the admitting transaction refuses a second decision
-- unless the previous one ended `not_sent` (the remote provably never
-- applied it), so the uniqueness rule lives in the transaction, not in a
-- table constraint that would forbid a justified retry.
CREATE TABLE IF NOT EXISTS seat_answer_decisions(
    decision_id TEXT PRIMARY KEY,
    question_id TEXT NOT NULL,
    seat_id TEXT NOT NULL,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_seat_decisions_question
    ON seat_answer_decisions(question_id, updated_commit_seq);
"#;

/// Exact Linux process/server observation for a local tmux pane. This is
/// evidence, not ownership or a continuous target-side write fence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalTmuxIdentity {
    pub boot_id: String,
    pub pane_pid: u32,
    pub pane_start_ticks: u64,
    pub server_pid: u32,
    pub server_start_ticks: u64,
    pub socket_path: String,
}

impl LocalTmuxIdentity {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.boot_id.trim().is_empty(), "local tmux boot identity unavailable");
        ensure!(self.pane_pid > 0 && self.pane_start_ticks > 0,
            "local tmux pane process identity unavailable");
        ensure!(self.server_pid > 0 && self.server_start_ticks > 0,
            "local tmux server process identity unavailable");
        ensure!(!self.socket_path.is_empty(), "local tmux socket identity unavailable");
        Ok(())
    }
}

/// Current physical reach of a seat — locator fields.
/// Kept as one sealed shape so future adapters add fields without changing
/// the records table. A pane id, a remote session anchor or a host string
/// here is evidence, not identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SeatLocation {
    /// Existing tmux foreground TUI (first path).
    Tmux {
        /// Host reach: "local" or an SSH target label the adapter resolved.
        host: String,
        session: String,
        window: u32,
        pane: u32,
        /// tmux's own pane id (e.g. `%42`); stable across reorder on one server.
        pane_id: String,
        cwd: String,
        current_command: String,
        /// Absent on legacy records and remote tmux locators. Never infer
        /// this identity from today's occupant during restart recovery.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_identity: Option<LocalTmuxIdentity>,
    },
    /// Managed remote agent over the native SSH PTY transport.
    /// Identity uniqueness comes from the single-writer lease, not a pane.
    SshPty {
        host: String,
        port: u16,
        user: String,
        /// The exact PTY program this seat was opened with.
        program_b64: String,
    },
    /// Managed OMP agent speaking its JSONL RPC protocol.
    /// `remote_session_id` is the durable session anchor the seat was bound
    /// to — what the OMP adapter reports as its session identity (the exact
    /// spawn command line for an unmanaged anchor). One active seat may
    /// claim a `(host, remote_session_id)` pair, enforced like a pane.
    OmpRpc {
        host: String,
        /// The durable session anchor (`SeatHandle.remote_session_id`).
        remote_session_id: String,
    },
    /// Managed Claude Code agent speaking its control protocol.
    /// `remote_session_id` is the durable session anchor: the resume anchor
    /// the caller bound, or the session id the remote side published, or
    /// the exact spawn command line when the session never reported one.
    ClaudeSdk {
        host: String,
        /// The durable session anchor (`SeatHandle.remote_session_id`).
        remote_session_id: String,
    },
}

impl SeatLocation {
    fn validate_local_identity(&self) -> Result<()> {
        if let Self::Tmux { host, local_identity: Some(identity), .. } = self {
            ensure!(host == "local", "local tmux identity cannot describe an SSH target");
            identity.validate()?;
        }
        Ok(())
    }

    /// The `(host, location key)` this location claims for the
    /// one-active-seat-per-location invariant. tmux seats claim a pane,
    /// managed RPC seats claim their durable session anchor; SSH PTY seats
    /// claim no location — their single-writer rule is the lease itself.
    pub fn claim_key(&self) -> Option<(&str, &str)> {
        match self {
            Self::Tmux { host, pane_id, .. } => Some((host.as_str(), pane_id.as_str())),
            Self::OmpRpc { host, remote_session_id }
            | Self::ClaudeSdk { host, remote_session_id } => {
                Some((host.as_str(), remote_session_id.as_str()))
            }
            Self::SshPty { .. } => None,
        }
    }
}

/// Lifecycle of one seat record. `retired` records keep full history and are
/// still addressable through the supersedes chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatStatus {
    Active,
    Retired,
}

impl SeatStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Retired => "retired",
        }
    }
}

/// Durable seat record — TaskSeat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatRecord {
    pub schema_version: u32,
    pub seat_id: EntityId,
    /// Stable incarnation; bumped by `bump_seat_incarnation` whenever an
    /// observer confirms the occupant behind this record's stored location
    /// changed in place. Planned moves still travel via `supersedes` (a new
    /// record at incarnation 0); a bump is the in-place counterpart that
    /// makes every lease acquired at an older incarnation stale.
    pub incarnation: SeatIncarnation,
    /// The task this seat serves. Facts only; a seat does not own the task.
    pub task_id: EntityId,
    /// Adapter family and the version/level it actually negotiated.
    pub adapter: String,
    pub adapter_version: String,
    pub location: SeatLocation,
    /// Original operation of an observation-only, owner-selected attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_operation: Option<EntityId>,
    /// Seat this record succeeds; the predecessor keeps its own history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<EntityId>,
    pub status: SeatStatus,
    pub record_revision: RecordRevision,
    pub created_commit: CommitRef,
    pub updated_commit: CommitRef,
}

/// A bounded inventory page includes one extra row to reveal truncation.
pub const SEAT_FLEET_LIMIT: usize = 32;

/// What one seat adapter can actually do — the flags an adapter declares
/// for itself.
///
/// This is a read-only publication: the durable seat record does not attest
/// what an adapter can do, so these flags travel only with an attached
/// adapter's declaration. They do not attest transport liveness and grant no
/// authority; the writer lease does.
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

/// Credential-free durable facts about a task's seat. Stored seat and question
/// revisions do not attest transport liveness or an observation timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatFleetEntry {
    pub seat_id: EntityId,
    pub task_id: EntityId,
    pub adapter: String,
    pub adapter_version: String,
    pub location_summary: String,
    pub status: SeatStatus,
    pub open_question_count: u64,
    pub last_observed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The capability flags the attached adapter declared for this seat. Absent
    /// when no adapter is attached: the durable record does not attest what
    /// an adapter can do, so the store never invents these flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<SeatCapability>,
}

impl SeatLocation {
    fn fleet_summary(&self, seat_id: EntityId) -> String {
        match self {
            Self::Tmux { host, pane_id, .. } => format!("tmux pane {pane_id} on {host}"),
            Self::SshPty { host, port, .. } => format!("SSH PTY seat {seat_id} on {host}:{port}"),
            // A managed session anchor may be the original command line. It
            // must not copy command arguments or credentials into inventory.
            Self::OmpRpc { host, .. } => format!("OMP seat {seat_id} on {host}"),
            Self::ClaudeSdk { host, .. } => format!("Claude seat {seat_id} on {host}"),
        }
    }
}

/// Durable writer lease — 单写者.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatLease {
    /// 2 carries the writer epoch and verified fence capability; a stored
    /// v1 row (no epoch) is refused by `check_seat_write` as legacy.
    pub schema_version: u32,
    pub lease_id: EntityId,
    pub seat_id: EntityId,
    /// The task holding write authority; bound at admission, never inferred.
    pub holder_task_id: EntityId,
    /// Location incarnation the holder verified when it acquired the lease.
    pub acquired_at_incarnation: SeatIncarnation,
    /// v2: the holder's writer epoch. Must be > 0; a lease written without
    /// an epoch is the legacy v1 shape and is never admitted to writes.
    #[serde(default)]
    pub lease_epoch: u64,
    /// v2: the exact input capability the holder verified at acquisition
    /// (e.g. `tmux.send-keys:%42`). Empty means the holder verified no
    /// concrete capability — refused by `check_seat_write`.
    #[serde(default)]
    pub fence_capability: String,
    /// v1→v2 cutover marker: this lease was stored before the writer
    /// contract carried an epoch and a fence capability, so it was never a
    /// writer fence. The marker makes that fact explicit in the stored
    /// document instead of leaving it to be re-derived at every read.
    #[serde(default)]
    pub legacy_writer: bool,
    pub state: SeatLeaseState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_reason: Option<LeaseEndReason>,
    /// Lease that took over, when this one was superseded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<EntityId>,
    pub record_revision: RecordRevision,
    pub created_commit: CommitRef,
    pub updated_commit: CommitRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatLeaseState {
    Current,
    Ended,
}

/// Why a lease stopped being writable. `human_takeover` is recorded by the
/// observer that proved it; nothing here fabricates the observation itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseEndReason {
    /// A different writer acquired a newer lease on the same seat.
    Superseded,
    /// The seat moved; the record was superseded by a new location record.
    SeatSuperseded,
    /// The seat record was retired without a successor.
    SeatRetired,
    /// A human (or unmanaged actor) took the input path over.
    HumanTakeover,
    /// The holder released the seat normally.
    Released,
}

/// End reasons a caller may record explicitly through `end_seat_lease`.
/// The remaining `LeaseEndReason` values describe transitions this operation
/// does not perform — supersede, relocation and retirement are recorded only
/// by `acquire_seat_lease`, `relocate_seat` and `retire_seat`, which make the
/// corresponding seat/lease change in the same commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatLeaseEnd {
    /// The holder released the seat normally.
    Released,
    /// A human (or unmanaged actor) took the input path over, proven by a
    /// real observation.
    HumanTakeover,
}

impl SeatLeaseEnd {
    fn reason(self) -> LeaseEndReason {
        match self {
            Self::Released => LeaseEndReason::Released,
            Self::HumanTakeover => LeaseEndReason::HumanTakeover,
        }
    }
}

impl SeatLease {
    pub fn is_current(&self) -> bool {
        self.state == SeatLeaseState::Current
    }

    /// A lease is a real v2 writer fence only when it carries an epoch and
    /// the concrete capability its holder verified.
    pub fn is_writer_fence(&self) -> bool {
        self.schema_version >= 2
            && self.lease_epoch > 0
            && !self.fence_capability.is_empty()
            && !self.legacy_writer
    }
}

/// Admission input for a v2 writer lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeatLeaseRequest {
    pub lease_id: EntityId,
    pub seat_id: EntityId,
    pub holder_task_id: EntityId,
    /// The holder's writer epoch (> 0), strictly greater than every prior
    /// acquisition on this seat, including leases that have already ended.
    pub lease_epoch: u64,
    /// The exact input capability the holder verified for this seat right
    /// before acquiring (e.g. `tmux.send-keys:%42`, `ssh.pty:host:pane`).
    pub fence_capability: String,
}

/// Lifecycle of one durable seat question.
///
/// The states are the product's own durable facts about one question; they
/// are never inferred from a screen. `DecisionCommitted` and `Responding`
/// are the two durable markers that make an answer at-most-once: the first
/// is written *before* any input reaches the remote side, the second when
/// the exact effect was armed. `AnswerUnknown` is the honest end state of
/// an answer whose application was never confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatQuestionState {
    /// Recorded, still waiting for a decision.
    Open,
    /// A decision was committed for it; no input has been sent yet.
    DecisionCommitted,
    /// The exact effect was armed and may already have reached the remote.
    Responding,
    /// An answer was sent but its application was never confirmed.
    AnswerUnknown,
    /// This product's answer was confirmed by the remote side.
    Answered,
    /// The remote side's deadline passed without an answer.
    Expired,
    /// The remote side withdrew the question.
    Withdrawn,
    /// A newer semantic revision of the same question replaced it.
    Superseded,
    /// The remote side resolved it without this product's decision
    /// (timeout, remote choice, session end). Not an agent decision.
    ExternallyResolved,
    /// An answer arrived after the question was already externally resolved;
    /// recorded, never applied.
    LateAnswerIgnored,
}

impl SeatQuestionState {
    /// The stable identifier of this state, used in durable records.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::DecisionCommitted => "decision_committed",
            Self::Responding => "responding",
            Self::AnswerUnknown => "answer_unknown",
            Self::Answered => "answered",
            Self::Expired => "expired",
            Self::Withdrawn => "withdrawn",
            Self::Superseded => "superseded",
            Self::ExternallyResolved => "externally_resolved",
            Self::LateAnswerIgnored => "late_answer_ignored",
        }
    }

    /// Whether the question still waits for a decision.
    pub fn is_open(self) -> bool {
        self == Self::Open
    }

    /// Whether the question is closed for good: no further decision may be
    /// committed for it.
    pub fn is_closed(self) -> bool {
        matches!(
            self,
            Self::Answered
                | Self::Expired
                | Self::Withdrawn
                | Self::Superseded
                | Self::ExternallyResolved
                | Self::LateAnswerIgnored
        )
    }
}

/// One answer option as durably recorded — a stable shape independent of
/// any adapter crate so the store stays the authority below all adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatOptionFact {
    pub option_id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meaning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_value: Option<String>,
}

/// Confidence as durably recorded (0..=100 + human/adapter basis).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatConfidenceFact {
    /// `high` | `medium` | `low`, the stable `ConfidenceLevel` identifier.
    pub level: String,
    pub basis: String,
}

/// The durable question shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatQuestionFact {
    pub source_kind: String,
    pub question_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_tool_use_id: Option<String>,
    /// The remote agent's original request payload, kept verbatim: the
    /// answer echoes what the remote side asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_payload: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<SeatOptionFact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_option: Option<String>,
    pub confidence: SeatConfidenceFact,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown: Vec<String>,
    pub observed_screen_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// Semantic fingerprint of the question content; replay detection binds
    /// this, never a whole-screen hash.
    pub fingerprint: String,
}

/// The durable answer shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatAnswerFact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<bool>,
    pub reason: String,
}

/// Durable seat question record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatQuestionRecord {
    pub schema_version: u32,
    pub question_id: EntityId,
    pub seat_id: EntityId,
    pub task_id: EntityId,
    /// The seat's location incarnation when the question was observed. A
    /// write is refused when the seat moved underneath the answer.
    #[serde(default)]
    pub seat_incarnation: SeatIncarnation,
    /// The owning task's revision when the question was observed. The
    /// answer decision compares against the current value, so a task that
    /// moved on cannot be answered from an old screen.
    #[serde(default)]
    pub task_revision: TaskRevision,
    /// The lease whose holder observed the question; answer admission
    /// re-verifies this lease is still current.
    pub lease_id: EntityId,
    /// The writer epoch of that lease. A decision is bound to one epoch;
    /// a newer writer displaces it.
    #[serde(default)]
    pub lease_epoch: u64,
    /// Semantic revision of the question. It moves only when
    /// the question's meaning, options, action, deadline or bindings
    /// change — never for a spinner or an unrelated log line.
    #[serde(default)]
    pub question_revision: u64,
    pub question: SeatQuestionFact,
    pub state: SeatQuestionState,
    pub record_revision: RecordRevision,
    pub created_commit: CommitRef,
    pub updated_commit: CommitRef,
}

/// Lifecycle of one durable answer decision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatAnswerDecisionState {
    /// The decision is committed; nothing has been sent to the remote yet.
    #[default]
    Committed,
    /// The exact effect was armed: it may already have been sent.
    Armed,
    /// The remote confirmed the answer.
    Answered,
    /// The answer was sent but its application was never confirmed.
    AnswerUnknown,
    /// The answer was refused before the write; the remote provably did
    /// not apply it, so a new decision may be made.
    NotSent,
    /// The remote rejected the answer.
    Rejected,
}

impl SeatAnswerDecisionState {
    /// The stable identifier of this state, used in durable records.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Armed => "armed",
            Self::Answered => "answered",
            Self::AnswerUnknown => "answer_unknown",
            Self::NotSent => "not_sent",
            Self::Rejected => "rejected",
        }
    }

    /// Whether the decision reached a final state.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Answered | Self::AnswerUnknown | Self::NotSent | Self::Rejected
        )
    }
}

/// Durable answer decision — `AnswerDecision`.
///
/// The record is written *before* any input reaches the remote side
/// (`Committed`), armed with the exact effect digest when the write is
/// about to happen (`Armed`), and closed with the observed evidence
/// afterwards. Every field of the expectation is a compare-and-swap token:
/// the decision names the exact question, seat incarnation, task revision
/// and writer epoch it was made against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatAnswerDecisionRecord {
    pub schema_version: u32,
    pub decision_id: EntityId,
    pub question_id: EntityId,
    pub seat_id: EntityId,
    pub task_id: EntityId,
    pub lease_id: EntityId,
    /// The writer epoch this decision is bound to.
    #[serde(default)]
    pub lease_epoch: u64,
    /// The exact input capability the writer verified for this seat.
    #[serde(default)]
    pub fence_capability: String,
    /// Semantic question revision the decider read.
    #[serde(default)]
    pub expected_question_revision: u64,
    /// Row revision of the question record the decider read.
    #[serde(default)]
    pub expected_question_record_revision: RecordRevision,
    /// Seat incarnation the decider read.
    #[serde(default)]
    pub expected_seat_incarnation: SeatIncarnation,
    /// Row revision of the seat record the decider read.
    #[serde(default)]
    pub expected_seat_revision: RecordRevision,
    /// Task revision the decider read.
    #[serde(default)]
    pub expected_task_revision: TaskRevision,
    /// Who decided: the subject, or a child with an explicit decision scope.
    #[serde(default)]
    pub deciding_actor: String,
    /// The delegation that gave the decider its decision scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_grant_ref: Option<String>,
    /// The authority revision under which the decision was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority_revision: Option<u64>,
    pub answer: SeatAnswerFact,
    /// Digest of the exact wire/input bytes this decision will send. It is
    /// written before the effect is armed, so a replay can be compared
    /// against the bytes that were actually intended.
    #[serde(default)]
    pub exact_effect_input_digest: String,
    /// `native_rpc` or `terminal_input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// `confirmed` | `unconfirmed` | `rejected` | `not_sent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<String>,
    /// The evidence that closed this decision (native confirmation, tool
    /// successor, or the reason the effect is unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure_evidence: Option<String>,
    /// The trusted operation that carries the effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<EntityId>,
    #[serde(default)]
    pub state: SeatAnswerDecisionState,
    pub record_revision: RecordRevision,
    pub created_commit: CommitRef,
    pub updated_commit: CommitRef,
}

/// The compare-and-swap expectation of one answer decision.
/// Every field names a version the decider actually read; a mismatch is a
/// typed refusal and the answer is never applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeatDecisionExpectation {
    /// Semantic revision of the question.
    pub question_revision: u64,
    /// Row revision of the question record.
    pub question_record_revision: RecordRevision,
    /// Incarnation of the seat record.
    pub seat_incarnation: SeatIncarnation,
    /// Row revision of the seat record.
    pub seat_revision: RecordRevision,
    /// Revision of the owning task.
    pub task_revision: TaskRevision,
    /// Writer epoch the decider held.
    pub lease_epoch: u64,
}

/// The terminal outcome of one armed answer decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SeatAnswerOutcome {
    /// The remote confirmed the answer.
    Answered { evidence: String },
    /// The answer was sent but nothing confirmed it.
    AnswerUnknown { reason: String },
    /// The answer was refused before the write: the remote provably did
    /// not apply it, so a new decision may be made.
    NotSent { reason: String },
    /// The remote rejected the answer.
    Rejected { reason: String },
}

fn seat_error(code: &str, retry: RetryStrategy, details: serde_json::Value) -> ShenkoError {
    ShenkoError::Protocol(
        ProtocolError::new(code, ErrorScope::Seat, retry).with_details(details),
    )
}

fn read_seat(conn: &rusqlite::Connection, id: EntityId) -> Result<Option<SeatRecord>> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM seat_records WHERE seat_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
}

fn check_seat_task(seat: &SeatRecord, task_id: EntityId) -> Result<()> {
    ensure!(
        seat.task_id == task_id,
        seat_error(
            "seat_task_mismatch",
            RetryStrategy::NeverAutomatically,
            serde_json::json!({
                "seat_id": seat.seat_id.to_string(),
                "seat_task_id": seat.task_id.to_string(),
                "claimed_task_id": task_id.to_string(),
            }),
        )
    );
    Ok(())
}

fn check_seat_writable(seat: &SeatRecord) -> Result<()> {
    ensure!(seat.read_only_operation.is_none(), seat_error(
        "seat_read_only", RetryStrategy::NeverAutomatically,
        serde_json::json!({"seat_id": seat.seat_id, "operation_id": seat.read_only_operation}),
    ));
    Ok(())
}

/// Read one task row inside a caller's transaction. The seat fence binds a
/// task revision, so the revision has to be read from the same transaction
/// that admits the write — never from a separate lock acquisition.
fn read_task_row(conn: &rusqlite::Connection, id: EntityId) -> Result<Option<TaskRecord>> {
    let data: Option<String> = conn
        .query_row("SELECT data FROM tasks WHERE task_id=?1", [id.to_string()], |r| r.get(0))
        .optional()?;
    data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
}

fn read_lease(conn: &rusqlite::Connection, id: EntityId) -> Result<Option<SeatLease>> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM seat_leases WHERE lease_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
}

fn write_seat(tx: &Transaction<'_>, seat: &SeatRecord, seq: u64) -> Result<()> {
    tx.execute(
        "INSERT INTO seat_records(seat_id,task_id,status,supersedes,data,updated_commit_seq)
         VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(seat_id) DO UPDATE SET
             task_id=excluded.task_id, status=excluded.status, supersedes=excluded.supersedes,
             data=excluded.data, updated_commit_seq=excluded.updated_commit_seq",
        params![
            seat.seat_id.to_string(),
            seat.task_id.to_string(),
            seat.status.as_str(),
            seat.supersedes.map(|s| s.to_string()),
            serde_json::to_string(seat)?,
            seq as i64,
        ],
    )?;
    Ok(())
}

fn write_lease(tx: &Transaction<'_>, lease: &SeatLease, seq: u64) -> Result<()> {
    tx.execute(
        "INSERT INTO seat_leases(lease_id,seat_id,holder_task_id,state,end_reason,superseded_by,data,updated_commit_seq)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(lease_id) DO UPDATE SET
             state=excluded.state, end_reason=excluded.end_reason,
             superseded_by=excluded.superseded_by, data=excluded.data,
             updated_commit_seq=excluded.updated_commit_seq",
        params![
            lease.lease_id.to_string(),
            lease.seat_id.to_string(),
            lease.holder_task_id.to_string(),
            match lease.state {
                SeatLeaseState::Current => "current",
                SeatLeaseState::Ended => "ended",
            },
            lease
                .ended_reason
                .map(|r| serde_json::to_string(&r)).transpose()?,
            lease.superseded_by.map(|s| s.to_string()),
            serde_json::to_string(lease)?,
            seq as i64,
        ],
    )?;
    Ok(())
}

fn current_lease(conn: &rusqlite::Connection, seat_id: EntityId) -> Result<Option<SeatLease>> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM seat_leases WHERE seat_id=?1 AND state='current'",
            [seat_id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
}

fn highest_seat_lease_epoch(conn: &rusqlite::Connection, seat_id: EntityId) -> Result<u64> {
    #[derive(Deserialize)]
    struct Epoch {
        #[serde(default)]
        lease_epoch: u64,
    }

    // Stream the retained acquisition facts. SQLite's JSON numeric projection
    // rounds epochs above i64::MAX to floating point; deserialize the original
    // JSON integer so distinct u64 epochs remain distinct, without collecting
    // the seat's history or allocating its other fields.
    let mut query = conn.prepare("SELECT data FROM seat_leases WHERE seat_id=?1")?;
    let mut rows = query.query([seat_id.to_string()])?;
    let mut highest = 0;
    while let Some(row) = rows.next()? {
        let epoch: Epoch = serde_json::from_str(row.get_ref(0)?.as_str()?)?;
        highest = highest.max(epoch.lease_epoch);
    }
    Ok(highest)
}

/// The single active seat bound to `(host, location key)` — the invariant
/// behind `idx_seat_active_location`, evaluated inside the caller's
/// transaction so a competing registration can never slip between lookup and
/// insert. The location key is the pane id for tmux seats and the durable
/// session anchor (`remote_session_id`) for managed RPC seats; it follows
/// the same `COALESCE` expression the index applies.
fn active_seat_at_location(
    conn: &rusqlite::Connection,
    host: &str,
    location_key: &str,
) -> Result<Option<SeatRecord>> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM seat_records WHERE status='active'
             AND json_extract(data,'$.location.host')=?1
             AND COALESCE(json_extract(data,'$.location.pane_id'),
                          json_extract(data,'$.location.remote_session_id'))=?2",
            [host, location_key],
            |r| r.get(0),
        )
        .optional()?;
    data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
}

/// Refuse to admit a second active seat at `location`. The typed error names
/// the incumbent so the caller can retire/relocate it or bind to it after
/// reconciliation — never silently stacked on the same location. A
/// relocation may replace its own predecessor at the same physical location:
/// the predecessor is retired before the successor is inserted in the same
/// transaction.
fn claim_seat_location(
    conn: &rusqlite::Connection,
    location: &SeatLocation,
    replacing: Option<EntityId>,
) -> Result<()> {
    // SSH PTY seats have no location claim: their single-writer rule is the
    // lease itself, verified at acquisition and re-verified on every write.
    // Every other location claims `(host, pane_id | remote_session_id)` so a
    // tmux pane and a managed session anchor get the same one-active-seat
    // guarantee inside the admitting transaction.
    let Some((host, location_key)) = location.claim_key() else {
        return Ok(());
    };
    if let Some(incumbent) = active_seat_at_location(conn, host, location_key)? {
        if Some(incumbent.seat_id) == replacing {
            return Ok(());
        }
        return Err(seat_error(
            "seat_location_occupied",
            RetryStrategy::AfterReconcile,
            serde_json::json!({
                "claimant_seat_id": incumbent.seat_id.to_string(),
                "claimant_task_id": incumbent.task_id.to_string(),
                "host": host,
                "location_key": location_key,
            }),
        )
        .into());
    }
    Ok(())
}

/// End `lease` inside `tx` at `commit`. No-op if already ended (idempotent —
/// a crashed commit must not become a phantom second ending).
fn end_lease_tx(
    tx: &Transaction<'_>,
    lease: &mut SeatLease,
    reason: LeaseEndReason,
    superseded_by: Option<EntityId>,
    commit: CommitRef,
) -> Result<()> {
    if !lease.is_current() {
        return Ok(());
    }
    lease.state = SeatLeaseState::Ended;
    lease.ended_reason = Some(reason);
    lease.superseded_by = superseded_by;
    lease.record_revision = lease.record_revision.next();
    lease.updated_commit = commit;
    write_lease(tx, lease, commit.seq.0)
}

fn seat_log(tx: &Transaction<'_>, store: &Store, commit: CommitRef, count: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count) VALUES(?1,?2,?3,?4)",
        params![
            commit.ledger_id.to_string(),
            commit.seq.0 as i64,
            store.capabilities.incarnation.to_string(),
            count
        ],
    )?;
    Ok(())
}

fn storage_failure(e: &anyhow::Error) -> bool {
    e.is::<rusqlite::Error>() || e.is::<serde_json::Error>()
        || e.downcast_ref::<ShenkoError>()
            .is_some_and(|e| matches!(e, ShenkoError::Storage(_)))
}

fn read_question(
    conn: &rusqlite::Connection,
    id: EntityId,
) -> Result<Option<SeatQuestionRecord>> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM seat_questions WHERE question_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
}

fn write_question(tx: &Transaction<'_>, question: &SeatQuestionRecord, seq: u64) -> Result<()> {
    tx.execute(
        "INSERT INTO seat_questions(question_id,seat_id,task_id,state,data,updated_commit_seq)
         VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(question_id) DO UPDATE SET
             state=excluded.state, data=excluded.data,
             updated_commit_seq=excluded.updated_commit_seq",
        params![
            question.question_id.to_string(),
            question.seat_id.to_string(),
            question.task_id.to_string(),
            question.state.as_str(),
            serde_json::to_string(question)?,
            seq as i64,
        ],
    )?;
    Ok(())
}

fn read_decision(
    conn: &rusqlite::Connection,
    id: EntityId,
) -> Result<Option<SeatAnswerDecisionRecord>> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM seat_answer_decisions WHERE decision_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
}

fn read_decision_for_question(
    conn: &rusqlite::Connection,
    question_id: EntityId,
) -> Result<Option<SeatAnswerDecisionRecord>> {
    Ok(read_latest_decision_for_question(conn, question_id)?)
}

/// The newest decision for one question. A question may carry more than one
/// decision only when the earlier one ended `NotSent` — the remote provably
/// never applied it — so the newest decision is always the live one.
fn read_latest_decision_for_question(
    conn: &rusqlite::Connection,
    question_id: EntityId,
) -> Result<Option<SeatAnswerDecisionRecord>> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM seat_answer_decisions WHERE question_id=?1
             ORDER BY updated_commit_seq DESC LIMIT 1",
            [question_id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
}

fn write_decision(tx: &Transaction<'_>, decision: &SeatAnswerDecisionRecord, seq: u64) -> Result<()> {
    tx.execute(
        "INSERT INTO seat_answer_decisions(decision_id,question_id,seat_id,data,updated_commit_seq)
         VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(decision_id) DO UPDATE SET
             data=excluded.data, updated_commit_seq=excluded.updated_commit_seq",
        params![
            decision.decision_id.to_string(),
            decision.question_id.to_string(),
            decision.seat_id.to_string(),
            serde_json::to_string(decision)?,
            seq as i64,
        ],
    )?;
    Ok(())
}

impl Store {
    /// Fresh owner-request admission after the installed handler's T1. Seat
    /// and original lease commit together; no location or lease is displaced.
    /// Recovery reads these records instead of invoking this fresh path.
    pub fn bind_local_read_only_seat(
        &self,
        invocation: &shenko_types::task::TaskInvocation,
        operation_id: EntityId,
        owning_task_id: EntityId,
        owning_task_revision: TaskRevision,
        location: SeatLocation,
    ) -> Result<(SeatRecord, SeatLease)> {
        ensure!(matches!(&location, SeatLocation::Tmux {
            host, local_identity: Some(_), ..
        } if host == "local"), "read-only attachment requires exact local identity");
        location.validate_local_identity()?;
        // The original CAS input, rather than fresh caller arguments, owns the
        // selected task/revision, principal and complete physical locator.
        let original = self.external_operation(operation_id)?;
        let bytes = self.cas.read_object(&original.final_input_ref.digest)?;
        ensure!(bytes.len() as u64 == original.final_input_ref.bytes
            && shenko_types::object::Digest::blake3(&bytes) == original.input_digest
            && original.input_digest == original.final_input_ref.digest,
            "attachment original input closure mismatch");
        let frozen: serde_json::Value = serde_json::from_slice(&bytes)?;
        ensure!(frozen["schema_version"] == 1
            && frozen["request"]["task_id"] == serde_json::json!(owning_task_id)
            && frozen["owning_task_revision"] == serde_json::json!(owning_task_revision)
            && frozen["request"]["location"] == serde_json::to_value(&location)?,
            "attachment arguments differ from original owner request");
        let installation_id: EntityId = serde_json::from_value(frozen["installation_id"].clone())?;
        let principal: shenko_types::object::ObjectRef = serde_json::from_value(frozen["principal_ref"].clone())?;
        ensure!(self.owner_grant(installation_id)?.is_some_and(|grant| grant.human_principal_ref == principal),
            "attachment original owner grant mismatch");
        let seat_id = EntityId::derived("local-seat-attach-seat/v1", operation_id.to_string().as_bytes());
        let lease_id = EntityId::derived("local-seat-attach-lease/v1", operation_id.to_string().as_bytes());
        self.ledger.with_mutation(|| {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(seat) = read_seat(&tx, seat_id)? {
                ensure!(seat.task_id == owning_task_id && seat.location == location
                    && seat.read_only_operation == Some(operation_id), "attachment identity conflict");
                let lease = read_lease(&tx, lease_id)?
                    .ok_or_else(|| ShenkoError::not_found("original attachment lease missing"))?;
                tx.commit()?;
                return Ok((seat, lease));
            }
            let task = read_task_row(&tx, invocation.task_id)?
                .ok_or_else(|| ShenkoError::not_found("attachment request task missing"))?;
            ensure!(self.native_invocation_current_tx(&tx, &task, invocation)?,
                "stale attachment invocation or generation");
            ensure!(task.kind == "seat.attach.local" && task.created_by_step_id.is_none()
                && task.memos.get("owner_local_seat_request") == Some(&serde_json::json!(true))
                && task.owner == (shenko_types::task::TaskOwner::Task { id: owning_task_id }),
                "attachment lacks an admitted owner request");
            let operation_json: String = tx.query_row("SELECT data FROM operations WHERE operation_id=?1",
                [operation_id.to_string()], |row| row.get(0))?;
            let operation: shenko_types::operation::OperationRecord = serde_json::from_str(&operation_json)?;
            ensure!(operation.task_id == task.task_id && operation.operation_name == "seat.attach.local.v1"
                && operation.final_input_ref == task.input_ref
                && operation.state == shenko_types::operation::OperationState::InFlight,
                "attachment original operation mismatch");
            let owner = read_task_row(&tx, owning_task_id)?
                .ok_or_else(|| ShenkoError::not_found("selected attachment task missing"))?;
            ensure!(owner.agent_id == task.agent_id && owner.task_revision == owning_task_revision
                && owner.abort_request.is_none() && !owner.is_terminal(), "selected attachment task changed");
            claim_seat_location(&tx, &location, None)?;
            let seq = self.ledger.allocate_seq(&tx)?;
            let commit = CommitRef::new(self.ledger.ledger_id(), seq);
            let seat = SeatRecord {
                schema_version: 1, seat_id, incarnation: SeatIncarnation(0), task_id: owning_task_id,
                adapter: "tmux".into(), adapter_version: "tmux-capture-v1".into(), location,
                read_only_operation: Some(operation_id), supersedes: None, status: SeatStatus::Active,
                record_revision: RecordRevision(0), created_commit: commit, updated_commit: commit,
            };
            let lease = SeatLease {
                schema_version: 2, lease_id, seat_id, holder_task_id: owning_task_id,
                acquired_at_incarnation: seat.incarnation, lease_epoch: 1,
                fence_capability: format!("local-read-only:{operation_id}"), legacy_writer: false,
                state: SeatLeaseState::Current, ended_reason: None, superseded_by: None,
                record_revision: RecordRevision(0), created_commit: commit, updated_commit: commit,
            };
            write_seat(&tx, &seat, seq.0)?;
            write_lease(&tx, &lease, seq.0)?;
            seat_log(&tx, self, commit, 2)?;
            tx.commit()?;
            Ok((seat, lease))
        }, storage_failure)
    }

    /// Register a newly discovered seat. Returns the existing record when the
    /// id is already known (idempotent re-registration after a restart).
    /// Refuses a second active seat at an already-claimed pane inside the same
    /// transaction — `seat_at_pane` is a lookup hint, not the admission check.
    pub fn register_seat(
        &self,
        seat_id: EntityId,
        task_id: EntityId,
        adapter: impl Into<String>,
        adapter_version: impl Into<String>,
        location: SeatLocation,
    ) -> Result<SeatRecord> {
        location.validate_local_identity()?;
        let adapter = adapter.into();
        let adapter_version = adapter_version.into();
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if let Some(existing) = read_seat(&tx, seat_id)? {
                    ensure!(existing.task_id == task_id && existing.adapter == adapter
                        && existing.adapter_version == adapter_version && existing.location == location,
                        "seat identity already bound to different facts");
                    tx.commit()?;
                    return Ok(existing);
                }
                claim_seat_location(&tx, &location, None)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let seat = SeatRecord {
                    schema_version: 1,
                    seat_id,
                    incarnation: SeatIncarnation(0),
                    task_id,
                    adapter: adapter.into(),
                    adapter_version: adapter_version.into(),
                    location,
                    read_only_operation: None,
                    supersedes: None,
                    status: SeatStatus::Active,
                    record_revision: RecordRevision(0),
                    created_commit: commit,
                    updated_commit: commit,
                };
                write_seat(&tx, &seat, seq.0)?;
                seat_log(&tx, self, commit, 1)?;
                tx.commit()?;
                Ok(seat)
            },
            storage_failure,
        )
    }

    /// Arm the exact effect of a committed decision — `T1 arm`.
    ///
    /// The arm rechecks the committed decision's complete question, seat,
    /// task and lease binding in its write transaction, records the exact
    /// effect digest and operation, and moves the question to `Responding`.
    /// From this commit on the effect *may* have reached the remote side, so
    /// recovery never re-sends it. This transaction does not hold a fence
    /// across the subsequent transport write.
    pub fn arm_seat_answer_decision(
        &self,
        decision_id: EntityId,
        expected_question_record_revision: RecordRevision,
        effect_input_digest: String,
        operation_id: EntityId,
    ) -> Result<SeatAnswerDecisionRecord> {
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut decision = read_decision(&tx, decision_id)?
                    .ok_or_else(|| ShenkoError::not_found("arm_seat_answer_decision: unknown decision"))?;
                ensure!(
                    decision.state == SeatAnswerDecisionState::Committed,
                    seat_error(
                        "seat_decision_not_armed",
                        RetryStrategy::NeverAutomatically,
                        serde_json::json!({
                            "decision_id": decision_id.to_string(),
                            "state": decision.state.as_str(),
                        }),
        )
                );
                let mut question = read_question(&tx, decision.question_id)?
                    .ok_or_else(|| ShenkoError::not_found("arm_seat_answer_decision: unknown question"))?;
                ensure!(
                    question.record_revision == expected_question_record_revision
                        && question.record_revision == decision.expected_question_record_revision.next()
                        && question.question_revision == decision.expected_question_revision
                        && question.state == SeatQuestionState::DecisionCommitted,
                    seat_error(
                        "stale_question",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({
                            "question_id": decision.question_id.to_string(),
                            "expected": expected_question_record_revision.0,
                            "current": question.record_revision.0,
                            "state": question.state.as_str(),
                        }),
        )
                );
                ensure!(
                    question.seat_id == decision.seat_id
                        && question.task_id == decision.task_id
                        && question.lease_id == decision.lease_id,
                    seat_error(
                        "seat_decision_binding_mismatch",
                        RetryStrategy::NeverAutomatically,
                        serde_json::json!({"decision_id": decision_id.to_string()}),
                    )
                );
                let lease = read_lease(&tx, decision.lease_id)?
                    .ok_or_else(|| ShenkoError::not_found("arm_seat_answer_decision: unknown lease"))?;
                let seat = read_seat(&tx, decision.seat_id)?
                    .ok_or_else(|| ShenkoError::not_found("arm_seat_answer_decision: unknown seat"))?;
                check_seat_task(&seat, decision.task_id)?;
                check_seat_writable(&seat)?;
                ensure!(
                    seat.status == SeatStatus::Active,
                    seat_error(
                        "seat_retired",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({"seat_id": decision.seat_id.to_string()}),
                    )
                );
                ensure!(
                    seat.incarnation == decision.expected_seat_incarnation
                        && lease.acquired_at_incarnation == seat.incarnation,
                    seat_error(
                        "seat_location_changed",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({
                            "seat_id": decision.seat_id.to_string(),
                            "expected": decision.expected_seat_incarnation.0,
                            "acquired_at": lease.acquired_at_incarnation.0,
                            "current": seat.incarnation.0,
                        }),
                    )
                );
                ensure!(
                    seat.record_revision == decision.expected_seat_revision,
                    seat_error(
                        "stale_task",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({
                            "seat_id": decision.seat_id.to_string(),
                            "expected": decision.expected_seat_revision.0,
                            "current": seat.record_revision.0,
                        }),
                    )
                );
                let task = read_task_row(&tx, decision.task_id)?.ok_or_else(|| {
                    seat_error(
                        "seat_task_missing",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({"task_id": decision.task_id.to_string()}),
                    )
                })?;
                ensure!(
                    task.task_revision == decision.expected_task_revision,
                    seat_error(
                        "stale_task",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({
                            "task_id": decision.task_id.to_string(),
                            "expected": decision.expected_task_revision.0,
                            "current": task.task_revision.0,
                        }),
                    )
                );
                ensure!(
                    lease.is_current() && lease.is_writer_fence()
                        && lease.lease_epoch == decision.lease_epoch
                        && lease.fence_capability == decision.fence_capability
                        && lease.seat_id == decision.seat_id
                        && lease.holder_task_id == decision.task_id,
                    seat_error(
                        "stale_authority",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({
                            "lease_id": decision.lease_id.to_string(),
                            "expected": decision.lease_epoch,
                            "current": lease.lease_epoch,
                        }),
        )
                );
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                decision.state = SeatAnswerDecisionState::Armed;
                decision.exact_effect_input_digest = effect_input_digest;
                decision.operation_id = Some(operation_id);
                decision.record_revision = decision.record_revision.next();
                decision.updated_commit = commit;
                write_decision(&tx, &decision, seq.0)?;
                question.state = SeatQuestionState::Responding;
                question.record_revision = question.record_revision.next();
                question.updated_commit = commit;
                write_question(&tx, &question, seq.0)?;
                seat_log(&tx, self, commit, 2)?;
                tx.commit()?;
                Ok(decision)
            },
            storage_failure,
        )
    }

    /// Close an armed decision with the evidence the remote side produced —
    /// `E1/T2 回执`.
    ///
    /// The question's end state follows the observation, never the intent:
    /// a confirmed answer ends `Answered`; an unconfirmed one ends
    /// `AnswerUnknown` (the effect may or may not have been applied and is
    /// never replayed); a refusal before the write ends the decision
    /// `NotSent` and returns the question to `Open` so a *new* decision may
    /// be made; a remote rejection ends `Withdrawn`.
    pub fn complete_seat_answer_decision(
        &self,
        decision_id: EntityId,
        outcome: SeatAnswerOutcome,
    ) -> Result<(SeatAnswerDecisionRecord, SeatQuestionRecord)> {
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut decision = read_decision(&tx, decision_id)?
                    .ok_or_else(|| ShenkoError::not_found("complete_seat_answer_decision: unknown decision"))?;
                let mut question = read_question(&tx, decision.question_id)?
                    .ok_or_else(|| ShenkoError::not_found("complete_seat_answer_decision: unknown question"))?;
                if decision.state.is_terminal() {
                    // Idempotent: a replayed completion returns the stored
                    // facts instead of moving a closed question again.
                    tx.commit()?;
                    return Ok((decision, question));
    }
                ensure!(
                    decision.state == SeatAnswerDecisionState::Armed
                        || (decision.state == SeatAnswerDecisionState::Committed
                            && matches!(outcome, SeatAnswerOutcome::NotSent { .. })),
                    seat_error(
                        "seat_decision_not_armed",
                        RetryStrategy::NeverAutomatically,
                        serde_json::json!({
                            "decision_id": decision_id.to_string(),
                            "state": decision.state.as_str(),
                        }),
        )
                );
                let (decision_state, question_state, confirmation, evidence) = match outcome {
                    SeatAnswerOutcome::Answered { evidence } => (
                        SeatAnswerDecisionState::Answered,
                        SeatQuestionState::Answered,
                        "confirmed",
                        evidence,
                    ),
                    SeatAnswerOutcome::AnswerUnknown { reason } => (
                        SeatAnswerDecisionState::AnswerUnknown,
                        SeatQuestionState::AnswerUnknown,
                        "unconfirmed",
                        reason,
                    ),
                    SeatAnswerOutcome::NotSent { reason } => (
                        SeatAnswerDecisionState::NotSent,
                        // The question returns to `Open` only when nothing
                        // else closed it meanwhile (a remote-side
                        // resolution is a fact that is never overwritten).
                        match question.state {
                            SeatQuestionState::DecisionCommitted | SeatQuestionState::Responding => {
                                SeatQuestionState::Open
                            }
                            other => other,
                        },
                        "not_sent",
                        reason,
                    ),
                    SeatAnswerOutcome::Rejected { reason } => (
                        SeatAnswerDecisionState::Rejected,
                        SeatQuestionState::Withdrawn,
                        "rejected",
                        reason,
                    ),
                };
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                decision.state = decision_state;
                decision.confirmation = Some(confirmation.to_string());
                decision.closure_evidence = Some(evidence.clone());
                decision.record_revision = decision.record_revision.next();
                decision.updated_commit = commit;
                write_decision(&tx, &decision, seq.0)?;
                question.state = question_state;
                question.record_revision = question.record_revision.next();
                question.updated_commit = commit;
                write_question(&tx, &question, seq.0)?;
                seat_log(&tx, self, commit, 2)?;
                tx.commit()?;
                Ok((decision, question))
            },
            storage_failure,
        )
    }

    /// Register the confirmed new location of work that used to live at
    /// `previous`. The predecessor record keeps its history, is retired, and
    /// its current lease (if any) ends — a stale pane id never stays writable.
    /// `task_id` must name the predecessor's own task: relocating a seat can
    /// never rebind the position to a different task. The target pane must be
    /// free or still occupied only by this predecessor.
    pub fn relocate_seat(
        &self,
        previous: EntityId,
        task_id: EntityId,
        adapter: impl Into<String>,
        adapter_version: impl Into<String>,
        location: SeatLocation,
    ) -> Result<SeatRecord> {
        location.validate_local_identity()?;
        let adapter = adapter.into();
        let adapter_version = adapter_version.into();
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut old = read_seat(&tx, previous)?.ok_or_else(|| {
                    ShenkoError::not_found("relocate_seat: unknown predecessor seat")
                })?;
                if old.task_id != task_id {
                    return Err(seat_error(
                        "seat_task_mismatch",
                        RetryStrategy::NeverAutomatically,
                        serde_json::json!({
                            "seat_id": previous.to_string(),
                            "seat_task_id": old.task_id.to_string(),
                            "claimed_task_id": task_id.to_string(),
                        }),
                    )
                    .into());
                }
                // Idempotent replay of an already-committed relocation.
                if let Some(next) = old.supersedes_or_successor(&tx)? {
                    ensure!(next.task_id == task_id && next.adapter == adapter
                        && next.adapter_version == adapter_version && next.location == location,
                        "seat relocation already bound to different facts");
                    tx.commit()?;
                    return Ok(next);
                }
                claim_seat_location(&tx, &location, Some(previous))?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let seat = SeatRecord {
                    schema_version: 1,
                    seat_id: EntityId::new(),
                    incarnation: SeatIncarnation(0),
                    task_id,
                    adapter: adapter.into(),
                    adapter_version: adapter_version.into(),
                    location,
                    read_only_operation: None,
                    supersedes: Some(previous),
                    status: SeatStatus::Active,
                    record_revision: RecordRevision(0),
                    created_commit: commit,
                    updated_commit: commit,
                };
                if let Some(mut lease) = current_lease(&tx, previous)? {
                    end_lease_tx(&tx, &mut lease, LeaseEndReason::SeatSuperseded, None, commit)?;
                }
                old.status = SeatStatus::Retired;
                old.record_revision = old.record_revision.next();
                old.updated_commit = commit;
                write_seat(&tx, &old, seq.0)?;
                write_seat(&tx, &seat, seq.0)?;
                seat_log(&tx, self, commit, 2)?;
                tx.commit()?;
                Ok(seat)
            },
            storage_failure,
        )
    }

    /// Mark a seat record unusable without a successor (pane dead, remote
    /// agent exited). Its current lease ends; the record stays for history.
    pub fn retire_seat(&self, seat_id: EntityId) -> Result<SeatRecord> {
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut seat = read_seat(&tx, seat_id)?
                    .ok_or_else(|| ShenkoError::not_found("retire_seat: unknown seat"))?;
                if seat.status == SeatStatus::Retired {
                    tx.commit()?;
                    return Ok(seat); // idempotent
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                if let Some(mut lease) = current_lease(&tx, seat_id)? {
                    end_lease_tx(&tx, &mut lease, LeaseEndReason::SeatRetired, None, commit)?;
                }
                seat.status = SeatStatus::Retired;
                seat.record_revision = seat.record_revision.next();
                seat.updated_commit = commit;
                write_seat(&tx, &seat, seq.0)?;
                seat_log(&tx, self, commit, 1)?;
                tx.commit()?;
                Ok(seat)
            },
            storage_failure,
        )
    }

    /// Acquire the single writer lease for a seat.
    ///
    /// A seat has at most one valid writer, and that writer is the seat's own
    /// durable owner: only the task the seat record names may hold its writer
    /// lease. A holder that names a foreign task is refused with
    /// `seat_task_mismatch` and never displaces the live writer, so the
    /// fleet can never keep naming one owner while another task writes.
    ///
    /// A takeover by that same owner needs a strictly greater
    /// `lease_epoch`: the writer epoch is monotonic across the seat's entire
    /// acquisition history, including release, human takeover and restart.
    /// An epoch that does not exceed that history is refused with
    /// `seat_lease_epoch_regression`, without changing any stored lease.
    /// A greater epoch ends the current lease with
    /// `Superseded` — a new valid writer displaces the old one, and the
    /// displaced holder's writes are refused by lease id afterwards.
    /// `lease_id` is the caller's acquisition identity, chosen and durably
    /// saved before the attempt so the outcome is always re-identifiable:
    /// replaying the same
    /// acquisition after a crash or an uncertain commit returns the stored
    /// lease — even one already ended — instead of minting a new lease that
    /// could displace a newer legitimate writer.
    /// Trusted runtime API, not a model-facing authority constructor: callers
    /// must validate the task/controller delegation before acquiring a lease.
    /// The seat family travels with the cognitive archive as one atomic
    /// conflict unit (`seat/{seat_id}`), so an armed answer decision cannot
    /// be separated from its question on a handover. A lease is still not
    /// *remote* fencing: it bounds this product's writes, not a human or
    /// another process typing into the same pane.
    pub fn acquire_seat_lease(&self, request: SeatLeaseRequest) -> Result<SeatLease> {
        let SeatLeaseRequest { lease_id, seat_id, holder_task_id, lease_epoch, fence_capability } =
            request;
        ensure!(
            lease_epoch > 0 && !fence_capability.is_empty() && fence_capability.len() <= 256,
            seat_error(
                "seat_lease_invalid_fence",
                RetryStrategy::NeverAutomatically,
                serde_json::json!({"seat_id": seat_id.to_string()}),
            )
        );
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if let Some(existing) = read_lease(&tx, lease_id)? {
                    // This exact acquisition already committed; replay returns
                    // it verbatim, never a phantom second displacement.
                    ensure!(
                        existing.seat_id == seat_id && existing.holder_task_id == holder_task_id
                            && existing.lease_epoch == lease_epoch
                            && existing.fence_capability == fence_capability,
                        seat_error(
                            "seat_lease_identity_conflict",
                            RetryStrategy::NeverAutomatically,
                            serde_json::json!({"lease_id": lease_id.to_string()}),
                        )
                    );
                    let seat = read_seat(&tx, seat_id)?
                        .ok_or_else(|| ShenkoError::not_found("acquire_seat_lease: unknown seat"))?;
                    check_seat_task(&seat, holder_task_id)?;
                    tx.commit()?;
                    return Ok(existing);
                }
                let seat = read_seat(&tx, seat_id)?
                    .ok_or_else(|| ShenkoError::not_found("acquire_seat_lease: unknown seat"))?;
                ensure!(
                    seat.status == SeatStatus::Active,
                    seat_error(
                        "seat_retired",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({"seat_id": seat_id.to_string()}),
                    )
                );
                // The seat's durable owner is the only task that may hold its
                // writer lease. A foreign holder would displace a live writer
                // while the fleet still names the owner.
                check_seat_task(&seat, holder_task_id)?;
                let current = current_lease(&tx, seat_id)?;
                let highest_epoch = highest_seat_lease_epoch(&tx, seat_id)?;
                if lease_epoch <= highest_epoch {
                    return Err(seat_error(
                        "seat_lease_epoch_regression",
                        RetryStrategy::NeverAutomatically,
                        serde_json::json!({
                            "seat_id": seat_id.to_string(),
                            "lease_id": lease_id.to_string(),
                            "requested_lease_epoch": lease_epoch,
                            "highest_lease_epoch": highest_epoch,
                            "current_lease_id": current.as_ref().map(|lease| lease.lease_id),
                            "current_lease_epoch": current.as_ref().map(|lease| lease.lease_epoch),
                            "current_holder_task_id": current.as_ref().map(|lease| lease.holder_task_id),
                        }),
                    )
                    .into());
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let lease = SeatLease {
                    schema_version: 2,
                    lease_id,
                    seat_id,
                    holder_task_id,
                    acquired_at_incarnation: seat.incarnation,
                    lease_epoch,
                    fence_capability,
                    legacy_writer: false,
                    state: SeatLeaseState::Current,
                    ended_reason: None,
                    superseded_by: None,
                    record_revision: RecordRevision(0),
                    created_commit: commit,
                    updated_commit: commit,
                };
                if let Some(mut old_lease) = current {
                    end_lease_tx(
                        &tx,
                        &mut old_lease,
                        LeaseEndReason::Superseded,
                        Some(lease.lease_id),
                        commit,
                    )?;
                }
                write_lease(&tx, &lease, seq.0)?;
                seat_log(&tx, self, commit, 1)?;
                tx.commit()?;
                Ok(lease)
            },
            storage_failure,
        )
    }

    /// End a lease explicitly: a normal release or a proven human takeover.
    /// `SeatLeaseEnd` cannot express `Superseded`/`SeatSuperseded`/`SeatRetired`
    /// — those reasons only become true alongside the transition that produces
    /// them, so the public path can never record a lifecycle claim without
    /// making it. Idempotent: replaying an end on an already-ended lease
    /// returns the stored record unchanged.
    pub fn end_seat_lease(&self, lease_id: EntityId, end: SeatLeaseEnd) -> Result<SeatLease> {
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut lease = read_lease(&tx, lease_id)?
                    .ok_or_else(|| ShenkoError::not_found("end_seat_lease: unknown lease"))?;
                if lease.is_current() {
                    let seq = self.ledger.allocate_seq(&tx)?;
                    let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                    end_lease_tx(&tx, &mut lease, end.reason(), None, commit)?;
                    seat_log(&tx, self, commit, 1)?;
                }
                tx.commit()?;
                Ok(lease)
            },
            storage_failure,
        )
    }

    /// Write admission. Returns the *committed* lease when
    /// `lease_id` still carries write authority on a live seat for `task_id`.
    /// Every refusal is typed: the caller must re-observe/reconcile rather
    /// than blind-send. Lease id binds the holder epoch; a lease superseded
    /// after admission can never become current again.
    pub fn check_seat_write(&self, lease_id: EntityId, task_id: EntityId) -> Result<SeatLease> {
        let mut connection = self.ledger.conn.lock();
        let conn = connection.transaction()?;
        let lease = read_lease(&conn, lease_id)?.ok_or_else(|| {
            seat_error(
                "seat_lease_unknown",
                RetryStrategy::AfterReconcile,
                serde_json::json!({"lease_id": lease_id.to_string()}),
            )
        })?;
        if lease.holder_task_id != task_id {
            return Err(seat_error(
                "seat_lease_holder_mismatch",
                RetryStrategy::NeverAutomatically,
                serde_json::json!({
                    "lease_id": lease_id.to_string(),
                    "holder": lease.holder_task_id.to_string(),
                    "claiming_task": task_id.to_string(),
                }),
            )
            .into());
        }
        if !lease.is_writer_fence() {
            // A stored v1 lease (no epoch/capability) never carried a real
            // writer fence. It cannot become writable by ageing; the holder
            // must re-acquire with a v2 request after re-verifying the seat.
            return Err(seat_error(
                "seat_lease_legacy",
                RetryStrategy::AfterReconcile,
                serde_json::json!({"lease_id": lease_id.to_string()}),
            )
            .into());
        }
        if !lease.is_current() {
            // The end reason goes into details as its real JSON value so
            // consumers can deserialize `reason` back into `LeaseEndReason`.
            let reason = lease
                .ended_reason
                .map(serde_json::to_value).transpose()?
                .unwrap_or_else(|| serde_json::Value::String("ended".to_string()));
            return Err(seat_error(
                "seat_lease_ended",
                match lease.ended_reason {
                    Some(LeaseEndReason::SeatSuperseded | LeaseEndReason::SeatRetired) =>
                        RetryStrategy::AfterReconcile,
                    _ => RetryStrategy::NeverAutomatically,
                },
                serde_json::json!({"lease_id": lease_id.to_string(), "reason": reason}),
            )
            .into());
        }
        let seat = read_seat(&conn, lease.seat_id)?.ok_or_else(|| {
            seat_error(
                "seat_record_missing",
                RetryStrategy::AfterReconcile,
                serde_json::json!({"seat_id": lease.seat_id.to_string()}),
            )
        })?;
        check_seat_task(&seat, task_id)?;
        check_seat_writable(&seat)?;
        if seat.status != SeatStatus::Active {
            return Err(seat_error(
                "seat_retired",
                RetryStrategy::AfterReconcile,
                serde_json::json!({"seat_id": seat.seat_id.to_string()}),
            )
            .into());
        }
        if seat.incarnation != lease.acquired_at_incarnation {
            return Err(seat_error(
                "seat_location_changed",
                RetryStrategy::AfterReconcile,
                serde_json::json!({
                    "seat_id": seat.seat_id.to_string(),
                    "acquired_at": lease.acquired_at_incarnation.0,
                    "current": seat.incarnation.0,
                }),
            )
            .into());
        }
        Ok(lease)
    }

    /// Read one seat record.
    pub fn seat(&self, seat_id: EntityId) -> Result<Option<SeatRecord>> {
        read_seat(&self.ledger.conn.lock(), seat_id)
    }

    /// Read one lease.
    pub fn seat_lease(&self, lease_id: EntityId) -> Result<Option<SeatLease>> {
        read_lease(&self.ledger.conn.lock(), lease_id)
    }

    /// All seats registered for a task, newest first — the location history
    /// requires keeping when a seat moves.
    pub fn seat_task_seats(&self, task_id: EntityId) -> Result<Vec<SeatRecord>> {
        let conn = self.ledger.conn.lock();
        let mut q = conn.prepare(
            "SELECT data FROM seat_records WHERE task_id=?1 ORDER BY updated_commit_seq DESC",
        )?;
        let rows = q
            .query_map([task_id.to_string()], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter()
            .map(|d| serde_json::from_str(d).map_err(Into::into))
            .collect()
    }

    /// Newest seats and their open questions in one bounded query, optionally
    /// filtered by owning task. Enumeration itself needs no prior task UUID.
    /// The 33rd row is a truncation witness, not a second page silently lost.
    pub fn seat_fleet_summary(&self, task_id: Option<EntityId>) -> Result<Vec<SeatFleetEntry>> {
        let conn = self.ledger.conn.lock();
        let mut query = conn.prepare(
            "WITH selected AS (
                 SELECT seat_id, data, updated_commit_seq FROM seat_records
                 WHERE (?1 IS NULL OR task_id=?1)
                 ORDER BY updated_commit_seq DESC, seat_id
                 LIMIT ?2
             )
             SELECT s.data, COUNT(q.question_id)
             FROM selected s
             LEFT JOIN seat_questions q ON q.seat_id=s.seat_id AND q.state='open'
             GROUP BY s.seat_id
             ORDER BY s.updated_commit_seq DESC, s.seat_id",
        )?;
        let rows = query.query_map(
            params![task_id.map(|id| id.to_string()), (SEAT_FLEET_LIMIT + 1) as i64],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
        )?;
        rows.map(|row| {
            let (data, open_question_count) = row?;
            let seat: SeatRecord = serde_json::from_str(&data)?;
            Ok(SeatFleetEntry {
                seat_id: seat.seat_id,
                task_id: seat.task_id,
                location_summary: seat.location.fleet_summary(seat.seat_id),
                adapter: seat.adapter,
                adapter_version: seat.adapter_version,
                status: seat.status,
                open_question_count,
                last_observed_at: None,
                // The durable record does not attest what an adapter can do.
                // Only an attached adapter publishes these flags.
                capability: None,
            })
        }).collect()
    }

    /// Follow the supersedes chain to the latest record for the
    /// work that used to sit at `seat_id` (or the record itself if it is
    /// still the latest one). The latest record may itself be retired.
    pub fn seat_successor(&self, seat_id: EntityId) -> Result<Option<SeatRecord>> {
        let conn = self.ledger.conn.lock();
        let mut current = match read_seat(&conn, seat_id)? {
            Some(s) => s,
            None => return Ok(None),
        };
        // Every link visits one more record, so a walk that revisits an id can
        // only be a malformed cycle: surface it, never loop or cap valid moves.
        let mut visited = std::collections::HashSet::from([current.seat_id]);
        loop {
            let next: Option<String> = conn
                .query_row(
                    "SELECT data FROM seat_records WHERE supersedes=?1",
                    [current.seat_id.to_string()],
                    |r| r.get(0),
                )
                .optional()?;
            match next {
                None => return Ok(Some(current)),
                Some(data) => {
                    current = serde_json::from_str(&data)?;
                    if !visited.insert(current.seat_id) {
                        return Err(ShenkoError::conflict(format!(
                            "seat supersedes chain cycles at {}",
                            current.seat_id
                        ))
                        .into());
                    }
                }
            }
        }
    }

    /// Find a live seat at a given location key — the "find the existing
    /// work position again" lookup. The key is the pane id for tmux seats
    /// and the durable session anchor (`remote_session_id`) for managed RPC
    /// seats. Returns at most one *active* record; a retired record at the
    /// same locator is not resurrected.
    pub fn seat_at_location(&self, host: &str, location_key: &str) -> Result<Option<SeatRecord>> {
        active_seat_at_location(&self.ledger.conn.lock(), host, location_key)
    }

    /// The current writer lease of one seat, if any — the authority a
    /// restarted runtime needs to rebind live adapters without displacing
    /// the recorded writer.
    pub fn seat_current_lease(&self, seat_id: EntityId) -> Result<Option<SeatLease>> {
        current_lease(&self.ledger.conn.lock(), seat_id)
    }

    /// Every seat record still marked active, newest first — the restart
    /// scan that rebuilds the live seat registry. Records the runtime can no
    /// longer reach (dead managed process, missing pane) are simply left to
    /// the observers that retire them; this read decides nothing.
    pub fn active_seat_records(&self) -> Result<Vec<SeatRecord>> {
        let conn = self.ledger.conn.lock();
        let mut q = conn.prepare(
            "SELECT data FROM seat_records WHERE status='active' ORDER BY updated_commit_seq DESC",
        )?;
        let rows = q
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter()
            .map(|d| serde_json::from_str(d).map_err(Into::into))
            .collect()
    }

    /// Bump the location incarnation of one active seat.
    ///
    /// This is the *in-place* change transition, the complement of
    /// `relocate_seat`/`supersedes`: relocation records a planned move by
    /// writing a successor record, while a bump records that an observer
    /// proved the occupant behind this record's stored location changed (a
    /// reused pane, a replaced session) without any record move. The bump
    /// exists so the existing fencing already wired into
    /// `check_seat_write`, `open_seat_question` and the answer-decision CAS
    /// refuses writes admitted against the old incarnation — every current
    /// lease was acquired at an incarnation that is now stale, so its holder
    /// must re-verify the live location and re-acquire before writing.
    /// Leases are not ended here: ending one fabricates a transition the
    /// observer did not prove; staleness is enforced by the incarnation
    /// comparison at every write admission.
    ///
    /// The supersedes model is untouched: the record keeps its seat id, its
    /// task and its supersedes chain; only `incarnation` and the row
    /// revision move, in one commit. Retired records cannot be bumped —
    /// their history stays frozen.
    pub fn bump_seat_incarnation(&self, seat_id: EntityId) -> Result<SeatRecord> {
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut seat = read_seat(&tx, seat_id)?
                    .ok_or_else(|| ShenkoError::not_found("bump_seat_incarnation: unknown seat"))?;
                ensure!(
                    seat.status == SeatStatus::Active,
                    seat_error(
                        "seat_retired",
                        RetryStrategy::NeverAutomatically,
                        serde_json::json!({"seat_id": seat_id.to_string()}),
                    )
                );
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                seat.incarnation = SeatIncarnation(seat.incarnation.0 + 1);
                seat.record_revision = seat.record_revision.next();
                seat.updated_commit = commit;
                write_seat(&tx, &seat, seq.0)?;
                seat_log(&tx, self, commit, 1)?;
                tx.commit()?;
                Ok(seat)
            },
            storage_failure,
        )
    }

    /// Persist one observed seat question. Every question is
    /// durable before it can be answered: the in-memory callback Promise is
    /// not a question record. Admission requires a current v2 writer lease
    /// bound to the same seat and task, verified inside the same transaction.
    /// Re-opening the same question id with the same fingerprint is an
    /// idempotent replay; a different fingerprint on the same id is a typed
    /// conflict, never an overwrite.
    ///
    /// The seat incarnation, the owning task's revision and the writer epoch
    /// are stamped by the store from the rows it reads inside the admitting
    /// transaction — the caller cannot name a version it did not observe.
    pub fn open_seat_question(&self, question: SeatQuestionRecord) -> Result<SeatQuestionRecord> {
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if let Some(existing) = read_question(&tx, question.question_id)? {
                    ensure!(
                        existing.seat_id == question.seat_id
                            && existing.task_id == question.task_id
                            && existing.lease_id == question.lease_id
                            && existing.question.fingerprint == question.question.fingerprint,
                        seat_error(
                            "seat_question_conflict",
                            RetryStrategy::NeverAutomatically,
                            serde_json::json!({"question_id": question.question_id.to_string()}),
                        )
                    );
                    tx.commit()?;
                    return Ok(existing);
                }
                let seat = read_seat(&tx, question.seat_id)?
                    .ok_or_else(|| ShenkoError::not_found("open_seat_question: unknown seat"))?;
                ensure!(
                    seat.task_id == question.task_id && seat.status == SeatStatus::Active,
                    seat_error(
                        "seat_task_mismatch",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({"seat_id": question.seat_id.to_string()}),
                    )
                );
                let lease = read_lease(&tx, question.lease_id)?
                    .ok_or_else(|| ShenkoError::not_found("open_seat_question: unknown lease"))?;
                ensure!(
                    lease.is_current() && lease.is_writer_fence()
                        && lease.seat_id == question.seat_id
                        && lease.holder_task_id == question.task_id,
                    seat_error(
                        "seat_lease_ended",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({"lease_id": question.lease_id.to_string()}),
                    )
                );
                let task = read_task_row(&tx, question.task_id)?.ok_or_else(|| {
                    seat_error(
                        "seat_task_missing",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({"task_id": question.task_id.to_string()}),
                    )
                })?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let mut question = question;
                question.seat_incarnation = seat.incarnation;
                question.task_revision = task.task_revision;
                question.lease_epoch = lease.lease_epoch;
                question.question_revision = 0;
                question.record_revision = RecordRevision(0);
                question.created_commit = commit;
                question.updated_commit = commit;
                write_question(&tx, &question, seq.0)?;
                seat_log(&tx, self, commit, 1)?;
                tx.commit()?;
                Ok(question)
            },
            storage_failure,
        )
    }

    /// Read one seat question.
    pub fn seat_question(&self, question_id: EntityId) -> Result<Option<SeatQuestionRecord>> {
        read_question(&self.ledger.conn.lock(), question_id)
    }

    /// All questions recorded for one seat, newest first.
    pub fn seat_questions(&self, seat_id: EntityId) -> Result<Vec<SeatQuestionRecord>> {
        let conn = self.ledger.conn.lock();
        let mut q = conn.prepare(
            "SELECT data FROM seat_questions WHERE seat_id=?1 ORDER BY updated_commit_seq DESC",
        )?;
        let rows = q
            .query_map([seat_id.to_string()], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter()
            .map(|d| serde_json::from_str(d).map_err(Into::into))
            .collect()
    }

    /// Commit the durable answer decision for one open question.
    ///
    /// This is the *pre-effect* claim: it runs before any input reaches the
    /// remote side, so a stale question can never be typed into, and a crash
    /// after this commit still leaves a durable decision behind. The claim
    /// is a compare-and-swap on every version the decider actually read:
    ///
    /// * `question_revision` / `question_record_revision` — the semantic and
    ///   row revision of the question; a moved revision is `stale_question`
    ///   and the answer is never applied to facts the caller did not read.
    /// * `seat_incarnation` / `seat_revision` — the seat the decider read; a
    ///   moved seat is `stale_task`, a moved incarnation is
    ///   `seat_location_changed`.
    /// * `task_revision` — the owning task's revision; a moved task is
    ///   `stale_task`.
    /// * `lease_epoch` — the writer epoch; a newer writer is
    ///   `stale_authority`, an ended lease is `seat_lease_ended`.
    ///
    /// On success the question transitions to `DecisionCommitted` in the same
    /// commit, so a second decision for the same question is refused by the
    /// state check below. The only exception is a previous decision that
    /// ended `NotSent`: the remote provably did not apply that answer, so a
    /// new decision may be made — with a new decision id.
    pub fn commit_seat_answer_decision(
        &self,
        expected: &SeatDecisionExpectation,
        decision: SeatAnswerDecisionRecord,
    ) -> Result<SeatAnswerDecisionRecord> {
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if let Some(existing) = read_decision(&tx, decision.decision_id)? {
                    ensure!(
                        existing.question_id == decision.question_id
                            && existing.seat_id == decision.seat_id
                            && existing.task_id == decision.task_id,
                        seat_error(
                            "seat_decision_conflict",
                            RetryStrategy::NeverAutomatically,
                            serde_json::json!({"decision_id": decision.decision_id.to_string()}),
                        )
                    );
                    tx.commit()?;
                    return Ok(existing);
                }
                let mut question = read_question(&tx, decision.question_id)?
                    .ok_or_else(|| ShenkoError::not_found("seat_answer_decision: unknown question"))?;
                ensure!(
                    question.seat_id == decision.seat_id && question.task_id == decision.task_id
                        && question.lease_id == decision.lease_id,
                    seat_error(
                        "seat_decision_binding_mismatch",
                        RetryStrategy::NeverAutomatically,
                        serde_json::json!({"question_id": decision.question_id.to_string()}),
                    )
                );
                ensure!(
                    question.record_revision == expected.question_record_revision
                        && question.question_revision == expected.question_revision,
                    seat_error(
                        "stale_question",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({
                            "question_id": decision.question_id.to_string(),
                            "expected_record": expected.question_record_revision.0,
                            "current_record": question.record_revision.0,
                            "expected_revision": expected.question_revision,
                            "current_revision": question.question_revision,
                        }),
                    )
                );
                // A question may only take a new decision while it is open,
                // or while it carries a decision that provably never
                // reached the remote side.
                let prior = read_latest_decision_for_question(&tx, decision.question_id)?;
                let retry_after_not_sent = prior
                    .as_ref()
                    .is_some_and(|prior| prior.state == SeatAnswerDecisionState::NotSent);
                ensure!(
                    question.state.is_open()
                        || (question.state == SeatQuestionState::DecisionCommitted
                            && retry_after_not_sent),
                    seat_error(
                        "seat_question_not_open",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({
                            "question_id": decision.question_id.to_string(),
                            "state": question.state.as_str(),
                        }),
                    )
                );
                let seat = read_seat(&tx, decision.seat_id)?
                    .ok_or_else(|| ShenkoError::not_found("seat_answer_decision: unknown seat"))?;
                check_seat_writable(&seat)?;
                ensure!(
                    seat.record_revision == expected.seat_revision,
                    seat_error(
                        "stale_task",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({
                            "seat_id": decision.seat_id.to_string(),
                            "expected": expected.seat_revision.0,
                            "current": seat.record_revision.0,
                        }),
                    )
                );
                ensure!(
                    seat.incarnation == expected.seat_incarnation,
                    seat_error(
                        "seat_location_changed",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({
                            "seat_id": decision.seat_id.to_string(),
                            "expected": expected.seat_incarnation.0,
                            "current": seat.incarnation.0,
                        }),
                    )
                );
                ensure!(
                    seat.status == SeatStatus::Active,
                    seat_error(
                        "seat_retired",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({"seat_id": decision.seat_id.to_string()}),
                    )
                );
                let task = read_task_row(&tx, decision.task_id)?.ok_or_else(|| {
                    seat_error(
                        "seat_task_missing",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({"task_id": decision.task_id.to_string()}),
                    )
                })?;
                ensure!(
                    task.task_revision == expected.task_revision,
                    seat_error(
                        "stale_task",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({
                            "task_id": decision.task_id.to_string(),
                            "expected": expected.task_revision.0,
                            "current": task.task_revision.0,
                        }),
                    )
                );
                let lease = read_lease(&tx, decision.lease_id)?
                    .ok_or_else(|| ShenkoError::not_found("seat_answer_decision: unknown lease"))?;
                ensure!(
                    lease.is_current() && lease.is_writer_fence()
                        && lease.seat_id == decision.seat_id
                        && lease.holder_task_id == decision.task_id,
                    seat_error(
                        "seat_lease_ended",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({"lease_id": decision.lease_id.to_string()}),
                    )
                );
                ensure!(
                    lease.lease_epoch == expected.lease_epoch,
                    seat_error(
                        "stale_authority",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({
                            "lease_id": decision.lease_id.to_string(),
                            "expected": expected.lease_epoch,
                            "current": lease.lease_epoch,
                        }),
                    )
                );
                // Persist the exact expectation that was admitted, not a
                // second caller-supplied set that could differ when arm
                // later checks the committed decision.
                ensure!(
                    decision.expected_question_revision == expected.question_revision
                        && decision.expected_question_record_revision == expected.question_record_revision
                        && decision.expected_seat_incarnation == expected.seat_incarnation
                        && decision.expected_seat_revision == expected.seat_revision
                        && decision.expected_task_revision == expected.task_revision
                        && decision.lease_epoch == expected.lease_epoch
                        && decision.fence_capability == lease.fence_capability,
                    seat_error(
                        "seat_decision_binding_mismatch",
                        RetryStrategy::NeverAutomatically,
                        serde_json::json!({"decision_id": decision.decision_id.to_string()}),
                    )
                );
                ensure!(
                    lease.acquired_at_incarnation == seat.incarnation,
                    seat_error(
                        "seat_location_changed",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({
                            "seat_id": decision.seat_id.to_string(),
                            "acquired_at": lease.acquired_at_incarnation.0,
                            "current": seat.incarnation.0,
                        }),
                    )
                );
                if let Some(prior) = &prior {
                    ensure!(
                        prior.decision_id == decision.decision_id || retry_after_not_sent,
                        seat_error(
                            "seat_decision_exists",
                            RetryStrategy::NeverAutomatically,
                            serde_json::json!({
                                "question_id": decision.question_id.to_string(),
                                "existing_decision_id": prior.decision_id.to_string(),
                            }),
                        )
                    );
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let mut decision = decision;
                decision.schema_version = 2;
                decision.state = SeatAnswerDecisionState::Committed;
                decision.record_revision = RecordRevision(0);
                decision.created_commit = commit;
                decision.updated_commit = commit;
                write_decision(&tx, &decision, seq.0)?;
                question.state = SeatQuestionState::DecisionCommitted;
                question.record_revision = question.record_revision.next();
                question.updated_commit = commit;
                write_question(&tx, &question, seq.0)?;
                seat_log(&tx, self, commit, 2)?;
                tx.commit()?;
                Ok(decision)
            },
            storage_failure,
        )
    }

    /// Read one answer decision.
    pub fn seat_answer_decision(&self, decision_id: EntityId) -> Result<Option<SeatAnswerDecisionRecord>> {
        read_decision(&self.ledger.conn.lock(), decision_id)
    }

    /// The decision committed for one question, if any.
    pub fn seat_answer_decision_for_question(
        &self,
        question_id: EntityId,
    ) -> Result<Option<SeatAnswerDecisionRecord>> {
        read_decision_for_question(&self.ledger.conn.lock(), question_id)
    }

    /// Every decision recorded for one question, oldest first. A question
    /// carries more than one decision only when an earlier one ended
    /// `NotSent`; the history is kept so the refusal is never invisible.
    pub fn seat_answer_decisions_for_question(
        &self,
        question_id: EntityId,
    ) -> Result<Vec<SeatAnswerDecisionRecord>> {
        let conn = self.ledger.conn.lock();
        let mut q = conn.prepare(
            "SELECT data FROM seat_answer_decisions WHERE question_id=?1
             ORDER BY updated_commit_seq ASC",
        )?;
        let rows = q
            .query_map([question_id.to_string()], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter()
            .map(|d| serde_json::from_str(d).map_err(Into::into))
            .collect()
    }

    /// Record that a question was resolved without this product's decision
    /// (remote timeout, remote choice, session end), or that a late answer
    /// arrived after that. Compare-and-swap on the question revision the
    /// caller observed; an applied answer (state `Answered`) is never moved
    /// backwards by a resolution claim.
    ///
    /// Only the decision completion may return a question to `Open` (after a
    /// `NotSent` refusal); a resolution claim never reopens it, and a closed
    /// question only moves to `LateAnswerIgnored`.
    pub fn mark_seat_question_state(
        &self,
        question_id: EntityId,
        expected_revision: RecordRevision,
        from_states: &[SeatQuestionState],
        to_state: SeatQuestionState,
    ) -> Result<SeatQuestionRecord> {
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut question = read_question(&tx, question_id)?
                    .ok_or_else(|| ShenkoError::not_found("mark_seat_question_state: unknown question"))?;
                ensure!(
                    question.record_revision == expected_revision,
                    seat_error(
                        "stale_question",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({
                            "question_id": question_id.to_string(),
                            "expected": expected_revision.0,
                            "current": question.record_revision.0,
                        }),
                    )
                );
                ensure!(
                    from_states.contains(&question.state),
                    seat_error(
                        "seat_question_state_mismatch",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({
                            "question_id": question_id.to_string(),
                            "current": question.state.as_str(),
                        }),
                    )
                );
                ensure!(
                    to_state != SeatQuestionState::Open,
                    seat_error(
                        "seat_question_state_mismatch",
                        RetryStrategy::NeverAutomatically,
                        serde_json::json!({
                            "question_id": question_id.to_string(),
                            "reason": "only a completed not_sent decision may reopen a question",
                        }),
                    )
                );
                ensure!(
                    !question.state.is_closed() || to_state == SeatQuestionState::LateAnswerIgnored,
                    seat_error(
                        "seat_question_state_mismatch",
                        RetryStrategy::NeverAutomatically,
                        serde_json::json!({
                            "question_id": question_id.to_string(),
                            "current": question.state.as_str(),
                            "reason": "a closed question is never moved backwards",
                        }),
                    )
                );
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                question.state = to_state;
                question.record_revision = question.record_revision.next();
                question.updated_commit = commit;
                write_question(&tx, &question, seq.0)?;
                seat_log(&tx, self, commit, 1)?;
                tx.commit()?;
                Ok(question)
            },
            storage_failure,
        )
    }
}

impl SeatRecord {
    /// Successor record that superseded this one, if relocation was recorded.
    fn supersedes_or_successor(&self, conn: &rusqlite::Connection) -> Result<Option<SeatRecord>> {
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM seat_records WHERE supersedes=?1",
                [self.seat_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
    }
}

#[cfg(test)]
mod local_tmux_identity_tests {
    use super::*;

    #[test]
    fn legacy_locator_stays_explicitly_without_identity() -> Result<()> {
        let json = serde_json::json!({"kind":"tmux", "host":"local", "session":"owned",
            "window":0, "pane":1, "pane_id":"%4", "cwd":"/project", "current_command":"omp"});
        let location: SeatLocation = serde_json::from_value(json.clone())?;
        assert!(matches!(&location, SeatLocation::Tmux { local_identity: None, .. }));
        assert_eq!(serde_json::to_value(location)?, json);
        Ok(())
    }

    #[test]
    fn local_identity_roundtrips_without_changing_location_claim() -> Result<()> {
        let mut location: SeatLocation = serde_json::from_value(serde_json::json!({
            "kind":"tmux", "host":"local", "session":"owned", "window":0, "pane":1,
            "pane_id":"%4", "cwd":"/project", "current_command":"omp",
            "local_identity":{"boot_id":"boot-a", "pane_pid":41, "pane_start_ticks":123,
                "server_pid":20, "server_start_ticks":80, "socket_path":"/owned/tmux.sock"}
        }))?;
        location.validate_local_identity()?;
        assert_eq!(location.claim_key(), Some(("local", "%4")));
        assert_eq!(serde_json::from_str::<SeatLocation>(&serde_json::to_string(&location)?)?, location);
        if let SeatLocation::Tmux { host, .. } = &mut location { *host = "ssh-target".into(); }
        assert!(location.validate_local_identity().is_err());
        Ok(())
    }

    #[test]
    fn partial_or_empty_identity_is_never_complete() -> Result<()> {
        assert!(serde_json::from_value::<LocalTmuxIdentity>(serde_json::json!({
            "boot_id":"boot-a", "pane_pid":41
        })).is_err());
        let identity = LocalTmuxIdentity { boot_id: "boot-a".into(), pane_pid: 41,
            pane_start_ticks: 0, server_pid: 20, server_start_ticks: 80,
            socket_path: "/owned/tmux.sock".into() };
        assert!(identity.validate().is_err());
        Ok(())
    }
}
