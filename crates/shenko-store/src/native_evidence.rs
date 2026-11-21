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
    pub schema_version: u32,
    pub arm: CanonicalFact,
    pub receipt: EvidenceReceipt,
    pub operation: OperationRecord,
    pub attempt: AttemptRecord,
    pub created_at: String,
    /// Version 1 supported only a definite first observation. Version 2
    /// freezes the actual decoder inputs, including an uncertain conclusion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fold: Option<NativeEvidenceFold>,
    /// Version 3 is an independent factual continuation, never a T2 proof.
    /// It names the exact first Unknown whose local application is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor_ref: Option<ObjectRef>,
    /// Codecs 4/5 bind reuse to an authenticated, already applied source E1.
    /// Carrying this closure never authorizes installing the source operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<NativeEvidenceSourceDependency>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeEvidenceSourceDependency {
    pub evidence_ref: ObjectRef,
    pub operation: OperationRecord,
}

fn validate_source_operation(operation: &OperationRecord, dependent: &OperationRecord) -> Result<(), ShenkoError> {
    if operation.operation_id == dependent.operation_id
        || operation.record_schema_version != 1
        || operation.outcome_conflict
        || (dependent.outcome == Some(OperationOutcome::Succeeded)
            && (operation.state != OperationState::Resolved
                || operation.outcome != Some(OperationOutcome::Succeeded)
                || operation.effect_status != EffectStatus::Complete))
    {
        return Err(conflict("E1 source is conflicted or cannot justify the dependent success"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeEvidenceFold {
    pub expected_revision: Option<RecordRevision>,
    pub outcome: Option<OperationOutcome>,
    pub effect: EffectStatus,
    pub terminal_content_conflict: bool,
    /// Frozen alongside the own-operation revision by the E1 producer.
    /// Absent fields retain the exact codec 1/2/3 serialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_operation_revision: Option<(EntityId, RecordRevision)>,
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

pub(crate) fn read_records(
    conn: &Connection,
    operation: EntityId,
    attempt: EntityId,
) -> Result<(OperationRecord, AttemptRecord), ShenkoError> {
    let operation: Option<String> = conn
        .query_row(
            "SELECT data FROM operations WHERE operation_id=?1",
            [operation.to_string()],
            |row| row.get(0),
        )
        .optional().map_err(storage_error)?;
    let attempt: Option<String> = conn
        .query_row(
            "SELECT data FROM attempts WHERE attempt_id=?1",
            [attempt.to_string()],
            |row| row.get(0),
        )
        .optional().map_err(storage_error)?;
    Ok((
        serde_json::from_str(&operation.ok_or_else(|| conflict("E1 original operation is missing; retain predecessor debt"))?).map_err(invalid)?,
        serde_json::from_str(&attempt.ok_or_else(|| conflict("E1 original attempt is missing; retain predecessor debt"))?).map_err(invalid)?,
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
        if !matches!(self.schema_version, 1 | 2 | 4)
            || (self.schema_version != 1) != self.fold.is_some()
            || (self.schema_version == 4) != self.source.is_some()
            || self.predecessor_ref.is_some()
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
                    source_operation_revision: None,
                },
            None => return Err(invalid("legacy E1 cannot describe an uncertain fold")),
        };
        if fold.source_operation_revision
            != self.source.as_ref().map(|source| (source.operation.operation_id, source.operation.record_revision))
            || (self.schema_version != 4 && fold.source_operation_revision.is_some())
            || (self.schema_version == 4 && fold.expected_revision != Some(original.operation.record_revision))
        {
            return Err(invalid("E1 source dependency and frozen decoder guard disagree"));
        }
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
    /// A source is usable only with its real local predecessor and exact sealed
    /// E1. No archive row, copied SQL marker or closure import creates that fact.
    pub(crate) fn verify_native_evidence_source(
        &self,
        conn: &Connection,
        source: &NativeEvidenceSourceDependency,
        dependent: &OperationRecord,
    ) -> Result<(), ShenkoError> {
        self.verify_native_evidence_source_at(conn, source, dependent, true)
    }

    pub(crate) fn verify_native_evidence_source_at(
        &self,
        conn: &Connection,
        source: &NativeEvidenceSourceDependency,
        dependent: &OperationRecord,
        current: bool,
    ) -> Result<(), ShenkoError> {
        // Source reuse can itself depend on an earlier reuse. Walk that graph
        // explicitly: transport's object bound must not become call-stack depth.
        let mut pending = Vec::new();
        let mut seen = std::collections::BTreeMap::new();
        self.verify_native_source_node(conn, source, dependent, current, &mut pending)?;
        seen.insert((source.evidence_ref.digest.clone(), current), std::borrow::Cow::Borrowed(&source.operation));
        while let Some((source, dependent, current)) = pending.pop() {
            let key = (source.evidence_ref.digest.clone(), current);
            if let Some(previous) = seen.get(&key) {
                validate_source_operation(&source.operation, &dependent)?;
                if !same(&source.operation, previous.as_ref())? {
                    return Err(invalid("one E1 source reference names different operation snapshots"));
                }
                continue;
            }
            // Each object can be checked once as current and once as historical.
            if seen.len() >= 2 * shenko_types::native_evidence::NATIVE_EVIDENCE_CLOSURE_OBJECTS {
                return Err(invalid("E1 source dependency graph exceeds the closure bound"));
            }
            self.verify_native_source_node(conn, &source, &dependent, current, &mut pending)?;
            seen.insert(key, std::borrow::Cow::Owned(source.operation));
        }
        Ok(())
    }

    fn verify_native_source_node(
        &self,
        conn: &Connection,
        source: &NativeEvidenceSourceDependency,
        dependent: &OperationRecord,
        current: bool,
        pending: &mut Vec<(NativeEvidenceSourceDependency, OperationRecord, bool)>,
    ) -> Result<(), ShenkoError> {
        let operation = &source.operation;
        validate_source_operation(operation, dependent)?;
        let (fact, _) = self.read_native_delivery_fact_at(conn, &source.evidence_ref, None)?;
        if !same(operation, &fact.operation)? {
            return Err(invalid("E1 source snapshot differs from its authenticated original observation"));
        }
        let marker = read_materialization(conn, fact.receipt.receipt_id)?
            .ok_or_else(|| conflict("dependent E1 lacks its original local source predecessor; retain debt"))?;
        if marker.evidence_ref != source.evidence_ref {
            return Err(conflict("dependent E1 source predecessor names different original bytes"));
        }
        verify_materialization(conn, &marker, &fact)?;
        if current {
            verify_projection(conn, &fact)?;
        }
        if let Some(reference) = &fact.predecessor_ref {
            // The structural successor reader already authenticated this exact
            // first-Unknown fact and its real local receipt.
            let first: NativeEvidenceFact = self.read_native_json(reference, CODEC, reference.codec.major)?;
            if let Some(source) = first.source {
                pending.push((source, first.operation, false));
            }
        }
        if let Some(source) = fact.source {
            pending.push((source, fact.operation, current));
        }
        Ok(())
    }

    pub(crate) fn native_evidence_source_dependency(
        &self,
        conn: &Connection,
        guard: Option<(EntityId, RecordRevision)>,
        dependent: &OperationRecord,
    ) -> Result<Option<NativeEvidenceSourceDependency>, ShenkoError> {
        let Some((id, revision)) = guard else { return Ok(None); };
        let raw: Option<String> = conn.query_row(
            "SELECT data FROM operations WHERE operation_id=?1", [id.to_string()], |row| row.get(0),
        ).optional().map_err(storage_error)?;
        let operation: OperationRecord = serde_json::from_str(&raw.ok_or_else(||
            conflict("dependent E1 lacks its original source operation; retain debt"))?).map_err(invalid)?;
        if operation.operation_id != id || operation.record_revision != revision {
            return Err(conflict("dependent E1 source guard differs from the actual operation"));
        }
        let head = operation.evidence_through.ok_or_else(|| conflict("dependent E1 source has no original evidence head; retain debt"))?;
        // Only actual local applications at this exact evidence head qualify.
        // The second row detects ambiguity without an unbounded archive scan.
        let mut statement = conn.prepare("SELECT m.data FROM native_evidence_materializations m JOIN evidence_receipts e ON e.receipt_id=m.receipt_id WHERE e.operation_id=?1 AND json_extract(e.data,'$.received_commit.ledger_id')=?2 AND json_extract(e.data,'$.received_commit.seq')=?3 LIMIT 2").map_err(storage_error)?;
        let rows = statement.query_map(params![id.to_string(), head.ledger_id.to_string(), i64::try_from(head.seq.0).map_err(invalid)?], |row| row.get::<_, String>(0))
            .map_err(storage_error)?;
        let mut selected = None;
        for row in rows {
            let marker: EvidenceMaterialization = serde_json::from_str(&row.map_err(storage_error)?).map_err(invalid)?;
            let (fact, _) = self.read_native_delivery_fact(conn, &marker.evidence_ref)?;
            if same(&operation, &fact.operation)? {
                let source = NativeEvidenceSourceDependency { evidence_ref: marker.evidence_ref, operation: operation.clone() };
                self.verify_native_evidence_source(conn, &source, dependent)?;
                if selected.is_some() {
                    return Err(conflict("dependent E1 source has ambiguous original materializations"));
                }
                selected = Some(source);
            }
        }
        selected.map(Some).ok_or_else(|| conflict("dependent E1 lacks its authenticated original source predecessor; retain debt"))
    }

    /// Called after the actual E1 writes, before their one COMMIT. It adds no
    /// control reservation and does not make a stale callback current again.
    pub(crate) fn seal_native_evidence(
        &self,
        tx: &Transaction<'_>,
        before: &(OperationRecord, AttemptRecord),
        receipt: &EvidenceReceipt,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        fold: NativeEvidenceFold,
    ) -> Result<(), ShenkoError> {
        // Proof debt may preserve a real observation, never excuse an ignored
        // or rewritten original E1 projection supplied by a local SQL trigger.
        verify_accepted_evidence(tx, receipt, operation, attempt)?;
        tx.execute_batch("SAVEPOINT native_evidence_seal").map_err(storage_error)?;
        let result = if first_armed(&before.0, &before.1) {
            self.try_seal_first_native_evidence(tx, before, receipt, operation, attempt, fold)
        } else {
            self.try_seal_native_successor(tx, before, receipt, operation, attempt, fold)
        };
        match result {
            Ok(()) => {
                tx.execute_batch("RELEASE native_evidence_seal").map_err(storage_error)?;
                Ok(())
            }
            Err(error @ ShenkoError::Storage(_)) => Err(error),
            Err(error) => {
                tx.execute_batch("ROLLBACK TO native_evidence_seal; RELEASE native_evidence_seal")
                    .map_err(storage_error)?;
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
                let rows = tx.execute("INSERT INTO native_evidence_seal_debts(receipt_id,operation_id,data,updated_commit_seq) VALUES(?1,?2,?3,?4)",
                    params![receipt.receipt_id.to_string(), operation.operation_id.to_string(), serde_json::to_string(&debt)?,
                        i64::try_from(receipt.received_commit.seq.0).map_err(invalid)?]).map_err(storage_error)?;
                let actual: Option<(String, String, u64)> = tx.query_row(
                    "SELECT operation_id,data,updated_commit_seq FROM native_evidence_seal_debts WHERE receipt_id=?1",
                    [receipt.receipt_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                ).optional().map_err(storage_error)?;
                let retained = actual.map(|(id, raw, seq)| -> Result<bool, ShenkoError> {
                    let actual: NativeEvidenceSealDebt = serde_json::from_str(&raw).map_err(invalid)?;
                    Ok(id == operation.operation_id.to_string() && seq == receipt.received_commit.seq.0 && same(&actual, &debt)?)
                }).transpose()?.unwrap_or(false);
                if rows != 1 || !retained {
                    return Err(conflict("original E1 seal debt was not physically retained"));
                }
                Ok(())
            }
        }?;
        verify_accepted_evidence(tx, receipt, operation, attempt)
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
            if fold.source_operation_revision.is_some() {
                return Err(conflict("dependent E1 lacks its original paired T1; retain debt"));
            }
            return Ok(());
        };
        // Version 1 intentionally retains its old whole-table dependency.
        // It cannot supply this new direct claim/paired-dispatch proof.
        if arm.transition_ref.codec.major == 1 {
            if fold.source_operation_revision.is_some() {
                return Err(conflict("dependent E1 has no supported original paired T1; retain debt"));
            }
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
            if fold.source_operation_revision.is_some() {
                return Err(conflict("dependent E1 is not its original first observation; retain debt"));
            }
            return Ok(());
        }
        let created_at: String = tx
            .query_row(
                "SELECT created_at FROM evidence_receipts WHERE receipt_id=?1",
                [receipt.receipt_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let source = self.native_evidence_source_dependency(tx, fold.source_operation_revision, operation)?;
        let fact = NativeEvidenceFact {
            schema_version: if source.is_some() { 4 } else { 2 },
            arm,
            receipt: receipt.clone(),
            operation: operation.clone(),
            attempt: attempt.clone(),
            created_at,
            fold: Some(fold),
            predecessor_ref: None,
            source,
        };
        fact.validate(&original)?;
        self.seal_native_evidence_fact(tx, &fact)
    }

    /// The caller independently validates the bounded first/successor fold.
    /// This remains inside the original observation's E1 transaction.
    pub(crate) fn seal_native_evidence_fact(
        &self,
        tx: &Transaction<'_>,
        fact: &NativeEvidenceFact,
    ) -> Result<(), ShenkoError> {
        if let Some(source) = &fact.source {
            self.verify_native_evidence_source(tx, source, &fact.operation)?;
        }
        verify_projection(tx, fact)?;
        let bytes = serde_json::to_vec(fact)?;
        if bytes.len() > crate::control_transition::MAX_IMAGE_BYTES {
            return Err(invalid(
                "original E1 proof exceeds the bounded admission size",
            ));
        }
        let reference = self
            .cas
            .seal_object(&bytes, "application/json", "commit-authority", CODEC, fact.schema_version)?
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
        record_materialization(tx, &reference, fact, fact.receipt.received_commit)?;
        if let Some(source) = &fact.source {
            self.verify_native_evidence_source(tx, source, &fact.operation)?;
        }
        Ok(())
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
        self.read_native_evidence_fact_at(conn, reference, Some(true))
    }

    /// Historical decoding authenticates original local source receipts but
    /// does not demand that their older source revision remain the live head.
    /// This mode is used only within an authenticated successor/source history.
    pub(crate) fn read_native_evidence_fact_at(
        &self,
        conn: &Connection,
        reference: &ObjectRef,
        current_source: Option<bool>,
    ) -> Result<(NativeEvidenceFact, NativeArmFacts), ShenkoError> {
        if !matches!(reference.codec.major, 1 | 2 | 4) {
            return Err(invalid("unsupported typed object for original E1"));
        }
        let fact: NativeEvidenceFact = self.read_native_json(reference, CODEC, reference.codec.major)?;
        if fact.schema_version != reference.codec.major
            || (reference.codec.major != 1) != fact.fold.is_some()
            || fact.predecessor_ref.is_some()
        {
            return Err(invalid("E1 codec and frozen fold disagree"));
        }
        let (_, missing) = if reference.codec.major != 1 {
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
        if let (Some(source), Some(current)) = (&fact.source, current_source) {
            self.verify_native_evidence_source_at(conn, source, &fact.operation, current)?;
        }
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
        write_projection(tx, &fact, local)?;
        if fact.operation.state == OperationState::Reconciling {
            let reason = if fact.operation.outcome_conflict {
                crate::reconcile::REASON_CONTRADICTORY_EVIDENCE
            } else {
                crate::reconcile::REASON_EFFECT_UNKNOWN
            };
            self.ensure_unknown_effect_case(tx, &fact.operation, fact.receipt.receipt_id, reason, local)?;
        }
        record_materialization(tx, reference, &fact, local)?;
        if let Some(source) = &fact.source {
            self.verify_native_evidence_source(tx, source, &fact.operation)?;
        }
        Ok(true)
    }
}

pub(crate) fn write_projection(
    tx: &Transaction<'_>,
    fact: &NativeEvidenceFact,
    local: CommitRef,
) -> Result<(), ShenkoError> {
    let seq = i64::try_from(local.seq.0).map_err(invalid)?;
    let receipt_rows = tx.execute("INSERT INTO evidence_receipts(receipt_id,operation_id,data,updated_commit_seq,created_at) VALUES(?1,?2,?3,?4,?5)",
        params![fact.receipt.receipt_id.to_string(), fact.operation.operation_id.to_string(), serde_json::to_string(&fact.receipt)?, seq, fact.created_at]).map_err(storage_error)?;
    let operation_rows = tx.execute("UPDATE operations SET record_revision=?2,data=?3,updated_commit_seq=?4 WHERE operation_id=?1",
        params![fact.operation.operation_id.to_string(), i64::try_from(fact.operation.record_revision.0).map_err(invalid)?, serde_json::to_string(&fact.operation)?, seq]).map_err(storage_error)?;
    let attempt_rows = tx.execute(
        "UPDATE attempts SET data=?2,updated_commit_seq=?3 WHERE attempt_id=?1",
        params![fact.attempt.attempt_id.to_string(), serde_json::to_string(&fact.attempt)?, seq],
    )
    .map_err(storage_error)?;
    if (receipt_rows, operation_rows, attempt_rows) != (1, 1, 1) {
        return Err(conflict("E1 physical projection did not accept every original row"));
    }
    verify_projection(tx, fact)
}

/// An ignored UPDATE or a trigger-rewritten row is not a Store accepted fact.
/// Check the physical identity columns as well as the authenticated typed data.
pub(crate) fn verify_accepted_evidence(
    conn: &Connection,
    receipt: &EvidenceReceipt,
    operation: &OperationRecord,
    attempt: &AttemptRecord,
) -> Result<(), ShenkoError> {
    verify_evidence_rows(conn, receipt, operation, attempt)?;
    let committed: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM commit_log WHERE ledger_id=?1 AND seq=?2)",
        params![receipt.received_commit.ledger_id.to_string(), i64::try_from(receipt.received_commit.seq.0).map_err(invalid)?],
        |row| row.get(0),
    ).map_err(storage_error)?;
    if !committed {
        return Err(conflict("original E1 physical commit is missing; observation was not accepted"));
    }
    Ok(())
}

fn verify_evidence_rows(
    conn: &Connection,
    receipt: &EvidenceReceipt,
    operation: &OperationRecord,
    attempt: &AttemptRecord,
) -> Result<(), ShenkoError> {
    let current = read_records(conn, operation.operation_id, attempt.attempt_id)?;
    let physical: Option<(String, u64, String)> = conn.query_row(
        "SELECT e.operation_id,o.record_revision,a.operation_id FROM evidence_receipts e JOIN operations o ON o.operation_id=e.operation_id JOIN attempts a ON a.attempt_id=?2 WHERE e.receipt_id=?1",
        params![receipt.receipt_id.to_string(), attempt.attempt_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional().map_err(storage_error)?;
    let raw: Option<String> = conn.query_row(
        "SELECT data FROM evidence_receipts WHERE receipt_id=?1", [receipt.receipt_id.to_string()], |row| row.get(0),
    ).optional().map_err(storage_error)?;
    let actual = raw.map(|raw| serde_json::from_str::<EvidenceReceipt>(&raw).map_err(invalid)).transpose()?;
    if physical != Some((operation.operation_id.to_string(), operation.record_revision.0, operation.operation_id.to_string()))
        || !same(&current.0, operation)? || !same(&current.1, attempt)?
        || !same(&actual, &Some(receipt))?
    {
        return Err(conflict("E1 physical rows differ from the original accepted observation"));
    }
    Ok(())
}

pub(crate) fn verify_projection(conn: &Connection, fact: &NativeEvidenceFact) -> Result<(), ShenkoError> {
    verify_evidence_rows(conn, &fact.receipt, &fact.operation, &fact.attempt)?;
    let created: Option<String> = conn.query_row(
        "SELECT created_at FROM evidence_receipts WHERE receipt_id=?1",
        [fact.receipt.receipt_id.to_string()], |row| row.get(0),
    ).optional().map_err(storage_error)?;
    if created.as_deref() != Some(fact.created_at.as_str()) {
        return Err(conflict("E1 physical observation time differs from its original seal"));
    }
    Ok(())
}

pub(crate) fn record_materialization(
    tx: &Transaction<'_>,
    reference: &ObjectRef,
    fact: &NativeEvidenceFact,
    local: CommitRef,
) -> Result<(), ShenkoError> {
    verify_projection(tx, fact)?;
    let receipt = EvidenceMaterialization {
        schema_version: 1,
        evidence_ref: reference.clone(),
        logical_result: fact.receipt.received_commit,
        committed_at: local,
    };
    let rows = tx.execute("INSERT INTO native_evidence_materializations(receipt_id,data,updated_commit_seq) VALUES(?1,?2,?3)",
        params![fact.receipt.receipt_id.to_string(), serde_json::to_string(&receipt)?, i64::try_from(local.seq.0).map_err(invalid)?]).map_err(storage_error)?;
    let actual = read_materialization(tx, fact.receipt.receipt_id)?;
    if rows != 1 || !same(&actual, &Some(receipt))? {
        return Err(conflict("E1 physical materialization did not retain the original accepted fact"));
    }
    verify_projection(tx, fact)?;
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
