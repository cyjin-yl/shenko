//! Durable admission and replay of browser input submissions — SPEC §41.18.
//!
//! A submission is deduplicated by (agent_id, submission_id): a replay of the
//! same payload returns the original event and admission commit, and the same
//! key with a different payload is a conflict that never rewrites the original
//! fact. The InboxEvent row and its submission binding commit in one ledger
//! transaction, so "accepted" is a durable fact before the transport answers.
//!
//! Consumption state stays honest: `inbox_events.consumed` means the row was
//! durably staged into a planned subject step, set in the same transaction as the
//! staging link. The normal loop drains unstaged inputs in bounded batches.
//! Staging is recovery evidence, not a selected receipt: selection requires
//! the exact event and raw-event identity in the step's sealed input plan.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::subject::{SubjectStep, SubjectStepState};

use crate::store::{Store, storage_error};

/// Source label recorded on InboxEvent rows admitted through the WebUI.
pub const WEB_INPUT_SOURCE: &str = "web_ui";

pub(crate) const WEB_INPUT_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS web_input_submissions (
    agent_id TEXT NOT NULL,
    submission_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    payload TEXT NOT NULL,
    accepted_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (agent_id, submission_id)
);
CREATE INDEX IF NOT EXISTS idx_web_input_event
    ON web_input_submissions(event_id);
";

/// The durable event identity behind one submission.
pub struct WebSubmissionRecord {
    /// submission key as stored (echo for callers that parsed a partial row)
    pub submission_id: String,
    /// stable identity of the durable InboxEvent
    pub event_id: EntityId,
    /// canonical payload JSON admitted under this key
    pub payload: String,
    /// ledger commit that admitted this input
    pub accepted_commit: CommitRef,
}

pub enum WebAdmission {
    /// New input committed; caller must attempt live-loop delivery once.
    Admitted(WebSubmissionRecord),
    /// Same key and payload replayed; do not deliver again.
    Replay(WebSubmissionRecord),
    /// Same key, different payload — original fact preserved, never reused.
    Conflict,
}

impl Store {
    /// Admit a browser input durably before any transport receipt. The
    /// InboxEvent and its submission binding commit atomically; a replayed
    /// identical payload replays the original record without another input.
    pub fn admit_web_submission(
        &self,
        agent_id: EntityId,
        submission_id: &str,
        payload: &str,
        event: &shenko_types::store::RawInboxEvent,
    ) -> Result<WebAdmission, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_admit_web_submission(agent_id, submission_id, payload, event),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_admit_web_submission(
        &self,
        agent_id: EntityId,
        submission_id: &str,
        payload: &str,
        event: &shenko_types::store::RawInboxEvent,
    ) -> Result<WebAdmission, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        if let Some(record) =
            read_web_submission(&tx, agent_id, submission_id, self.ledger.ledger_id())?
        {
            if record.payload != payload {
                return Ok(WebAdmission::Conflict);
            }
            tx.commit().map_err(storage_error)?;
            return Ok(WebAdmission::Replay(record));
        }
        let commit_seq = self.ledger.allocate_seq(&tx)?;
        tx.execute(
            "INSERT INTO inbox_events (event_id, agent_id, kind, source, consumed, data)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            params![
                event.event_id.to_string(),
                event.agent_id.to_string(),
                event.kind,
                event.source,
                event.payload_json,
            ],
        )
        .map_err(storage_error)?;
        tx.execute(
            "INSERT INTO web_input_submissions
                (agent_id, submission_id, event_id, payload, accepted_commit_seq)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                agent_id.to_string(),
                submission_id,
                event.event_id.to_string(),
                payload,
                commit_seq.0 as i64,
            ],
        )
        .map_err(storage_error)?;
        self.record_subject_commit(
            &tx,
            CommitRef::new(self.ledger.ledger_id(), commit_seq),
            1,
        )?;
        tx.commit().map_err(storage_error)?;
        Ok(WebAdmission::Admitted(WebSubmissionRecord {
            submission_id: submission_id.to_owned(),
            event_id: event.event_id,
            payload: payload.to_owned(),
            accepted_commit: CommitRef::new(self.ledger.ledger_id(), commit_seq),
        }))
    }

    /// The durable record behind one submission key, for reconnect queries.
    pub fn web_submission(
        &self,
        agent_id: EntityId,
        submission_id: &str,
    ) -> Result<Option<WebSubmissionRecord>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        read_web_submission(&conn, agent_id, submission_id, self.ledger.ledger_id())
    }

    /// The step an input was selected into, if its S0 actually sealed.
    /// `subject_raw_inputs` rows persist while a step is still `Planned` —
    /// staging alone is not selection, so a failed projection, CAS failure
    /// or resnapshot must not report `selected`. The sealed plan must belong
    /// to the actual step and contain both this raw identity and the exact
    /// admitted event. `prepare_context` admits every fresh raw ID into its
    /// projection before S0; Store's S0 validation freezes that set atomically.
    pub fn web_input_selected_step(
        &self,
        agent_id: EntityId,
        event_id: EntityId,
    ) -> Result<Option<EntityId>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT s.data, r.raw_event_id, i.data FROM subject_raw_inputs r
             JOIN subject_steps s ON s.step_id = r.step_id
             JOIN subject_branch_heads h ON h.branch_id = s.branch_id
             JOIN inbox_events i ON i.event_id = r.source_event_id
               AND i.agent_id = h.agent_id
             WHERE h.agent_id = ?1 AND r.source_event_id = ?2
               AND s.state != 'planned'
             ORDER BY r.commit_seq DESC, r.seq DESC",
        ).map_err(storage_error)?;
        let mut rows = stmt.query(params![agent_id.to_string(), event_id.to_string()])
            .map_err(storage_error)?;
        while let Some(row) = rows.next().map_err(storage_error)? {
            let step: SubjectStep = serde_json::from_str(&row.get::<_, String>(0).map_err(storage_error)?)?;
            if step.state == SubjectStepState::Planned {
                continue;
            }
            let raw_id: String = row.get(1).map_err(storage_error)?;
            let original: serde_json::Value = serde_json::from_str(
                &row.get::<_, String>(2).map_err(storage_error)?,
            )?;
            let object = step.input_plan_ref.as_ref()
                .ok_or_else(|| ShenkoError::conflict("sealed web-input step has no input plan"))?;
            let bytes = self.cas.read_object(&object.digest)?;
            if object.locator.is_empty()
                || bytes.len() as u64 != object.bytes
                || shenko_types::object::Digest::blake3(&bytes) != object.digest
            {
                return Err(ShenkoError::ObjectClosureIncomplete { missing_count: 1 });
            }
            let plan: serde_json::Value = serde_json::from_slice(&bytes)?;
            if plan.get("schema_version").and_then(serde_json::Value::as_u64) != Some(10)
                || plan.get("step_id") != Some(&serde_json::to_value(step.step_id)?)
                || plan.get("step_no").and_then(serde_json::Value::as_u64) != Some(step.step_no)
            {
                return Err(ShenkoError::conflict("sealed web-input plan identity mismatch"));
            }
            let raw_is_selected = plan.pointer("/context/source/input_raw_event_ids")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(raw_id.as_str())));
            let event_is_selected = plan.get("events").and_then(serde_json::Value::as_array)
                .is_some_and(|events| events.contains(&original));
            if raw_is_selected && event_is_selected {
                return Ok(Some(step.step_id));
            }
        }
        Ok(None)
    }
}

fn read_web_submission(
    conn: &rusqlite::Connection,
    agent_id: EntityId,
    submission_id: &str,
    ledger_id: EntityId,
) -> Result<Option<WebSubmissionRecord>, ShenkoError> {
    conn.query_row(
        "SELECT event_id, payload, accepted_commit_seq FROM web_input_submissions
         WHERE agent_id = ?1 AND submission_id = ?2",
        params![agent_id.to_string(), submission_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )
    .optional()
    .map_err(storage_error)?
    .map(|(event_id, payload, seq)| {
        let event_id = event_id.parse::<EntityId>().map_err(|e| {
            ShenkoError::storage(format!("bad web submission event id {event_id}: {e}"))
        })?;
        Ok(WebSubmissionRecord {
            submission_id: submission_id.to_owned(),
            event_id,
            payload,
            accepted_commit: CommitRef::new(
                ledger_id,
                LedgerSeq(u64::try_from(seq).map_err(|error| {
                    ShenkoError::storage(format!("bad commit seq {seq}: {error}"))
                })?),
            ),
        })
    })
    .transpose()
}


