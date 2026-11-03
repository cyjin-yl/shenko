//! Durable seat records and single-writer lease admission — SPEC §17.1, §35.5.
//!
//! A `TaskSeat` is a stable product identity for one remote-agent work
//! position; a tmux pane id is only its current *location*, never its durable
//! identity. When the same work moves to a different pane/host the old record
//! keeps its history and the new record names it via `supersedes`; when the
//! position is gone the record is retired, not rewritten in place.
//! At most one active record may claim a `(host, pane_id)` location; the
//! invariant is enforced inside the admitting transaction and backed by a
//! partial unique index.
//!
//! `check_seat_write` exposes durable lease/seat facts to trusted executors;
//! it neither arms an operation nor sends input. The current runtime does not
//! yet have a seat-child handler consuming this interface. A successful read
//! is not an atomic check-and-send fence, and native tmux/human input remains
//! outside this database's control (SPEC §35.5).

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
-- A pane id is a physical location, and one location hosts at most one live
-- work position (SPEC §17.1): SQLite enforces it on the stored location facts,
-- so no trusted caller can admit a second active seat for the same pane.
CREATE UNIQUE INDEX IF NOT EXISTS idx_seat_active_location ON seat_records(
    json_extract(data,'$.location.host'), json_extract(data,'$.location.pane_id')
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
-- The durable uniqueness rule behind §17.1: at most one lease may be current
-- for one seat at a time, enforced by SQLite rather than by callers.
CREATE UNIQUE INDEX IF NOT EXISTS idx_seat_current_lease
    ON seat_leases(seat_id) WHERE state='current';
CREATE INDEX IF NOT EXISTS idx_lease_holder ON seat_leases(seat_id, holder_task_id);
-- Durable seat questions (SPEC §41.11): the in-memory callback Promise is
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
-- The durable answer decision, one per question (SPEC §41.11). The unique
-- question binding makes a second decision for the same question a typed
-- conflict inside the admitting transaction, never a silent overwrite.
-- The durable answer decision (SPEC §41.11). A question carries at most
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

/// Current physical reach of a seat — SPEC §17.1 locator fields.
/// Kept as one sealed shape so future adapters add fields without changing
/// the records table. A pane id or host string here is evidence, not identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SeatLocation {
    /// Existing tmux foreground TUI (SPEC §35.5 first path).
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
    },
    /// Managed remote agent over the native SSH PTY transport (SPEC §17.5).
    /// Identity uniqueness comes from the single-writer lease, not a pane.
    SshPty {
        host: String,
        port: u16,
        user: String,
        /// The exact PTY program this seat was opened with.
        program_b64: String,
    },
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

/// Durable seat record — SPEC §17.1 TaskSeat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatRecord {
    pub schema_version: u32,
    pub seat_id: EntityId,
    /// Stable incarnation; bumps whenever the same record's location was
    /// confirmed to have changed in place (kept for future adapters; this
    /// slice always moves via supersedes so it stays 0).
    pub incarnation: SeatIncarnation,
    /// The task this seat serves. Facts only; a seat does not own the task.
    pub task_id: EntityId,
    /// Adapter family and the version/level it actually negotiated.
    pub adapter: String,
    pub adapter_version: String,
    pub location: SeatLocation,
    /// Seat this record succeeds; the predecessor keeps its own history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<EntityId>,
    pub status: SeatStatus,
    pub record_revision: RecordRevision,
    pub created_commit: CommitRef,
    pub updated_commit: CommitRef,
}

/// Durable writer lease — SPEC §17.1 单写者.
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

/// Admission input for a v2 writer lease — SPEC §17.1, §35.5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeatLeaseRequest {
    pub lease_id: EntityId,
    pub seat_id: EntityId,
    pub holder_task_id: EntityId,
    /// The holder's writer epoch (> 0). A newer epoch always displaces an
    /// older current lease on the same seat.
    pub lease_epoch: u64,
    /// The exact input capability the holder verified for this seat right
    /// before acquiring (e.g. `tmux.send-keys:%42`, `ssh.pty:host:pane`).
    pub fence_capability: String,
}

/// Lifecycle of one durable seat question — SPEC §41.11.
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

/// The durable question shape — SPEC §41.11.
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

/// The durable answer shape — SPEC §41.11.
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

/// Durable seat question record — SPEC §41.11.
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
    /// Semantic revision of the question (SPEC §41.11). It moves only when
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

/// Lifecycle of one durable answer decision — SPEC §41.11.
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

/// Durable answer decision — SPEC §41.11 `AnswerDecision`.
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

/// The compare-and-swap expectation of one answer decision — SPEC §41.11.
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

/// The terminal outcome of one armed answer decision — SPEC §41.11.
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

fn current_lease(tx: &Transaction<'_>, seat_id: EntityId) -> Result<Option<SeatLease>> {
    let data: Option<String> = tx
        .query_row(
            "SELECT data FROM seat_leases WHERE seat_id=?1 AND state='current'",
            [seat_id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
}

/// The single active seat bound to `(host, pane_id)` — the invariant behind
/// `idx_seat_active_location`, evaluated inside the caller's transaction so a
/// competing registration can never slip between lookup and insert.
fn active_seat_at_pane(
    conn: &rusqlite::Connection,
    host: &str,
    pane_id: &str,
) -> Result<Option<SeatRecord>> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM seat_records WHERE status='active'
             AND json_extract(data,'$.location.host')=?1
             AND json_extract(data,'$.location.pane_id')=?2",
            [host, pane_id],
            |r| r.get(0),
        )
        .optional()?;
    data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
}

/// Refuse to admit a second active seat at `location`. The typed error names
/// the incumbent so the caller can retire/relocate it or bind to it after
/// reconciliation — never silently stacked on the same pane. A relocation may
/// replace its own predecessor at the same physical pane: the predecessor is
/// retired before the successor is inserted in the same transaction.
fn claim_seat_location(
    conn: &rusqlite::Connection,
    location: &SeatLocation,
    replacing: Option<EntityId>,
) -> Result<()> {
    // The pane invariant binds only tmux locations. SSH PTY seats have no
    // pane: their single-writer rule is the lease itself, verified at
    // acquisition and re-verified on every write.
    let (host, pane_id) = match location {
        SeatLocation::Tmux { host, pane_id, .. } => (host.as_str(), pane_id.as_str()),
        SeatLocation::SshPty { .. } => return Ok(()),
    };
    if let Some(incumbent) = active_seat_at_pane(conn, host, pane_id)? {
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
                "pane_id": pane_id,
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

    /// Arm the exact effect of a committed decision — SPEC §41.11 `T1 arm`.
    ///
    /// The arm records the digest of the bytes that are about to be written
    /// and the operation that carries them, and moves the question to
    /// `Responding`. From this commit on the effect *may* have reached the
    /// remote side, so recovery never re-sends it.
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
                let lease = read_lease(&tx, decision.lease_id)?
                    .ok_or_else(|| ShenkoError::not_found("arm_seat_answer_decision: unknown lease"))?;
                ensure!(
                    lease.is_current() && lease.is_writer_fence()
                        && lease.lease_epoch == decision.lease_epoch,
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
    /// SPEC §41.11 `E1/T2 回执`.
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

    /// Acquire the single writer lease for a seat. If another lease is
    /// current it is ended with `Superseded` — by §17.1 a new valid writer
    /// always displaces the old one, and the displaced holder's writes are
    /// refused by lease id afterwards.
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
                    ensure!(existing.seat_id == seat_id && existing.holder_task_id == holder_task_id,
                        "seat lease id already bound to different acquisition facts");
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
                if let Some(mut old_lease) = current_lease(&tx, seat_id)? {
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

    /// Write admission — SPEC §17.1, §35.5. Returns the *committed* lease when
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
    /// §17.1 requires keeping when a seat moves.
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

    /// Find a live seat at a given tmux pane id — the "find the existing
    /// work position again" lookup. Returns at most one *active* record; a
    /// retired record at the same locator is not resurrected.
    pub fn seat_at_pane(&self, host: &str, pane_id: &str) -> Result<Option<SeatRecord>> {
        active_seat_at_pane(&self.ledger.conn.lock(), host, pane_id)
    }

    /// Persist one observed seat question — SPEC §41.11. Every question is
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

    /// Commit the durable answer decision for one open question — SPEC §41.11.
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
