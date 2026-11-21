//! One factual successor: the original attempt's first Unknown becomes definite.
//!
//! Codec 3 has an explicit first-E1 predecessor and requires its real local
//! materialization. It carries no case decision, task transition or send permit.
//! Codec 5 retains this successor contract with an exact source dependency.
//! Codec 6 wraps either original shape without changing that distinction.
//! Chosen T2 accepts only first E1, never either successor shape.
use rusqlite::{Connection, Transaction, params};
use shenko_types::{
    commit::CommitRef,
    error::ShenkoError,
    evidence::EvidenceReceipt,
    ids::RecordRevision,
    object::ObjectRef,
    operation::{AttemptDispatchStage, AttemptRecord, EffectStatus, OperationRecord, OperationState},
};

use crate::{
    Store,
    arming_transition::NativeArmFacts,
    native_evidence::{
        NativeEvidenceFact, NativeEvidenceFold, read_materialization, read_records,
        record_materialization, verify_materialization, write_projection,
    },
    native_facts::{conflict, invalid, same},
    store::storage_error,
};

fn first_unknown(operation: &OperationRecord, attempt: &AttemptRecord) -> bool {
    operation.record_schema_version == 1
        && attempt.record_schema_version == 1
        && operation.record_revision == RecordRevision(1)
        && operation.state == OperationState::Reconciling
        && operation.outcome.is_none()
        && !operation.outcome_conflict
        && operation.effect_status == EffectStatus::Unknown
        && operation.evidence_through.is_some()
        && attempt.dispatch_stage == AttemptDispatchStage::Unknown
        && attempt.final_receipt_id.is_some()
}

fn successor_scope(
    conn: &Connection,
    original: &NativeArmFacts,
    expected_receipts: u64,
) -> Result<(), ShenkoError> {
    let counts: (u64, u64, u64) = conn.query_row(
        "SELECT (SELECT count(*) FROM operations WHERE json_extract(data,'$.task_id')=?1),
            (SELECT count(*) FROM attempts WHERE operation_id=?2),
            (SELECT count(*) FROM evidence_receipts WHERE operation_id=?2)",
        params![original.task.task_id.to_string(), original.operation.operation_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).map_err(storage_error)?;
    if counts != (1, 1, expected_receipts)
        || crate::native_facts::has_ordinary_children(conn, original.task.task_id)?
    {
        return Err(conflict(
            "native E1 successor requires one original operation/attempt and no ordinary child",
        ));
    }
    Ok(())
}

fn verify_first_receipt(
    conn: &Connection,
    reference: &ObjectRef,
    first: &NativeEvidenceFact,
) -> Result<(), ShenkoError> {
    let marker = read_materialization(conn, first.receipt.receipt_id)?
        .ok_or_else(|| conflict("successor E1 requires the first Unknown's actual local receipt"))?;
    if marker.evidence_ref != *reference {
        return Err(invalid("first-Unknown local receipt names a different original seal"));
    }
    verify_materialization(conn, &marker, first)
}

impl NativeEvidenceFact {
    fn validate_successor(
        &self,
        predecessor_ref: &ObjectRef,
        predecessor: &Self,
    ) -> Result<(), ShenkoError> {
        let fold = self.fold.ok_or_else(|| invalid("successor E1 omitted its original fold"))?;
        let outcome = fold.outcome.ok_or_else(|| invalid("successor E1 is not definite"))?;
        if !matches!(self.schema_version, 3 | 5)
            || self.predecessor_ref.as_ref() != Some(predecessor_ref)
            || !matches!(predecessor.schema_version, 2 | 4)
            || (self.schema_version == 5) != self.source.is_some()
            || (self.schema_version == 3 && (predecessor.schema_version != 2 || fold.source_operation_revision.is_some()))
            || !first_unknown(&predecessor.operation, &predecessor.attempt)
            || !same(&self.arm, &predecessor.arm)?
            || self.receipt.record_schema_version != 1
            || self.receipt.receipt_id == predecessor.receipt.receipt_id
            || self.receipt.operation_id != Some(predecessor.operation.operation_id)
            || self.receipt.attempt_id != Some(predecessor.attempt.attempt_id)
            || self.receipt.received_commit.seq.0 == 0
            || (self.receipt.received_commit.ledger_id == predecessor.receipt.received_commit.ledger_id
                && self.receipt.received_commit.seq <= predecessor.receipt.received_commit.seq)
            || fold.effect == EffectStatus::Unknown
            || fold.terminal_content_conflict
            || fold.expected_revision.is_some_and(|revision| revision != RecordRevision(1))
            || (self.schema_version == 5 && fold.expected_revision != Some(RecordRevision(1)))
            || self.attempt.last_observed_at.is_none()
            || chrono::NaiveDateTime::parse_from_str(&self.created_at, "%Y-%m-%d %H:%M:%S").is_err()
        {
            return Err(invalid("successor E1 changed its original first-Unknown causal predecessor"));
        }
        let guard = self.source.as_ref().map(|source|
            (source.operation.operation_id, source.operation.record_revision));
        if fold.source_operation_revision.is_some_and(|fold_guard| Some(fold_guard) != guard)
            || (fold.source_operation_revision.is_none()
                && !same(&self.source, &predecessor.source)?)
            || predecessor.source.as_ref().is_some_and(|previous| self.source.as_ref().is_none_or(|source|
                source.operation.operation_id != previous.operation.operation_id
                    || source.operation.record_revision.0 < previous.operation.record_revision.0
                    || (source.operation.record_revision == previous.operation.record_revision
                        && source.evidence_ref != previous.evidence_ref)))
        {
            return Err(invalid("successor E1 changed or dropped its exact source dependency"));
        }
        // The predecessor is independently validated from the original paired
        // T1. Only the normal Unknown -> definite fold may follow it here.
        let mut operation = predecessor.operation.clone();
        let mut attempt = predecessor.attempt.clone();
        operation.state = OperationState::Resolved;
        operation.outcome = Some(outcome);
        operation.effect_status = fold.effect;
        operation.record_revision = RecordRevision(2);
        operation.evidence_through = Some(self.receipt.received_commit);
        attempt.dispatch_stage = AttemptDispatchStage::Ended;
        attempt.final_receipt_id = Some(self.receipt.receipt_id);
        attempt.last_observed_at = self.attempt.last_observed_at;
        if !same(&operation, &self.operation)? || !same(&attempt, &self.attempt)? {
            return Err(invalid("successor E1 rewrote the original operation, attempt or frozen fold"));
        }
        Ok(())
    }
}

impl Store {
    /// This is a local fact check, not a source marker imported from CAS. A
    /// successor closure alone must never install its missing first E1.
    fn actual_first_unknown(
        &self,
        conn: &Connection,
        reference: &ObjectRef,
    ) -> Result<(NativeEvidenceFact, NativeArmFacts), ShenkoError> {
        let (fact, _) = self.read_native_evidence_object(reference)?;
        if !matches!(fact.schema_version, 2 | 4) {
            return Err(invalid("successor E1 must name a codec-2/4 first Unknown"));
        }
        let (first, original) = self.read_native_evidence_fact_at(conn, reference, Some(false))?;
        if !first_unknown(&first.operation, &first.attempt) {
            return Err(invalid("successor E1 predecessor is not the first Unknown"));
        }
        verify_first_receipt(conn, reference, &first)?;
        Ok((first, original))
    }

    pub(crate) fn try_seal_native_successor(
        &self,
        tx: &Transaction<'_>,
        before: &(OperationRecord, AttemptRecord),
        receipt: &EvidenceReceipt,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        fold: NativeEvidenceFold,
    ) -> Result<(), ShenkoError> {
        if !first_unknown(&before.0, &before.1)
            || receipt.record_schema_version != 1
            || fold.outcome.is_none()
            || fold.effect == EffectStatus::Unknown
            || fold.terminal_content_conflict
        {
            if fold.source_operation_revision.is_some() {
                return Err(conflict("dependent later E1 lacks the supported original Unknown predecessor; retain debt"));
            }
            return Ok(());
        }
        let Some(arm) = self.local_native_fact(tx, before.0.prepared_commit, "native.arm")? else {
            if fold.source_operation_revision.is_some() {
                return Err(conflict("dependent successor lacks its original paired T1; retain debt"));
            }
            return Ok(());
        };
        if arm.transition_ref.codec.major == 1 {
            if fold.source_operation_revision.is_some() {
                return Err(conflict("dependent successor has no supported original paired T1; retain debt"));
            }
            return Ok(());
        }
        let first_id = before.1.final_receipt_id
            .ok_or_else(|| invalid("first Unknown omitted its original receipt"))?;
        let marker = read_materialization(tx, first_id)?
            .ok_or_else(|| conflict("later E1 retained without an actual first-Unknown application"))?;
        let (first, original) = self.actual_first_unknown(tx, &marker.evidence_ref)?;
        if !same(&arm, &first.arm)?
            || !same(&before.0, &first.operation)?
            || !same(&before.1, &first.attempt)?
            || (marker.committed_at.ledger_id == receipt.received_commit.ledger_id
                && marker.committed_at.seq >= receipt.received_commit.seq)
        {
            return Err(conflict("later E1 did not observe the locally materialized first Unknown"));
        }
        // The new receipt has already been admitted in this same transaction.
        // Non-storage proof failures are retained as seal debt, never E1 loss.
        successor_scope(tx, &original, 2)?;
        let created_at = tx.query_row(
            "SELECT created_at FROM evidence_receipts WHERE receipt_id=?1",
            [receipt.receipt_id.to_string()], |row| row.get(0),
        ).map_err(storage_error)?;
        let source = match fold.source_operation_revision {
            Some(_) => self.native_evidence_source_dependency(tx, fold.source_operation_revision, operation)?,
            None => first.source.clone(),
        };
        let fact = NativeEvidenceFact {
            schema_version: if source.is_some() { 5 } else { 3 },
            arm,
            receipt: receipt.clone(),
            operation: operation.clone(),
            attempt: attempt.clone(),
            created_at,
            fold: Some(fold),
            predecessor_ref: Some(marker.evidence_ref.clone()),
            source,
        };
        fact.validate_successor(&marker.evidence_ref, &first)?;
        self.seal_native_evidence_fact(tx, &fact)
    }

    /// Factual intake only. The T2 reader rejects both successor codecs.
    pub(crate) fn read_native_delivery_fact(
        &self,
        conn: &Connection,
        reference: &ObjectRef,
    ) -> Result<(NativeEvidenceFact, NativeArmFacts), ShenkoError> {
        self.read_native_delivery_fact_at(conn, reference, Some(true))
    }

    pub(crate) fn read_native_delivery_fact_at(
        &self,
        conn: &Connection,
        reference: &ObjectRef,
        current_source: Option<bool>,
    ) -> Result<(NativeEvidenceFact, NativeArmFacts), ShenkoError> {
        let (fact, envelope) = self.read_native_evidence_object(reference)?;
        if !matches!(fact.schema_version, 3 | 5) {
            return self.read_native_evidence_fact_at(conn, reference, current_source);
        }
        let predecessor_ref = fact.predecessor_ref.as_ref()
            .ok_or_else(|| invalid("successor E1 omitted its causal predecessor"))?;
        if envelope.is_none() { self.native_evidence_closure(reference, None)?; }
        let (first, original) = self.read_native_evidence_fact_at(conn, predecessor_ref,
            current_source.map(|_| false))?;
        fact.validate_successor(predecessor_ref, &first)?;
        if let (Some(source), Some(current)) = (&fact.source, current_source) {
            self.verify_native_evidence_source_at(conn, source, &fact.operation, current)?;
        }
        // Malformed typed bytes are rejected before a missing local first-E1
        // receipt is classified as retryable predecessor debt.
        verify_first_receipt(conn, predecessor_ref, &first)?;
        if envelope.is_some() {
            self.verify_anchored_native_predecessors(conn, &fact)?;
            self.native_evidence_closure(reference, envelope.as_ref())?;
        }
        Ok((fact, original))
    }

    /// Fresh factual application has no worker/dispatch result. Existing
    /// materializations are handled by the delivery's non-rewinding replay path.
    pub(crate) fn materialize_native_successor(
        &self,
        tx: &Transaction<'_>,
        reference: &ObjectRef,
        local: CommitRef,
    ) -> Result<usize, ShenkoError> {
        let (fact, original) = self.read_native_delivery_fact(tx, reference)?;
        let predecessor_ref = fact.predecessor_ref.as_ref()
            .ok_or_else(|| invalid("successor intake requires its causal predecessor"))?;
        let (first, _) = self.actual_first_unknown(tx, predecessor_ref)?;
        let current = read_records(tx, fact.operation.operation_id, fact.attempt.attempt_id)?;
        if !same(&current.0, &first.operation)? || !same(&current.1, &first.attempt)? {
            return Err(conflict("successor E1 cannot overwrite a different or later local observation"));
        }
        successor_scope(tx, &original, 1)?;
        let has_case: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM reconciliation_cases WHERE operation_id=?1)",
            [fact.operation.operation_id.to_string()], |row| row.get(0),
        ).map_err(storage_error)?;
        if !has_case || !matches!(
            crate::receipt_admission::decide(tx, &fact.receipt).map_err(storage_error)?,
            crate::receipt_admission::Admission::New
        ) {
            return Err(conflict("successor E1 lacks its local Unknown case or has conflicting receipt identity"));
        }
        write_projection(tx, &fact, local)?;
        // This updates only an undecided local case, or appends a local
        // resolution after an acknowledgement. No source decision is in E1.
        let cases = self.resolve_unknown_effect_cases(
            tx, fact.operation.operation_id, fact.receipt.receipt_id, local,
        )?;
        record_materialization(tx, reference, &fact, local)?;
        if let Some(source) = &fact.source {
            self.verify_native_evidence_source(tx, source, &fact.operation)?;
        }
        Ok(4 + cases)
    }
}
