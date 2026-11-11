//! SQLite migrations for Shenko store.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

pub fn run_migrations(conn: &Connection) -> Result<()> {
    // Step 1 (v1): base schema, incl. the legacy `memory_entries` table.
    conn.execute_batch(SCHEMA_V1)?;
    // 加法迁移不改写已有历史；旧重复步会显式阻止迁移，不能静默选赢家。
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS ledger_identity (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), ledger_id TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_commit_sequence ON commit_log(seq);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_subject_committed_step
            ON subject_steps(branch_id, step_no) WHERE state='committed';
        CREATE TABLE IF NOT EXISTS subject_event_dispositions (
            branch_id TEXT NOT NULL,
            event_id TEXT NOT NULL,
            step_id TEXT NOT NULL REFERENCES subject_steps(step_id),
            disposition TEXT NOT NULL,
            commit_seq INTEGER NOT NULL,
            PRIMARY KEY (branch_id, event_id)
        );
    ",
    )?;

    // Step 2 (memory v2): intent persistence plus the columns that the
    // memory store append/search path needs. `memory_entries` predates this
    // step with a narrower shape, so we extend it additively instead of
    // recreating it (that would discard already persisted memories).
    conn.execute_batch(MEMORY_INTENTS_SCHEMA)?;
    // A missing binding row is meaningful: legacy locator-only intents cannot
    // be silently treated as if their full object identity were known.
    conn.execute_batch(MEMORY_INTENT_BINDINGS_SCHEMA)?;
    ensure_memory_columns(conn)?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_memory_scope ON memory_entries(scope, created_at_ms);",
    )?;

    // Versioned instance identity (docs/specs/subject-runtime.md#spec-6): additive, never rewrites rows.
    conn.execute_batch(crate::identity::IDENTITY_SCHEMA)?;

    // Configuration, owner claim and Playground business state (docs/specs/configuration-playground.md#spec-43-1,
    // docs/specs/configuration-playground.md#spec-43-2, docs/specs/plugins.md#spec-44-4): additive, own tables only. These rows are ordinary
    // ledger facts; nothing here rewrites an existing installation.
    crate::config::ensure_config_schema(conn)?;
    crate::artifacts::ensure_artifact_schema(conn)?;

    // Committed context windows (docs/specs/context-policies.md#spec-12-3): additive, own tables only.
    conn.execute_batch(crate::context_windows::CONTEXT_WINDOWS_SCHEMA)?;
    ensure_column(conn, "context_window_scopes", "profile_digest", "TEXT")?;
    // Branch-journaled working notes (docs/specs/context-policies.md#spec-12-3-1): additive revision log.
    // Clearing is a revision, never a delete, so a reset stays recorded.
    conn.execute_batch(crate::context_notes::CONTEXT_NOTES_SCHEMA)?;

    // Subject raw input links (docs/specs/memory.md#spec-12-1): additive index over the raw
    // session events, never a second truth store.
    conn.execute_batch(crate::raw_session::RAW_SESSION_SCHEMA)?;

    // Raw-session FTS is a rebuildable projection, never another history
    // store. Committed rows remain readable through LIKE when FTS5 is absent.
    if let Err(e) = apply_session_event_fts(conn) {
        tracing::warn!(error = %e, "FTS5 unavailable; raw history search will use LIKE fallback");
    }

    // Step 3 (best effort): FTS5 index + sync triggers. FTS5 is optional in
    // the SQLite build; when it is missing, migrations must still succeed and
    // searches fall back to a LIKE scan.
    if let Err(e) = apply_memory_fts(conn) {
        tracing::warn!(error = %e, "FTS5 unavailable; memory search will use LIKE fallback");
    }
    conn.execute_batch(crate::intents::SCHEMA)?;
    // Step 4 (memory v3): open knowledge kinds separate from the operation
    // kind, provenance/applicability/confidence, supersession, Git knowledge
    // refs and the full request envelope (docs/specs/memory.md#spec-12-2, docs/specs/memory.md#spec-45-12, docs/specs/memory.md#spec-10-3, docs/specs/memory.md#spec-10-5).
    // This runs after the subject intent schema because the subject memory
    // migration reads subject_intents.
    ensure_memory_knowledge_columns(conn)?;
    ensure_memory_intent_envelope(conn)?;
    conn.execute_batch(crate::child::SCHEMA)?;
    conn.execute_batch(crate::runtime_boundary::SCHEMA)?;
    // Semantic waits (docs/specs/subject-runtime.md#spec-5-1, R143): additive, own tables only. The wait
    // history and its per step receipts are never rewritten by a migration.
    conn.execute_batch(crate::semantic_wait::SEMANTIC_WAIT_SCHEMA)?;
    // Cognition replication bookkeeping (docs/specs/nodes.md#spec-5-4). It is local state, not
    // a replicated fact, so it never enters a snapshot fingerprint.
    conn.execute_batch(crate::cognition::COGNITION_SCHEMA)?;
    // Cognitive atoms (docs/specs/cognition-units.md#spec-51-1): one canonical content registry and one
    // append-only attitude ledger, plus the insert-only mapping that points
    // at a legacy row's original bytes. Additive: no existing table gains a
    // column and no historical row is rewritten or backfilled here.
    conn.execute_batch(crate::atoms::COGNITION_ATOMS_SCHEMA)?;
    // Continuous-media transcript ingress (docs/specs/contacts.md#spec-15-2, docs/specs/media.md#spec-20): additive raw
    // revision facts plus effective-observation links; rows are never
    // rewritten or backfilled by this step.
    conn.execute_batch(crate::media::MEDIA_SCHEMA)?;
    // Never invent an old receipt from a stream's latest-state commit. Older
    // incomplete media facts stay retained, and recovery admission rejects
    // missing original receipt/delivery bindings instead of acknowledging them.
    ensure_column(conn, "media_streams", "open_commit_ref", "TEXT")?;
    ensure_column(conn, "media_segment_revisions", "admitted_commit_ref", "TEXT")?;
    // Durable provider-quota state (docs/specs/quota-resources.md#spec-42-5): snapshots, forecasts,
    // policies, decisions, the single-writer pacer and attempt ledger. All
    // writes happen inside the shared IMMEDIATE mutation line.
    conn.execute_batch(crate::quota::QUOTA_SCHEMA)?;
    // Durable browser input submission bindings (docs/specs/web-interface.md#spec-41-18). Additive,
    // never rewrites the original event or its admission fact.
    conn.execute_batch(crate::web_input::WEB_INPUT_SCHEMA)?;
    // Durable main-request receipts (docs/specs/subject-runtime.md#spec-41-5, docs/specs/durable-state.md#spec-41-4): the subject's own
    // generation arms an attempt before any wire send and records terminal
    // evidence independently of the caller. Additive, own table only.
    conn.execute_batch(crate::subject_request::SCHEMA)?;
    // Durable commitments the subject made about itself (docs/specs/subject-runtime.md#spec-13-8): the
    // obligations it is still carrying and its own attention decision.
    // Additive, own tables only.
    conn.execute_batch(crate::subject_state::SCHEMA)?;
    // Durable reconciliation cases for unknown external effects (docs/specs/durable-state.md#spec-41-4
    // X11). Additive, own table only: it records which immutable receipts a
    // reconciling operation already has and which explicit S1 decision (if
    // any) accepted the remaining uncertainty. No operation, attempt or
    // receipt row is rewritten by it.
    conn.execute_batch(crate::reconcile::SCHEMA)?;
    // Cognitive atoms: immutable content + append-only attitudes (docs/specs/cognition-units.md#spec-51-1).
    conn.execute_batch(crate::atoms::COGNITION_ATOMS_SCHEMA)?;
    // Durable ordered public-text output parts (docs/specs/web-interface.md#spec-21-11). Append-only;
    // no row is ever updated or deleted by a migration.
    conn.execute_batch(crate::output_parts::OUTPUT_PARTS_SCHEMA)?;
    // Refused receipt facts and the source-event scope lookup (SPEC X04,
    // docs/specs/plugins.md#spec-41-7-3). Additive, own table only: the index over the existing
    // receipts is not unique, because a store written before the admission
    // rule can already hold several receipts for one scope and no migration
    // may re-key or delete a durable receipt.
    conn.execute_batch(crate::receipt_admission::SCHEMA)?;
    ensure_column(conn, "cognition_receipts", "updated", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(
        conn,
        "cognition_file_baselines",
        "state",
        "TEXT NOT NULL DEFAULT 'present'",
    )?;
    // Durable seat records and single-writer leases (docs/specs/remote-seats.md#spec-17-1, docs/specs/remote-seats.md#spec-35-5):
    // additive, own tables only; location history is never rewritten.
    conn.execute_batch(crate::seat::SEAT_SCHEMA)?;
    // Durable stream journal (docs/specs/durable-state.md#content-stream-journal): whole units and chunks of every
    // source-bound stream, journaled with their delivery identity.
    conn.execute_batch(crate::content_stream::CONTENT_STREAM_SCHEMA)?;
    // Aggregate domain schemas (research, seats v2, contacts, config and
    // playground artifacts, channels, media v2).
    // Every statement is CREATE TABLE/INDEX IF NOT EXISTS: the archive
    // schema is the sum of all domains, so the tables exist on every store
    // regardless of which module first asked for them.
    conn.execute_batch(AGGREGATE_DOMAIN_SCHEMA)?;
    conn.execute_batch(crate::activation::SCHEMA)?;
    conn.execute_batch(crate::cognitive_root::COGNITIVE_ROOT_SCHEMA)?;
    // Seat schema v1 → v2 cutover (docs/specs/remote-seats.md#spec-17-1, docs/specs/remote-seats.md#spec-41-11): ONE explicit step.
    //   * `seat_answer_decisions` v1 carried `UNIQUE(question_id)`, which
    //     forbids a second decision for the same question even when the
    //     earlier one ended `not_sent` — a refusal the remote provably
    //     never applied. v2 keeps the uniqueness rule in the admitting
    //     transaction and drops the table constraint; the rows are kept.
    //   * A stored v1 lease carries no writer epoch and no verified fence
    //     capability, so it was never a writer fence under the v2 contract.
    //     The cutover marks those leases explicitly as legacy non-writers in
    //     their stored document, so the fact is visible in the ledger
    //     instead of being re-derived from a serde default at every read.
    //     The lease is not *ended* here: ending it is a lease transition
    //     that belongs to a ledger commit (the first v2 acquisition
    //     displaces it), and a migration must not fabricate one.
    migrate_seat_v2(conn)?;
    Ok(())
}

/// The one explicit seat v1 → v2 cutover.
///
/// Both steps are format migrations of *local* seat bookkeeping: no seat
/// location history, no question fact and no decision fact is dropped. The
/// old decision rows are copied into the rebuilt table, and the legacy
/// leases keep their identity, their holder and their history — only their
/// writability ends, with the reason recorded.
fn migrate_seat_v2(conn: &Connection) -> Result<()> {
    // 1. Rebuild `seat_answer_decisions` without the v1 UNIQUE(question_id).
    let has_unique: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE type='table' AND name='seat_answer_decisions' AND sql LIKE '%UNIQUE%'",
        [],
        |row| row.get(0),
    )?;
    if has_unique != 0 {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE seat_answer_decisions_v2(
                 decision_id TEXT PRIMARY KEY,
                 question_id TEXT NOT NULL,
                 seat_id TEXT NOT NULL,
                 data TEXT NOT NULL,
                 updated_commit_seq INTEGER NOT NULL
             );
             INSERT INTO seat_answer_decisions_v2(decision_id,question_id,seat_id,data,updated_commit_seq)
                 SELECT decision_id,question_id,seat_id,data,updated_commit_seq FROM seat_answer_decisions;
             DROP TABLE seat_answer_decisions;
             ALTER TABLE seat_answer_decisions_v2 RENAME TO seat_answer_decisions;
             CREATE INDEX IF NOT EXISTS idx_seat_decisions_question
                 ON seat_answer_decisions(question_id, updated_commit_seq);
             COMMIT;",
        )?;
    }
    // 1b. Bring the stored decision documents to the v2 shape. A v1
    //     decision recorded the confirmation as a plain string; the v2
    //     document keeps that string and derives the state from it, so the
    //     original fact survives the migration unchanged.
    conn.execute_batch(
        "UPDATE seat_answer_decisions
            SET data = json_set(
                    data,
                    '$.schema_version', 2,
                    '$.state', CASE json_extract(data,'$.confirmation')
                        WHEN 'confirmed' THEN 'answered'
                        WHEN 'unconfirmed' THEN 'answer_unknown'
                        WHEN 'rejected' THEN 'rejected'
                        WHEN 'not_sent' THEN 'not_sent'
                        ELSE 'committed' END,
                    '$.transport', json_extract(data,'$.transport'),
                    '$.question_revision', COALESCE(json_extract(data,'$.question_revision'),0),
                    '$.expected_question_revision', COALESCE(json_extract(data,'$.expected_question_revision'),0),
                    '$.lease_epoch', COALESCE(json_extract(data,'$.lease_epoch'),0),
                    '$.fence_capability', COALESCE(json_extract(data,'$.fence_capability'),'')
                )
          WHERE json_extract(data,'$.schema_version') IS NULL OR json_extract(data,'$.schema_version') < 2;",
    )?;
    // 2. Mark the current v1 leases as legacy non-writers: they never
    //    carried a writer epoch or a verified fence capability.
    conn.execute_batch(
        "UPDATE seat_leases
            SET data=json_set(data,'$.schema_version',1,'$.legacy_writer',json('true'))
          WHERE state='current'
            AND COALESCE(json_extract(data,'$.lease_epoch'),0)=0;",
    )?;
    // 3. Widen the one-active-seat-per-location index. v1 keyed it on
    //    `$.location.pane_id`, which only tmux seats carry; managed RPC seats
    //    anchor on `$.location.remote_session_id` instead. The index is a
    //    backstop for the in-transaction claim check, which already uses the
    //    same COALESCE expression; rebuilding it on old stores re-keys every
    //    existing active seat under the widened rule. Rows that claim no
    //    location (SSH PTY) still yield a NULL key and stay excluded, and
    //    the in-transaction claim can never have admitted a conflicting pair,
    //    so the rebuild cannot fail on legitimately stored data.
    let index_sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master
              WHERE type='index' AND name='idx_seat_active_location'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(sql) = index_sql {
        if !sql.contains("remote_session_id") {
            conn.execute_batch(
                "BEGIN;
                 DROP INDEX idx_seat_active_location;
                 CREATE UNIQUE INDEX idx_seat_active_location ON seat_records(
                     json_extract(data,'$.location.host'),
                     COALESCE(json_extract(data,'$.location.pane_id'),
                              json_extract(data,'$.location.remote_session_id'))
                 ) WHERE status='active';
                 COMMIT;",
            )?;
        }
    }
    Ok(())
}

/// Every domain's additive tables, applied unconditionally on open so the
/// archive's fact classes exist wherever the store does. The module that
/// owns a domain applies the same CREATE IF NOT EXISTS statements; running
/// them here too is idempotent and keeps open-order independent.
const AGGREGATE_DOMAIN_SCHEMA: &str = r#"
-- Research cognition (research.rs: epochs, candidates, intervention bank,
-- reminders, retention tombstones, archive moves).
CREATE TABLE IF NOT EXISTS research_epochs (
    epoch_id TEXT PRIMARY KEY,
    reset_request_id TEXT,
    lineage_id TEXT NOT NULL,
    epoch_no INTEGER NOT NULL,
    task_id TEXT,
    status TEXT NOT NULL,
    constraints_ref TEXT,
    budget_ref TEXT,
    effects_ref TEXT,
    environment_ref TEXT,
    best_candidate_id TEXT,
    brief_ref TEXT,
    reset_from_epoch TEXT,
    reset_reason TEXT,
    reset_receipt TEXT,
    created_at_ms INTEGER NOT NULL,
    frozen_at_ms INTEGER
);
CREATE INDEX IF NOT EXISTS research_epochs_lineage ON research_epochs(lineage_id, epoch_no);
CREATE UNIQUE INDEX IF NOT EXISTS research_epochs_reset_request ON research_epochs(reset_request_id)
    WHERE reset_request_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS research_candidates (
    candidate_id TEXT PRIMARY KEY,
    epoch_id TEXT NOT NULL,
    artifact_ref TEXT NOT NULL,
    score REAL,
    score_source TEXT,
    environment_ref TEXT,
    notes TEXT,
    frozen INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS research_candidates_epoch ON research_candidates(epoch_id, created_at_ms);
CREATE TABLE IF NOT EXISTS intervention_bank_entries (
    entry_id TEXT PRIMARY KEY,
    epoch_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    content TEXT NOT NULL,
    source_refs TEXT NOT NULL,
    scope TEXT NOT NULL,
    confidence REAL,
    base_revision INTEGER NOT NULL DEFAULT 0,
    supersedes TEXT,
    superseded_by TEXT,
    carried_from TEXT,
    tombstoned_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS intervention_bank_epoch ON intervention_bank_entries(epoch_id, kind, created_at_ms);
CREATE TABLE IF NOT EXISTS intervention_reminders (
    reminder_id TEXT PRIMARY KEY,
    epoch_id TEXT NOT NULL,
    lineage_id TEXT NOT NULL,
    decision TEXT NOT NULL,
    policy TEXT NOT NULL,
    policy_revision TEXT NOT NULL,
    content TEXT,
    provenance TEXT NOT NULL,
    relevance TEXT,
    scope TEXT NOT NULL,
    expires_at_ms INTEGER,
    transient INTEGER NOT NULL DEFAULT 1,
    bank_entry_ids TEXT NOT NULL,
    reason TEXT,
    created_at_ms INTEGER NOT NULL,
    used_step_id TEXT,
    used_at_ms INTEGER
);
CREATE INDEX IF NOT EXISTS intervention_reminders_epoch ON intervention_reminders(epoch_id, created_at_ms);
CREATE TABLE IF NOT EXISTS retention_tombstones (
    tombstone_id TEXT PRIMARY KEY,
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    scope TEXT NOT NULL,
    reason TEXT NOT NULL,
    effect_record TEXT NOT NULL,
    deleted_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS retention_tombstones_target ON retention_tombstones(target_kind, target_id);
CREATE TABLE IF NOT EXISTS archive_moves (
    archive_id TEXT PRIMARY KEY,
    source_ref TEXT NOT NULL,
    target_provider TEXT NOT NULL,
    target_namespace TEXT NOT NULL,
    target_locator TEXT NOT NULL,
    manifest_ref TEXT NOT NULL,
    object_count INTEGER NOT NULL,
    total_bytes INTEGER NOT NULL,
    hash_verified INTEGER NOT NULL,
    readback_verified INTEGER NOT NULL,
    local_released INTEGER NOT NULL DEFAULT 0,
    local_release_note TEXT,
    created_at_ms INTEGER NOT NULL
);
-- Durable seat questions and answer decisions (seat.rs docs/specs/remote-seats.md#spec-41-11).
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
CREATE TABLE IF NOT EXISTS seat_answer_decisions(
    decision_id TEXT PRIMARY KEY,
    question_id TEXT NOT NULL,
    seat_id TEXT NOT NULL,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_seat_decisions_question
    ON seat_answer_decisions(question_id, updated_commit_seq);
-- Contacts person model (contacts.rs): claim revisions, merge/split events,
-- admin grants and Life voiceprint ranks.
CREATE TABLE IF NOT EXISTS person_claims (
    claim_id TEXT PRIMARY KEY,
    slot_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    person_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    claim_key TEXT NOT NULL,
    revision INTEGER NOT NULL,
    payload_json TEXT NOT NULL,
    provenance_json TEXT NOT NULL,
    confidence_json TEXT NOT NULL,
    state TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    commit_seq INTEGER NOT NULL,
    UNIQUE(slot_id, revision)
);
CREATE INDEX IF NOT EXISTS idx_person_claims_person
    ON person_claims(agent_id, person_id);
CREATE INDEX IF NOT EXISTS idx_person_claims_slot
    ON person_claims(agent_id, slot_id);
CREATE INDEX IF NOT EXISTS idx_person_claims_kind_key
    ON person_claims(agent_id, kind, claim_key);
CREATE TABLE IF NOT EXISTS person_identity_events (
    event_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    source_person_id TEXT NOT NULL,
    target_person_id TEXT NOT NULL,
    moved_slot_ids_json TEXT NOT NULL,
    returned_slot_ids_json TEXT NOT NULL,
    reason TEXT,
    provenance_json TEXT NOT NULL,
    occurred_at_ms INTEGER NOT NULL,
    commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_person_identity_events_agent
    ON person_identity_events(agent_id, occurred_at_ms, commit_seq);
CREATE INDEX IF NOT EXISTS idx_person_identity_events_person
    ON person_identity_events(agent_id, source_person_id, target_person_id);
CREATE TABLE IF NOT EXISTS admin_facts (
    agent_id TEXT NOT NULL,
    subject_kind TEXT NOT NULL,
    subject_key TEXT NOT NULL,
    granted INTEGER NOT NULL,
    granted_by TEXT NOT NULL,
    reason TEXT,
    updated_at_ms INTEGER NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (agent_id, subject_kind, subject_key)
);
CREATE TABLE IF NOT EXISTS life_voiceprint_ranks (
    agent_id TEXT NOT NULL,
    recording_scope TEXT NOT NULL,
    rank_id TEXT NOT NULL,
    model_revision TEXT NOT NULL,
    cluster_id TEXT,
    confirmed_identity TEXT,
    data TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (agent_id, recording_scope, rank_id, model_revision)
);
CREATE INDEX IF NOT EXISTS idx_life_ranks_rank
    ON life_voiceprint_ranks(agent_id, rank_id, model_revision);
-- Owner grants, config entries/changes/adoptions (config.rs). The
-- credential digest table owner_admin_tokens stays local and is never
-- replicated.
CREATE TABLE IF NOT EXISTS owner_grants (
    installation_id TEXT PRIMARY KEY,
    grant_id TEXT NOT NULL,
    claim_id TEXT NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS owner_admin_tokens (
    installation_id TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL,
    issued_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS config_entries (
    namespace TEXT NOT NULL,
    logical_key TEXT NOT NULL,
    desired_revision INTEGER NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (namespace, logical_key)
);
CREATE TABLE IF NOT EXISTS config_changes (
    change_id TEXT PRIMARY KEY,
    origin_identity TEXT NOT NULL,
    logical_key TEXT NOT NULL,
    namespace TEXT NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_config_changes_origin
    ON config_changes(origin_identity, logical_key);
CREATE TABLE IF NOT EXISTS config_adoptions (
    namespace TEXT NOT NULL,
    logical_key TEXT NOT NULL,
    consumer_id TEXT NOT NULL,
    adopted_revision INTEGER NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (namespace, logical_key, consumer_id)
);
-- Playground artifact records, manifests and runtime instances
-- (artifacts.rs).
CREATE TABLE IF NOT EXISTS artifact_records (
    artifact_id TEXT PRIMARY KEY,
    subject_id TEXT NOT NULL,
    lifecycle TEXT NOT NULL,
    state_revision TEXT NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_artifact_records_subject ON artifact_records(subject_id);
CREATE TABLE IF NOT EXISTS artifact_manifests (
    artifact_id TEXT NOT NULL,
    version_digest TEXT NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (artifact_id, version_digest)
);
CREATE TABLE IF NOT EXISTS artifact_runtime_instances (
    instance_id TEXT PRIMARY KEY,
    artifact_id TEXT NOT NULL,
    state TEXT NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_artifact_runtime_artifact
    ON artifact_runtime_instances(artifact_id, state);
-- Channel runtime facts and record migrations (channel.rs).
CREATE TABLE IF NOT EXISTS channel_runtime_facts(
 fact_id TEXT PRIMARY KEY,
 channel TEXT NOT NULL,
 account TEXT NOT NULL,
 kind TEXT NOT NULL,
 state TEXT NOT NULL,
 subject_ref TEXT,
 detail TEXT NOT NULL,
 revision INTEGER NOT NULL DEFAULT 0,
 created_seq INTEGER NOT NULL,
 updated_seq INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS channel_runtime_facts_pending
 ON channel_runtime_facts(channel,kind,state,created_seq);
CREATE TABLE IF NOT EXISTS channel_record_migrations(
 record_kind TEXT NOT NULL,
 record_id TEXT NOT NULL,
 from_version INTEGER NOT NULL,
 to_version INTEGER NOT NULL,
 source_digest TEXT NOT NULL,
 target_digest TEXT NOT NULL,
 migrated_seq INTEGER NOT NULL,
 PRIMARY KEY(record_kind,record_id));
-- Recording control and priority inputs (media.rs v2): the latest-state
-- recording projection plus its immutable control notices.
CREATE TABLE IF NOT EXISTS media_recordings (
    stream_key TEXT NOT NULL REFERENCES media_streams(stream_key),
    recording_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    state TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    stopped_at_ms INTEGER,
    scope_json TEXT NOT NULL,
    start_control_id TEXT NOT NULL,
    stop_control_id TEXT,
    reason TEXT,
    event_id TEXT NOT NULL,
    admitted_at_ms INTEGER NOT NULL,
    admitted_commit_ref TEXT NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (stream_key, recording_id)
);
CREATE INDEX IF NOT EXISTS idx_media_recordings_event
    ON media_recordings(event_id);
CREATE TABLE IF NOT EXISTS media_recording_controls (
    stream_key TEXT NOT NULL REFERENCES media_streams(stream_key),
    control_id TEXT NOT NULL,
    recording_id TEXT NOT NULL,
    action TEXT NOT NULL,
    state TEXT NOT NULL,
    data TEXT NOT NULL,
    event_id TEXT NOT NULL,
    admitted_at_ms INTEGER NOT NULL,
    admitted_commit_ref TEXT NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (stream_key, control_id)
);
CREATE INDEX IF NOT EXISTS idx_media_recording_controls_event
    ON media_recording_controls(event_id);
CREATE TABLE IF NOT EXISTS media_priority_inputs (
    stream_key TEXT NOT NULL REFERENCES media_streams(stream_key),
    priority_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    text TEXT,
    occurred_at_ms INTEGER,
    audio_json TEXT,
    reason TEXT,
    data TEXT NOT NULL,
    event_id TEXT NOT NULL,
    admitted_at_ms INTEGER NOT NULL,
    admitted_commit_ref TEXT NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (stream_key, priority_id)
);
CREATE INDEX IF NOT EXISTS idx_media_priority_event
    ON media_priority_inputs(event_id);
-- Provider quota events (quota.rs): every event names the fact it
-- describes, and it is written in the same transaction as that fact.
CREATE TABLE IF NOT EXISTS quota_events (
    event_id TEXT PRIMARY KEY,
    provider_id TEXT NOT NULL,
    account_scope_ref TEXT NOT NULL,
    kind TEXT NOT NULL,
    data TEXT NOT NULL,
    commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_quota_events_scope ON quota_events(account_scope_ref, commit_seq);
-- The subject's own durable request attempts (subject_request.rs): the row
-- is armed BEFORE any wire send, the broker's send boundary flips it to
-- dispatched exactly once, and the terminal provider evidence ends it.
CREATE TABLE IF NOT EXISTS subject_request_attempts (
    request_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    step_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    send_id TEXT,
    state TEXT NOT NULL,
    input_plan_ref TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    provider_binding_digest TEXT NOT NULL,
    data TEXT NOT NULL,
    created_commit_seq INTEGER NOT NULL,
    updated_commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_subject_request_step
    ON subject_request_attempts(agent_id, step_id);
-- The subject's obligations and its own attention decisions
-- (subject_state.rs): an obligation stays open until a later committed
-- step completes or cancels it.
CREATE TABLE IF NOT EXISTS subject_obligations(
 obligation_id TEXT PRIMARY KEY, agent_id TEXT NOT NULL, step_id TEXT NOT NULL,
 origin_event_id TEXT, description TEXT NOT NULL, depends_on_task_id TEXT,
 resume_condition TEXT NOT NULL, due_at TEXT, state TEXT NOT NULL,
 decision_ref TEXT NOT NULL, created_seq INTEGER NOT NULL, updated_seq INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS subject_obligations_open
 ON subject_obligations(agent_id,state,created_seq,obligation_id);
CREATE TABLE IF NOT EXISTS subject_attention_policies(
 intent_id TEXT PRIMARY KEY, agent_id TEXT NOT NULL, step_id TEXT NOT NULL,
 state TEXT NOT NULL, sources TEXT NOT NULL, reason TEXT NOT NULL,
 review_after TEXT, decision_ref TEXT NOT NULL,
 created_seq INTEGER NOT NULL, updated_seq INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS subject_attention_agent
 ON subject_attention_policies(agent_id,created_seq,intent_id);
-- Withdrawn browser input (web_input.rs): the withdrawal of one admitted
-- submission, with the durable event identity that cancelled it.
CREATE TABLE IF NOT EXISTS web_input_withdrawals (
    agent_id TEXT NOT NULL,
    submission_id TEXT NOT NULL,
    cancel_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    result TEXT NOT NULL,
    commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (agent_id, submission_id, cancel_id)
);
CREATE INDEX IF NOT EXISTS idx_web_withdrawals_event
    ON web_input_withdrawals(event_id);
"#;

/// Add memory-v2 columns to an existing `memory_entries` table.
/// SQLite has no `ADD COLUMN IF NOT EXISTS`, so each column is checked first.
fn ensure_memory_columns(conn: &Connection) -> Result<()> {
    ensure_column(conn, "memory_entries", "source_ref", "TEXT")?;
    ensure_column(
        conn,
        "memory_entries",
        "scope",
        "TEXT NOT NULL DEFAULT 'personal'",
    )?;
    ensure_column(
        conn,
        "memory_entries",
        "record_revision",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "memory_entries",
        "created_at_ms",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(conn, "memory_entries", "updated_commit_seq", "INTEGER")?;

/// Backfill millisecond timestamps for rows written by the legacy schema.
    conn.execute_batch(
        "UPDATE memory_entries
            SET created_at_ms = CAST(strftime('%s', created_at) AS INTEGER) * 1000
          WHERE created_at_ms = 0 AND created_at IS NOT NULL;",
    )?;
    Ok(())
}

/// Memory-v3 entry columns: supersession, confidence and Git knowledge refs
/// (ArchMemory). Additive; the operation kind moves to the intent row.
fn ensure_memory_knowledge_columns(conn: &Connection) -> Result<()> {
    ensure_column(conn, "memory_entries", "confidence", "REAL")?;
    ensure_column(conn, "memory_entries", "superseded_by", "TEXT")?;
    ensure_column(conn, "memory_entries", "supersedes", "TEXT")?;
    ensure_column(conn, "memory_entries", "git_commit", "TEXT")?;
    ensure_column(conn, "memory_entries", "git_path", "TEXT")?;
    // Unify operation kind and knowledge kind without losing facts: the
    // operation stays on the memory_intents row that produced the entry, and
    // the entry's knowledge kind becomes the open "note" vocabulary.
    conn.execute_batch(
        "UPDATE memory_entries SET kind='note' WHERE kind IN ('save','correct','recall','forget');",
    )?;
    Ok(())
}

/// Memory-v3 intent envelope: the request's knowledge kind, applicability,
/// confidence, base revision, idempotency identity, provenance and the
/// supersession target (ArchMemory). Additive.
fn ensure_memory_intent_envelope(conn: &Connection) -> Result<()> {
    ensure_column(conn, "memory_intents", "knowledge_kind", "TEXT NOT NULL DEFAULT 'note'")?;
    ensure_column(conn, "memory_intents", "applicability", "TEXT")?;
    ensure_column(conn, "memory_intents", "confidence", "REAL")?;
    ensure_column(conn, "memory_intents", "base_revision", "INTEGER")?;
    ensure_column(conn, "memory_intents", "idempotency_key", "TEXT")?;
    ensure_column(conn, "memory_intents", "sources", "TEXT NOT NULL DEFAULT '[]'")?;
    ensure_column(conn, "memory_intents", "supersedes_entry_id", "TEXT")?;
    // Subject-owned memory facts recorded the knowledge kind 'note' in the
    // operation column; the operation is 'save' and the knowledge kind stays
    // 'note'. A missing binding sidecar is that representation's intentional
    // shape, never a corruption signal.
    conn.execute_batch(
        "UPDATE memory_intents SET kind='save', knowledge_kind='note'
          WHERE kind='note' AND intent_id IN
                (SELECT intent_id FROM subject_intents WHERE kind='memory');",
    )?;
    Ok(())
}

pub(crate) fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if !columns.iter().any(|c| c == column) {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
    }
    Ok(())
}

/// Create the FTS5 index and its sync triggers, rebuilding when the index is
/// new or fails its external-content integrity check against `memory_entries`.
///
/// CJK limitation (honest note): the `unicode61` tokenizer splits on
/// non-alphanumeric characters, and a run of Han characters with no spaces is
/// a single token. A Chinese substring such as "深刻" therefore does not match
/// the FTS token for "我的深刻记忆"; `search_memory` routes CJK queries to the
/// LIKE path below, which matches substrings and is exercised by the tests.
fn apply_memory_fts(conn: &Connection) -> Result<()> {
    let existed: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='memory_entries_fts'",
        [],
        |r| r.get(0),
    )?;
    conn.execute_batch(FTS_SCHEMA)?;

    // Rebuild when the index is new, or when its external-content integrity
    // check fails. A crash between table creation and the first rebuild (or a
    // table created by an older build without sync triggers) would otherwise
    // leave committed memories missing from every FTS search forever;
    // reopening the store repairs the index. A plain `count(*)` cannot detect
    // this: for an external-content table it scans `memory_entries` itself.
    let stale = existed != 0 && fts_index_is_stale(conn, "memory_entries_fts");
    if existed == 0 || stale {
        conn.execute_batch(
            "INSERT INTO memory_entries_fts(memory_entries_fts) VALUES('rebuild');",
        )?;
    }
    Ok(())
}

fn apply_session_event_fts(conn: &Connection) -> Result<()> {
    let existed: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='session_events_fts'",
        [],
        |r| r.get(0),
    )?;
    conn.execute_batch(SESSION_EVENT_FTS_SCHEMA)?;
    let stale = existed != 0 && fts_index_is_stale(conn, "session_events_fts");
    if existed == 0 || stale {
        conn.execute_batch(
            "INSERT INTO session_events_fts(session_events_fts) VALUES('rebuild');",
        )?;
    }
    Ok(())
}

/// Ask FTS5 to compare an index with its external content table.
/// The `rank` argument `1` selects the external-content consistency check; it
/// fails when the index is missing rows the content table has (or vice versa).
fn fts_index_is_stale(conn: &Connection, table: &str) -> bool {
    conn.execute(
        &format!("INSERT INTO {table}({table}, rank) VALUES('integrity-check', 1)"),
        [],
    )
    .is_err()
}

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL,
    applied_at TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT OR IGNORE INTO schema_version (version) VALUES (1);

CREATE TABLE IF NOT EXISTS commit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ledger_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    incarnation TEXT NOT NULL,
    entity_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (ledger_id, seq)
);

CREATE TABLE IF NOT EXISTS tasks (
    task_id TEXT PRIMARY KEY,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS operations (
    operation_id TEXT PRIMARY KEY,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS attempts (
    attempt_id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS evidence_receipts (
    receipt_id TEXT PRIMARY KEY,
    operation_id TEXT,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_evidence_operation ON evidence_receipts(operation_id);
CREATE INDEX IF NOT EXISTS idx_operations_name ON operations(json_extract(data,'$.operation_name'));

CREATE TABLE IF NOT EXISTS agent_profiles (
    agent_id TEXT PRIMARY KEY,
    data TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS raw_sessions (
    session_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    data TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS session_events (
    event_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES raw_sessions(session_id),
    seq INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    role TEXT NOT NULL,
    data TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (session_id, seq)
);

CREATE TABLE IF NOT EXISTS memory_entries (
    entry_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    content TEXT NOT NULL,
    source TEXT,
    applicability TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    deleted INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_memory_agent ON memory_entries(agent_id, deleted);
CREATE INDEX IF NOT EXISTS idx_memory_kind ON memory_entries(kind);
CREATE INDEX IF NOT EXISTS idx_session_events_session ON session_events(session_id, seq);
CREATE INDEX IF NOT EXISTS idx_tasks_state ON tasks(task_id);
CREATE INDEX IF NOT EXISTS idx_ops_task ON operations(operation_id);

CREATE TABLE IF NOT EXISTS subject_branch_heads (
    agent_id TEXT PRIMARY KEY,
    branch_id TEXT NOT NULL,
    committed_step_no INTEGER NOT NULL DEFAULT 0,
    committed_step_id TEXT,
    next_step_id TEXT NOT NULL,
    mode TEXT NOT NULL DEFAULT 'active',
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS subject_steps (
    step_id TEXT PRIMARY KEY,
    branch_id TEXT NOT NULL,
    step_no INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'planned',
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS subject_genesis (
    genesis_id TEXT PRIMARY KEY,
    subject_id TEXT NOT NULL UNIQUE,
    bootstrap_id TEXT NOT NULL,
    data TEXT NOT NULL,
    created_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS bootstrap_records (
    bootstrap_id TEXT PRIMARY KEY,
    installation_id TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'preparing',
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS inbox_events (
    event_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    source TEXT NOT NULL,
    consumed INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_inbox_agent ON inbox_events(agent_id, consumed, created_at);
"#;

/// Memory-v2 tables: durable intent queue (docs/specs/memory.md#spec-12-2) and supporting indexes.
/// Never contains FTS statements, so this step always succeeds.
const MEMORY_INTENTS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memory_intents (
    intent_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    scope TEXT NOT NULL DEFAULT 'personal',
    content TEXT NOT NULL DEFAULT '',
    content_ref TEXT,
    entry_id TEXT,
    state TEXT NOT NULL DEFAULT 'accepted',
    reason TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_memory_intents_state ON memory_intents(state, created_at_ms);

INSERT OR IGNORE INTO schema_version (version) VALUES (2);
"#;

/// Exact object-reference identity for newly admitted library memory intents.
/// Rows are additive; old locator-only history is preserved without backfill.
const MEMORY_INTENT_BINDINGS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memory_intent_bindings (
    intent_id TEXT PRIMARY KEY,
    data TEXT NOT NULL
);
"#;

/// FTS5 index over memory content. Creation is best-effort: some SQLite builds
/// (and the bundled feature in some configurations) omit FTS5.
const FTS_SCHEMA: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS memory_entries_fts USING fts5(
    content,
    content='memory_entries',
    content_rowid='rowid',
    tokenize='unicode61'
);
CREATE TRIGGER IF NOT EXISTS memory_entries_ai AFTER INSERT ON memory_entries BEGIN
    INSERT INTO memory_entries_fts(rowid, content) VALUES (new.rowid, new.content);
END;
CREATE TRIGGER IF NOT EXISTS memory_entries_ad AFTER DELETE ON memory_entries BEGIN
    INSERT INTO memory_entries_fts(memory_entries_fts, rowid, content)
        VALUES ('delete', old.rowid, old.content);
END;
CREATE TRIGGER IF NOT EXISTS memory_entries_au AFTER UPDATE ON memory_entries BEGIN
    INSERT INTO memory_entries_fts(memory_entries_fts, rowid, content)
        VALUES ('delete', old.rowid, old.content);
    INSERT INTO memory_entries_fts(rowid, content) VALUES (new.rowid, new.content);
END;
"#;

/// The raw payload is indexed verbatim. Search results always join back to
/// session_events and retain its session, seq, identity and original data.
const SESSION_EVENT_FTS_SCHEMA: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS session_events_fts USING fts5(
    data,
    content='session_events',
    content_rowid='rowid',
    tokenize='unicode61'
);
CREATE TRIGGER IF NOT EXISTS session_events_fts_ai AFTER INSERT ON session_events BEGIN
    INSERT INTO session_events_fts(rowid, data) VALUES (new.rowid, new.data);
END;
CREATE TRIGGER IF NOT EXISTS session_events_fts_ad AFTER DELETE ON session_events BEGIN
    INSERT INTO session_events_fts(session_events_fts, rowid, data)
        VALUES ('delete', old.rowid, old.data);
END;
CREATE TRIGGER IF NOT EXISTS session_events_fts_au AFTER UPDATE ON session_events BEGIN
    INSERT INTO session_events_fts(session_events_fts, rowid, data)
        VALUES ('delete', old.rowid, old.data);
    INSERT INTO session_events_fts(rowid, data) VALUES (new.rowid, new.data);
END;
"#;
