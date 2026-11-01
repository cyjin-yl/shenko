//! SQLite migrations for Shenko store.
use anyhow::Result;
use rusqlite::Connection;

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

    // Versioned instance identity (SPEC §6): additive, never rewrites rows.
    conn.execute_batch(crate::identity::IDENTITY_SCHEMA)?;

    // Committed context windows (SPEC §12.3): additive, own tables only.
    conn.execute_batch(crate::context_windows::CONTEXT_WINDOWS_SCHEMA)?;
    ensure_column(conn, "context_window_scopes", "profile_digest", "TEXT")?;

    // Subject raw input links (SPEC §12.1): additive index over the raw
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
    conn.execute_batch(crate::child::SCHEMA)?;
    conn.execute_batch(crate::runtime_boundary::SCHEMA)?;
    // Semantic waits (SPEC §5.1, R143): additive, own tables only. The wait
    // history and its per step receipts are never rewritten by a migration.
    conn.execute_batch(crate::semantic_wait::SEMANTIC_WAIT_SCHEMA)?;
    // Cognition replication bookkeeping (SPEC §5.4). It is local state, not
    // a replicated fact, so it never enters a snapshot fingerprint.
    conn.execute_batch(crate::cognition::COGNITION_SCHEMA)?;
    // Continuous-media transcript ingress (SPEC §15.2, §20): additive raw
    // revision facts plus effective-observation links; rows are never
    // rewritten or backfilled by this step.
    conn.execute_batch(crate::media::MEDIA_SCHEMA)?;
    // Never invent an old receipt from a stream's latest-state commit. Older
    // incomplete media facts stay retained, and recovery admission rejects
    // missing original receipt/delivery bindings instead of acknowledging them.
    ensure_column(conn, "media_streams", "open_commit_ref", "TEXT")?;
    ensure_column(conn, "media_segment_revisions", "admitted_commit_ref", "TEXT")?;
    // Durable provider-quota state (SPEC §42.5): snapshots, forecasts,
    // policies, decisions, the single-writer pacer and attempt ledger. All
    // writes happen inside the shared IMMEDIATE mutation line.
    conn.execute_batch(crate::quota::QUOTA_SCHEMA)?;
    // Durable browser input submission bindings (SPEC §41.18). Additive,
    // never rewrites the original event or its admission fact.
    conn.execute_batch(crate::web_input::WEB_INPUT_SCHEMA)?;
    ensure_column(conn, "cognition_receipts", "updated", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(
        conn,
        "cognition_file_baselines",
        "state",
        "TEXT NOT NULL DEFAULT 'present'",
    )?;
    // Durable seat records and single-writer leases (SPEC §17.1, §35.5):
    // additive, own tables only; location history is never rewritten.
    conn.execute_batch(crate::seat::SEAT_SCHEMA)?;
    // Seat schema v1 → v2 cutover (SPEC §17.1, §41.11): ONE explicit step.
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
    if has_unique {
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
    Ok(())
}

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

    // Backfill millisecond timestamps for rows written by the legacy schema.
    conn.execute_batch(
        "UPDATE memory_entries
            SET created_at_ms = CAST(strftime('%s', created_at) AS INTEGER) * 1000
          WHERE created_at_ms = 0 AND created_at IS NOT NULL;",
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

/// Memory-v2 tables: durable intent queue (§12.2) and supporting indexes.
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
