//! Durable source-bound content stream journal — docs/specs/durable-state.md#content-stream-journal stream journal.
//!
//! A streamed response (model output, execution output, seat/channel output)
//! survives a crash or a UI reconnect: every unit the transport delivered is
//! journaled in the ledger, bound to the durable source identity of the work
//! (request, attempt, task, step) rather than to an in-memory callback.
//!
//! Two unit shapes exist deliberately and are NEVER merged:
//!   * `whole` — a complete request/result/effect unit admitted at once;
//!   * `chunk` — one bounded piece of a streamed unit, ordered by `seq`.
//! A unit's `terminal` flag marks the final unit of a stream; a stream also
//! closes explicitly, and rows arriving after either close are refused.
//!
//! Replay semantics match the raw history contract: an identical unit under
//! the same (stream, seq) is a no-op, a different unit under the same key is
//! a typed conflict, never an overwrite. All writes ride the single
//! IMMEDIATE mutation line and land in the same commit-log sequence space as
//! every other store fact; the tables replicate through the cognition
//! archive under the `stream/{stream_id}` conflict unit.

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use shenko_types::commit::{CommitReceipt, CommitRef};
use shenko_types::error::ShenkoError;
use shenko_types::ids::{EntityId, LedgerSeq};

use crate::store::{Store, storage_error};

/// The durable bound of one streamed chunk unit (docs/specs/durable-state.md#content-stream-journal): a producer
/// flushes at 4 KiB or 100 ms, and the store refuses a chunk unit above the
/// byte bound, so an unbounded payload can never be admitted as one unit.
pub const CONTENT_CHUNK_MAX_BYTES: usize = 4096;
/// The durable bound of one whole unit. A whole request/result/effect unit
/// is admitted atomically, so it may be larger than a streamed chunk, but
/// never unbounded.
pub const CONTENT_WHOLE_MAX_BYTES: usize = 1024 * 1024;
/// The producer-side flush bound: a batch is journaled when it reaches
/// `CONTENT_CHUNK_MAX_BYTES` or `CONTENT_FLUSH_MAX_MILLIS`, whichever comes
/// first.
pub const CONTENT_FLUSH_MAX_MILLIS: u64 = 100;

/// The canonical stream identity of a provider request. Producer, subject
/// and UI use this one name, so a reconnecting consumer reads the same
/// stream the producer wrote.
pub fn provider_stream_id(request_id: EntityId) -> String {
    format!("provider/{request_id}")
}

pub const CONTENT_STREAM_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS content_streams (
    stream_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    binding TEXT NOT NULL,
    state TEXT NOT NULL,
    opened_commit_ref TEXT NOT NULL,
    closed_commit_ref TEXT,
    commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_content_streams_agent
    ON content_streams(agent_id, commit_seq);

CREATE TABLE IF NOT EXISTS content_stream_units (
    stream_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    shape TEXT NOT NULL,
    content_kind TEXT NOT NULL,
    data TEXT NOT NULL,
    terminal INTEGER NOT NULL DEFAULT 0,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (stream_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_content_stream_units_commit
    ON content_stream_units(commit_seq);
"#;

/// A journaled unit of one stream, exactly as admitted.
#[derive(Debug, Clone)]
pub struct ContentStreamUnit {
    pub stream_id: String,
    pub seq: i64,
    /// `whole` or `chunk` — never merged across shapes.
    pub shape: String,
    pub content_kind: String,
    pub data: String,
    pub terminal: bool,
    pub commit_seq: i64,
}

/// A registered stream and its current durable state.
#[derive(Debug, Clone)]
pub struct ContentStreamRecord {
    pub stream_id: String,
    pub agent_id: EntityId,
    pub kind: String,
    pub binding: String,
    pub state: String,
    pub opened_commit_ref: String,
    pub closed_commit_ref: Option<String>,
    pub commit_seq: i64,
}

fn conflict(reason: &str) -> ShenkoError {
    ShenkoError::conflict(reason)
}

fn stream_state(tx: &Transaction<'_>, stream_id: &str) -> Result<Option<(String, String)>, ShenkoError> {
    tx.query_row(
        "SELECT state, binding FROM content_streams WHERE stream_id=?1",
        [stream_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .map_err(storage_error)
}

impl Store {
    /// Register a stream against the durable identity of the work that
    /// produces it (`request_id`/`attempt_id`/`task_id`/`step_no` in the
    /// caller's own binding JSON — the store keeps it verbatim, never
    /// invents one). Re-registering with an identical binding is a no-op;
    /// a different binding under the same stream id is a conflict.
    pub fn open_content_stream(
        &self,
        stream_id: &str,
        agent_id: EntityId,
        kind: &str,
        binding: &str,
    ) -> Result<CommitReceipt, ShenkoError> {
        if stream_id.is_empty() || kind.is_empty() {
            return Err(conflict("content stream needs an id and a kind"));
        }
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                if let Some((_, bound)) = stream_state(&tx, stream_id)? {
                    if bound != binding {
                        return Err(conflict("content stream id is bound to different work"));
                    }
                    let opened: String = tx
                        .query_row(
                            "SELECT opened_commit_ref FROM content_streams WHERE stream_id=?1",
                            [stream_id],
                            |r| r.get(0),
                        )
                        .map_err(storage_error)?;
                    tx.commit().map_err(storage_error)?;
                    let original: CommitRef = serde_json::from_str(&opened)?;
                    return Ok(self.subject_receipt(original, vec![]));
                }
                tx.execute(
                    "INSERT INTO content_streams
                     (stream_id, agent_id, kind, binding, state, opened_commit_ref, commit_seq)
                     VALUES (?1,?2,?3,?4,'open',?5,?6)",
                    params![
                        stream_id,
                        agent_id.to_string(),
                        kind,
                        binding,
                        serde_json::to_string(&commit)?,
                        seq.0 as i64,
                    ],
                )
                .map_err(storage_error)?;
                self.record_subject_commit(&tx, commit, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(self.subject_receipt(commit, vec![]))
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Journal one unit of an open stream. `shape` is "whole" or "chunk";
    /// `terminal` marks the stream's final unit. An identical replay of a
    /// stored (stream_id, seq) is a no-op; a different fact under the same
    /// key is a conflict. Units after a terminal unit or an explicit close
    /// are refused.
    pub fn journal_content_unit(
        &self,
        stream_id: &str,
        seq_no: i64,
        shape: &str,
        content_kind: &str,
        data: &str,
        terminal: bool,
    ) -> Result<CommitReceipt, ShenkoError> {
        if shape != "whole" && shape != "chunk" {
            return Err(conflict("content unit shape must be whole or chunk"));
        }
        if seq_no < 0 {
            return Err(conflict("content unit seq must be non-negative"));
        }
        // The durable bound of a unit: a streamed chunk is bounded like the
        // producer's batch (4 KiB), a whole unit is bounded like an inline
        // artifact (1 MiB). An oversized payload is refused, never silently
        // split — the producer decides the batch boundary.
        let bound = if shape == "chunk" {
            CONTENT_CHUNK_MAX_BYTES
        } else {
            CONTENT_WHOLE_MAX_BYTES
        };
        if data.len() > bound {
            return Err(conflict("content unit exceeds its durable byte bound"));
        }
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let Some((state, _)) = stream_state(&tx, stream_id)? else {
                    return Err(ShenkoError::not_found("content stream"));
                };
                if state == "closed" {
                    return Err(conflict("content stream is closed"));
                }
                let finished: Option<(String, String, String, i64, i64)> = tx
                    .query_row(
                        "SELECT shape, content_kind, data, terminal, commit_seq
                         FROM content_stream_units WHERE stream_id=?1 AND seq=?2",
                        params![stream_id, seq_no],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                    )
                    .optional()
                    .map_err(storage_error)?;
                if let Some((old_shape, old_kind, old_data, old_terminal, old_seq)) = finished {
                    if old_shape != shape
                        || old_kind != content_kind
                        || old_data != data
                        || old_terminal != (terminal as i64)
                    {
                        return Err(conflict(
                            "a different unit already holds this stream position",
                        ));
                    }
                    tx.commit().map_err(storage_error)?;
                    // An identical replay answers with the ORIGINAL admission
                    // commit: the caller re-asks for the durable fact, never
                    // a freshly invented sequence.
                    return Ok(self.subject_receipt(
                        CommitRef::new(self.ledger.ledger_id(), LedgerSeq(old_seq as u64)),
                        vec![],
                    ));
                }
                let after_terminal: i64 = tx
                    .query_row(
                        "SELECT count(*) FROM content_stream_units
                         WHERE stream_id=?1 AND terminal=1 AND seq<=?2",
                        params![stream_id, seq_no],
                        |r| r.get(0),
                    )
                    .map_err(storage_error)?;
                if after_terminal > 0 {
                    return Err(conflict(
                        "the terminal unit of this stream is already journaled",
                    ));
                }
                tx.execute(
                    "INSERT INTO content_stream_units
                     (stream_id, seq, shape, content_kind, data, terminal, commit_seq)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        stream_id,
                        seq_no,
                        shape,
                        content_kind,
                        data,
                        terminal as i64,
                        seq.0 as i64,
                    ],
                )
                .map_err(storage_error)?;
                self.record_subject_commit(&tx, commit, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(self.subject_receipt(commit, vec![]))
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Close an open stream. Idempotent: closing a closed stream returns the
    /// closing commit without writing a second fact.
    pub fn close_content_stream(&self, stream_id: &str) -> Result<CommitReceipt, ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let Some((state, _)) = stream_state(&tx, stream_id)? else {
                    return Err(ShenkoError::not_found("content stream"));
                };
                if state == "closed" {
                    let closed_ref: String = tx
                        .query_row(
                            "SELECT closed_commit_ref FROM content_streams WHERE stream_id=?1",
                            [stream_id],
                            |r| r.get(0),
                        )
                        .map_err(storage_error)?;
                    tx.commit().map_err(storage_error)?;
                    let original: CommitRef = serde_json::from_str(&closed_ref)?;
                    return Ok(self.subject_receipt(original, vec![]));
                }
                tx.execute(
                    "UPDATE content_streams SET state='closed', closed_commit_ref=?2, commit_seq=?3
                     WHERE stream_id=?1 AND state='open'",
                    params![stream_id, serde_json::to_string(&commit)?, seq.0 as i64],
                )
                .map_err(storage_error)?;
                self.record_subject_commit(&tx, commit, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(self.subject_receipt(commit, vec![]))
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// All units of a stream at or after `since_seq`, in journaled order.
    /// This is the reconnect read: a UI or a recovering consumer re-asks the
    /// store for the units it missed instead of trusting a live callback.
    pub fn content_stream_units(
        &self,
        stream_id: &str,
        since_seq: i64,
    ) -> Result<Vec<ContentStreamUnit>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT stream_id, seq, shape, content_kind, data, terminal, commit_seq
                 FROM content_stream_units WHERE stream_id=?1 AND seq>=?2 ORDER BY seq",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![stream_id, since_seq], |r| {
                Ok(ContentStreamUnit {
                    stream_id: r.get(0)?,
                    seq: r.get(1)?,
                    shape: r.get(2)?,
                    content_kind: r.get(3)?,
                    data: r.get(4)?,
                    terminal: r.get::<_, i64>(5)? != 0,
                    commit_seq: r.get(6)?,
                })
            })
            .map_err(storage_error)?;
        let mut units = Vec::new();
        for row in rows {
            units.push(row.map_err(storage_error)?);
        }
        Ok(units)
    }

    /// Every journal stream of one agent in journal order.
    pub fn content_streams(&self, agent_id: EntityId) -> Result<Vec<ContentStreamRecord>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT stream_id, agent_id, kind, binding, state,
                        opened_commit_ref, closed_commit_ref, commit_seq
                 FROM content_streams WHERE agent_id=?1 ORDER BY commit_seq",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![agent_id.to_string()], |r| {
                let agent: String = r.get(1)?;
                Ok(ContentStreamRecord {
                    stream_id: r.get(0)?,
                    agent_id: agent.parse().unwrap_or(EntityId::nil()),
                    kind: r.get(2)?,
                    binding: r.get(3)?,
                    state: r.get(4)?,
                    opened_commit_ref: r.get(5)?,
                    closed_commit_ref: r.get(6)?,
                    commit_seq: r.get(7)?,
                })
            })
            .map_err(storage_error)?;
        let mut streams = Vec::new();
        for row in rows {
            streams.push(row.map_err(storage_error)?);
        }
        Ok(streams)
    }

    /// Journal one bounded batch of a streamed unit. A batch is one chunk
    /// unit: the producer's flush boundary IS the durable unit boundary, so
    /// a reconnecting consumer re-reads whole batches.
    pub fn journal_content_batch(
        &self,
        stream_id: &str,
        seq_no: i64,
        content_kind: &str,
        data: &str,
    ) -> Result<CommitReceipt, ShenkoError> {
        self.journal_content_unit(stream_id, seq_no, "chunk", content_kind, data, false)
    }

    /// Journal the terminal unit of a stream: the whole result or effect
    /// that ends it. The terminal row exists even when the stream published
    /// no chunk at all, so a consumer never has to guess how a stream ended.
    pub fn journal_content_terminal(
        &self,
        stream_id: &str,
        seq_no: i64,
        content_kind: &str,
        data: &str,
    ) -> Result<CommitReceipt, ShenkoError> {
        self.journal_content_unit(stream_id, seq_no, "whole", content_kind, data, true)
    }

    /// Journal the durable result unit of a committed subject step as a
    /// `whole` unit of the `model/{agent}/{branch}` stream. Called inside
    /// the step's own commit transaction so the journal entry and the step
    /// commit are one durable fact; an identical replay is a no-op.
    pub(crate) fn journal_step_model_result(
        &self,
        tx: &Transaction<'_>,
        agent_id: EntityId,
        branch_id: &str,
        step_no: i64,
        binding: &str,
        data: &str,
        commit_seq: LedgerSeq,
    ) -> Result<(), ShenkoError> {
        let stream_id = format!("model/{agent_id}/{branch_id}");
        if stream_state(tx, &stream_id)?.is_none() {
            tx.execute(
                "INSERT INTO content_streams
                 (stream_id, agent_id, kind, binding, state, opened_commit_ref, commit_seq)
                 VALUES (?1,?2,'model_result',?3,'open',?4,?5)",
                params![
                    stream_id,
                    agent_id.to_string(),
                    binding,
                    serde_json::to_string(&CommitRef::new(self.ledger.ledger_id(), commit_seq))?,
                    commit_seq.0 as i64,
                ],
            )
            .map_err(storage_error)?;
        }
        let existing: Option<(String, String, String, i64)> = tx
            .query_row(
                "SELECT shape, content_kind, data, terminal FROM content_stream_units
                 WHERE stream_id=?1 AND seq=?2",
                params![stream_id, step_no],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(storage_error)?;
        if let Some((shape, kind, old_data, terminal)) = existing {
            if shape != "whole" || kind != "model_result" || old_data != data || terminal != 1 {
                return Err(conflict(
                    "a different unit already holds this step's stream position",
                ));
            }
            return Ok(());
        }
        tx.execute(
            "INSERT INTO content_stream_units
             (stream_id, seq, shape, content_kind, data, terminal, commit_seq)
             VALUES (?1,?2,'whole','model_result',?3,1,?4)",
            params![stream_id, step_no, data, commit_seq.0 as i64],
        )
        .map_err(storage_error)?;
        Ok(())
    }
}

/// The producer-side bound of the stream journal: a batch is journaled when
/// it reaches 4 KiB or when 100 ms have passed since the batch opened,
/// whichever comes first. The batcher owns that rule, so every producer
/// (provider stream, subject output, seat output) publishes the same
/// bounded units and a reconnecting consumer re-reads whole batches.
pub struct ContentStreamBatcher<'a> {
    store: &'a Store,
    stream_id: String,
    content_kind: String,
    buffer: String,
    bytes: usize,
    seq: i64,
    opened: Option<std::time::Instant>,
}

impl<'a> ContentStreamBatcher<'a> {
    /// A batcher for one stream. The stream must already be open.
    pub fn new(store: &'a Store, stream_id: &str, content_kind: &str) -> Self {
        Self {
            store,
            stream_id: stream_id.to_string(),
            content_kind: content_kind.to_string(),
            buffer: String::new(),
            bytes: 0,
            seq: 0,
            opened: None,
        }
    }

    /// The stream this batcher writes to.
    pub fn stream_id(&self) -> &str {
        &self.stream_id
    }

    /// The sequence the next flushed batch will carry.
    pub fn next_seq(&self) -> i64 {
        self.seq
    }

    /// Append a chunk of the streamed unit. The batch is journaled when the
    /// byte bound or the time bound is reached; otherwise it stays buffered.
    pub fn push(&mut self, chunk: &str) -> Result<Option<CommitReceipt>, ShenkoError> {
        self.buffer.push_str(chunk);
        self.bytes += chunk.len();
        if self.opened.is_none() {
            self.opened = Some(std::time::Instant::now());
        }
        let by_bytes = self.bytes >= CONTENT_CHUNK_MAX_BYTES;
        let by_time = self
            .opened
            .is_some_and(|opened| opened.elapsed().as_millis() as u64 >= CONTENT_FLUSH_MAX_MILLIS);
        if by_bytes || by_time {
            self.flush()
        } else {
            Ok(None)
        }
    }

    /// Journal whatever is buffered, whatever the bounds are. The last
    /// partial batch of a stream is a real unit: it is never dropped.
    pub fn flush(&mut self) -> Result<Option<CommitReceipt>, ShenkoError> {
        if self.buffer.is_empty() {
            return Ok(None);
        }
        let data = std::mem::take(&mut self.buffer);
        self.bytes = 0;
        self.opened = None;
        let seq = self.seq;
        self.seq += 1;
        let store = self.store;
        let receipt =
            store.journal_content_batch(&self.stream_id, seq, &self.content_kind, &data)?;
        Ok(Some(receipt))
    }
}
