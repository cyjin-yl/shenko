//! Durable main-loop active-runtime segments — SPEC §51.7 measurement input.
//!
//! §51.7 measures forgetting in **subject-relative running time**, not wall
//! clock time: only the interval in which the *main* inference loop is
//! actually executing counts. This module records that interval as a
//! mechanism fact, and nothing else:
//!
//! * one segment is opened by the subject loop immediately before it starts
//!   working on one main step, and closed when that invocation stops working
//!   on it (committed, retired, left `RetryPending`, yielded, or errored);
//! * a child inference never opens a segment, so child time cannot enter;
//! * a paused subject never executes a step, and the gap between steps is
//!   outside every segment, so pause and shutdown cannot enter;
//! * a provider back-off (`RetryPending`) closes the segment, and the wait
//!   before the retry is not counted;
//! * a segment whose end was never recorded (crash, restart, panic, stale
//!   writer) is **discarded**, not closed at "now": its end is unknown, and
//!   counting it would silently accumulate exactly the downtime §51.7
//!   excludes. Discarded rows stay visible as evidence of the unknown end.
//!
//! The ledger therefore holds mechanical durations. It does **not** hold a
//! forgetting formula, thresholds, weights or stage decisions: those are
//! revisited as content in the `forgetting` plugin (see the
//! `shenko-forgetting` crate), and a stage is only ever written as a subject
//! attitude — this module has no opinion about it.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;

use crate::store::Store;

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS subject_active_runtime_segments(
 segment_id TEXT PRIMARY KEY,
 agent_id TEXT NOT NULL,
 step_id TEXT NOT NULL,
 step_no INTEGER NOT NULL,
 started_at_ms INTEGER NOT NULL,
 ended_at_ms INTEGER,
 end_reason TEXT,
 durable_step_state TEXT,
 discarded_reason TEXT,
 created_seq INTEGER NOT NULL,
 -- An end and a discard are mutually exclusive and exhaustive: a row is
 -- either a measured interval (both end columns set) or an unknown-end
 -- segment (discard set, no end). A row can never be both, and never neither.
 CHECK ((ended_at_ms IS NULL) = (end_reason IS NULL)),
 CHECK ((ended_at_ms IS NOT NULL) <> (discarded_reason IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS subject_active_runtime_open
 ON subject_active_runtime_segments(agent_id) WHERE ended_at_ms IS NULL AND discarded_reason IS NULL;
CREATE INDEX IF NOT EXISTS subject_active_runtime_closed
 ON subject_active_runtime_segments(agent_id, started_at_ms, ended_at_ms)
 WHERE ended_at_ms IS NOT NULL AND discarded_reason IS NULL;
CREATE INDEX IF NOT EXISTS subject_active_runtime_step
 ON subject_active_runtime_segments(agent_id, step_id, started_at_ms);
"#;

fn storage(error: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::storage(error.to_string())
}

fn parse_id(value: &str) -> Result<EntityId, ShenkoError> {
    value.parse().map_err(storage)
}

/// Why one measured main-loop interval ended.
///
/// Each variant is a real stopping condition of the main loop, not a
/// forgetting judgement. `Yielded` covers every return that leaves the step
/// resumable without reaching a terminal step state (no context prepared yet,
/// a resnapshot, cancellation before dispatch, a recorded retry hand-off).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveSegmentEnd {
    /// The step reached its committed S1 and advanced the head.
    Committed,
    /// The step was retired (for example its output was rejected and a
    /// replacement step will carry the work).
    Retired,
    /// The step recorded `RetryPending`; the back-off wait is not active time.
    RetryPending,
    /// The invocation returned with the step still resumable.
    Yielded,
    /// The invocation stopped on an error; whatever ran is real active time.
    Errored,
}

impl ActiveSegmentEnd {
    pub fn as_str(self) -> &'static str {
        match self {
            ActiveSegmentEnd::Committed => "committed",
            ActiveSegmentEnd::Retired => "retired",
            ActiveSegmentEnd::RetryPending => "retry_pending",
            ActiveSegmentEnd::Yielded => "yielded",
            ActiveSegmentEnd::Errored => "errored",
        }
    }

    fn parse(value: &str) -> Result<Self, ShenkoError> {
        Ok(match value {
            "committed" => ActiveSegmentEnd::Committed,
            "retired" => ActiveSegmentEnd::Retired,
            "retry_pending" => ActiveSegmentEnd::RetryPending,
            "yielded" => ActiveSegmentEnd::Yielded,
            "errored" => ActiveSegmentEnd::Errored,
            other => return Err(storage(format!("unknown active-segment end {other}"))),
        })
    }
}

/// Why a segment's end is unknown and therefore excluded from the total.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveSegmentDiscard {
    /// A previous loop process/incarnation left the segment open. The runtime
    /// after it is downtime or a pause, so the whole interval is unusable.
    ProcessRestart,
    /// A still-open segment was found when a new one was about to start. The
    /// previous writer never recorded an end; its true length is unknown.
    StaleOpenSegment,
}

impl ActiveSegmentDiscard {
    pub fn as_str(self) -> &'static str {
        match self {
            ActiveSegmentDiscard::ProcessRestart => "process_restart",
            ActiveSegmentDiscard::StaleOpenSegment => "stale_open_segment",
        }
    }

    fn parse(value: &str) -> Result<Self, ShenkoError> {
        Ok(match value {
            "process_restart" => ActiveSegmentDiscard::ProcessRestart,
            "stale_open_segment" => ActiveSegmentDiscard::StaleOpenSegment,
            other => return Err(storage(format!("unknown active-segment discard {other}"))),
        })
    }
}

/// One durable segment row, exactly as stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveRuntimeSegment {
    pub segment_id: EntityId,
    pub agent_id: EntityId,
    pub step_id: EntityId,
    pub step_no: u64,
    pub started_at_ms: i64,
    /// Set only for a measured interval.
    pub ended_at_ms: Option<i64>,
    pub end_reason: Option<ActiveSegmentEnd>,
    /// The durable `SubjectStepState` label observed when the segment closed.
    /// Informational: the reason above is the loop's own stopping condition.
    pub durable_step_state: Option<String>,
    /// Set only for an unknown-end segment; such a row is never counted.
    pub discarded_reason: Option<ActiveSegmentDiscard>,
    pub created_seq: i64,
}

impl ActiveRuntimeSegment {
    /// The measured duration of a segment, or `None` when its end is unknown.
    pub fn measured_ms(&self) -> Option<i64> {
        self.ended_at_ms
            .map(|end| (end - self.started_at_ms).max(0))
            .filter(|_| self.discarded_reason.is_none())
    }

    pub fn is_open(&self) -> bool {
        self.ended_at_ms.is_none() && self.discarded_reason.is_none()
    }
}

/// One read of the subject's main-loop active runtime.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MainActiveRuntime {
    /// Sum of every measured segment. Unknown-end segments contribute zero.
    pub total_active_ms: i64,
    pub measured_segments: Vec<ActiveRuntimeSegment>,
    /// The segment currently executing, if this read caught one.
    pub open_segment: Option<ActiveRuntimeSegment>,
    /// Segments whose end is unknown. They stay visible and never count.
    pub discarded_segments: Vec<ActiveRuntimeSegment>,
}

const COLUMNS: &str = "segment_id,agent_id,step_id,step_no,started_at_ms,ended_at_ms,\
                       end_reason,durable_step_state,discarded_reason,created_seq";

fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ActiveRuntimeSegment> {
    Ok(ActiveRuntimeSegment {
        segment_id: row.get::<_, String>(0)?.parse().map_err(|error: ShenkoError| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
        })?,
        agent_id: row.get::<_, String>(1)?.parse().map_err(|error: ShenkoError| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(error))
        })?,
        step_id: row.get::<_, String>(2)?.parse().map_err(|error: ShenkoError| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(error))
        })?,
        step_no: row.get::<_, i64>(3)? as u64,
        started_at_ms: row.get(4)?,
        ended_at_ms: row.get(5)?,
        end_reason: row
            .get::<_, Option<String>>(6)?
            .map(|value| ActiveSegmentEnd::parse(&value))
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(error))
            })?,
        durable_step_state: row.get(7)?,
        discarded_reason: row
            .get::<_, Option<String>>(8)?
            .map(|value| ActiveSegmentDiscard::parse(&value))
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(error))
            })?,
        created_seq: row.get(9)?,
    })
}

impl Store {
    /// Open one main-loop active segment for `step_id`.
    ///
    /// Any still-open segment for this subject is first discarded as
    /// `stale_open_segment`: a second `begin` can only happen when the first
    /// writer never recorded its end, and inventing that end would count an
    /// unknown interval. Nothing here waits, retries or sends.
    pub fn begin_subject_active_segment(
        &self,
        agent_id: EntityId,
        step_id: EntityId,
        step_no: u64,
        now_ms: i64,
    ) -> Result<ActiveRuntimeSegment, ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(storage)?;
                let mut seq = self.ledger.allocate_seq(&tx)?;
                // A stale open segment has an unknown end. Discard it in the
                // same transaction that opens the successor; the row keeps its
                // start and the reason instead of a fabricated end.
                let stale: Vec<String> = {
                    let mut stmt = tx
                        .prepare(
                            "SELECT segment_id FROM subject_active_runtime_segments
                             WHERE agent_id=?1 AND ended_at_ms IS NULL AND discarded_reason IS NULL",
                        )
                        .map_err(storage)?;
                    let rows = stmt
                        .query_map(params![agent_id.to_string()], |row| row.get::<_, String>(0))
                        .map_err(storage)?;
                    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)?
                };
                for segment in &stale {
                    tx.execute(
                        "UPDATE subject_active_runtime_segments SET discarded_reason=?2
                         WHERE segment_id=?1 AND ended_at_ms IS NULL AND discarded_reason IS NULL",
                        params![segment, ActiveSegmentDiscard::StaleOpenSegment.as_str()],
                    )
                    .map_err(storage)?;
                    self.write_commit_log(&tx, seq)?;
                    seq = self.ledger.allocate_seq(&tx)?;
                }
                let segment_id = EntityId::new();
                tx.execute(
                    "INSERT INTO subject_active_runtime_segments
                     (segment_id,agent_id,step_id,step_no,started_at_ms,ended_at_ms,end_reason,
                      durable_step_state,discarded_reason,created_seq)
                     VALUES (?1,?2,?3,?4,?5,NULL,NULL,NULL,NULL,?6)",
                    params![
                        segment_id.to_string(),
                        agent_id.to_string(),
                        step_id.to_string(),
                        step_no as i64,
                        now_ms,
                        seq.0 as i64,
                    ],
                )
                .map_err(storage)?;
                self.write_commit_log(&tx, seq)?;
                tx.commit().map_err(storage)?;
                Ok(ActiveRuntimeSegment {
                    segment_id,
                    agent_id,
                    step_id,
                    step_no,
                    started_at_ms: now_ms,
                    ended_at_ms: None,
                    end_reason: None,
                    durable_step_state: None,
                    discarded_reason: None,
                    created_seq: seq.0 as i64,
                })
            },
            |error| matches!(error, ShenkoError::Storage(_)),
        )
    }

    /// Close the open segment of `step_id` as a measured interval.
    ///
    /// A missing open segment is not an error: a meter write is observability,
    /// and it must never turn a completed step into a failure. `None` says no
    /// segment was open.
    pub fn end_subject_active_segment(
        &self,
        agent_id: EntityId,
        step_id: EntityId,
        end: ActiveSegmentEnd,
        step_state_label: Option<&'static str>,
        now_ms: i64,
    ) -> Result<Option<ActiveRuntimeSegment>, ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(storage)?;
                let open: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT segment_id, started_at_ms FROM subject_active_runtime_segments
                         WHERE agent_id=?1 AND step_id=?2 AND ended_at_ms IS NULL
                           AND discarded_reason IS NULL
                         ORDER BY started_at_ms DESC, created_seq DESC LIMIT 1",
                        params![agent_id.to_string(), step_id.to_string()],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()
                    .map_err(storage)?;
                let Some((segment_id, started_at_ms)) = open else {
                    return Ok(None);
                };
                // A clock that moved backwards can never produce a negative
                // interval: the end is clamped to the recorded start.
                let ended_at_ms = now_ms.max(started_at_ms);
                let seq = self.ledger.allocate_seq(&tx)?;
                tx.execute(
                    "UPDATE subject_active_runtime_segments
                     SET ended_at_ms=?2, end_reason=?3, durable_step_state=?4
                     WHERE segment_id=?1 AND ended_at_ms IS NULL AND discarded_reason IS NULL",
                    params![
                        segment_id,
                        ended_at_ms,
                        end.as_str(),
                        step_state_label,
                    ],
                )
                .map_err(storage)?;
                self.write_commit_log(&tx, seq)?;
                // Return the row as stored, never a reconstruction: the caller
                // reports exactly the segment the ledger now holds.
                let stored = tx
                    .query_row(
                        &format!("SELECT {COLUMNS} FROM subject_active_runtime_segments WHERE segment_id=?1"),
                        params![segment_id],
                        decode_row,
                    )
                    .map_err(storage)?;
                tx.commit().map_err(storage)?;
                Ok(Some(stored))
            },
            |error| matches!(error, ShenkoError::Storage(_)),
        )
    }

    /// Discard every segment a previous loop incarnation left open.
    ///
    /// The main-active total must not include process downtime, a pause or a
    /// crash: those intervals have no known active portion, so the whole
    /// segment is excluded and its unknown end stays recorded.
    pub fn discard_open_subject_active_segments(
        &self,
        agent_id: EntityId,
        reason: ActiveSegmentDiscard,
    ) -> Result<usize, ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(storage)?;
                let open: Vec<String> = {
                    let mut stmt = tx
                        .prepare(
                            "SELECT segment_id FROM subject_active_runtime_segments
                             WHERE agent_id=?1 AND ended_at_ms IS NULL AND discarded_reason IS NULL
                             ORDER BY started_at_ms, created_seq",
                        )
                        .map_err(storage)?;
                    let rows = stmt
                        .query_map(params![agent_id.to_string()], |row| row.get::<_, String>(0))
                        .map_err(storage)?;
                    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)?
                };
                for segment in &open {
                    let seq = self.ledger.allocate_seq(&tx)?;
                    tx.execute(
                        "UPDATE subject_active_runtime_segments SET discarded_reason=?2
                         WHERE segment_id=?1 AND ended_at_ms IS NULL AND discarded_reason IS NULL",
                        params![segment, reason.as_str()],
                    )
                    .map_err(storage)?;
                    self.write_commit_log(&tx, seq)?;
                }
                tx.commit().map_err(storage)?;
                Ok(open.len())
            },
            |error| matches!(error, ShenkoError::Storage(_)),
        )
    }

    /// Every segment of one subject, oldest first, including unknown-end rows.
    pub fn subject_active_runtime_segments(
        &self,
        agent_id: EntityId,
    ) -> Result<Vec<ActiveRuntimeSegment>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM subject_active_runtime_segments
                 WHERE agent_id=?1 ORDER BY started_at_ms, created_seq"
            ))
            .map_err(storage)?;
        let rows = stmt
            .query_map(params![agent_id.to_string()], decode_row)
            .map_err(storage)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)
    }

    /// The measured main-loop active runtime of one subject.
    pub fn subject_active_runtime(
        &self,
        agent_id: EntityId,
    ) -> Result<MainActiveRuntime, ShenkoError> {
        let mut view = MainActiveRuntime::default();
        for segment in self.subject_active_runtime_segments(agent_id)? {
            if segment.discarded_reason.is_some() {
                view.discarded_segments.push(segment);
            } else if let Some(measured) = segment.measured_ms() {
                view.total_active_ms += measured;
                view.measured_segments.push(segment);
            } else {
                view.open_segment = Some(segment);
            }
        }
        Ok(view)
    }

    /// Measured main-loop active time inside `[from_ms, to_ms]`.
    ///
    /// This is the subject-relative clock a forgetting signal needs: it never
    /// counts an open (unknown-end) segment and never substitutes wall clock.
    /// Intervals are the overlap of measured segments with the window, so a
    /// partially covered segment contributes only its covered part.
    pub fn subject_active_runtime_between(
        &self,
        agent_id: EntityId,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<i64, ShenkoError> {
        if to_ms <= from_ms {
            return Ok(0);
        }
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT started_at_ms, ended_at_ms FROM subject_active_runtime_segments
                 WHERE agent_id=?1 AND ended_at_ms IS NOT NULL AND discarded_reason IS NULL
                   AND ended_at_ms > ?2 AND started_at_ms < ?3",
            )
            .map_err(storage)?;
        let rows = stmt
            .query_map(params![agent_id.to_string(), from_ms, to_ms], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(storage)?;
        let mut total = 0i64;
        for row in rows {
            let (started_at_ms, ended_at_ms) = row.map_err(storage)?;
            total += (ended_at_ms.min(to_ms) - started_at_ms.max(from_ms)).max(0);
        }
        Ok(total)
    }

    /// Main-loop active time since one durable event timestamp.
    ///
    /// `from_ms` must come from a mechanism record (for example an
    /// intervention's `used_at_ms`). When the caller does not know it, the
    /// answer is `None` — an unknown last-use is not "zero" and not wall clock.
    pub fn subject_active_runtime_since(
        &self,
        agent_id: EntityId,
        from_ms: Option<i64>,
        to_ms: i64,
    ) -> Result<Option<i64>, ShenkoError> {
        match from_ms {
            Some(from_ms) => self
                .subject_active_runtime_between(agent_id, from_ms, to_ms)
                .map(Some),
            None => Ok(None),
        }
    }

    fn write_commit_log(
        &self,
        tx: &rusqlite::Transaction<'_>,
        seq: shenko_types::ids::LedgerSeq,
    ) -> Result<(), ShenkoError> {
        tx.execute(
            "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count)
             VALUES (?1,?2,?3,1)",
            params![
                self.ledger.ledger_id().to_string(),
                seq.0 as i64,
                self.capabilities.incarnation.to_string(),
            ],
        )
        .map_err(storage)?;
        Ok(())
    }
}
