//! Durable commitments the subject made about itself — SPEC §13.8/§41.5.
//!
//! Two facts live here, both written in the SAME transaction as the S1
//! decision that authored them:
//!
//! * `subject_obligations` — a promise the subject took on. It stays open
//!   until a later committed step completes or cancels it, and it is read
//!   back into every later S0. An obligation is not a scheduler: nothing
//!   here wakes the subject by itself.
//! * `subject_attention_policies` — the subject's own do-not-disturb
//!   decision. It changes input selection only: it never rewrites history,
//!   never grants authority, and an `off` decision without sources and a
//!   review time is refused at S1 admission, not here.

use rusqlite::{OptionalExtension, params};

use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;
use shenko_types::subject::{AttentionPolicy, AttentionState, PendingObligation};

use crate::store::Store;

pub(crate) const SCHEMA: &str = r#"
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
"#;

fn storage(error: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::storage(error.to_string())
}

fn parse_entity_id(value: &str) -> Result<EntityId, ShenkoError> {
    value.parse().map_err(storage)
}

impl Store {
    /// The obligations this subject is still carrying. They are read back
    /// into every later S0 until a later committed step completes or
    /// cancels them.
    pub fn open_subject_obligations(
        &self,
        agent_id: EntityId,
    ) -> Result<Vec<PendingObligation>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT obligation_id,description,depends_on_task_id,resume_condition,
                        origin_event_id,due_at
                 FROM subject_obligations WHERE agent_id=?1 AND state='open'
                 ORDER BY created_seq,obligation_id",
            )
            .map_err(storage)?;
        let rows = stmt
            .query_map(params![agent_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        rows.into_iter()
            .map(
                |(id, description, depends, resume, origin, due)| {
                    Ok(PendingObligation {
                        obligation_id: parse_entity_id(&id)?,
                        description,
                        depends_on_task_id: depends
                            .as_deref()
                            .map(parse_entity_id)
                            .transpose()?,
                        resume_condition: resume,
                        origin_event_id: origin.as_deref().map(parse_entity_id).transpose()?,
                        due_at: due
                            .as_deref()
                            .map(parse_timestamp)
                            .transpose()?,
                    })
                },
            )
            .collect()
    }

    /// The attention (免打扰) decision currently in force. The newest
    /// committed decision wins; it is never overwritten by arrival order.
    pub fn current_subject_attention(
        &self,
        agent_id: EntityId,
    ) -> Result<Option<AttentionPolicy>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let row: Option<(String, String, String, String, Option<String>, String)> = conn
            .query_row(
                "SELECT intent_id,step_id,state,sources,review_after,reason
                 FROM subject_attention_policies WHERE agent_id=?1
                 ORDER BY created_seq DESC, intent_id DESC LIMIT 1",
                params![agent_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(storage)?;
        let Some((intent_id, step_id, state, sources, review_after, reason)) = row else {
            return Ok(None);
        };
        let state: AttentionState = serde_json::from_str(&state).map_err(ShenkoError::Serde)?;
        let sources: Vec<String> =
            serde_json::from_str(&sources).map_err(ShenkoError::Serde)?;
        Ok(Some(AttentionPolicy {
            intent_id: parse_entity_id(&intent_id)?,
            state,
            sources,
            reason,
            review_after: review_after.as_deref().map(parse_timestamp).transpose()?,
            created_step_id: parse_entity_id(&step_id)?,
        }))
    }
}

fn parse_timestamp(value: &str) -> Result<chrono::DateTime<chrono::Utc>, ShenkoError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&chrono::Utc))
        .map_err(storage)
}
