//! Ordered recovery-row catalog, parked-work projections and SQL row primitives.
//!
//! The column order and key encoding are archive contracts. These helpers use
//! the caller's transaction; they never publish a commit of their own.

use anyhow::{Result, anyhow};
use rusqlite::types::ValueRef;
use rusqlite::{OptionalExtension, Transaction, params};

use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::ObjectRef;
use shenko_types::task::{DispatchEligibility, TaskRecord, TaskState};

use crate::child::COGNITION_IMPORTED;
use crate::cognition_archive::{CognitionDerivative, CognitionRow};

use super::db;

/// How a replicated row is treated on import.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RowKind {
    /// The row is replicated as it is stored.
    Verbatim,
    /// A task. Imported work is parked on the receiving store.
    Task,
    /// A subject intent. The dispatcher lease is dropped and unfinished
    /// intents become explicitly unknown, so nothing is dispatched here.
    SubjectIntent,
    /// An inbox event. The pending input is recovery data; the local
    /// consumption cursor is not.
    InboxEvent,
    /// A tombstone-bearing fact. The tombstone state is monotone: a peer may
    /// retire a live local row, and a peer may never revive a retired one.
    /// The merge follows the tombstone, never arrival order.
    Monotonic,
    /// A fact carrying its own explicit revision. The peer wins only with a
    /// strictly greater revision — the record's own order decides, never
    /// which peer happened to answer first (no last-writer-wins
    /// on a durable record).
    Revisioned,
    /// A fact with a monotone state ladder. The peer may move the row
    /// forward along the ladder and never backward; a terminal state is
    /// never rewritten.
    Ladder,
}

/// One replicated table.
pub(super) struct TableSpec {
    pub(super) name: &'static str,
    /// Primary key columns.
    pub(super) key: &'static [&'static str],
    /// Replicated columns, in a fixed order.
    pub(super) columns: &'static [&'static str],
    /// Column the importing store owns; it receives the local commit seq.
    pub(super) local_seq: Option<&'static str>,
    pub(super) kind: RowKind,
}

/// The replicated tables, in the order the rows must be applied: a row may
/// only reference a row that already exists.
pub(super) const TABLES: &[TableSpec] = &[
    TableSpec {
        name: "identity_profiles",
        key: &["agent_id", "revision"],
        columns: &["agent_id", "revision", "profile", "created_at"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_genesis",
        key: &["genesis_id"],
        columns: &[
            "genesis_id",
            "subject_id",
            "bootstrap_id",
            "data",
            "created_at",
        ],
        local_seq: Some("created_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_branch_heads",
        key: &["agent_id"],
        columns: &[
            "agent_id",
            "branch_id",
            "committed_step_no",
            "committed_step_id",
            "next_step_id",
            "mode",
            "record_revision",
            "data",
            "created_at",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_head_history",
        key: &["snapshot_id"],
        columns: &["snapshot_id", "agent_id", "branch_id", "data"],
        local_seq: Some("received_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_branch_forks",
        key: &["branch_id"],
        columns: &["branch_id", "agent_id", "parent_branch_id", "data"],
        local_seq: Some("created_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_steps",
        key: &["step_id"],
        columns: &[
            "step_id",
            "branch_id",
            "step_no",
            "state",
            "record_revision",
            "data",
            "created_at",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_identity_receipts",
        key: &["step_id"],
        columns: &["step_id", "data"],
        // The receipt's local commit position follows the receiving store's
        // import commit. Its identity outcome, step binding and source
        // `data.committed_at` coordinate stay verbatim.
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    // Wait facts and receipts travel with the branch and their referenced
    // sealed inputs; their local commit positions belong to the receiver.
    TableSpec {
        name: "semantic_waits",
        key: &["wait_id"],
        columns: &["wait_id", "agent_id", "status", "data"],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "semantic_wait_receipts",
        key: &["step_id"],
        columns: &["step_id", "data"],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_event_dispositions",
        key: &["branch_id", "event_id"],
        columns: &[
            "branch_id",
            "event_id",
            "step_id",
            "disposition",
            "commit_seq",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_intent_steps",
        key: &["step_id"],
        columns: &["step_id", "migration"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_intents",
        key: &["intent_id"],
        columns: &[
            "intent_id",
            "step_id",
            "agent_id",
            "kind",
            "contract",
            "payload",
            "decision_ref",
            "state",
            "revision",
            "created_seq",
            "updated_seq",
            "result_ref",
        ],
        local_seq: None,
        kind: RowKind::SubjectIntent,
    },
    TableSpec {
        name: "bootstrap_records",
        key: &["bootstrap_id"],
        columns: &[
            "bootstrap_id",
            "installation_id",
            "subject_id",
            "state",
            "data",
            "created_at",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "agent_profiles",
        key: &["agent_id"],
        columns: &["agent_id", "data", "updated_at"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "memory_entries",
        key: &["entry_id"],
        columns: &[
            "entry_id",
            "agent_id",
            "kind",
            "content",
            "source",
            "applicability",
            "confidence",
            "superseded_by",
            "supersedes",
            "git_commit",
            "git_path",
            "created_at",
            "updated_at",
            "deleted",
            "source_ref",
            "scope",
            "record_revision",
            "created_at_ms",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "memory_intents",
        key: &["intent_id"],
        columns: &[
            "intent_id",
            "agent_id",
            "kind",
            "knowledge_kind",
            "supersedes_entry_id",
            "applicability",
            "confidence",
            "base_revision",
            "idempotency_key",
            "sources",
            "scope",
            "content",
            "content_ref",
            "entry_id",
            "state",
            "reason",
            "created_at_ms",
            "updated_at_ms",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "memory_intent_bindings",
        key: &["intent_id"],
        columns: &["intent_id", "data"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "raw_sessions",
        key: &["session_id"],
        columns: &["session_id", "agent_id", "started_at", "ended_at", "data"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "recovery_sources",
        key: &["source_id"],
        columns: &["source_id", "kind", "data"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "session_events",
        key: &["event_id"],
        columns: &[
            "event_id",
            "session_id",
            "seq",
            "event_type",
            "role",
            "data",
            "created_at",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_raw_inputs",
        key: &["step_id", "source_event_id"],
        columns: &[
            "step_id",
            "source_event_id",
            "raw_event_id",
            "session_id",
            "seq",
            "commit_seq",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "inbox_events",
        key: &["event_id"],
        columns: &[
            "event_id",
            "agent_id",
            "kind",
            "source",
            "consumed",
            "data",
            "created_at",
        ],
        local_seq: None,
        kind: RowKind::InboxEvent,
    },
    TableSpec {
        name: "tasks",
        key: &["task_id"],
        columns: &["task_id", "record_revision", "data", "created_at"],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Task,
    },
    TableSpec {
        name: "operations",
        key: &["operation_id"],
        columns: &["operation_id", "record_revision", "data", "created_at"],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "attempts",
        key: &["attempt_id"],
        columns: &["attempt_id", "operation_id", "data", "created_at"],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "evidence_receipts",
        key: &["receipt_id"],
        columns: &["receipt_id", "operation_id", "data", "created_at"],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    // Durable media truth: stream identity and latest state first, then the
    // immutable terminal notices and raw revision facts bound to it. Origin
    // CommitRefs travel verbatim: local import sequencing never changes an
    // already-issued event/receipt binding.
    TableSpec {
        name: "media_streams",
        key: &["stream_key"],
        columns: &[
            "stream_key",
            "agent_id",
            "provider",
            "account",
            "stream_id",
            "open_id",
            "open_event_id",
            "end_id",
            "end_event_id",
            "state",
            "opened_at_ms",
            "ended_at_ms",
            "end_reason",
            "implicit",
            "descriptor",
            "open_commit_ref",
        ],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "media_stream_ends",
        key: &["stream_key", "end_id"],
        columns: &[
            "stream_key",
            "end_id",
            "state",
            "ended_at_ms",
            "reason",
            "event_id",
            "admitted_at_ms",
            "admitted_commit_ref",
        ],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "media_segment_revisions",
        key: &["stream_key", "revision_id"],
        columns: &[
            "stream_key",
            "segment_key",
            "segment",
            "revision",
            "revision_id",
            "partial",
            "text",
            "captured_at_ms",
            "speaker_json",
            "audio_json",
            "provider_facts_json",
            "data",
            "disposition",
            "supersedes_revision",
            "superseded_by_revision",
            "event_id",
            "admitted_at_ms",
            "admitted_commit_ref",
        ],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    // Recording control and priority inputs travel with the same media
    // conflict unit: control/notice rows are immutable like terminal
    // notices; the recording row is a latest-state projection bound to the
    // event of its latest control.
    TableSpec {
        name: "media_recordings",
        key: &["stream_key", "recording_id"],
        columns: &[
            "stream_key",
            "recording_id",
            "agent_id",
            "state",
            "started_at_ms",
            "stopped_at_ms",
            "scope_json",
            "start_control_id",
            "stop_control_id",
            "reason",
            "event_id",
            "admitted_at_ms",
            "admitted_commit_ref",
        ],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "media_recording_controls",
        key: &["stream_key", "control_id"],
        columns: &[
            "stream_key",
            "control_id",
            "recording_id",
            "action",
            "state",
            "data",
            "event_id",
            "admitted_at_ms",
            "admitted_commit_ref",
        ],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "media_priority_inputs",
        key: &["stream_key", "priority_id"],
        columns: &[
            "stream_key",
            "priority_id",
            "kind",
            "text",
            "occurred_at_ms",
            "audio_json",
            "reason",
            "data",
            "event_id",
            "admitted_at_ms",
            "admitted_commit_ref",
        ],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "task_results",
        key: &["result_id"],
        columns: &[
            "result_id",
            "task_id",
            "agent_id",
            "kind",
            "data",
            "commit_seq",
            "created_at",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "context_window_scopes",
        key: &["scope_id"],
        columns: &["scope_id", "model_generation", "profile_digest"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "context_windows",
        key: &["window_id"],
        columns: &["window_id", "scope_id", "generation", "body", "commit_seq"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    // The authored working-note journal of a branch scope
    // One scope's journal is one
    // conflict unit, merged by `apply_context_notebook`.
    TableSpec {
        name: "context_notes",
        key: &["scope_id", "revision"],
        columns: &["scope_id", "revision", "body", "source", "written_at_ms", "visible"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    // The scope's explicit rollover request. Local control state: exported
    // only by the declared same-installation handover and installed only
    // when the importer declares one (see `LOCAL_CONTROL_CLASSES`).
    TableSpec {
        name: "context_rollover_requests",
        key: &["scope_id"],
        columns: &["scope_id", "request_id", "requested_at_ms", "consumed_window"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    // Research cognition: one epoch, its candidates, intervention bank
    // entries and reminders form ONE conflict unit (`epoch/…`); tombstones
    // and archive moves are row facts.
    TableSpec {
        name: "research_epochs",
        key: &["epoch_id"],
        columns: &[
            "epoch_id", "reset_request_id", "lineage_id", "epoch_no", "task_id",
            "status", "constraints_ref", "budget_ref", "effects_ref",
            "environment_ref", "best_candidate_id", "brief_ref",
            "reset_from_epoch", "reset_reason", "reset_receipt",
            "created_at_ms", "frozen_at_ms",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "research_candidates",
        key: &["candidate_id"],
        columns: &[
            "candidate_id", "epoch_id", "artifact_ref", "score", "score_source",
            "environment_ref", "notes", "frozen", "created_at_ms",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "intervention_bank_entries",
        key: &["entry_id"],
        columns: &[
            "entry_id", "epoch_id", "kind", "content", "source_refs", "scope",
            "confidence", "base_revision", "supersedes", "superseded_by",
            "carried_from", "tombstoned_at_ms", "created_at_ms",
        ],
        local_seq: None,
        // A bank entry carries its own tombstone: supersession is monotone,
        // so a peer may retire an entry but never revive one.
        kind: RowKind::Monotonic,
    },
    TableSpec {
        name: "intervention_reminders",
        key: &["reminder_id"],
        columns: &[
            "reminder_id", "epoch_id", "lineage_id", "decision", "policy",
            "policy_revision", "content", "provenance", "relevance", "scope",
            "expires_at_ms", "transient", "bank_entry_ids", "reason",
            "created_at_ms", "used_step_id", "used_at_ms",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "retention_tombstones",
        key: &["tombstone_id"],
        columns: &[
            "tombstone_id", "target_kind", "target_id", "scope", "reason",
            "effect_record", "deleted_at_ms",
        ],
        local_seq: None,
        // The row IS the tombstone: the first durable deletion wins and is
        // never replaced by a later, differently-worded one.
        kind: RowKind::Monotonic,
    },
    TableSpec {
        name: "archive_moves",
        key: &["archive_id"],
        columns: &[
            "archive_id", "source_ref", "target_provider", "target_namespace",
            "target_locator", "manifest_ref", "object_count", "total_bytes",
            "hash_verified", "readback_verified", "local_released",
            "local_release_note", "created_at_ms",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    // Durable browser submissions: the submission binding,
    // its durable event identity and the admitted payload are one immutable
    // fact. The admission sequence is the receiving store's own, never the
    // peer's — the binding, not the sequence, is the replicated fact.
    TableSpec {
        name: "web_input_submissions",
        key: &["agent_id", "submission_id"],
        columns: &[
            "agent_id", "submission_id", "event_id", "payload",
        ],
        local_seq: Some("accepted_commit_seq"),
        kind: RowKind::Verbatim,
    },
    // Withdrawn browser input: the withdrawal of one admitted submission,
    // with the durable event identity that cancelled it. The submission
    // and its withdrawals form ONE unit (`web_input/…`): a peer can never
    // hold a withdrawal without the submission it withdraws.
    TableSpec {
        name: "web_input_withdrawals",
        key: &["agent_id", "submission_id", "cancel_id"],
        columns: &[
            "agent_id", "submission_id", "cancel_id", "event_id", "result",
        ],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    // Durable output parts: append-only; the original commit sequence is
    // local to the admitting store.
    TableSpec {
        name: "subject_output_parts",
        key: &["agent_id", "stream_id", "message_id", "part_id", "chunk_seq"],
        columns: &[
            "agent_id", "stream_id", "request_id", "attempt_id", "message_id",
            "part_id", "part_revision", "chunk_seq", "terminal", "content",
            "content_kind",
        ],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    // Durable seats: a seat record, its leases, its durable
    // questions and the answer decisions on them form ONE conflict unit
    // (`seat/…`). A diverged peer can never graft a foreign lease or
    // decision onto a local seat.
    TableSpec {
        name: "seat_records",
        key: &["seat_id"],
        columns: &["seat_id", "task_id", "status", "supersedes", "data"],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "seat_leases",
        key: &["lease_id"],
        columns: &[
            "lease_id", "seat_id", "holder_task_id", "state", "end_reason",
            "superseded_by", "data",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "seat_questions",
        key: &["question_id"],
        columns: &["question_id", "seat_id", "task_id", "state", "data"],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "seat_answer_decisions",
        key: &["decision_id"],
        columns: &["decision_id", "question_id", "seat_id", "data"],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    // Provider quota facts: the admission facts are replicated,
    // the local admission SEQUENCE is not — the receiving store records its
    // own, so "the newest snapshot/decision" stays meaningful locally.
    // Requests and their sends group as `quota_request/…` units.
    TableSpec {
        name: "quota_snapshots",
        key: &["snapshot_id"],
        columns: &["snapshot_id", "provider_id", "account_scope_ref", "data"],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "quota_forecasts",
        key: &["forecast_id"],
        columns: &["forecast_id", "based_on_snapshot", "data"],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "quota_policies",
        key: &["policy_id", "revision"],
        columns: &["policy_id", "revision", "data"],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "quota_decisions",
        key: &["decision_id"],
        columns: &["decision_id", "policy_id", "policy_revision", "data"],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "quota_pacer",
        key: &["singleton"],
        columns: &["singleton", "data"],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "quota_attempts",
        key: &["request_id", "provider_id", "account_scope_ref", "window_key", "commit_seq"],
        columns: &["request_id", "provider_id", "account_scope_ref", "window_key", "commit_seq", "data"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "quota_sends",
        key: &["send_id"],
        // `claimed` travels with the send: an armed effect that the peer
        // already claimed must not be armed again here. `claimed_seq` is the
        // claiming store's own sequence and stays local.
        columns: &["send_id", "request_id", "send_seq", "claimed"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "quota_events",
        key: &["event_id"],
        columns: &["event_id", "provider_id", "account_scope_ref", "kind", "data"],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    // Channel runtime facts and record migrations: every fact keeps its
    // original sequences verbatim; `revision` orders the fact.
    TableSpec {
        name: "channel_runtime_facts",
        key: &["fact_id"],
        columns: &[
            "fact_id", "channel", "account", "kind", "state", "subject_ref",
            "detail", "revision", "created_seq", "updated_seq",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "channel_record_migrations",
        key: &["record_kind", "record_id"],
        columns: &[
            "record_kind", "record_id", "from_version", "to_version",
            "source_digest", "target_digest", "migrated_seq",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    // Activation records and the live selector (upgrade/activation.rs): the
    // selector is an armed effect — a peer advances it only with a strictly
    // greater selector revision, never by arrival order.
    TableSpec {
        name: "activation_records",
        key: &["activation_id"],
        columns: &["activation_id", "agent_id", "phase", "record_revision", "data"],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Revisioned,
    },
    TableSpec {
        name: "activation_selectors",
        key: &["agent_id"],
        columns: &["agent_id", "selector_revision", "active_epoch", "data"],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Revisioned,
    },
    TableSpec {
        name: "activation_launch_receipts",
        key: &["launch_id"],
        columns: &["launch_id", "agent_id", "selector_revision", "data"],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    // Playground artifacts (artifacts.rs): the artifact record, its
    // version manifests and the runtime instances bound to it. Divergent
    // peer revisions reconcile through the existing baseline/conflict-unit
    // path; a larger revision alone does not authorize an overwrite.
    TableSpec {
        name: "artifact_records",
        key: &["artifact_id"],
        columns: &[
            "artifact_id", "subject_id", "lifecycle", "state_revision",
            "record_revision", "data",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "artifact_manifests",
        key: &["artifact_id", "version_digest"],
        columns: &[
            "artifact_id", "version_digest", "record_revision", "data",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "artifact_runtime_instances",
        key: &["instance_id"],
        columns: &[
            "instance_id", "artifact_id", "state", "record_revision", "data",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    // Owner grants and the versioned configuration (config.rs): the
    // credential digest table (`owner_admin_tokens`) stays local and is
    // never replicated — a token hash is not a replicated fact.
    TableSpec {
        name: "owner_grants",
        key: &["installation_id"],
        columns: &[
            "installation_id",
            "grant_id",
            "claim_id",
            "record_revision",
            "data",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "config_entries",
        key: &["namespace", "logical_key"],
        columns: &[
            "namespace",
            "logical_key",
            "desired_revision",
            "record_revision",
            "data",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "config_changes",
        key: &["change_id"],
        columns: &[
            "change_id",
            "origin_identity",
            "logical_key",
            "namespace",
            "record_revision",
            "data",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "config_adoptions",
        key: &["namespace", "logical_key", "consumer_id"],
        columns: &[
            "namespace", "logical_key", "consumer_id", "adopted_revision",
            "record_revision", "data",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    // Contacts person model: every claim revision, every merge or split
    // event and the admin grants referencing a person form ONE conflict
    // unit (`person/{agent}/{person}`), so a peer can never graft claims
    // onto a diverged identity.
    TableSpec {
        name: "person_claims",
        key: &["claim_id"],
        columns: &[
            "claim_id", "slot_id", "agent_id", "person_id", "kind", "claim_key",
            "revision", "payload_json", "provenance_json", "confidence_json",
            "state", "scope_json", "created_at_ms",
        ],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "person_identity_events",
        key: &["event_id"],
        columns: &[
            "event_id", "agent_id", "kind", "source_person_id",
            "target_person_id", "moved_slot_ids_json", "returned_slot_ids_json",
            "reason", "provenance_json", "occurred_at_ms",
        ],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "admin_facts",
        key: &["agent_id", "subject_kind", "subject_key"],
        columns: &[
            "agent_id", "subject_kind", "subject_key", "granted", "granted_by",
            "reason", "updated_at_ms",
        ],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "life_voiceprint_ranks",
        key: &["agent_id", "recording_scope", "rank_id", "model_revision"],
        columns: &[
            "agent_id", "recording_scope", "rank_id", "model_revision",
            "cluster_id", "confirmed_identity", "data", "recorded_at_ms",
        ],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    // The durable stream journal: stream registration plus all
    // of its journaled units form ONE conflict unit (`stream/…`). Whole
    // units and chunks stay distinct shapes; the closing facts are part of
    // the unit, never rewritten.
    TableSpec {
        name: "content_streams",
        key: &["stream_id"],
        columns: &[
            "stream_id", "agent_id", "kind", "binding", "state",
            "opened_commit_ref", "closed_commit_ref",
        ],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "content_stream_units",
        key: &["stream_id", "seq"],
        columns: &["stream_id", "seq", "shape", "content_kind", "data", "terminal"],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    // The subject's own durable request attempts: one row per request, with
    // a monotone state ladder (armed -> dispatched -> terminal). The row is
    // armed BEFORE any wire send, so an armed row proves the request never
    // dispatched and a dispatched row without terminal evidence stays
    // recoverable debt.
    TableSpec {
        name: "subject_request_attempts",
        key: &["request_id"],
        columns: &[
            "request_id", "agent_id", "step_id", "attempt_id", "send_id",
            "state", "input_plan_ref", "payload_digest",
            "provider_binding_digest", "data", "created_commit_seq",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Ladder,
    },
    // The subject's obligations: open until a later committed step
    // completes or cancels them. Both end states are final.
    TableSpec {
        name: "subject_obligations",
        key: &["obligation_id"],
        columns: &[
            "obligation_id", "agent_id", "step_id", "origin_event_id",
            "description", "depends_on_task_id", "resume_condition", "due_at",
            "state", "decision_ref", "created_seq",
        ],
        local_seq: Some("updated_seq"),
        kind: RowKind::Ladder,
    },
    // The subject's own attention decisions: one row per intent. The row is
    // replaced only by a strictly newer decision, never by arrival order.
    TableSpec {
        name: "subject_attention_policies",
        key: &["intent_id"],
        columns: &[
            "intent_id", "agent_id", "step_id", "state", "sources", "reason",
            "review_after", "decision_ref", "created_seq",
        ],
        local_seq: Some("updated_seq"),
        kind: RowKind::Revisioned,
    },
    // Cognitive atoms: the canonical content registry, the
    // append-only attitude ledger, and the insert-only mapping that points at
    // a legacy row's original bytes. Contents come first, because the rows
    // after them reference a digest. `created_commit_seq` on an attitude is
    // the SOURCE commit and is preserved verbatim — never replaced by the
    // receiver's own sequence, which would rewrite when the subject recorded
    // it. Verbatim facts never merge over a committed local attitude row.
    TableSpec {
        name: "cognition_contents",
        key: &["digest"],
        columns: &[
            "digest", "bytes", "media_type", "store_namespace", "locator",
            "codec_name", "codec_major", "codec_digest", "object_json",
            "registered_at_ms",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "cognition_attitudes",
        key: &["attitude_id"],
        columns: &[
            "attitude_id", "agent_id", "digest", "role_kind", "role_json",
            "adoption_unit_id", "definition_slot", "supersedes_content",
            "reasons_json", "confidence_json", "knowledge_kind_hint",
            "supersedes", "created_commit_ledger", "created_commit_seq",
            "created_at_ms",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "cognition_legacy_content",
        key: &["source_table", "source_key"],
        columns: &[
            "source_table", "source_key", "content_digest", "media_type",
            "sealed_at_ms", "registered_commit_seq",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
];

pub(super) fn spec_for(table: &str) -> Option<&'static TableSpec> {
    TABLES.iter().find(|spec| spec.name == table)
}

/// The row as it will be stored here.
///
/// Imported work is parked: a receiving store is not a second live writer.
/// The original state is kept in a memo, so no fact is lost.
pub(super) fn effective_row(row: &CognitionRow, spec: &TableSpec) -> Result<CognitionRow> {
    let mut effective = row.clone();
    match spec.kind {
        RowKind::Verbatim
        | RowKind::InboxEvent
        | RowKind::Monotonic
        | RowKind::Revisioned
        | RowKind::Ladder => {}
        RowKind::SubjectIntent => {
            let state = row.value("state").unwrap_or_default().to_string();
            if state == "ready" || state == "claimed" {
                set_value(&mut effective, "state", Some("unknown".into()));
                let result_ref = row.value("result_ref").unwrap_or_default();
                if result_ref.is_empty() {
                    set_value(
                        &mut effective,
                        "result_ref",
                        Some(format!("imported:source_state={state}")),
                    );
                }
            }
        }
        RowKind::Task => {
            let data = row.value("data").unwrap_or_default().to_string();
            let (transformed, revision) = transform_task(&data)?;
            set_value(&mut effective, "data", Some(transformed));
            if let Some(revision) = revision {
                set_value(
                    &mut effective,
                    "record_revision",
                    Some(revision.to_string()),
                );
            }
        }
    }
    Ok(effective)
}

/// Park an imported task and report the revision the local record carries.
fn transform_task(data: &str) -> Result<(String, Option<u64>)> {
    match serde_json::from_str::<TaskRecord>(data) {
        Ok(mut task) => {
            if task.is_terminal() {
                return Ok((data.to_string(), None));
            }
            if task.memos.contains_key(COGNITION_IMPORTED)
                && matches!(
                    task.dispatch_eligibility,
                    DispatchEligibility::Parked { .. }
                )
            {
                // Already parked by an earlier import. Parking it again would
                // invent a new revision, so the record stays as it is.
                return Ok((data.to_string(), None));
            }
            let checkpoint = checkpoint_of(&task.state);
            let source_state = serde_json::to_string(&task.state)?;
            task.state = TaskState::Pending {
                checkpoint_ref: checkpoint,
            };
            task.dispatch_eligibility = DispatchEligibility::Parked {
                reason_ref: task.task_id,
                until_utc: None,
            };
            task.record_revision = task.record_revision.next();
            task.memos.insert(
                COGNITION_IMPORTED.into(),
                serde_json::json!({
                    "source_state": source_state,
                    "note": "imported work is parked on the receiving store",
                }),
            );
            Ok((serde_json::to_string(&task)?, Some(task.record_revision.0)))
        }
        Err(_) => {
            // The stored fact stays, but it must not become dispatchable here.
            let mut value: serde_json::Value = match serde_json::from_str(data) {
                Ok(value) => value,
                Err(_) => return Ok((data.to_string(), None)),
            };
            if !value.is_object() {
                return Ok((data.to_string(), None));
            }
            let task_id = value
                .get("task_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            value["state"] = serde_json::json!({"status": "pending", "checkpoint_ref": null});
            value["dispatch_eligibility"] = serde_json::json!({
                "status": "parked",
                "reason_ref": task_id,
                "until_utc": null,
            });
            if let Some(object) = value.as_object_mut() {
                let memos = object
                    .entry("memos")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(memos) = memos.as_object_mut() {
                    memos.insert(
                        COGNITION_IMPORTED.into(),
                        serde_json::json!({
                            "note": "imported work is parked on the receiving store"
                        }),
                    );
                }
            }
            Ok((serde_json::to_string(&value)?, None))
        }
    }
}

fn checkpoint_of(state: &TaskState) -> Option<ObjectRef> {
    match state {
        TaskState::Pending { checkpoint_ref } => checkpoint_ref.clone(),
        TaskState::Running { checkpoint_ref, .. } => checkpoint_ref.clone(),
        TaskState::Waiting { checkpoint_ref, .. } => checkpoint_ref.clone(),
        TaskState::Sleeping { checkpoint_ref, .. } => checkpoint_ref.clone(),
        TaskState::Completing { .. } | TaskState::Terminal { .. } => None,
    }
}

pub(super) fn set_value(row: &mut CognitionRow, column: &str, value: Option<String>) {
    for entry in row.values.iter_mut() {
        if entry.0 == column {
            entry.1 = value;
            return;
        }
    }
}

/// Read one replicated table.
pub(super) fn read_table(conn: &rusqlite::Connection, spec: &TableSpec) -> Result<Vec<CognitionRow>> {
    let columns = spec.columns.join(", ");
    let order = spec.key.join(", ");
    let sql = format!(
        "SELECT {columns} FROM {table} ORDER BY {order}",
        table = spec.name
    );
    let mut stmt = conn.prepare(&sql).map_err(db)?;
    let mut rows: Vec<CognitionRow> = Vec::new();
    let mut cursor = stmt.query([]).map_err(db)?;
    while let Some(row) = cursor.next().map_err(db)? {
        let mut values: Vec<(String, Option<String>)> = Vec::with_capacity(spec.columns.len());
        for index in 0..spec.columns.len() {
            values.push((spec.columns[index].to_string(), column_text(row, index)?));
        }
        let key = spec
            .key
            .iter()
            .map(|name| {
                values
                    .iter()
                    .find(|(column, _)| column == name)
                    .and_then(|(_, value)| value.clone())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("\u{1f}");
        rows.push(CognitionRow {
            table: spec.name.to_string(),
            key,
            values,
        });
    }
    Ok(rows)
}

pub(super) fn read_derivatives(conn: &rusqlite::Connection) -> Result<Vec<CognitionDerivative>> {
    let mut stmt = conn
        .prepare("SELECT window_id,idx,media_type,blake3,bytes FROM context_window_derivatives ORDER BY window_id,idx")
        .map_err(db)?;
    let mut out: Vec<CognitionDerivative> = Vec::new();
    let mut cursor = stmt.query([]).map_err(db)?;
    while let Some(row) = cursor.next().map_err(db)? {
        let window_id: String = row.get(0).map_err(db)?;
        out.push(CognitionDerivative {
            window_id: window_id
                .parse()
                .map_err(|_| anyhow!("invalid context window id {window_id}"))?,
            idx: row.get(1).map_err(db)?,
            media_type: row.get(2).map_err(db)?,
            blake3: row.get(3).map_err(db)?,
            bytes: row.get(4).map_err(db)?,
        });
    }
    Ok(out)
}

fn column_text(row: &rusqlite::Row<'_>, index: usize) -> Result<Option<String>> {
    match row.get_ref(index).map_err(db)? {
        ValueRef::Null => Ok(None),
        ValueRef::Integer(value) => Ok(Some(value.to_string())),
        ValueRef::Real(value) => Ok(Some(value.to_string())),
        ValueRef::Text(value) => Ok(Some(String::from_utf8_lossy(value).into_owned())),
        ValueRef::Blob(_) => Err(anyhow!(
            "column {index} holds binary data and is not replicated"
        )),
    }
}

/// The stored row with the same primary key, if it exists.
pub(super) fn read_existing(
    conn: &rusqlite::Connection,
    spec: &TableSpec,
    row: &CognitionRow,
) -> Result<Option<Vec<(String, Option<String>)>>> {
    let mut conditions: Vec<String> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    for (index, name) in spec.key.iter().enumerate() {
        conditions.push(format!("{name}=?{}", index + 1));
        keys.push(row.value(name).unwrap_or_default().to_string());
    }
    let sql = format!(
        "SELECT {} FROM {table} WHERE {conditions}",
        spec.columns.join(", "),
        table = spec.name,
        conditions = conditions.join(" AND ")
    );
    let mut stmt = conn.prepare(&sql).map_err(db)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(keys)).map_err(db)?;
    let mut found: Option<Vec<(String, Option<String>)>> = None;
    while let Some(row) = rows.next().map_err(db)? {
        let mut values: Vec<(String, Option<String>)> = Vec::with_capacity(spec.columns.len());
        for index in 0..spec.columns.len() {
            values.push((spec.columns[index].to_string(), column_text(row, index)?));
        }
        found = Some(values);
    }
    Ok(found)
}

/// Insert one replicated row.
pub(super) fn insert_row(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    row: &CognitionRow,
    seq: LedgerSeq,
) -> Result<()> {
    let mut columns: Vec<&str> = spec.columns.to_vec();
    let mut values: Vec<Option<String>> =
        row.values.iter().map(|(_, value)| value.clone()).collect();
    if let Some(local_seq) = spec.local_seq {
        columns.push(local_seq);
        values.push(Some(seq.0.to_string()));
    }
    let placeholders: Vec<String> = (1..=columns.len())
        .map(|index| format!("?{index}"))
        .collect();
    let sql = format!(
        "INSERT INTO {table} ({columns}) VALUES ({placeholders})",
        table = spec.name,
        columns = columns.join(", "),
        placeholders = placeholders.join(", ")
    );
    tx.execute(&sql, rusqlite::params_from_iter(values))
        .map_err(db)?;
    Ok(())
}

/// Take a newer value from the peer for a fact this store still holds as the
/// peer last delivered it.
pub(super) fn update_row(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    row: &CognitionRow,
    seq: LedgerSeq,
) -> Result<()> {
    let mut columns: Vec<&str> = spec.columns.to_vec();
    let mut values: Vec<Option<String>> =
        row.values.iter().map(|(_, value)| value.clone()).collect();
    if let Some(local_seq) = spec.local_seq {
        columns.push(local_seq);
        values.push(Some(seq.0.to_string()));
    }
    let assignments: Vec<String> = (1..=columns.len())
        .map(|index| format!("{}=?{index}", columns[index - 1]))
        .collect();
    let offset = columns.len();
    let conditions: Vec<String> = spec
        .key
        .iter()
        .enumerate()
        .map(|(index, name)| format!("{name}=?{}", offset + index + 1))
        .collect();
    let keys: Vec<String> = spec
        .key
        .iter()
        .map(|name| row.value(name).unwrap_or_default().to_string())
        .collect();
    let sql = format!(
        "UPDATE {table} SET {assignments} WHERE {conditions}",
        table = spec.name,
        assignments = assignments.join(", "),
        conditions = conditions.join(" AND ")
    );
    tx.execute(
        &sql,
        rusqlite::params_from_iter(values.into_iter().chain(keys.into_iter().map(Some))),
    )
    .map_err(db)?;
    Ok(())
}

/// The value the peer last delivered for one fact.
pub(super) fn read_baseline(
    tx: &Transaction<'_>,
    peer_id: EntityId,
    table: &str,
    key: &str,
) -> Result<Option<Vec<(String, Option<String>)>>> {
    let stored: Option<String> = tx
        .query_row(
            "SELECT row_values FROM cognition_baselines WHERE peer_id=?1 AND table_name=?2 AND entity_key=?3",
            params![peer_id.to_string(), table, key],
            |row| row.get(0),
        )
        .optional()
        .map_err(db)?;
    match stored {
        Some(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|error| anyhow!("invalid cognition baseline: {error}")),
        None => Ok(None),
    }
}

/// Remember the value the peer delivered for one fact.
pub(super) fn write_baseline(
    tx: &Transaction<'_>,
    peer_id: EntityId,
    table: &str,
    key: &str,
    values: &[(String, Option<String>)],
) -> Result<()> {
    let json = serde_json::to_string(values)?;
    tx.execute(
        "INSERT INTO cognition_baselines(peer_id,table_name,entity_key,row_values,updated_at)
         VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(peer_id,table_name,entity_key) DO UPDATE SET
            row_values=excluded.row_values,
            updated_at=excluded.updated_at",
        params![
            peer_id.to_string(),
            table,
            key,
            json,
            chrono::Utc::now().to_rfc3339(),
        ],
    )
    .map_err(db)?;
    Ok(())
}

pub(super) fn describe_difference(
    local: &[(String, Option<String>)],
    incoming: &[(String, Option<String>)],
) -> String {
    for (index, (name, value)) in incoming.iter().enumerate() {
        let other = local.get(index).and_then(|(_, other)| other.as_ref());
        if other != value.as_ref() {
            return format!("column {name} differs; the local value is kept");
        }
    }
    "the rows differ; the local value is kept".into()
}

/// Are the stored row and the imported row the same fact?
///
/// A subject intent carries a dispatch state. The receiving store adapts that
/// state, because it must not dispatch an intent it did not create, and it
/// remembers the state the source recorded. Both sides therefore hold the
/// same fact even when their local dispatch state differs.
pub(super) fn rows_equivalent(
    spec: &TableSpec,
    local: &[(String, Option<String>)],
    incoming: &[(String, Option<String>)],
) -> bool {
    if spec.kind == RowKind::SubjectIntent {
        return intent_equivalent(local, incoming);
    }
    let cursor = cursor_columns(spec);
    if cursor.is_empty() {
        return local == incoming;
    }
    if local.len() != incoming.len() {
        return false;
    }
    for (index, (name, value)) in incoming.iter().enumerate() {
        if cursor.contains(&name.as_str()) {
            // A local cursor, not a fact of the other store.
            continue;
        }
        let other = local.get(index).and_then(|(_, other)| other.as_ref());
        if other != value.as_ref() {
            return false;
        }
    }
    true
}

/// The columns that are a local cursor rather than a replicated fact.
fn cursor_columns(spec: &TableSpec) -> &'static [&'static str] {
    match spec.kind {
        RowKind::InboxEvent => &["consumed"],
        _ => &[],
    }
}

fn intent_equivalent(
    local: &[(String, Option<String>)],
    incoming: &[(String, Option<String>)],
) -> bool {
    if memory_row_value(local, "contract") == Some(crate::subject_state::OBLIGATION_TERMINAL_CONTRACT)
        || memory_row_value(incoming, "contract") == Some(crate::subject_state::OBLIGATION_TERMINAL_CONTRACT)
    {
        // Terminal settlements are immutable facts, not dispatch cursors.
        return local == incoming;
    }
    if intent_source_state(local) != intent_source_state(incoming) {
        return false;
    }
    if local.len() != incoming.len() {
        return false;
    }
    for (index, (name, value)) in incoming.iter().enumerate() {
        if name == "state" || name == "result_ref" {
            // The dispatch state is a local adaptation of the same fact.
            continue;
        }
        let other = local.get(index).and_then(|(_, other)| other.as_ref());
        if other != value.as_ref() {
            return false;
        }
    }
    true
}

/// Whether a row is an immutable terminal obligation settlement whose body
/// must never be replaced by a later import, even when the local value
/// matches its saved peer baseline.
pub(super) fn is_immutable_terminal_settlement(values: &[(String, Option<String>)]) -> bool {
    memory_row_value(values, "contract") == Some(crate::subject_state::OBLIGATION_TERMINAL_CONTRACT)
        && memory_row_value(values, "state") == Some("settled")
}

/// The dispatch state as the source recorded it.
fn intent_source_state(values: &[(String, Option<String>)]) -> Option<String> {
    let state = values
        .iter()
        .find(|(name, _)| name == "state")
        .and_then(|(_, value)| value.clone())?;
    let marker = values
        .iter()
        .find(|(name, _)| name == "result_ref")
        .and_then(|(_, value)| value.clone())
        .unwrap_or_default();
    if state == "unknown" {
        if let Some(source) = marker.strip_prefix("imported:source_state=") {
            return Some(source.to_string());
        }
    }
    Some(state)
}

pub(super) fn memory_row_value<'a>(values: &'a [(String, Option<String>)], name: &str) -> Option<&'a str> {
    values.iter().find(|(column, _)| column == name).and_then(|(_, value)| value.as_deref())
}
