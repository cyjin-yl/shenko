//! A durable subject-only pause boundary.
//!
//! This is not an activation selector: it neither selects a copied store nor
//! fences independent stores, ingress, or previously admitted operations.
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    commit::CommitRef,
    error::ShenkoError,
    ids::{ControllerEpoch, EntityId, RecordRevision, RuntimeGeneration},
    subject::{SubjectBranchHead, SubjectMode, SubjectStepState},
};

use crate::{Store, store::{checked_head, checked_head_mode, read_step, storage_error}};

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS subject_quiesce_requests (
    request_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    state TEXT NOT NULL,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS subject_quiesce_one_open
    ON subject_quiesce_requests(agent_id) WHERE state IN ('requested','paused');
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectQuiesceState {
    Requested,
    Paused,
    Cancelled,
    Resumed,
}

impl SubjectQuiesceState {
    fn name(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Paused => "paused",
            Self::Cancelled => "cancelled",
            Self::Resumed => "resumed",
        }
    }
}

/// The boundary commit orders facts in this one ledger, but is not a final
/// migration watermark: other writers and admitted operations may continue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubjectQuiesce {
    pub request_id: EntityId,
    pub agent_id: EntityId,
    pub generation: RuntimeGeneration,
    pub epoch: ControllerEpoch,
    pub state: SubjectQuiesceState,
    pub requested_at: CommitRef,
    pub boundary_at: Option<CommitRef>,
    pub closed_at: Option<CommitRef>,
}

impl Store {
    /// Stable request IDs make an uncertain commit queryable on reopen. This
    /// does not claim that a candidate is ready or stop an in-flight step.
    pub fn request_subject_quiesce(
        &self,
        agent_id: EntityId,
        request_id: EntityId,
    ) -> Result<SubjectQuiesce, ShenkoError> {
        self.ledger.with_mutation(|| {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage_error)?;
            if let Some(old) = read_request(&tx, request_id)? {
                if old.agent_id != agent_id {
                    return Err(ShenkoError::conflict("quiesce request identity already belongs to another subject"));
                }
                return Ok(old);
            }
            let head = read_head(&tx, agent_id)?;
            let head = checked_head(&tx, &head)?;
            if open_request(&tx, agent_id)?.is_some() {
                return Err(ShenkoError::conflict("subject already has an open quiesce request"));
            }
            let seq = self.ledger.allocate_seq(&tx)?;
            let commit = CommitRef::new(self.ledger.ledger_id(), seq);
            let record = SubjectQuiesce {
                request_id, agent_id, generation: head.software_generation,
                epoch: head.controller_epoch, state: SubjectQuiesceState::Requested,
                requested_at: commit, boundary_at: None, closed_at: None,
            };
            tx.execute(
                "INSERT INTO subject_quiesce_requests(request_id,agent_id,state,data,updated_commit_seq) VALUES(?1,?2,?3,?4,?5)",
                params![request_id.to_string(), agent_id.to_string(), record.state.name(), serde_json::to_string(&record)?, seq.0 as i64],
            ).map_err(storage_error)?;
            log_commit(&tx, self, commit, 1)?;
            tx.commit().map_err(storage_error)?;
            Ok(record)
        }, |e| matches!(e, ShenkoError::Storage(_)))
    }

    pub fn subject_quiesce(&self, request_id: EntityId) -> Result<Option<SubjectQuiesce>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        read_request(&conn, request_id)
    }

    /// Called by the real loop before admitting a new step. The head, the
    /// planned successor and this receipt change in one SQLite transaction.
    pub fn pause_subject_at_boundary(
        &self,
        expected: &SubjectBranchHead,
    ) -> Result<Option<SubjectBranchHead>, ShenkoError> {
        // A request arriving just after this read is handled at the following
        // boundary; no write transaction is needed for the normal case.
        {
            let conn = self.ledger.conn.lock();
            if open_request(&conn, expected.agent_id)?.is_none() {
                return Ok(None);
            }
        }
        self.ledger.with_mutation(|| {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage_error)?;
            let mut head = checked_head(&tx, expected)?;
            let Some(mut record) = open_request(&tx, head.agent_id)? else { return Ok(None) };
            if record.state != SubjectQuiesceState::Requested
                || record.epoch != head.controller_epoch
                || record.generation != head.software_generation
            {
                return Err(ShenkoError::conflict("quiesce request does not bind the current controller"));
            }
            let step = read_step(&tx, &head.branch_id, head.next_step_id)?
                .ok_or_else(|| ShenkoError::conflict("quiesce successor missing"))?;
            crate::store::validate_step_binding(&head, &step)?;
            if step.state != SubjectStepState::Planned || step.active_request_id.is_some() || step.active_attempt_id.is_some() {
                return Ok(None);
            }
            let seq = self.ledger.allocate_seq(&tx)?;
            let commit = CommitRef::new(self.ledger.ledger_id(), seq);
            let old_revision = head.record_revision;
            head.record_revision = next_revision(old_revision)?;
            head.mode = SubjectMode::Paused;
            write_head(&tx, &head, old_revision, seq.0)?;
            record.state = SubjectQuiesceState::Paused;
            record.boundary_at = Some(commit);
            write_request(&tx, &record, "requested", seq.0)?;
            log_commit(&tx, self, commit, 2)?;
            tx.commit().map_err(storage_error)?;
            Ok(Some(head))
        }, |e| matches!(e, ShenkoError::Storage(_)))
    }

    /// Cancel an unaccepted request or resume this same controller. This does
    /// not switch generation/epoch, delete B, or replay any operation.
    pub fn resume_subject_quiesce(
        &self,
        agent_id: EntityId,
        request_id: EntityId,
    ) -> Result<SubjectQuiesce, ShenkoError> {
        self.ledger.with_mutation(|| {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage_error)?;
            let mut record = read_request(&tx, request_id)?
                .ok_or_else(|| ShenkoError::not_found("quiesce request"))?;
            if record.agent_id != agent_id {
                return Err(ShenkoError::conflict("quiesce request subject mismatch"));
            }
            let old_state = record.state;
            if matches!(old_state, SubjectQuiesceState::Cancelled | SubjectQuiesceState::Resumed) {
                return Ok(record);
            }
            let mut head = read_head(&tx, agent_id)?;
            head = checked_head_mode(&tx, &head, false)?;
            if head.controller_epoch != record.epoch || head.software_generation != record.generation {
                return Err(ShenkoError::conflict("quiesce controller changed; explicit reconciliation required"));
            }
            let seq = self.ledger.allocate_seq(&tx)?;
            let commit = CommitRef::new(self.ledger.ledger_id(), seq);
            match old_state {
                SubjectQuiesceState::Requested if head.mode == SubjectMode::Active => {
                    record.state = SubjectQuiesceState::Cancelled;
                }
                SubjectQuiesceState::Paused if head.mode == SubjectMode::Paused => {
                    let step = read_step(&tx, &head.branch_id, head.next_step_id)?
                        .ok_or_else(|| ShenkoError::conflict("paused successor missing"))?;
                    crate::store::validate_step_binding(&head, &step)?;
                    if step.state != SubjectStepState::Planned || step.active_request_id.is_some() || step.active_attempt_id.is_some() {
                        return Err(ShenkoError::conflict("paused successor is not a clean boundary"));
                    }
                    let old_revision = head.record_revision;
                    head.record_revision = next_revision(old_revision)?;
                    head.mode = SubjectMode::Active;
                    write_head(&tx, &head, old_revision, seq.0)?;
                    record.state = SubjectQuiesceState::Resumed;
                }
                _ => return Err(ShenkoError::conflict("quiesce state/head mismatch")),
            }
            record.closed_at = Some(commit);
            write_request(&tx, &record, old_state.name(), seq.0)?;
            log_commit(&tx, self, commit, if old_state == SubjectQuiesceState::Paused { 2 } else { 1 })?;
            tx.commit().map_err(storage_error)?;
            Ok(record)
        }, |e| matches!(e, ShenkoError::Storage(_)))
    }
}

fn next_revision(revision: RecordRevision) -> Result<RecordRevision, ShenkoError> {
    revision.0.checked_add(1).map(RecordRevision)
        .ok_or_else(|| ShenkoError::conflict("subject revision exhausted"))
}

fn read_head(conn: &Connection, agent_id: EntityId) -> Result<SubjectBranchHead, ShenkoError> {
    let data: String = conn.query_row("SELECT data FROM subject_branch_heads WHERE agent_id=?1", [agent_id.to_string()], |row| row.get(0)).map_err(storage_error)?;
    let head: SubjectBranchHead = serde_json::from_str(&data)?;
    if head.agent_id != agent_id { return Err(ShenkoError::conflict("subject identity mismatch")); }
    Ok(head)
}

fn read_request(conn: &Connection, request_id: EntityId) -> Result<Option<SubjectQuiesce>, ShenkoError> {
    let data: Option<String> = conn.query_row("SELECT data FROM subject_quiesce_requests WHERE request_id=?1", [request_id.to_string()], |row| row.get(0)).optional().map_err(storage_error)?;
    data.map(|value| serde_json::from_str(&value).map_err(ShenkoError::from)).transpose()
}

fn open_request(conn: &Connection, agent_id: EntityId) -> Result<Option<SubjectQuiesce>, ShenkoError> {
    let data: Option<String> = conn.query_row("SELECT data FROM subject_quiesce_requests WHERE agent_id=?1 AND state IN ('requested','paused')", [agent_id.to_string()], |row| row.get(0)).optional().map_err(storage_error)?;
    data.map(|value| serde_json::from_str(&value).map_err(ShenkoError::from)).transpose()
}

/// A copied/imported paused head without its local control record is not a
/// resumable boundary. Read both in the same snapshot as the successor.
pub(crate) fn validate_paused_head(conn: &Connection, head: &SubjectBranchHead) -> Result<(), ShenkoError> {
    let record = open_request(conn, head.agent_id)?
        .ok_or_else(|| ShenkoError::conflict("paused subject has no local boundary record; explicit recovery required"))?;
    if record.agent_id != head.agent_id || record.state != SubjectQuiesceState::Paused
        || record.boundary_at.is_none() || record.epoch != head.controller_epoch
        || record.generation != head.software_generation
    {
        return Err(ShenkoError::conflict("paused subject boundary binding mismatch"));
    }
    Ok(())
}

fn write_request(tx: &Transaction<'_>, record: &SubjectQuiesce, old_state: &str, seq: u64) -> Result<(), ShenkoError> {
    let changed = tx.execute("UPDATE subject_quiesce_requests SET state=?1,data=?2,updated_commit_seq=?3 WHERE request_id=?4 AND agent_id=?5 AND state=?6", params![record.state.name(), serde_json::to_string(record)?, seq as i64, record.request_id.to_string(), record.agent_id.to_string(), old_state]).map_err(storage_error)?;
    if changed != 1 { return Err(ShenkoError::conflict("stale quiesce request")); }
    Ok(())
}

fn write_head(tx: &Transaction<'_>, head: &SubjectBranchHead, old: RecordRevision, seq: u64) -> Result<(), ShenkoError> {
    let mode = match head.mode { SubjectMode::Active => "active", SubjectMode::Paused => "paused", _ => return Err(ShenkoError::conflict("invalid boundary mode")) };
    let changed = tx.execute("UPDATE subject_branch_heads SET mode=?1,record_revision=?2,data=?3,updated_commit_seq=?4 WHERE agent_id=?5 AND branch_id=?6 AND record_revision=?7 AND next_step_id=?8", params![mode, head.record_revision.0 as i64, serde_json::to_string(head)?, seq as i64, head.agent_id.to_string(), head.branch_id.0.to_string(), old.0 as i64, head.next_step_id.to_string()]).map_err(storage_error)?;
    if changed != 1 { return Err(ShenkoError::conflict("stale subject head")); }
    Ok(())
}

fn log_commit(tx: &Transaction<'_>, store: &Store, commit: CommitRef, count: i64) -> Result<(), ShenkoError> {
    tx.execute("INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count) VALUES(?1,?2,?3,?4)", params![commit.ledger_id.to_string(), commit.seq.0 as i64, store.capabilities.incarnation.to_string(), count]).map_err(storage_error)?;
    Ok(())
}
