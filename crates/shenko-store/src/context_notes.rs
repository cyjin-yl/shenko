//! Branch-journaled working notes for notes-backed context windows — docs/specs/context-policies.md#spec-12-3-1.
//!
//! A note revision is an append-only journal entry of one branch scope. Only
//! the newest revision may enter a model context; a clearing revision is kept
//! in the journal so "reset" stays a recorded fact instead of a deletion, and
//! an older note never resurfaces because a newer one was cleared.
//!
//! Revisions are compare-and-append: a writer names the revision it read, and
//! a writer whose expected revision is no longer the head is refused instead
//! of overwriting another writer's note. The size bound is enforced by the
//! caller before it reaches the store (the journal contract owns it); the
//! column stores whatever bytes were admitted with that revision.
use rusqlite::{OptionalExtension, params};

use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;

use crate::store::Store;

pub(crate) const CONTEXT_NOTES_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS context_notes (
    scope_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    body TEXT NOT NULL,
    source TEXT NOT NULL,
    written_at_ms INTEGER NOT NULL,
    visible INTEGER NOT NULL,
    PRIMARY KEY (scope_id, revision)
);
CREATE TABLE IF NOT EXISTS context_rollover_requests (
    scope_id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL,
    requested_at_ms INTEGER NOT NULL,
    consumed_window TEXT
);
CREATE TABLE IF NOT EXISTS context_rollover_reminders (
    scope_id TEXT NOT NULL,
    window_id TEXT NOT NULL,
    policy_epoch INTEGER NOT NULL,
    issued_at_ms INTEGER NOT NULL,
    PRIMARY KEY (scope_id, window_id)
);
";

/// One stored note revision. `visible` distinguishes a replacement note from
/// a clearing revision; a clearing revision keeps the journal chronological.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredContextNote {
    pub scope_id: EntityId,
    pub revision: i64,
    pub body: String,
    /// Stable source label written by the producing path (`tool`, `reset`,
    /// `fork`, `bootstrap`). The source label is provenance, not authority.
    pub source: String,
    pub written_at_ms: i64,
    pub visible: bool,
}

/// The outcome of a compare-and-append. `Stale` reports the revision that is
/// actually current, so the caller can re-read instead of guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextNoteAppend {
    Appended { revision: i64 },
    Stale { current: Option<i64> },
}

/// A durable request to start a new window at the next safe boundary.
///
/// The row is the authority across a restart: a request armed before a crash
/// is still pending afterwards, and `consumed_window` records the committed
/// window that answered it. A request is not a window and never cuts history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRolloverRequest {
    pub scope_id: EntityId,
    pub request_id: EntityId,
    pub requested_at_ms: i64,
    pub consumed_window: Option<EntityId>,
}

fn storage(e: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::storage(e.to_string())
}

fn read_note(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredContextNote> {
    let scope: String = row.get(0)?;
    Ok(StoredContextNote {
        scope_id: scope.parse::<EntityId>().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        revision: row.get(1)?,
        body: row.get(2)?,
        source: row.get(3)?,
        written_at_ms: row.get(4)?,
        visible: row.get::<_, i64>(5)? != 0,
    })
}

impl Store {
    /// The newest note revision of this scope, visible or cleared.
    ///
    /// No row means the scope never recorded a note. A cleared revision is
    /// returned with `visible == false`; readers must not substitute an
    /// older revision for it.
    pub fn context_note_head(&self, scope_id: EntityId) -> Result<Option<StoredContextNote>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        conn.query_row(
            "SELECT scope_id, revision, body, source, written_at_ms, visible
             FROM context_notes WHERE scope_id = ?1 ORDER BY revision DESC LIMIT 1",
            params![scope_id.to_string()],
            read_note,
        )
        .optional()
        .map_err(storage)
    }

    /// Bounded page of note revisions, newest first. `before` excludes
    /// revisions `>= before`; at most `limit` rows are returned.
    pub fn context_note_page(
        &self,
        scope_id: EntityId,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<StoredContextNote>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        conn.prepare_cached(
            "SELECT scope_id, revision, body, source, written_at_ms, visible
             FROM context_notes WHERE scope_id = ?1 AND revision < ?2
             ORDER BY revision DESC LIMIT ?3",
        )
        .map_err(storage)?
        .query_map(
            params![scope_id.to_string(), before.unwrap_or(i64::MAX), limit.max(0)],
            read_note,
        )
        .map_err(storage)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage)
    }

    /// Append note revision `head + 1` only while the current head is
    /// `expected` (`None`: the scope has no note yet). The expected-revision
    /// check and the append share one immediate transaction, so two writers
    /// naming the same head cannot both land.
    pub fn append_context_note(
        &self,
        scope_id: EntityId,
        expected: Option<i64>,
        body: &str,
        source: &str,
        written_at_ms: i64,
        visible: bool,
    ) -> Result<ContextNoteAppend, ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(storage)?;
                let head: Option<i64> = tx
                    .query_row(
                        "SELECT revision FROM context_notes
                         WHERE scope_id = ?1 ORDER BY revision DESC LIMIT 1",
                        params![scope_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(storage)?;
                if head != expected {
                    return Ok(ContextNoteAppend::Stale { current: head });
                }
                let revision = head.map_or(1, |head| head + 1);
                tx.execute(
                    "INSERT INTO context_notes
                     (scope_id, revision, body, source, written_at_ms, visible)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        scope_id.to_string(),
                        revision,
                        body,
                        source,
                        written_at_ms,
                        i64::from(visible)
                    ],
                )
                .map_err(storage)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                tx.execute(
                    "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count)
                     VALUES (?1, ?2, ?3, 1)",
                    params![
                        self.ledger.ledger_id().to_string(),
                        seq.0 as i64,
                        self.capabilities.incarnation.to_string()
                    ],
                )
                .map_err(storage)?;
                tx.commit().map_err(storage)?;
                Ok(ContextNoteAppend::Appended { revision })
            },
            |_| true,
        )
    }
}

impl Store {
    /// The scope's rollover request row, pending or answered.
    pub fn context_rollover_request(
        &self,
        scope_id: EntityId,
    ) -> Result<Option<StoredRolloverRequest>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        conn.query_row(
            "SELECT scope_id, request_id, requested_at_ms, consumed_window
             FROM context_rollover_requests WHERE scope_id = ?1",
            params![scope_id.to_string()],
            |row| {
                let scope: String = row.get(0)?;
                let request: String = row.get(1)?;
                let consumed: Option<String> = row.get(3)?;
                let convert = |value: String| {
                    value.parse::<EntityId>().map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                };
                Ok(StoredRolloverRequest {
                    scope_id: convert(scope)?,
                    request_id: convert(request)?,
                    requested_at_ms: row.get(2)?,
                    consumed_window: consumed.map(convert).transpose()?,
                })
            },
        )
        .optional()
        .map_err(storage)
    }

    /// Arm a rollover request for this scope.
    ///
    /// An already pending request is kept: a repeated request before the
    /// boundary must not stack windows. An answered request is replaced by a
    /// new one, so a later request is a new fact rather than a replay.
    pub fn arm_context_rollover(
        &self,
        scope_id: EntityId,
        requested_at_ms: i64,
    ) -> Result<StoredRolloverRequest, ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(storage)?;
                let existing: Option<(String, i64, Option<String>)> = tx
                    .query_row(
                        "SELECT request_id, requested_at_ms, consumed_window
                         FROM context_rollover_requests WHERE scope_id = ?1",
                        params![scope_id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()
                    .map_err(storage)?;
                if let Some((request_id, requested_at_ms, None)) = existing {
                    let request_id =
                        request_id.parse::<EntityId>().map_err(storage)?;
                    return Ok(StoredRolloverRequest {
                        scope_id,
                        request_id,
                        requested_at_ms,
                        consumed_window: None,
                    });
                }
                let request_id = EntityId::new();
                tx.execute(
                    "INSERT INTO context_rollover_requests
                     (scope_id, request_id, requested_at_ms, consumed_window)
                     VALUES (?1, ?2, ?3, NULL)
                     ON CONFLICT(scope_id) DO UPDATE SET
                         request_id = excluded.request_id,
                         requested_at_ms = excluded.requested_at_ms,
                         consumed_window = NULL",
                    params![scope_id.to_string(), request_id.to_string(), requested_at_ms],
                )
                .map_err(storage)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                tx.execute(
                    "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count)
                     VALUES (?1, ?2, ?3, 1)",
                    params![
                        self.ledger.ledger_id().to_string(),
                        seq.0 as i64,
                        self.capabilities.incarnation.to_string()
                    ],
                )
                .map_err(storage)?;
                tx.commit().map_err(storage)?;
                Ok(StoredRolloverRequest {
                    scope_id,
                    request_id,
                    requested_at_ms,
                    consumed_window: None,
                })
            },
            |_| true,
        )
    }

    /// Mark the pending request answered by `window_id`. Returns whether a
    /// pending request existed; an already answered row is left as it was, so
    /// a repeated adoption cannot rewrite the audit.
    pub fn consume_context_rollover(
        &self,
        scope_id: EntityId,
        window_id: EntityId,
    ) -> Result<bool, ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(storage)?;
                let pending: Option<Option<String>> = tx
                    .query_row(
                        "SELECT consumed_window FROM context_rollover_requests WHERE scope_id = ?1",
                        params![scope_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(storage)?;
                let Some(None) = pending else {
                    return Ok(false);
                };
                tx.execute(
                    "UPDATE context_rollover_requests SET consumed_window = ?2 WHERE scope_id = ?1",
                    params![scope_id.to_string(), window_id.to_string()],
                )
                .map_err(storage)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                tx.execute(
                    "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count)
                     VALUES (?1, ?2, ?3, 1)",
                    params![
                        self.ledger.ledger_id().to_string(),
                        seq.0 as i64,
                        self.capabilities.incarnation.to_string()
                    ],
                )
                .map_err(storage)?;
                tx.commit().map_err(storage)?;
                Ok(true)
            },
            |_| true,
        )
    }
}

impl Store {
    /// Whether this committed window already received its once-per-window
    /// notebook reminder.
    ///
    /// The row is keyed by (scope, window): a reminder belongs to the window
    /// the model is looking at, so a process that restarts inside the same
    /// window finds the reminder answered. A missing row means "not reminded
    /// yet", never an error.
    pub fn context_reminder_issued(
        &self,
        scope_id: EntityId,
        window_id: EntityId,
    ) -> Result<bool, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM context_rollover_reminders
                 WHERE scope_id = ?1 AND window_id = ?2",
                params![scope_id.to_string(), window_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        Ok(exists.is_some())
    }

    /// Record the committed window as reminded under `policy_epoch`.
    ///
    /// The insert is idempotent: a repeated acknowledgement keeps the first
    /// record, so a late writer cannot rewrite when a window was reminded. It
    /// shares the store's immediate mutation line with every other durable
    /// write. Returns whether this call created the row.
    pub fn record_context_reminder(
        &self,
        scope_id: EntityId,
        window_id: EntityId,
        policy_epoch: i64,
        issued_at_ms: i64,
    ) -> Result<bool, ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(storage)?;
                let changed = tx
                    .execute(
                        "INSERT OR IGNORE INTO context_rollover_reminders
                         (scope_id, window_id, policy_epoch, issued_at_ms)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            scope_id.to_string(),
                            window_id.to_string(),
                            policy_epoch,
                            issued_at_ms
                        ],
                    )
                    .map_err(storage)?;
                if changed == 0 {
                    return Ok(false);
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                tx.execute(
                    "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count)
                     VALUES (?1, ?2, ?3, 1)",
                    params![
                        self.ledger.ledger_id().to_string(),
                        seq.0 as i64,
                        self.capabilities.incarnation.to_string()
                    ],
                )
                .map_err(storage)?;
                tx.commit().map_err(storage)?;
                Ok(true)
            },
            |_| true,
        )
    }
}
