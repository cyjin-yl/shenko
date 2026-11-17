//! Durable commitments the subject made about itself.
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
use shenko_types::subject::{AttentionPolicy, AttentionState, PendingObligation, StepDecision, SubjectBranchHead, SubjectStep, PolicyKind, ObligationTerminalChange, ObligationTerminalOutcome, ObligationTerminalReceipt, ObligationTerminalState};
use shenko_types::{commit::CommitRef, output::OutputAction};
use std::collections::HashSet;

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
-- Rebuildable execution invalidation index, never an execution authority.
-- Only the newest revision per binding is retained; no duplicate event log.
CREATE TABLE IF NOT EXISTS context_execution_clock(
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), revision INTEGER NOT NULL);
INSERT OR IGNORE INTO context_execution_clock(singleton,revision) VALUES(1,0);
CREATE TABLE IF NOT EXISTS context_execution_revisions(
 binding_id TEXT PRIMARY KEY NOT NULL, revision INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS context_execution_revision
 ON context_execution_revisions(revision,binding_id);
CREATE TRIGGER IF NOT EXISTS context_revision_subject_intents_insert AFTER INSERT ON subject_intents BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT NEW.intent_id,revision FROM context_execution_clock WHERE singleton=1 AND NEW.intent_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
CREATE TRIGGER IF NOT EXISTS context_revision_subject_intents_update AFTER UPDATE ON subject_intents BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT NEW.intent_id,revision FROM context_execution_clock WHERE singleton=1 AND NEW.intent_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
CREATE TRIGGER IF NOT EXISTS context_revision_subject_intents_delete AFTER DELETE ON subject_intents BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT OLD.intent_id,revision FROM context_execution_clock WHERE singleton=1 AND OLD.intent_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
CREATE TRIGGER IF NOT EXISTS context_revision_tasks_insert AFTER INSERT ON tasks BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT NEW.task_id,revision FROM context_execution_clock WHERE singleton=1 AND NEW.task_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
CREATE TRIGGER IF NOT EXISTS context_revision_tasks_update AFTER UPDATE ON tasks BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT NEW.task_id,revision FROM context_execution_clock WHERE singleton=1 AND NEW.task_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
CREATE TRIGGER IF NOT EXISTS context_revision_tasks_delete AFTER DELETE ON tasks BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT OLD.task_id,revision FROM context_execution_clock WHERE singleton=1 AND OLD.task_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
CREATE TRIGGER IF NOT EXISTS context_revision_memory_intents_insert AFTER INSERT ON memory_intents BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT NEW.intent_id,revision FROM context_execution_clock WHERE singleton=1 AND NEW.intent_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
CREATE TRIGGER IF NOT EXISTS context_revision_memory_intents_update AFTER UPDATE ON memory_intents BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT NEW.intent_id,revision FROM context_execution_clock WHERE singleton=1 AND NEW.intent_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
CREATE TRIGGER IF NOT EXISTS context_revision_memory_intents_delete AFTER DELETE ON memory_intents BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT OLD.intent_id,revision FROM context_execution_clock WHERE singleton=1 AND OLD.intent_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
CREATE TRIGGER IF NOT EXISTS context_revision_task_results_insert AFTER INSERT ON task_results BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT NEW.task_id,revision FROM context_execution_clock WHERE singleton=1 AND NEW.task_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
CREATE TRIGGER IF NOT EXISTS context_revision_task_results_update AFTER UPDATE ON task_results BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT NEW.task_id,revision FROM context_execution_clock WHERE singleton=1 AND NEW.task_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
CREATE TRIGGER IF NOT EXISTS context_revision_task_results_delete AFTER DELETE ON task_results BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT OLD.task_id,revision FROM context_execution_clock WHERE singleton=1 AND OLD.task_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
CREATE TRIGGER IF NOT EXISTS context_revision_operations_insert AFTER INSERT ON operations BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT NEW.operation_id,revision FROM context_execution_clock WHERE singleton=1 AND NEW.operation_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT json_extract(NEW.data,'$.task_id'),revision FROM context_execution_clock WHERE singleton=1 AND json_extract(NEW.data,'$.task_id') IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
CREATE TRIGGER IF NOT EXISTS context_revision_operations_update AFTER UPDATE ON operations BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT NEW.operation_id,revision FROM context_execution_clock WHERE singleton=1 AND NEW.operation_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT json_extract(NEW.data,'$.task_id'),revision FROM context_execution_clock WHERE singleton=1 AND json_extract(NEW.data,'$.task_id') IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
CREATE TRIGGER IF NOT EXISTS context_revision_operations_delete AFTER DELETE ON operations BEGIN
 UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT OLD.operation_id,revision FROM context_execution_clock WHERE singleton=1 AND OLD.operation_id IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
 INSERT INTO context_execution_revisions(binding_id,revision)
 SELECT json_extract(OLD.data,'$.task_id'),revision FROM context_execution_clock WHERE singleton=1 AND json_extract(OLD.data,'$.task_id') IS NOT NULL
 ON CONFLICT(binding_id) DO UPDATE SET revision=excluded.revision;
END;
"#;

fn storage(error: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::storage(error.to_string())
}

fn parse_entity_id(value: &str) -> Result<EntityId, ShenkoError> {
    value.parse().map_err(storage)
}

/// Stable keyset coordinate for complete obligation retrieval.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ObligationCursor {
    pub created_seq: u64,
    pub obligation_id: EntityId,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ObligationPage {
    pub total_open: u64,
    pub obligations: Vec<PendingObligation>,
    pub next: Option<ObligationCursor>,
}

#[derive(Debug, Clone)]
pub struct ObligationFrontier {
    pub total_open: u64,
    pub rotation_start: u64,
    pub obligations: Vec<PendingObligation>,
}

type ObligationRow = (ObligationCursor, PendingObligation);

fn obligation_rows(conn: &rusqlite::Connection, suffix: &str, params: impl rusqlite::Params)
    -> Result<Vec<ObligationRow>, ShenkoError> {
    let sql = format!("SELECT obligation_id,description,depends_on_task_id,resume_condition,origin_event_id,due_at,created_seq FROM subject_obligations {suffix}");
    let mut statement = conn.prepare(&sql).map_err(storage)?;
    let raw = statement.query_map(params, |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
        row.get::<_, Option<String>>(2)?, row.get::<_, String>(3)?, row.get::<_, Option<String>>(4)?,
        row.get::<_, Option<String>>(5)?, row.get::<_, u64>(6)?)))
        .map_err(storage)?.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)?;
    raw.into_iter().map(|(id, description, depends, resume_condition, origin, due, created_seq)| {
        let obligation_id = parse_entity_id(&id)?;
        Ok((ObligationCursor { created_seq, obligation_id }, PendingObligation {
            obligation_id, description, resume_condition,
            depends_on_task_id: depends.as_deref().map(parse_entity_id).transpose()?,
            origin_event_id: origin.as_deref().map(parse_entity_id).transpose()?,
            due_at: due.as_deref().map(parse_timestamp).transpose()?,
        }))
    }).collect()
}

impl Store {
    /// A local invalidation watermark and the bindings changed since it.
    /// This index follows actual SQLite writes (including imported/recovered
    /// rows); callers still read authoritative execution facts for each hit.
    /// `None` starts a fresh cache without scanning historical invalidations.
    pub fn context_execution_changes(
        &self,
        since: Option<u64>,
    ) -> Result<(u64, Vec<String>), ShenkoError> {
        let conn = self.ledger.conn.lock();
        let revision: u64 = conn.query_row(
            "SELECT revision FROM context_execution_clock WHERE singleton=1", [], |row| row.get(0),
        ).map_err(storage)?;
        let Some(since) = since.filter(|since| *since <= revision) else {
            return Ok((revision, Vec::new()));
        };
        let mut statement = conn.prepare(
            "SELECT binding_id FROM context_execution_revisions WHERE revision>?1 AND revision<=?2 ORDER BY revision,binding_id"
        ).map_err(storage)?;
        let changed = statement.query_map(params![since, revision], |row| row.get(0))
            .map_err(storage)?.collect::<rusqlite::Result<Vec<String>>>().map_err(storage)?;
        Ok((revision, changed))
    }

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

    /// Complete, bounded live read. A cursor is a keyset coordinate, so
    /// completing an earlier obligation cannot make the next page skip one.
    pub fn subject_obligations_page(&self, agent_id: EntityId, after: Option<&ObligationCursor>, limit: usize)
        -> Result<ObligationPage, ShenkoError> {
        let limit = limit.clamp(1, 50);
        let conn = self.ledger.conn.lock();
        let total_open = conn.query_row("SELECT COUNT(*) FROM subject_obligations WHERE agent_id=?1 AND state='open'",
            [agent_id.to_string()], |row| row.get(0)).map_err(storage)?;
        let mut rows = obligation_rows(&conn,
            "WHERE agent_id=?1 AND state='open' AND (?2 IS NULL OR created_seq>?2 OR (created_seq=?2 AND obligation_id>?3)) ORDER BY created_seq,obligation_id LIMIT ?4",
            params![agent_id.to_string(), after.map(|cursor| cursor.created_seq),
                after.map(|cursor| cursor.obligation_id.to_string()), (limit + 1) as i64])?;
        let next = (rows.len() > limit).then(|| rows[limit - 1].0.clone());
        rows.truncate(limit);
        Ok(ObligationPage { total_open, obligations: rows.into_iter().map(|(_, item)| item).collect(), next })
    }

    /// Full retained body and state, including completed/cancelled records.
    pub fn subject_obligation(&self, agent_id: EntityId, id: EntityId)
        -> Result<Option<(PendingObligation, String)>, ShenkoError> {
        Ok(read_obligation(&self.ledger.conn.lock(), id)?
            .filter(|(owner, _, _)| *owner == agent_id).map(|(_, body, state)| (body, state)))
    }

    /// A bounded working frontier. Explicit references get at most one
    /// quarter of the slots; the durable turn rotates the remaining slots.
    /// Every body stays intact in storage and available through keyset reads.
    pub fn subject_obligation_frontier(&self, agent_id: EntityId, turn: u64,
        preferred: &[EntityId], limit: usize) -> Result<ObligationFrontier, ShenkoError> {
        let limit = limit.clamp(1, 50);
        let conn = self.ledger.conn.lock();
        let total_open: u64 = conn.query_row("SELECT COUNT(*) FROM subject_obligations WHERE agent_id=?1 AND state='open'",
            [agent_id.to_string()], |row| row.get(0)).map_err(storage)?;
        let rotation_start = if total_open == 0 { 0 } else { turn % total_open };
        let mut obligations = Vec::new();
        let mut seen = HashSet::new();
        for id in preferred.iter().take((limit / 4).max(1)) {
            if let Some((owner, body, state)) = read_obligation(&conn, *id)?
                && owner == agent_id && state == "open" && seen.insert(*id) {
                obligations.push(body);
            }
        }
        let mut rows = obligation_rows(&conn,
            "WHERE agent_id=?1 AND state='open' ORDER BY created_seq,obligation_id LIMIT ?2 OFFSET ?3",
            params![agent_id.to_string(), limit as i64, rotation_start])?;
        if rows.len() < limit && rotation_start > 0 {
            rows.extend(obligation_rows(&conn,
                "WHERE agent_id=?1 AND state='open' ORDER BY created_seq,obligation_id LIMIT ?2",
                params![agent_id.to_string(), (limit - rows.len()) as i64])?);
        }
        for (_, body) in rows {
            if obligations.len() == limit { break; }
            if seen.insert(body.obligation_id) { obligations.push(body); }
        }
        Ok(ObligationFrontier { total_open, rotation_start, obligations })
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


pub(crate) const OBLIGATION_TERMINAL_CONTRACT: &str = "subject/obligation-terminal/1";

/// Bind the parsed action to exactly one policy, rejecting duplicate targets.
pub(crate) fn obligation_requests(
    decision: &StepDecision,
) -> Result<Vec<(EntityId, &ObligationTerminalChange)>, ShenkoError> {
    let mut requests = Vec::new();
    let mut targets = HashSet::new();
    let mut ids = HashSet::new();
    for control in &decision.output.controls {
        let OutputAction::ObligationTerminal { change } = &control.action else { continue };
        if control.action_id == EntityId::nil() || change.obligation_id == EntityId::nil()
            || change.reason.trim().is_empty() || !targets.insert(change.obligation_id)
            || !ids.insert(control.action_id)
        {
            return Err(ShenkoError::conflict("invalid or duplicate obligation terminal control"));
        }
        let mut policies = decision.policy_intents.iter().filter(|intent| intent.intent_id == control.action_id);
        let Some(policy) = policies.next() else {
            return Err(ShenkoError::conflict("obligation control has no matching policy"));
        };
        if policies.next().is_some()
            || !matches!(&policy.kind, PolicyKind::ObligationTerminal { change: policy_change } if policy_change == change)
            || policy.target_revision.is_some()
            || decision.output.controls.iter().filter(|other| other.action_id == control.action_id).count() != 1
        {
            return Err(ShenkoError::conflict("obligation policy differs from its parsed control"));
        }
        requests.push((control.action_id, change));
    }
    for policy in &decision.policy_intents {
        if matches!(policy.kind, PolicyKind::ObligationTerminal { .. }) && !ids.contains(&policy.intent_id) {
            return Err(ShenkoError::conflict("obligation policy has no matching parsed control"));
        }
    }
    Ok(requests)
}

impl Store {
    pub(crate) fn sealed_obligations(&self, decision: &StepDecision) -> Result<Vec<PendingObligation>, ShenkoError> {
        #[derive(serde::Deserialize)]
        struct Snapshot {
            schema_version: u32,
            step_id: EntityId,
            obligations: Vec<PendingObligation>,
        }
        // Deserialize the owned vector once. The payload and other large S0
        // fields are skipped rather than copied into an intermediate Value.
        // P6: authenticate the sealed bytes (digest + length) before
        // deserializing, so a pathname swap cannot inject a different S0.
        let bytes = self.obligation_object_bytes(&decision.input_plan_ref)
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let input: Snapshot = serde_json::from_slice(&bytes)?;
        // The obligation snapshot rides the sealed S0 plan. Which plan
        // schemas this store still reads is decided in one place
        // (`admitted_input_plan_schema`), because a reader that pins its own
        // version silently refuses every obligation the subject settled
        // after the plan schema moved on. 13 adds the reconciliation docket
        // and leaves the obligation body untouched, so a 13 plan carries the
        // same obligations a 12 plan did; old bytes keep their old meaning.
        if !crate::store::admitted_input_plan_schema(Some(u64::from(input.schema_version)))
            || input.step_id != decision.step_id
        {
            return Err(ShenkoError::conflict("obligation change is not bound to its actual S0"));
        }
        let obligations = input.obligations;
        let mut ids = HashSet::new();
        if obligations.iter().any(|item| item.obligation_id == EntityId::nil() || !ids.insert(item.obligation_id)) {
            return Err(ShenkoError::conflict("invalid obligation identities in S0"));
        }
        Ok(obligations)
    }

    /// Only the projection state changes. Creation provenance is immutable.
    pub(crate) fn apply_obligation_changes(
        &self,
        tx: &rusqlite::Transaction<'_>,
        head: &SubjectBranchHead,
        step: &SubjectStep,
        decision: &StepDecision,
        commit: CommitRef,
    ) -> Result<Vec<ObligationTerminalReceipt>, ShenkoError> {
        let requests = obligation_requests(decision)?;
        if requests.is_empty() { return Ok(Vec::new()); }
        let sealed = self.sealed_obligations(decision)?;
        let mut receipts = Vec::with_capacity(requests.len());
        for (intent_id, change) in requests {
            let snapshot = sealed.iter().find(|item| item.obligation_id == change.obligation_id)
                .ok_or_else(|| ShenkoError::conflict("obligation target was not shown in S0"))?;
            let current = read_obligation(tx, change.obligation_id)?
                .ok_or_else(|| ShenkoError::conflict("obligation target no longer exists"))?;
            if current.0 != head.agent_id || &current.1 != snapshot {
                return Err(ShenkoError::conflict("obligation target is foreign or differs from its sealed body"));
            }
            let outcome = match current.2.as_str() {
                "open" => {
                    let changed = tx.execute(
                        "UPDATE subject_obligations SET state=?2,updated_seq=?3 WHERE obligation_id=?1 AND state='open'",
                        params![change.obligation_id.to_string(), change.state.as_str(), commit.seq.0 as i64],
                    ).map_err(storage)?;
                    if changed != 1 { return Err(ShenkoError::conflict("obligation state changed during S1")); }
                    ObligationTerminalOutcome::Applied
                }
                "completed" | "cancelled" => ObligationTerminalOutcome::Rejected {
                    current_state: if current.2 == "completed" { ObligationTerminalState::Completed } else { ObligationTerminalState::Cancelled },
                    reason: "the sealed obligation is already terminal; terminal states are sticky".into(),
                },
                _ => return Err(ShenkoError::conflict("invalid stored obligation state")),
            };
            receipts.push(ObligationTerminalReceipt {
                schema_version: 1, step_id: step.step_id, intent_id,
                input_plan_ref: decision.input_plan_ref.clone(), change: change.clone(),
                sealed_obligation: snapshot.clone(), outcome, committed_at: commit,
            });
        }
        Ok(receipts)
    }

    pub(crate) fn settle_obligation_intents(
        &self,
        tx: &rusqlite::Transaction<'_>,
        receipts: &[ObligationTerminalReceipt],
        seq: u64,
    ) -> Result<(), ShenkoError> {
        for receipt in receipts {
            let changed = tx.execute(
                "UPDATE subject_intents SET state='settled',result_ref=?2,revision=revision+1,updated_seq=?3
                 WHERE intent_id=?1 AND step_id=?4 AND kind='policy' AND contract=?5 AND state='ready'",
                params![receipt.intent_id.to_string(), serde_json::to_string(receipt)?, seq as i64,
                    receipt.step_id.to_string(), OBLIGATION_TERMINAL_CONTRACT],
            ).map_err(storage)?;
            if changed != 1 { return Err(ShenkoError::conflict("missing obligation terminal materialization")); }
        }
        Ok(())
    }
}

fn read_obligation(
    conn: &rusqlite::Connection,
    id: EntityId,
) -> Result<Option<(EntityId, PendingObligation, String)>, ShenkoError> {
    let row: Option<(String, String, Option<String>, String, Option<String>, Option<String>, String)> = conn.query_row(
        "SELECT agent_id,description,depends_on_task_id,resume_condition,origin_event_id,due_at,state
         FROM subject_obligations WHERE obligation_id=?1", [id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
    ).optional().map_err(storage)?;
    row.map(|(agent, description, depends, resume, origin, due, state)| Ok((parse_entity_id(&agent)?, PendingObligation {
        obligation_id: id, description, depends_on_task_id: depends.as_deref().map(parse_entity_id).transpose()?,
        resume_condition: resume, origin_event_id: origin.as_deref().map(parse_entity_id).transpose()?,
        due_at: due.as_deref().map(parse_timestamp).transpose()?,
    }, state))).transpose()
}


/// Required recovery facts are checked for every supported wire schema. The
/// legacy label never supplies evidence that the sender did not retain.
impl Store {
    pub(crate) fn validate_obligation_rows(
        &self,
        rows: &[crate::cognition_archive::CognitionRow],
    ) -> anyhow::Result<()> {
        use anyhow::{Context, ensure};
        use std::collections::HashMap;
        use shenko_types::object::ObjectRef;
        use shenko_types::subject::SubjectStepState;
        use crate::cognition_archive::CognitionRow;

        let projections: HashMap<&str, &CognitionRow> = rows.iter()
            .filter(|row| row.table == "subject_obligations")
            .map(|row| (row.key.as_str(), row)).collect();
        let steps: HashMap<&str, &CognitionRow> = rows.iter()
            .filter(|row| row.table == "subject_steps")
            .map(|row| (row.key.as_str(), row)).collect();
        let intents: HashMap<&str, &CognitionRow> = rows.iter()
            .filter(|row| row.table == "subject_intents")
            .map(|row| (row.key.as_str(), row)).collect();
        let mut bodies = HashMap::new();
        for row in projections.values() {
            ensure!(matches!(row.value("state"), Some("open" | "completed" | "cancelled")),
                "invalid obligation projection state: {}", row.key);
            let body = obligation_row_body(row)?;
            ensure!(body.obligation_id != EntityId::nil() && body.obligation_id.to_string() == row.key,
                "invalid obligation projection identity");
            if let Some(origin) = body.origin_event_id {
                ensure!(rows.iter().any(|event| event.table == "inbox_events" && event.key == origin.to_string()),
                    "obligation origin event is missing");
            }
            let creator_row = steps.get(row.value("step_id").context("obligation creation step missing")?)
                .context("obligation creation step is absent")?;
            let creator: SubjectStep = serde_json::from_str(creator_row.value("data").context("creation step body missing")?)?;
            let decision_ref: ObjectRef = serde_json::from_str(row.value("decision_ref").context("creation decision missing")?)?;
            ensure!(creator.state == SubjectStepState::Committed
                && creator.step_id.to_string() == creator_row.key
                && creator.commit_ref.context("creation commit missing")?.seq.0.to_string() == row.value("created_seq").unwrap_or_default(),
                "obligation creation provenance differs from its committed step");
            let original = self.obligation_object_bytes(&decision_ref)?;
            let decision: StepDecision = if creator.decision_ref.as_ref() == Some(&decision_ref) {
                serde_json::from_slice(&original)?
            } else {
                let current = creator.decision_ref.as_ref().context("migrated creation decision missing")?;
                ensure!(rows.iter().any(|migration| migration.table == "channel_record_migrations"
                    && migration.value("record_kind") == Some("subject_step")
                    && migration.value("record_id") == Some(creator_row.key.as_str())
                    && migration.value("from_version").and_then(|version| version.parse::<u32>().ok())
                        == Some(crate::channel::LEGACY_DECISION_SCHEMA_VERSION)
                    // The rewrite produced decision schema 8 (it added the
                    // versioned publication target, not an acknowledgement).
                    // The record must name the version it actually produced,
                    // not the schema this build writes today.
                    && migration.value("to_version").and_then(|version| version.parse::<u32>().ok())
                        == Some(crate::channel::PUBLICATION_TARGET_DECISION_VERSION)
                    && migration.value("source_digest") == Some(decision_ref.digest.0.as_str())
                    && migration.value("target_digest") == Some(current.digest.0.as_str())),
                    "obligation creation has no recorded publication migration");
                let mut migrated: serde_json::Value = serde_json::from_slice(&original)?;
                ensure!(crate::channel::decision_has_legacy_target(&migrated),
                    "obligation creation migration has no legacy publication target");
                crate::channel::rewrite_legacy_targets(&mut migrated);
                let actual: serde_json::Value = serde_json::from_slice(&self.obligation_object_bytes(current)?)?;
                ensure!(actual == migrated, "obligation creation migration changes non-publication facts");
                serde_json::from_value(migrated)?
            };
            ensure!(decision.step_id == creator.step_id
                && Some(&decision.input_plan_ref) == creator.input_plan_ref.as_ref()
                && decision.model_result_ref == creator.model_result_ref
                && decision.pending_obligations.iter().filter(|item| item.obligation_id == body.obligation_id).count() == 1
                && decision.pending_obligations.iter().any(|item| item == &body),
                "obligation creation body is not bound to its decision");
            let head = rows.iter().find(|head| matches!(head.table.as_str(), "subject_branch_heads" | "subject_head_history")
                && head.value("branch_id") == Some(creator.branch_id.0.to_string().as_str()))
                .context("obligation creator has no subject head")?;
            ensure!(head.value("agent_id") == row.value("agent_id"), "foreign obligation creation");
            bodies.insert(row.key.as_str(), (body, creator));
        }

        let mut applied: HashMap<String, (ObligationTerminalState, u64)> = HashMap::new();
        let mut rejected = Vec::new();
        let mut bound_intents = HashSet::new();
        for step_row in steps.values() {
            // Unrelated legacy steps may predate the typed decision shape.
            // Only decisions claiming an obligation fact enter this contract.
            let step: SubjectStep = match step_row.value("data").map(serde_json::from_str).transpose() {
                Ok(Some(step)) => step,
                _ => continue,
            };
            if step.state != SubjectStepState::Committed { continue; }
            let Some(decision_ref) = &step.decision_ref else { continue };
            // A historical step may still hold a decision this store cannot
            // read: the object was never retained, and recovery keeps that
            // step degraded with its own blocked marker. That absence cannot
            // silently waive a present obligation fact either: creations are
            // bound from their projection above (a mandatory read), terminal
            // projections require an applied fact below, and every terminal
            // settlement intent must be bound by an actual request. So an
            // unreadable decision is skipped here instead of aborting the
            // whole dispatcher and rolling back those retained markers.
            let Ok(bytes) = self.obligation_object_bytes(decision_ref) else { continue };
            let value: serde_json::Value = match serde_json::from_slice(&bytes) { Ok(value) => value, Err(_) => continue };
            let has_creation = value.get("pending_obligations").and_then(serde_json::Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item.get("obligation_id")
                    .and_then(serde_json::Value::as_str).is_some_and(|id| id != EntityId::nil().to_string())));
            let has_terminal = value.get("policy_intents").and_then(serde_json::Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item.pointer("/kind/obligation_terminal").is_some()))
                || value.pointer("/output/controls").and_then(serde_json::Value::as_array)
                    .is_some_and(|items| items.iter().any(|item| item.pointer("/action/kind")
                        .and_then(serde_json::Value::as_str) == Some("obligation_terminal")));
            if !has_creation && !has_terminal { continue; }
            let decision: StepDecision = serde_json::from_value(value)?;
            for created in &decision.pending_obligations {
                if created.obligation_id != EntityId::nil() {
                    let (_, creator) = bodies.get(created.obligation_id.to_string().as_str())
                        .context("creation decision is missing its obligation projection")?;
                    ensure!(creator.step_id == step.step_id, "obligation identity is reused by another creation");
                }
            }
            let requests = obligation_requests(&decision)?;
            if requests.is_empty() { continue; }
            // Which decision schemas this build still admits is decided in
            // one place (`ADMITTED_DECISION_SCHEMA_VERSIONS`): a reader that
            // pins one version silently refuses every obligation the subject
            // settled after the decision schema moved on. 9 adds the
            // subject's acknowledgements of unknown external effects and
            // leaves the obligation body untouched, so an 8 decision and a 9
            // decision carry the same obligation requests; old bytes keep
            // their old meaning.
            ensure!(crate::channel::ADMITTED_DECISION_SCHEMA_VERSIONS.contains(&decision.schema_version)
                && decision.output.schema_version == crate::channel::OUTPUT_SCHEMA_VERSION
                && decision.step_id == step.step_id && decision.output.step_id == step.step_id
                && step.step_id.to_string() == step_row.key
                && decision.output.raw_ref == step.model_result_ref.clone().context("terminal step result missing")?
                && decision.model_result_ref == step.model_result_ref
                && Some(&decision.input_plan_ref) == step.input_plan_ref.as_ref(),
                "terminal decision is not bound to its committed result and S0");
            let committed_at = step.commit_ref.context("terminal step commit missing")?;
            let marker = rows.iter().find(|row| row.table == "subject_intent_steps" && row.key == step_row.key)
                .context("terminal step is missing intent materialization")?;
            ensure!(marker.value("migration") == Some("S1-native-v1"), "terminal step has no native S1 materialization");
            let sealed = self.sealed_obligations(&decision)?;
            for (id, change) in requests {
                let target = change.obligation_id.to_string();
                let projection = projections.get(target.as_str()).context("terminal policy has no obligation projection")?;
                let (body, creator) = bodies.get(target.as_str()).context("terminal target has no creation body")?;
                ensure!(creator.branch_id == step.branch_id && creator.step_no < step.step_no,
                    "terminal target is not an earlier obligation of this subject");
                let snapshot = sealed.iter().find(|item| item.obligation_id == change.obligation_id)
                    .context("terminal target is absent from its actual S0")?;
                ensure!(snapshot == body, "terminal S0 body differs from immutable creation");
                let intent_key = id.to_string();
                let intent = intents.get(intent_key.as_str()).context("terminal policy is missing its exact settlement")?;
                let policy = decision.policy_intents.iter().find(|policy| policy.intent_id == id).expect("requests binds policy");
                ensure!(intent.value("kind") == Some("policy")
                    && intent.value("contract") == Some(OBLIGATION_TERMINAL_CONTRACT)
                    && intent.value("step_id") == Some(step_row.key.as_str())
                    && intent.value("agent_id") == projection.value("agent_id")
                    && intent.value("state") == Some("settled")
                    && serde_json::from_str::<serde_json::Value>(intent.value("payload").context("terminal payload missing")?)? == serde_json::to_value(policy)?
                    && serde_json::from_str::<ObjectRef>(intent.value("decision_ref").context("terminal intent decision missing")?)? == *decision_ref,
                    "terminal policy materialization differs from its committed decision");
                let receipt: ObligationTerminalReceipt = serde_json::from_str(intent.value("result_ref")
                    .context("required terminal settlement is missing")?)?;
                ensure!(receipt.schema_version == 1 && receipt.step_id == step.step_id && receipt.intent_id == id
                    && receipt.change == *change && receipt.sealed_obligation == *snapshot
                    && receipt.input_plan_ref == decision.input_plan_ref && receipt.committed_at == committed_at,
                    "terminal settlement differs from its action, original seal or S1");
                bound_intents.insert(intent_key);
                match receipt.outcome {
                    ObligationTerminalOutcome::Applied => {
                        ensure!(projection.value("state") == Some(change.state.as_str())
                            && applied.insert(target, (change.state, step.step_no)).is_none(),
                            "applied terminal fact conflicts with its projection or another applied fact");
                    }
                    ObligationTerminalOutcome::Rejected { current_state, reason } => {
                        ensure!(!reason.trim().is_empty() && projection.value("state") == Some(current_state.as_str()),
                            "rejected terminal request is not an applied projection");
                        rejected.push((target, current_state, step.step_no));
                    }
                }
            }
        }
        for row in projections.values() {
            ensure!((row.value("state") == Some("open")) == !applied.contains_key(row.key.as_str()),
                "terminal obligation projection requires its applied S1 fact: {}", row.key);
        }
        for (id, state, step_no) in rejected {
            ensure!(applied.get(&id).is_some_and(|(applied_state, applied_step)| *applied_state == state && *applied_step < step_no),
                "rejected terminal request lacks the earlier applied terminal fact");
        }
        for intent in intents.values().filter(|intent| intent.value("contract") == Some(OBLIGATION_TERMINAL_CONTRACT)
            || intent.value("payload").is_some_and(|payload| serde_json::from_str::<serde_json::Value>(payload)
                .ok().is_some_and(|value| value.pointer("/kind/obligation_terminal").is_some())))
        {
            ensure!(bound_intents.contains(intent.key.as_str()), "unbound required terminal settlement");
        }
        Ok(())
    }

    /// Read a sealed object only when its bytes authenticate the reference
    /// that names them. Obligation validation must never decide membership,
    /// binding or terminal facts from bytes a swapped pathname supplied under
    /// an unrelated digest.
    pub(crate) fn obligation_object_bytes(&self, object: &shenko_types::object::ObjectRef) -> anyhow::Result<Vec<u8>> {
        let bytes = self.cas.read_object(&object.digest)?;
        anyhow::ensure!(bytes.len() as u64 == object.bytes && shenko_types::object::Digest::blake3(&bytes) == object.digest,
            "obligation recovery object does not match its sealed reference");
        Ok(bytes)
    }
}

fn obligation_row_body(row: &crate::cognition_archive::CognitionRow) -> anyhow::Result<PendingObligation> {
    use anyhow::Context;
    Ok(PendingObligation {
        obligation_id: parse_entity_id(row.value("obligation_id").context("obligation id missing")?)?,
        description: row.value("description").context("obligation description missing")?.to_owned(),
        depends_on_task_id: row.value("depends_on_task_id").map(parse_entity_id).transpose()?,
        resume_condition: row.value("resume_condition").context("obligation resume condition missing")?.to_owned(),
        origin_event_id: row.value("origin_event_id").map(parse_entity_id).transpose()?,
        due_at: row.value("due_at").map(parse_timestamp).transpose()?,
    })
}
