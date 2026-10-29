//! Durable admission and replay of browser input submissions — SPEC §41.18.
//!
//! A submission is deduplicated by (agent_id, submission_id): a replay of the
//! same payload returns the original event and admission commit, and the same
//! key with a different payload is a conflict that never rewrites the original
//! fact. The InboxEvent row and its submission binding commit in one ledger
//! transaction, so "accepted" is a durable fact before the transport answers.
//!
//! Consumption state stays honest: `inbox_events.consumed` means the row was
//! durably staged into a subject step (S0), set in the same transaction as the
//! staging link. A row still marked unconsumed at loop start was admitted but
//! never staged, so the subject loop re-feeds it; already-staged or already-
//! disposed events are never delivered twice.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::ids::{EntityId, LedgerSeq};

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

    /// Web input rows admitted but never staged into a subject step. The
    /// loop re-feeds these once at start so a crash between admission and
    /// delivery cannot lose accepted input.
    pub fn pending_web_inputs(&self, agent_id: EntityId) -> Result<Vec<String>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT i.data FROM inbox_events i
                 JOIN web_input_submissions w
                   ON w.agent_id = i.agent_id AND w.event_id = i.event_id
                 WHERE i.agent_id = ?1 AND i.consumed = 0
                 ORDER BY i.rowid",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map([agent_id.to_string()], |row| row.get::<_, String>(0))
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<String>>>()
            .map_err(storage_error)?;
        Ok(rows)
    }

    /// The step an input was selected into (S0 staging link), if any.
    /// Selection is not a disposition; the caller reports it separately.
    pub fn web_input_selected_step(
        &self,
        agent_id: EntityId,
        event_id: EntityId,
    ) -> Result<Option<EntityId>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        conn.query_row(
            "SELECT r.step_id FROM subject_raw_inputs r
             JOIN subject_steps s ON s.step_id = r.step_id
             JOIN subject_branch_heads h ON h.branch_id = s.branch_id
             WHERE h.agent_id = ?1 AND r.source_event_id = ?2
             ORDER BY r.seq DESC LIMIT 1",
            params![agent_id.to_string(), event_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(storage_error)?
        .map(|id| {
            id.parse::<EntityId>()
                .map_err(|e| ShenkoError::storage(format!("bad selected step id {id}: {e}")))
        })
        .transpose()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path().join("store.db"), dir.path().join("cas"))
            .expect("open store");
        (dir, store)
    }

    fn raw_event(agent_id: EntityId, data: &serde_json::Value) -> shenko_types::store::RawInboxEvent {
        shenko_types::store::RawInboxEvent {
            event_id: EntityId::new(),
            agent_id,
            source: WEB_INPUT_SOURCE.into(),
            kind: "message".into(),
            payload_json: data.to_string(),
            received_at_ms: 0,
        }
    }

    #[test]
    fn identical_replay_replays_original_and_conflict_refuses() {
        let (_dir, store) = store();
        let agent = EntityId::new();
        let event = raw_event(agent, &serde_json::json!({"content": "继续"}));
        let first = store
            .admit_web_submission(agent, "sub-1", "{\"content\":\"继续\"}", &event)
            .expect("admit");
        let admitted = match first {
            WebAdmission::Admitted(record) => record,
            _ => panic!("first admit must be Admitted"),
        };
        // New event id on retry: replay must still resolve to the original.
        let retry = raw_event(agent, &serde_json::json!({"content": "继续"}));
        let replay = store
            .admit_web_submission(agent, "sub-1", "{\"content\":\"继续\"}", &retry)
            .expect("replay");
        match replay {
            WebAdmission::Replay(record) => {
                assert_eq!(record.event_id, admitted.event_id);
                assert_eq!(record.accepted_commit, admitted.accepted_commit);
            }
            _ => panic!("identical payload must replay original"),
        }
        let conflict = store
            .admit_web_submission(agent, "sub-1", "{\"content\":\"改\"}", &retry)
            .expect("conflict lookup");
        assert!(matches!(conflict, WebAdmission::Conflict));
        // The pending row is exactly the admitted event payload.
        let pending = store.pending_web_inputs(agent).expect("pending");
        assert_eq!(pending, vec![event.payload_json]);
        assert_eq!(
            store.web_submission(agent, "sub-1").expect("lookup").map(|r| r.event_id),
            Some(admitted.event_id),
        );
        // Consumed rows are no longer pending (S0 staging sets consumed).
        {
            let conn = store.ledger.conn.lock();
            conn.execute(
                "UPDATE inbox_events SET consumed=1 WHERE event_id=?1",
                [admitted.event_id.to_string()],
            )
            .expect("mark consumed");
        }
        assert!(store.pending_web_inputs(agent).expect("pending").is_empty());
    }
}
