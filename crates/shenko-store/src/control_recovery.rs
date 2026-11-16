//! Preserve a chosen control action without inventing a foreign Store commit.
//! A source-local S1 may finish against its exact original predecessor. Every
//! other unapplied action remains evidence for reconciliation; no replay here
//! grants permission to execute an external attempt.
use anyhow::{Context as _, Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    authority::{CommitAdmission, CommitAdmissionObject, CommitAuthorityGrant},
    commit::CommitRef,
    error::ShenkoError,
    ids::EntityId,
    inbox::{InboxEvent, InboxEventKind},
    object::{Digest, ObjectRef},
    operation::EffectStatus,
    subject::{SubjectBranchHead, SubjectStep},
    task::TaskRecord,
};

use crate::{Store, authority::{admission_applied, admission_materialized}};

pub const CONTROL_RECONCILIATION_SOURCE: &str = "commit-authority-reconciliation";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdmissionEnvelope {
    schema_version: u32,
    kind: String,
    source_ledger_id: EntityId,
    head: SubjectBranchHead,
    predecessor_ref: ObjectRef,
    expected_tasks: Vec<TaskRecord>,
    snapshot_tasks: Vec<TaskRecord>,
    expected_steps: Vec<SubjectStep>,
    action: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlRecoveryOutcome {
    Applied,
    RecoveredOriginal,
    ReconciliationRequired,
}

impl Store {
    /// Exact facts win before current authority checks, including after a
    /// newer branch or epoch has replaced the source runtime.
    pub fn control_admission_applied(&self, admission: &CommitAdmission) -> Result<bool, ShenkoError> {
        admission_applied(&self.ledger.conn.lock(), admission)
    }

    /// Imported source facts describe that source's commit, not a local
    /// canonical materialization. Ordinary cognition does not supply that
    /// stronger guarantee.
    pub fn control_admission_applied_here(&self, admission: &CommitAdmission) -> Result<bool, ShenkoError> {
        admission_materialized(&self.ledger.conn.lock(), admission)
    }

    pub fn recover_control_admission(
        &self, grant: &CommitAuthorityGrant, objects: &[CommitAdmissionObject], try_original: bool,
    ) -> Result<ControlRecoveryOutcome> {
        if self.control_admission_applied_here(&grant.admission)? { return Ok(ControlRecoveryOutcome::Applied); }
        // Acceptor-retained bytes are checked again at the receiving CAS.
        // They are immutable evidence, never imported control rows.
        for object in objects {
            ensure!(object.bytes.len() as u64 == object.reference.bytes
                && Digest::blake3(&object.bytes) == object.reference.digest,
                "chosen control object does not match its declared identity");
            self.cas.seal_object(&object.bytes, &object.reference.media_type,
                &object.reference.store_namespace, &object.reference.codec.name, object.reference.codec.major)?;
        }
        self.admission_objects(&grant.admission.payload_ref)?;
        let bytes = self.cas.read_object(&grant.admission.payload_ref.digest)?;
        let envelope: AdmissionEnvelope = serde_json::from_slice(&bytes)?;
        ensure!(envelope.schema_version == 1 && envelope.kind == "store.control_admission"
            && envelope.head.agent_id == grant.admission.subject_id
            && envelope.head.branch_id == grant.admission.branch_id
            && envelope.head.controller_epoch == grant.admission.controller_epoch
            && envelope.head.software_generation == grant.admission.software_generation,
            "chosen control envelope disagrees with its immutable admission");
        let evidence = self.inspect_control_predecessor(&envelope.predecessor_ref, &grant.admission)?;
        ensure!(evidence.source_ledger_id == envelope.source_ledger_id
            && serde_json::to_value(&evidence.original_head)? == serde_json::to_value(&envelope.head)?,
            "chosen control predecessor disagrees with its source envelope");
        let current = self.load_subject(grant.admission.subject_id)?.0;
        let compatible = evidence.original_source && !self.control_admission_applied(&grant.admission)?
            && serde_json::to_value(&current)? == serde_json::to_value(&envelope.head)?;
        if try_original && compatible && envelope.action["kind"] == "subject.step_commit" {
            let step: SubjectStep = serde_json::from_value(envelope.action["step"].clone())?;
            let expected_head: SubjectBranchHead = serde_json::from_value(envelope.action["expected_head"].clone())?;
            let next_step_id: EntityId = serde_json::from_value(envelope.action["next_step_id"].clone())?;
            ensure!(serde_json::to_value(&expected_head)? == serde_json::to_value(&envelope.head)?,
                "chosen S1 original head was replaced inside its action");
            let reservation = self.reserve_recovered_admission(grant.clone(),
                envelope.expected_tasks.clone(), envelope.expected_steps.clone())?;
            let outcome = self.ledger.with_mutation(
                || self.try_commit_step_and_advance_head(&step, &expected_head, next_step_id, &reservation),
                |error| matches!(error, ShenkoError::Storage(_)),
            );
            match outcome {
                Ok(_) => return Ok(ControlRecoveryOutcome::RecoveredOriginal),
                Err(ShenkoError::Conflict(_) | ShenkoError::NotFound(_) | ShenkoError::ResnapshotRequired { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.retain_control_reconciliation(grant, &envelope, &evidence.fingerprint)?;
        Ok(ControlRecoveryOutcome::ReconciliationRequired)
    }

    fn retain_control_reconciliation(
        &self, grant: &CommitAuthorityGrant, envelope: &AdmissionEnvelope, predecessor_fingerprint: &Digest,
    ) -> Result<()> {
        let event_id = EntityId::derived("control-reconciliation-v1", grant.grant_id.as_bytes());
        self.ledger.with_mutation(|| -> Result<()> {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if admission_materialized(&tx, &grant.admission)? { return Ok(()); }
            let old: Option<(String, String, String, String)> = tx.query_row(
                "SELECT agent_id,kind,source,data FROM inbox_events WHERE event_id=?1",
                [event_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).optional()?;
            if let Some((agent, kind, source, data)) = old {
                let event: InboxEvent = serde_json::from_str(&data)?;
                let original: CommitAuthorityGrant = serde_json::from_value(event.payload["grant"].clone())?;
                ensure!(agent == grant.admission.subject_id.to_string() && kind == "system_event"
                    && source == CONTROL_RECONCILIATION_SOURCE && event.source == source
                    && event.kind == InboxEventKind::SystemEvent && event.event_id == event_id
                    && event.payload["kind"] == "authority.reconciliation_required"
                    && original.grant_id == grant.grant_id && original.admission == grant.admission,
                    "chosen action reconciliation identity conflicts with its retained fact");
                return Ok(());
            }
            let original_effect_observation = if let Some(operation_id) = grant.admission.operation_id {
                let data: Option<String> = tx.query_row("SELECT data FROM operations WHERE operation_id=?1",
                    [operation_id.to_string()], |row| row.get(0)).optional()?;
                data.map(|data| -> Result<serde_json::Value> {
                    let operation: shenko_types::operation::OperationRecord = serde_json::from_str(&data)?;
                    Ok(serde_json::json!({"operation_id":operation.operation_id,
                        "effect_status": operation.effect_status,
                        "outcome": operation.outcome, "record_revision": operation.record_revision,
                        "needs_effect_reconciliation": operation.effect_status == EffectStatus::Unknown}))
                }).transpose()?
            } else if let Some(attempt_id) = grant.admission.attempt_id {
                let data: Option<String> = tx.query_row("SELECT data FROM subject_request_attempts WHERE attempt_id=?1",
                    [attempt_id.to_string()], |row| row.get(0)).optional()?;
                data.map(|data| -> Result<serde_json::Value> {
                    let attempt: crate::SubjectRequestAttempt = serde_json::from_str(&data)?;
                    Ok(serde_json::json!({"request_id":attempt.request_id,"attempt_id":attempt.attempt_id,
                        "state":attempt.state,"terminal":attempt.terminal}))
                }).transpose()?
            } else { None };
            let commit = CommitRef::new(self.ledger.ledger_id(), self.ledger.allocate_seq(&tx)?);
            let event = InboxEvent { event_id, kind: InboxEventKind::SystemEvent,
                source: CONTROL_RECONCILIATION_SOURCE.into(), created_at: chrono::Utc::now(),
                payload: serde_json::json!({"kind":"authority.reconciliation_required",
                    "reason":"selected-plan recovery unavailable",
                    "grant_id":grant.grant_id, "grant":grant, "original_branch":grant.admission.branch_id,
                    "payload_ref":grant.admission.payload_ref, "predecessor_ref":envelope.predecessor_ref,
                    "predecessor_fingerprint":predecessor_fingerprint,
                    "source_ledger_id":envelope.source_ledger_id, "action_kind":envelope.action["kind"],
                    "operation_id":grant.admission.operation_id, "attempt_id":grant.admission.attempt_id,
                    "original_effect_observation":original_effect_observation,
                    "recorded_at":commit}),
            };
            tx.execute("INSERT INTO inbox_events(event_id,agent_id,kind,source,consumed,data)
                VALUES(?1,?2,'system_event',?3,0,?4)", params![event_id.to_string(),
                    grant.admission.subject_id.to_string(), CONTROL_RECONCILIATION_SOURCE, serde_json::to_string(&event)?])?;
            self.record_subject_commit(&tx, commit, 1)?;
            tx.commit()?;
            Ok(())
        }, |error| error.is::<rusqlite::Error>())
    }

    /// Durable debts stay visible after their ordinary inbox notification is
    /// consumed. A later exact applied marker closes only that chosen action.
    pub fn pending_control_reconciliations(&self, agent_id: EntityId) -> Result<Vec<InboxEvent>> {
        let conn = self.ledger.conn.lock();
        let mut statement = conn.prepare("SELECT event_id,data FROM inbox_events
            WHERE agent_id=?1 AND source=?2 AND kind='system_event' ORDER BY rowid")?;
        let rows = statement.query_map(params![agent_id.to_string(), CONTROL_RECONCILIATION_SOURCE],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut debts = Vec::new();
        for (id, data) in rows {
            let event: InboxEvent = serde_json::from_str(&data)?;
            ensure!(event.event_id.to_string() == id && event.kind == InboxEventKind::SystemEvent
                && event.source == CONTROL_RECONCILIATION_SOURCE
                && event.payload["kind"] == "authority.reconciliation_required",
                "control reconciliation event identity mismatch");
            let grant: CommitAuthorityGrant = serde_json::from_value(event.payload.get("grant")
                .cloned().context("control reconciliation grant missing")?)?;
            if !admission_materialized(&conn, &grant.admission)? { debts.push(event); }
        }
        Ok(debts)
    }
}
