//! Exact first E1 of the bounded direct native chain.
//!
//! The receipt is an external observation, not a quorum execution fact. Its
//! original E1 commit, fold and observation time are sealed in that same E1
//! transaction. Independent factual delivery, or a later chosen T2, can carry
//! those bytes to an installation which actually materialized the original
//! paired T1. Neither transport makes E1 a quorum execution fact.
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    commit::CommitRef,
    error::ShenkoError,
    evidence::EvidenceReceipt,
    ids::{EntityId, RecordRevision},
    object::ObjectRef,
    operation::{
        AttemptDispatchStage, AttemptRecord, EffectStatus, OperationOutcome, OperationRecord,
        OperationState,
    },
    task::TaskRecord,
};

use crate::{
    Store,
    arming_transition::NativeArmFacts,
    native_facts::{CanonicalFact, conflict, invalid, same},
    store::storage_error,
};

pub(crate) const CODEC: &str = "store-native-evidence-fact";

// Local physical application history is never imported as a peer fact. The
// immutable evidence_ref is carried by factual delivery or a later T2 closure.
pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS native_evidence_materializations (
    receipt_id TEXT PRIMARY KEY, data TEXT NOT NULL, updated_commit_seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS native_evidence_seal_debts (
    receipt_id TEXT PRIMARY KEY, operation_id TEXT NOT NULL,
    data TEXT NOT NULL, updated_commit_seq INTEGER NOT NULL
);
"#;

/// The original E1 was admitted, but this installation could not derive the
/// bounded continuation proof. This is not a replacement receipt or a failed
/// external effect, and its presence never permits generic T2 success.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidenceSealDebt {
    pub schema_version: u32,
    pub receipt_id: EntityId,
    pub operation_id: EntityId,
    pub attempt_id: EntityId,
    pub received_commit: CommitRef,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeEvidenceFact {
    schema_version: u32,
    pub arm: CanonicalFact,
    pub receipt: EvidenceReceipt,
    pub operation: OperationRecord,
    pub attempt: AttemptRecord,
    created_at: String,
    /// Version 1 supported only a definite first observation. Version 2
    /// freezes the actual decoder inputs, including an uncertain conclusion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fold: Option<NativeEvidenceFold>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeEvidenceFold {
    pub expected_revision: Option<RecordRevision>,
    pub outcome: Option<OperationOutcome>,
    pub effect: EffectStatus,
    pub terminal_content_conflict: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceMaterialization {
    schema_version: u32,
    pub evidence_ref: ObjectRef,
    pub logical_result: CommitRef,
    pub committed_at: CommitRef,
}

fn first_armed(operation: &OperationRecord, attempt: &AttemptRecord) -> bool {
    operation.record_schema_version == 1
        && attempt.record_schema_version == 1
        && operation.record_revision == RecordRevision(0)
        && operation.state == OperationState::InFlight
        && operation.outcome.is_none()
        && !operation.outcome_conflict
        && operation.evidence_through.is_none()
        && attempt.dispatch_stage == AttemptDispatchStage::Armed
        && attempt.final_receipt_id.is_none()
}

fn read_records(
    conn: &Connection,
    operation: EntityId,
    attempt: EntityId,
) -> Result<(OperationRecord, AttemptRecord), ShenkoError> {
    let operation: String = conn
        .query_row(
            "SELECT data FROM operations WHERE operation_id=?1",
            [operation.to_string()],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    let attempt: String = conn
        .query_row(
            "SELECT data FROM attempts WHERE attempt_id=?1",
            [attempt.to_string()],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    Ok((
        serde_json::from_str(&operation).map_err(invalid)?,
        serde_json::from_str(&attempt).map_err(invalid)?,
    ))
}

fn has_reconciliation(conn: &Connection, operation: EntityId) -> Result<bool, ShenkoError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM reconciliation_cases WHERE operation_id=?1)",
        [operation.to_string()],
        |row| row.get(0),
    )
    .map_err(storage_error)
}

impl NativeEvidenceFact {
    /// Reconstruct the first observation from the original Armed predecessor
    /// and the frozen decoder inputs. No later receipt or source case decision
    /// participates in this projection.
    fn validate(&self, original: &NativeArmFacts) -> Result<(), ShenkoError> {
        let receipt = &self.receipt;
        if !matches!(self.schema_version, 1 | 2)
            || (self.schema_version == 2) != self.fold.is_some()
            || receipt.record_schema_version != 1
            || !first_armed(&original.operation, &original.attempt)
            || receipt.operation_id != Some(original.operation.operation_id)
            || receipt.attempt_id != Some(original.attempt.attempt_id)
            || self.attempt.last_observed_at.is_none()
            || chrono::NaiveDateTime::parse_from_str(&self.created_at, "%Y-%m-%d %H:%M:%S").is_err()
        {
            return Err(invalid(
                "E1 is not the original attempt's first observation",
            ));
        }
        let fold = match self.fold {
            Some(fold) => fold,
            None if self.operation.outcome.is_some()
                && self.operation.effect_status != EffectStatus::Unknown => NativeEvidenceFold {
                    expected_revision: None,
                    outcome: self.operation.outcome,
                    effect: self.operation.effect_status,
                    terminal_content_conflict: false,
                },
            None => return Err(invalid("legacy E1 cannot describe an uncertain fold")),
        };
        if fold.expected_revision.is_some_and(|revision| revision != original.operation.record_revision) {
            return Err(invalid("E1 decoder read a different operation revision"));
        }
        let mut operation = original.operation.clone();
        let mut attempt = original.attempt.clone();
        if fold.terminal_content_conflict {
            operation.outcome_conflict = true;
            operation.state = OperationState::Reconciling;
            operation.outcome = None;
            operation.effect_status = EffectStatus::Unknown;
            attempt.dispatch_stage = AttemptDispatchStage::Ended;
        } else if let Some(outcome) = fold.outcome {
            operation.state = if fold.effect == EffectStatus::Unknown {
                OperationState::Reconciling
            } else {
                OperationState::Resolved
            };
            operation.outcome = Some(outcome);
            operation.effect_status = fold.effect;
            attempt.dispatch_stage = AttemptDispatchStage::Ended;
        } else {
            operation.state = OperationState::Reconciling;
            operation.outcome = None;
            operation.effect_status = EffectStatus::Unknown;
            attempt.dispatch_stage = AttemptDispatchStage::Unknown;
        }
        operation.record_revision = RecordRevision(1);
        operation.evidence_through = Some(receipt.received_commit);
        attempt.final_receipt_id = Some(receipt.receipt_id);
        attempt.last_observed_at = self.attempt.last_observed_at;
        if !same(&operation, &self.operation)? || !same(&attempt, &self.attempt)? {
            return Err(invalid(
                "E1 rewrote its original operation, attempt or fold",
            ));
        }
        Ok(())
    }
}

impl Store {
    /// Called after the actual E1 writes, before their one COMMIT. It adds no
    /// control reservation and does not make a stale callback current again.
    pub(crate) fn seal_first_native_evidence(
        &self,
        tx: &Transaction<'_>,
        before: &(OperationRecord, AttemptRecord),
        receipt: &EvidenceReceipt,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        fold: NativeEvidenceFold,
    ) -> Result<(), ShenkoError> {
        match self.try_seal_first_native_evidence(tx, before, receipt, operation, attempt, fold) {
            Ok(()) => Ok(()),
            Err(error @ ShenkoError::Storage(_)) => Err(error),
            Err(error) => {
                // An unavailable derived proof cannot erase the original
                // external observation. Keep a separate, explicit debt in
                // the SAME E1 commit; later T2 must reconcile that debt.
                let debt = NativeEvidenceSealDebt {
                    schema_version: 1,
                    receipt_id: receipt.receipt_id,
                    operation_id: operation.operation_id,
                    attempt_id: attempt.attempt_id,
                    received_commit: receipt.received_commit,
                    reason: error.to_string(),
                };
                tx.execute("INSERT INTO native_evidence_seal_debts(receipt_id,operation_id,data,updated_commit_seq) VALUES(?1,?2,?3,?4)",
                    params![receipt.receipt_id.to_string(), operation.operation_id.to_string(), serde_json::to_string(&debt)?,
                        i64::try_from(receipt.received_commit.seq.0).map_err(invalid)?]).map_err(storage_error)?;
                Ok(())
            }
        }
    }

    fn try_seal_first_native_evidence(
        &self,
        tx: &Transaction<'_>,
        before: &(OperationRecord, AttemptRecord),
        receipt: &EvidenceReceipt,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        fold: NativeEvidenceFold,
    ) -> Result<(), ShenkoError> {
        if !first_armed(&before.0, &before.1)
            || receipt.record_schema_version != 1
        {
            return Ok(());
        }
        let Some(arm) = self.local_native_fact(tx, before.0.prepared_commit, "native.arm")? else {
            return Ok(());
        };
        // Version 1 intentionally retains its old whole-table dependency.
        // It cannot supply this new direct claim/paired-dispatch proof.
        if arm.transition_ref.codec.major == 1 {
            return Ok(());
        }
        let original = self.verify_native_arm_fact(tx, &arm)?;
        if !same(&before.0, &original.operation)? || !same(&before.1, &original.attempt)? {
            return Err(invalid(
                "original E1 predecessor differs from the locally materialized T1",
            ));
        }
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM evidence_receipts WHERE operation_id=?1",
                [operation.operation_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        if count != 1 {
            return Ok(());
        }
        let created_at: String = tx
            .query_row(
                "SELECT created_at FROM evidence_receipts WHERE receipt_id=?1",
                [receipt.receipt_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let fact = NativeEvidenceFact {
            schema_version: 2,
            arm,
            receipt: receipt.clone(),
            operation: operation.clone(),
            attempt: attempt.clone(),
            created_at,
            fold: Some(fold),
        };
        fact.validate(&original)?;
        let bytes = serde_json::to_vec(&fact)?;
        if bytes.len() > crate::control_transition::MAX_IMAGE_BYTES {
            return Err(invalid(
                "original E1 proof exceeds the bounded admission size",
            ));
        }
        let reference = self
            .cas
            .seal_object(&bytes, "application/json", "commit-authority", CODEC, 2)?
            .object;
        let (_, missing) = self
            .collect_closure_bounded(
                std::slice::from_ref(&reference),
                shenko_types::native_evidence::NATIVE_EVIDENCE_CLOSURE_OBJECTS,
                shenko_types::native_evidence::NATIVE_EVIDENCE_CLOSURE_BYTES,
            )
            .map_err(invalid)?;
        if !missing.is_empty() {
            return Err(invalid("original E1 closure is incomplete"));
        }
        record_materialization(tx, &reference, &fact, receipt.received_commit)
    }

    pub fn native_evidence_seal_debts(
        &self,
        operation: EntityId,
    ) -> Result<Vec<NativeEvidenceSealDebt>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut statement = conn.prepare("SELECT data FROM native_evidence_seal_debts WHERE operation_id=?1 ORDER BY updated_commit_seq,receipt_id")
            .map_err(storage_error)?;
        statement
            .query_map([operation.to_string()], |row| row.get::<_, String>(0))
            .map_err(storage_error)?
            .map(|row| serde_json::from_str(&row.map_err(storage_error)?).map_err(invalid))
            .collect()
    }

    pub(crate) fn read_native_evidence_fact(
        &self,
        conn: &Connection,
        reference: &ObjectRef,
    ) -> Result<(NativeEvidenceFact, NativeArmFacts), ShenkoError> {
        if !matches!(reference.codec.major, 1 | 2) {
            return Err(invalid("unsupported typed object for original E1"));
        }
        let fact: NativeEvidenceFact = self.read_native_json(reference, CODEC, reference.codec.major)?;
        if fact.schema_version != reference.codec.major
            || (reference.codec.major == 2) != fact.fold.is_some()
        {
            return Err(invalid("E1 codec and frozen fold disagree"));
        }
        let (_, missing) = if reference.codec.major == 2 {
            self.collect_closure_bounded(
                std::slice::from_ref(reference),
                shenko_types::native_evidence::NATIVE_EVIDENCE_CLOSURE_OBJECTS,
                shenko_types::native_evidence::NATIVE_EVIDENCE_CLOSURE_BYTES,
            )
            .map_err(invalid)?
        } else {
            self.collect_closure(std::slice::from_ref(reference))
                .map_err(storage_error)?
        };
        if !missing.is_empty() {
            return Err(invalid("original E1 closure is incomplete"));
        }
        let original = self.verify_native_arm_fact(conn, &fact.arm)?;
        fact.validate(&original)?;
        Ok((fact, original))
    }

    pub(crate) fn native_completion_evidence(
        &self,
        conn: &Connection,
        task: &TaskRecord,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
    ) -> Result<Option<ObjectRef>, ShenkoError> {
        let blocked: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM native_evidence_seal_debts WHERE operation_id=?1)",
                [operation.operation_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        if blocked {
            return Err(conflict(
                "original E1 has a retained seal debt; reconciliation required",
            ));
        }
        if operation.record_revision != RecordRevision(1)
            || operation.outcome.is_none()
            || operation.state != OperationState::Resolved
            || operation.outcome_conflict
            || operation.effect_status == EffectStatus::Unknown
            || has_reconciliation(conn, operation.operation_id)?
        {
            return Ok(None);
        }
        let Some(arm) = self.local_native_fact(conn, operation.prepared_commit, "native.arm")?
        else {
            return Ok(None);
        };
        if arm.transition_ref.codec.major == 1 {
            return Ok(None);
        }
        let original = self.verify_native_arm_fact(conn, &arm)?;
        if !same(task, &original.task)? {
            return Ok(None);
        }
        if crate::native_facts::has_ordinary_children(conn, task.task_id)? {
            return Ok(None);
        }
        let receipt = attempt
            .final_receipt_id
            .ok_or_else(|| invalid("definite native E1 has no receipt"))?;
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM evidence_receipts WHERE operation_id=?1",
                [operation.operation_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        if count != 1 {
            return Ok(None);
        }
        let marker = read_materialization(conn, receipt)?.ok_or_else(|| {
            conflict(
                "original definite E1 has no sealed source application; reconciliation required",
            )
        })?;
        let (fact, _) = self.read_native_evidence_fact(conn, &marker.evidence_ref)?;
        verify_materialization(conn, &marker, &fact)?;
        if !same(operation, &fact.operation)? || !same(attempt, &fact.attempt)? {
            return Err(conflict("original E1 projection changed before T2"));
        }
        Ok(Some(marker.evidence_ref))
    }

    /// Add the source E1 and its exact fold inside the consuming T2 transaction.
    /// If any later T2 precondition fails, these writes and this local receipt
    /// roll back together. Existing unrelated/late evidence is never replaced.
    pub(crate) fn materialize_native_evidence(
        &self,
        tx: &Transaction<'_>,
        reference: &ObjectRef,
        local: CommitRef,
    ) -> Result<bool, ShenkoError> {
        let (fact, original) = self.read_native_evidence_fact(tx, reference)?;
        let current = read_records(tx, fact.operation.operation_id, fact.attempt.attempt_id)?;
        if let Some(marker) = read_materialization(tx, fact.receipt.receipt_id)? {
            if marker.evidence_ref != *reference {
                return Err(invalid(
                    "local E1 receipt points at different original bytes",
                ));
            }
            verify_materialization(tx, &marker, &fact)?;
            if !same(&current.0, &fact.operation)? || !same(&current.1, &fact.attempt)? {
                return Err(conflict(
                    "late E1 advanced the original operation or attempt",
                ));
            }
            return Ok(false);
        }
        if !same(&current.0, &original.operation)?
            || !same(&current.1, &original.attempt)?
            || has_reconciliation(tx, fact.operation.operation_id)?
        {
            return Err(conflict(
                "original armed E1 predecessor differs; retain reconciliation debt",
            ));
        }
        let any: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM evidence_receipts WHERE operation_id=?1)",
                [fact.operation.operation_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        if any
            || !matches!(
                crate::receipt_admission::decide(tx, &fact.receipt).map_err(storage_error)?,
                crate::receipt_admission::Admission::New
            )
        {
            return Err(conflict(
                "E1 receipt or source-event identity already has a local fact",
            ));
        }
        let seq = i64::try_from(local.seq.0).map_err(invalid)?;
        tx.execute("INSERT INTO evidence_receipts(receipt_id,operation_id,data,updated_commit_seq,created_at) VALUES(?1,?2,?3,?4,?5)",
            params![fact.receipt.receipt_id.to_string(), fact.operation.operation_id.to_string(), serde_json::to_string(&fact.receipt)?, seq, fact.created_at]).map_err(storage_error)?;
        tx.execute("UPDATE operations SET record_revision=?2,data=?3,updated_commit_seq=?4 WHERE operation_id=?1",
            params![fact.operation.operation_id.to_string(), i64::try_from(fact.operation.record_revision.0).map_err(invalid)?, serde_json::to_string(&fact.operation)?, seq]).map_err(storage_error)?;
        tx.execute(
            "UPDATE attempts SET data=?2,updated_commit_seq=?3 WHERE attempt_id=?1",
            params![
                fact.attempt.attempt_id.to_string(),
                serde_json::to_string(&fact.attempt)?,
                seq
            ],
        )
        .map_err(storage_error)?;
        if fact.operation.state == OperationState::Reconciling {
            let reason = if fact.operation.outcome_conflict {
                crate::reconcile::REASON_CONTRADICTORY_EVIDENCE
            } else {
                crate::reconcile::REASON_EFFECT_UNKNOWN
            };
            self.ensure_unknown_effect_case(tx, &fact.operation, fact.receipt.receipt_id, reason, local)?;
        }
        record_materialization(tx, reference, &fact, local)?;
        Ok(true)
    }
}

fn record_materialization(
    tx: &Transaction<'_>,
    reference: &ObjectRef,
    fact: &NativeEvidenceFact,
    local: CommitRef,
) -> Result<(), ShenkoError> {
    let receipt = EvidenceMaterialization {
        schema_version: 1,
        evidence_ref: reference.clone(),
        logical_result: fact.receipt.received_commit,
        committed_at: local,
    };
    tx.execute("INSERT INTO native_evidence_materializations(receipt_id,data,updated_commit_seq) VALUES(?1,?2,?3)",
        params![fact.receipt.receipt_id.to_string(), serde_json::to_string(&receipt)?, i64::try_from(local.seq.0).map_err(invalid)?]).map_err(storage_error)?;
    Ok(())
}

pub(crate) fn read_materialization(
    conn: &Connection,
    receipt: EntityId,
) -> Result<Option<EvidenceMaterialization>, ShenkoError> {
    let row: Option<(String, i64)> = conn.query_row("SELECT data,updated_commit_seq FROM native_evidence_materializations WHERE receipt_id=?1",
        [receipt.to_string()], |row| Ok((row.get(0)?, row.get(1)?))).optional().map_err(storage_error)?;
    row.map(|(raw, seq)| {
        let marker: EvidenceMaterialization = serde_json::from_str(&raw).map_err(invalid)?;
        if marker.schema_version != 1 || i64::try_from(marker.committed_at.seq.0).ok() != Some(seq)
        {
            return Err(invalid("local E1 materialization identity differs"));
        }
        Ok(marker)
    })
    .transpose()
}

pub(crate) fn verify_materialization(
    conn: &Connection,
    marker: &EvidenceMaterialization,
    fact: &NativeEvidenceFact,
) -> Result<(), ShenkoError> {
    let committed: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM commit_log WHERE ledger_id=?1 AND seq=?2)",
            params![
                marker.committed_at.ledger_id.to_string(),
                i64::try_from(marker.committed_at.seq.0).map_err(invalid)?
            ],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    let receipt: Option<(String, String)> = conn
        .query_row(
            "SELECT data,created_at FROM evidence_receipts WHERE receipt_id=?1",
            [fact.receipt.receipt_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(storage_error)?;
    if !committed
        || marker.logical_result != fact.receipt.received_commit
        || receipt
            .map(|(raw, created)| {
                Ok::<_, ShenkoError>(
                    serde_json::from_str::<serde_json::Value>(&raw).map_err(invalid)?
                        == serde_json::to_value(&fact.receipt)?
                        && created == fact.created_at,
                )
            })
            .transpose()?
            != Some(true)
    {
        return Err(invalid(
            "local E1 materialization lacks its original observation or physical commit",
        ));
    }
    Ok(())
}
