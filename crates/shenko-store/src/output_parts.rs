//! Durable ordered public-text output parts — SPEC §21.11 流数据与显示状态.
//!
//! Real streaming starts at the provider stream (ArchBroker), is redacted to
//! public text by the subject (`public_text_prefix` in shenko-subject), and is
//! persisted here as ordered bounded batches BEFORE the terminal S1 decision.
//! The UI replays parts by a durable `(commit_seq, chunk_seq)` cursor, so a
//! reconnect resumes without re-deriving keys from text length or wall clock.
//!
//! Durability contract:
//! - Append-only. The conflict unit is the single part row keyed by
//!   `(agent_id, stream_id, message_id, part_id, chunk_seq)`: same key and
//!   content is an idempotent no-op returning the original commit; same key
//!   with different content is a conflict and never overwrites.
//! - No update or delete path exists. A corrected tail is a NEW chunk_seq
//!   (higher revision), never a rewrite — history stays intact.
//! - Recovery is a cursor re-read by `(agent_id, commit_seq)`; a gap at the
//!   terminal boundary is surfaced by the reader, never silently patched.
//! - `content_kind` is `public_text` by construction: internal control or
//!   reasoning text never enters this table (redaction is the producer's
//!   responsibility; this table does not re-filter).

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::ids::{EntityId, LedgerSeq};

use crate::store::{Store, storage_error};

pub(crate) const OUTPUT_PARTS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS subject_output_parts (
    agent_id TEXT NOT NULL,
    stream_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    part_id TEXT NOT NULL,
    part_revision TEXT NOT NULL,
    chunk_seq INTEGER NOT NULL,
    terminal INTEGER NOT NULL DEFAULT 0,
    content TEXT NOT NULL,
    content_kind TEXT NOT NULL DEFAULT 'public_text',
    commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (agent_id, stream_id, message_id, part_id, chunk_seq)
);
CREATE INDEX IF NOT EXISTS idx_subject_output_parts_cursor
    ON subject_output_parts(agent_id, commit_seq);
";

/// The only content kind this table admits. Provider-native/internal frames
/// never cross the producer boundary into a stored part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputPartKind {
    PublicText,
}

impl OutputPartKind {
    fn as_str(&self) -> &'static str {
        match self {
            OutputPartKind::PublicText => "public_text",
        }
    }

    fn parse(value: &str) -> Result<Self, ShenkoError> {
        match value {
            "public_text" => Ok(OutputPartKind::PublicText),
            other => Err(ShenkoError::storage(format!(
                "unknown stored output part kind {other}"
            ))),
        }
    }
}

/// One ordered chunk of one output part. `message_id` is the committed
/// decision's `step_id`; `part_id` is stable across the part's whole life
/// (e.g. `"text/0"`), so UI keys never derive from content or arrival order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OutputPart {
    pub stream_id: EntityId,
    pub request_id: EntityId,
    pub attempt_id: EntityId,
    pub message_id: EntityId,
    pub part_id: String,
    /// Revision of the part content this chunk advances. A corrected tail
    /// bumps the revision and appends at a new `chunk_seq`.
    pub part_revision: String,
    /// Monotonic per (stream, message, part). Gaps are surfaced, not patched.
    pub chunk_seq: u64,
    pub content: String,
    pub kind: OutputPartKind,
    /// The final chunk of the part. Concatenating all chunks of the part in
    /// `chunk_seq` order MUST equal the committed decision text for the
    /// effective attempt; the producer records a mismatch as a new revision.
    pub terminal: bool,
}

/// A stored part bound to its ledger position — the durable replay unit.
#[derive(Debug, Clone)]
pub struct StoredOutputPart {
    pub part: OutputPart,
    /// Receiver-local commit sequence for cursor replay. Together with
    /// `chunk_seq` it pages parts committed in one ledger transaction.
    pub commit_seq: u64,
    pub commit: CommitRef,
}

pub enum OutputPartAdmission {
    /// New chunk committed; callers may publish it immediately.
    Appended(CommitRef),
    /// Same key and identical content replayed; original commit returned.
    Replayed(CommitRef),
    /// Same key with different content; the original fact is preserved.
    Conflict,
}

/// Replay position of one part: the highest committed chunk_seq and the
/// concatenated public text so far. Lets the committed-decision projection
/// append exactly the remainder (never re-send streamed text, never drop it).
#[derive(Debug, Clone)]
pub struct OutputPartState {
    pub last_chunk_seq: u64,
    pub content: String,
}

impl Store {
    /// Persist one ordered public-text chunk before it may be displayed.
    /// Commits in one ledger transaction so "durable" precedes "visible".
    pub fn append_output_part(
        &self,
        agent_id: EntityId,
        part: &OutputPart,
    ) -> Result<OutputPartAdmission, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_append_output_part(agent_id, part),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_append_output_part(
        &self,
        agent_id: EntityId,
        part: &OutputPart,
    ) -> Result<OutputPartAdmission, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let existing: Option<(String, i64)> = tx
            .query_row(
                "SELECT content, commit_seq FROM subject_output_parts
                 WHERE agent_id=?1 AND stream_id=?2 AND message_id=?3
                   AND part_id=?4 AND chunk_seq=?5",
                params![
                    agent_id.to_string(),
                    part.stream_id.to_string(),
                    part.message_id.to_string(),
                    part.part_id,
                    part.chunk_seq as i64,
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(storage_error)?;
        if let Some((content, seq)) = existing {
            if content == part.content {
                tx.commit().map_err(storage_error)?;
                return Ok(OutputPartAdmission::Replayed(CommitRef::new(
                    self.ledger.ledger_id(),
                    LedgerSeq(u64::try_from(seq).map_err(|error| {
                        ShenkoError::storage(format!("bad commit seq {seq}: {error}"))
                    })?),
                )));
            }
            return Ok(OutputPartAdmission::Conflict);
        }
        let commit_seq = self.ledger.allocate_seq(&tx)?;
        tx.execute(
            "INSERT INTO subject_output_parts
                (agent_id, stream_id, request_id, attempt_id, message_id, part_id,
                 part_revision, chunk_seq, terminal, content, content_kind, commit_seq)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                agent_id.to_string(),
                part.stream_id.to_string(),
                part.request_id.to_string(),
                part.attempt_id.to_string(),
                part.message_id.to_string(),
                part.part_id,
                part.part_revision,
                part.chunk_seq as i64,
                if part.terminal { 1 } else { 0 },
                part.content,
                part.kind.as_str(),
                commit_seq.0 as i64,
            ],
        )
        .map_err(storage_error)?;
        let commit = CommitRef::new(self.ledger.ledger_id(), commit_seq);
        self.record_subject_commit(&tx, commit, 1)?;
        tx.commit().map_err(storage_error)?;
        Ok(OutputPartAdmission::Appended(commit))
    }

    /// Durable replay cursor: parts committed after `after_commit_seq`, in
    /// commit then chunk order. A hole in `chunk_seq` within a returned part
    /// is left for the reader to surface — this query never fabricates rows.
    pub fn output_parts_after(
        &self,
        agent_id: EntityId,
        after_commit_seq: u64,
        limit: usize,
    ) -> Result<Vec<StoredOutputPart>, ShenkoError> {
        let limit = i64::try_from(limit)
            .map_err(|_| ShenkoError::conflict("output part replay limit is too large"))?;
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT stream_id, request_id, attempt_id, message_id, part_id,
                        part_revision, chunk_seq, terminal, content, content_kind, commit_seq
                 FROM subject_output_parts
                 WHERE agent_id=?1 AND commit_seq>?2
                 ORDER BY commit_seq, chunk_seq LIMIT ?3",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(
                params![agent_id.to_string(), after_commit_seq as i64, limit],
                |row| {
                    // Column order must mirror the SELECT list exactly.
                    let stream_id: String = row.get(0)?;
                    let request_id: String = row.get(1)?;
                    let attempt_id: String = row.get(2)?;
                    let message_id: String = row.get(3)?;
                    let part_id: String = row.get(4)?;
                    let part_revision: String = row.get(5)?;
                    let chunk_seq: i64 = row.get(6)?;
                    let terminal: i64 = row.get(7)?;
                    let content: String = row.get(8)?;
                    let kind: String = row.get(9)?;
                    let commit_seq: i64 = row.get(10)?;
                    Ok((
                        stream_id, request_id, attempt_id, message_id, part_id, part_revision,
                        chunk_seq, terminal, content, kind, commit_seq,
                    ))
                },
            )
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        rows.into_iter()
            .map(
                |(
                    stream_id,
                    request_id,
                    attempt_id,
                    message_id,
                    part_id,
                    part_revision,
                    chunk_seq,
                    terminal,
                    content,
                    kind,
                    commit_seq,
                )| {
                    let seq = u64::try_from(commit_seq).map_err(|error| {
                        ShenkoError::storage(format!("bad commit seq {commit_seq}: {error}"))
                    })?;
                    Ok(StoredOutputPart {
                        part: OutputPart {
                            stream_id: stream_id.parse().map_err(|e| {
                                ShenkoError::storage(format!("bad stream id: {e}"))
                            })?,
                            request_id: request_id.parse().map_err(|e| {
                                ShenkoError::storage(format!("bad request id: {e}"))
                            })?,
                            attempt_id: attempt_id.parse().map_err(|e| {
                                ShenkoError::storage(format!("bad attempt id: {e}"))
                            })?,
                            message_id: message_id.parse().map_err(|e| {
                                ShenkoError::storage(format!("bad message id: {e}"))
                            })?,
                            part_id,
                            part_revision,
                            chunk_seq: u64::try_from(chunk_seq).map_err(|error| {
                                ShenkoError::storage(format!("bad chunk seq {chunk_seq}: {error}"))
                            })?,
                            content,
                            kind: OutputPartKind::parse(&kind)?,
                            terminal: terminal != 0,
                        },
                        commit_seq: seq,
                        commit: CommitRef::new(self.ledger.ledger_id(), LedgerSeq(seq)),
                    })
                },
            )
            .collect()
    }
}

impl Store {
    /// The replay position of one (message, part), if any chunks were stored.
    pub fn output_part_state(
        &self,
        agent_id: EntityId,
        message_id: EntityId,
        part_id: &str,
    ) -> Result<Option<OutputPartState>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT chunk_seq, content FROM subject_output_parts
                 WHERE agent_id=?1 AND message_id=?2 AND part_id=?3
                 ORDER BY chunk_seq",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(
                params![agent_id.to_string(), message_id.to_string(), part_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        if rows.is_empty() {
            return Ok(None);
        }
        let mut content = String::new();
        // Parts start at chunk_seq 0 by producer contract; any deviation is
        // a hole and must be surfaced, never patched over.
        let mut expected = 0u64;
        for (seq, chunk) in rows {
            let seq = u64::try_from(seq)
                .map_err(|error| ShenkoError::storage(format!("bad chunk seq: {error}")))?;
            if seq != expected {
                return Err(ShenkoError::conflict(
                    "output part has a chunk_seq hole; cannot compute replay position",
                ));
            }
            expected = seq + 1;
            content.push_str(&chunk);
        }
        Ok(Some(OutputPartState {
            last_chunk_seq: expected - 1,
            content,
        }))
    }
}
