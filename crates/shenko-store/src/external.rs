//! Conditional T1 and append-only E1 for native external executors.
//! No network I/O occurs under these transactions; task completion is separate.
use crate::Store;
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::evidence::EvidenceReceipt;
use shenko_types::{commit::CommitRef, ids::*, operation::*, task::*};

/// Trusted executor context, not authority supplied by a model tool argument.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInvocation {
    pub task_id: EntityId,
    pub task_revision: TaskRevision,
    pub record_revision: RecordRevision,
    pub invocation_id: EntityId,
    pub epoch: ControllerEpoch,
    pub software_generation: RuntimeGeneration,
    pub worker_id: EntityId,
    pub node_id: EntityId,
}

/// Exact quota send selected before T1; persisted on the original attempt.
/// None is legal only when this ledger has no configured quota policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceSendBinding {
    pub schema_version: u32,
    pub request_id: EntityId,
    pub send_id: Option<EntityId>,
}

impl Store {
    pub fn close_external_writer(&self) {
        self.ledger.close_writer();
    }

    pub fn external_task(&self, id: EntityId) -> Result<TaskRecord> {
        let c = self.ledger.conn.lock();
        let data: String = c.query_row(
            "SELECT data FROM tasks WHERE task_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn external_operation(&self, id: EntityId) -> Result<OperationRecord> {
        let c = self.ledger.conn.lock();
        let data: String = c.query_row(
            "SELECT data FROM operations WHERE operation_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn external_operation_attempt(&self, id: EntityId) -> Result<AttemptRecord> {
        let c = self.ledger.conn.lock();
        let data: String = c.query_row(
            "SELECT data FROM attempts WHERE operation_id=?1 ORDER BY rowid LIMIT 1",
            [id.to_string()],
            |r| r.get(0),
        )?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn external_attempt(&self, id: EntityId) -> Result<AttemptRecord> {
        let c = self.ledger.conn.lock();
        let data: String = c.query_row(
            "SELECT data FROM attempts WHERE attempt_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )?;
        Ok(serde_json::from_str(&data)?)
    }

    /// Startup resource reconstruction uses the original operation identities,
    /// including cancelled tasks. It never schedules another attempt.
    pub fn external_operations_after(
        &self, name: &str, after: Option<EntityId>, limit: usize,
    ) -> Result<Vec<OperationRecord>> {
        let conn = self.ledger.conn.lock();
        let mut query = conn.prepare(
            "SELECT data FROM operations WHERE json_extract(data,'$.operation_name')=?1
             AND operation_id>?2
             ORDER BY operation_id LIMIT ?3",
        )?;
        let rows = query.query_map(
            params![name, after.map(|id| id.to_string()).unwrap_or_default(), limit.min(256) as i64],
            |row| row.get::<_, String>(0),
        )?.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(|data| serde_json::from_str(&data).map_err(Into::into)).collect()
    }

    /// Returns None for a new armed admission, or the original operation for a
    /// matching repeated logical key. A hit NEVER authorizes another dispatch.
    pub fn arm_external_operation(
        &self,
        ctx: &TaskInvocation,
        op: &mut OperationRecord,
        att: &mut AttemptRecord,
    ) -> Result<Option<OperationRecord>> {
        self.ledger.with_mutation(
            || self.try_arm_external_operation(ctx, op, att, None),
            |e| e.is::<rusqlite::Error>() || e.is::<serde_json::Error>(),
        )
    }

    /// Child model T1 atomically claims the gate's exact R→S reservation with
    /// the native operation/attempt. Native tools keep the non-inference path.
    pub fn arm_external_inference_operation(
        &self, ctx: &TaskInvocation, op: &mut OperationRecord, att: &mut AttemptRecord,
        binding: &InferenceSendBinding,
    ) -> Result<Option<OperationRecord>> {
        self.ledger.with_mutation(
            || self.try_arm_external_operation(ctx, op, att, Some(binding)),
            |error| error.is::<rusqlite::Error>() || error.is::<serde_json::Error>()
                || error.downcast_ref::<shenko_types::error::ShenkoError>()
                    .is_some_and(|error| matches!(error, shenko_types::error::ShenkoError::Storage(_))),
        )
    }

    fn try_arm_external_operation(
        &self,
        ctx: &TaskInvocation,
        op: &mut OperationRecord,
        att: &mut AttemptRecord,
        inference: Option<&InferenceSendBinding>,
    ) -> Result<Option<OperationRecord>> {
        let mut c = self.ledger.conn.lock();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx.query_row(
            "SELECT data FROM operations WHERE json_extract(data,'$.task_id')=?1 AND json_extract(data,'$.logical_key')=?2",
            params![ctx.task_id.to_string(), op.logical_key], |r| r.get(0)).optional()?;
        if let Some(data) = existing {
            let old: OperationRecord = serde_json::from_str(&data)?;
            ensure!(
                old.operation_name == op.operation_name
                    && old.input_digest == op.input_digest
                    && old.resource_bindings_ref.digest == op.resource_bindings_ref.digest
                    && old.contract_ref.contract_digest == op.contract_ref.contract_digest
                    && old.authority_ref.delegation_id == op.authority_ref.delegation_id
                    && old.authority_ref.delegation_revision
                        == op.authority_ref.delegation_revision
                    && old.authority_ref.decision_ref == op.authority_ref.decision_ref
                    && old.acceptance_predicate_ref.digest == op.acceptance_predicate_ref.digest
                    && old.retry_contract_ref.digest == op.retry_contract_ref.digest,
                "operation_conflict: frozen input, binding or contract differs"
            );
            return Ok(Some(old));
        }
        // Subject publications use this same T1, with a revocable dispatcher
        // owner. A stale callback can enter E1 but cannot arm a new send.
        let subject_owner: Option<String> = tx.query_row(
            "SELECT CASE WHEN state='claimed' THEN COALESCE(owner,'') ELSE '' END FROM subject_intents WHERE intent_id=?1 AND kind='publication'",
            [ctx.task_id.to_string()], |r| r.get(0)).optional()?;
        if let Some(owner) = subject_owner {
            self.check_dispatch_owner(&tx)?;
            ensure!(
                owner == self.capabilities.incarnation.to_string(),
                "stale_subject_claim"
            );
        }
        let data: String = tx.query_row(
            "SELECT data FROM tasks WHERE task_id=?1",
            [ctx.task_id.to_string()],
            |r| r.get(0),
        )?;
        let mut task: TaskRecord = serde_json::from_str(&data)?;
        ensure!(
            task.task_revision == ctx.task_revision && task.record_revision == ctx.record_revision,
            "stale_task"
        );
        ensure!(
            task.abort_request.is_none()
                && matches!(task.dispatch_eligibility, DispatchEligibility::Ready),
            "task_not_admissible"
        );
        ensure!(
            matches!(task.state, TaskState::Running { invocation_id, epoch, mode: RunMode::Run, .. }
            if invocation_id == ctx.invocation_id && epoch == ctx.epoch),
            "stale_invocation"
        );
        ensure!(
            !task.memos.contains_key("native_settlement_only"),
            "settlement_invocation_cannot_arm"
        );
        if crate::child::memo_flag(&task, crate::child::NATIVE_CHILD) {
            ensure!(self.native_invocation_current_tx(&tx, &task, ctx)?, "stale_native_generation_or_owner");
        }
        ensure!(
            op.task_id == ctx.task_id
                && op.state == OperationState::InFlight
                && att.operation_id == op.operation_id
                && att.dispatch_stage == AttemptDispatchStage::Armed
                && att.invocation_id == ctx.invocation_id
                && att.epoch == ctx.epoch,
            "invalid_armed_operation"
        );
        let seq = self.ledger.allocate_seq(&tx)?;
        if task.kind == "model.child" {
            let binding = inference.ok_or_else(|| anyhow::anyhow!("model inference must use quota-bound T1 admission"))?;
            ensure!(binding.schema_version == 1, "unsupported inference send binding");
            let read = |object: &shenko_types::object::ObjectRef| -> Result<serde_json::Value> {
                let bytes = self.cas.read_object(&object.digest)?;
                ensure!(bytes.len() as u64 == object.bytes
                    && shenko_types::object::Digest::blake3(&bytes) == object.digest,
                    "inference T1 input/binding closure mismatch");
                Ok(serde_json::from_slice(&bytes)?)
            };
            let frozen = read(&op.final_input_ref)?;
            let decision = read(&task.input_ref)?;
            let intent = decision.get("delegated_task_intents").and_then(|value| value.as_array())
                .and_then(|intents| intents.iter().find(|intent| intent.get("intent_id") == Some(&serde_json::json!(task.task_id))))
                .ok_or_else(|| anyhow::anyhow!("model inference has no frozen S1 delegation"))?;
            ensure!(intent.pointer("/definition/input") == Some(&frozen)
                && frozen.get("request_id") == Some(&serde_json::to_value(binding.request_id)?)
                && op.input_digest == op.final_input_ref.digest
                && read(att.transport_ref.as_ref().ok_or_else(|| anyhow::anyhow!("inference attempt lacks selected send binding"))?)?
                    == serde_json::to_value(binding)?,
                "inference T1 request/payload/selected-send mismatch");
            if crate::quota::quota_gate_open(&tx)? {
                let send_id = binding.send_id.ok_or_else(|| anyhow::anyhow!("configured quota requires exact selected send"))?;
                ensure!(crate::quota::quota_claim_send(&tx, binding.request_id, send_id, seq)?,
                    "inference selected send is not admitted, armed and unclaimed for this request");
            } else {
                ensure!(binding.send_id.is_none(), "unexpected quota send without a recorded policy");
            }
        } else {
            ensure!(inference.is_none(), "inference T1 requires the registered model-child task contract");
        }
        let commit = CommitRef::new(self.ledger.ledger_id(), seq);
        op.prepared_commit = commit;
        att.armed_commit = commit;
        task.record_revision = RecordRevision(task.record_revision.0 + 1);
        task.updated_commit = commit;
        task.memos.insert(
            "external:last_operation".into(),
            serde_json::json!({"operation_id":op.operation_id,"attempt_id":att.attempt_id}),
        );
        tx.execute("INSERT INTO operations(operation_id,record_revision,data,updated_commit_seq) VALUES(?1,?2,?3,?4)",
            params![op.operation_id.to_string(),op.record_revision.0 as i64,serde_json::to_string(op)?,seq.0 as i64])?;
        tx.execute("INSERT INTO attempts(attempt_id,operation_id,data,updated_commit_seq) VALUES(?1,?2,?3,?4)",
            params![att.attempt_id.to_string(),op.operation_id.to_string(),serde_json::to_string(att)?,seq.0 as i64])?;
        tx.execute(
            "UPDATE tasks SET record_revision=?2,data=?3,updated_commit_seq=?4 WHERE task_id=?1",
            params![
                task.task_id.to_string(),
                task.record_revision.0 as i64,
                serde_json::to_string(&task)?,
                seq.0 as i64
            ],
        )?;
        tx.execute(
            "INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count) VALUES(?1,?2,?3,3)",
            params![
                commit.ledger_id.to_string(),
                seq.0 as i64,
                self.capabilities.incarnation.to_string()
            ],
        )?;
        tx.commit()?;
        Ok(None)
    }

    /// Evidence may arrive after cancellation/epoch change. This method does
    /// not advance a task, create children, or grant the old callback authority.
    pub fn record_external_evidence(
        &self,
        operation_id: EntityId,
        attempt_id: EntityId,
        ev: EvidenceReceipt,
        outcome: Option<OperationOutcome>,
        effect: EffectStatus,
    ) -> Result<OperationRecord> {
        self.ledger.with_mutation(
            || self.try_record_external_evidence(operation_id, attempt_id, ev, outcome, effect),
            |e| e.is::<rusqlite::Error>() || e.is::<serde_json::Error>(),
        )
    }

    fn try_record_external_evidence(
        &self,
        operation_id: EntityId,
        attempt_id: EntityId,
        mut ev: EvidenceReceipt,
        outcome: Option<OperationOutcome>,
        effect: EffectStatus,
    ) -> Result<OperationRecord> {
        let mut c = self.ledger.conn.lock();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let data: String = tx.query_row(
            "SELECT data FROM operations WHERE operation_id=?1",
            [operation_id.to_string()],
            |r| r.get(0),
        )?;
        let mut op: OperationRecord = serde_json::from_str(&data)?;
        let data: String = tx.query_row(
            "SELECT data FROM attempts WHERE attempt_id=?1",
            [attempt_id.to_string()],
            |r| r.get(0),
        )?;
        let mut att: AttemptRecord = serde_json::from_str(&data)?;
        ensure!(
            att.operation_id == operation_id
                && ev.operation_id == Some(operation_id)
                && ev.attempt_id == Some(attempt_id),
            "receipt_identity_mismatch"
        );
        let seq = self.ledger.allocate_seq(&tx)?;
        let commit = CommitRef::new(self.ledger.ledger_id(), seq);
        ev.received_commit = commit;
        // Process completion and application effect settlement are different.
        // A lost later query cannot erase a previously observed ended attempt.
        // Pre-marker records used Ended + no outcome for an unresolved conflict.
        // Migrate that projection conservatively; repeated facts cannot settle it.
        op.outcome_conflict |=
            att.dispatch_stage == AttemptDispatchStage::Ended && op.outcome.is_none();
        if let Some(observed) = outcome {
            if op.outcome_conflict || (op.outcome.is_some() && op.outcome != Some(observed)) {
                op.outcome_conflict = true;
                op.state = OperationState::Reconciling;
                op.outcome = None;
                op.effect_status = EffectStatus::Unknown;
            } else {
                op.state = if effect == EffectStatus::Unknown {
                    OperationState::Reconciling
                } else {
                    OperationState::Resolved
                };
                op.outcome = Some(observed);
                op.effect_status = effect;
            }
            att.dispatch_stage = AttemptDispatchStage::Ended;
            att.final_receipt_id = Some(ev.receipt_id);
        } else if att.dispatch_stage != AttemptDispatchStage::Ended {
            op.state = OperationState::Reconciling;
            op.outcome = None;
            op.effect_status = EffectStatus::Unknown;
            att.dispatch_stage = AttemptDispatchStage::Unknown;
            att.final_receipt_id = Some(ev.receipt_id);
        }
        op.record_revision = RecordRevision(op.record_revision.0 + 1);
        op.evidence_through = Some(commit);
        att.last_observed_at = Some(chrono::Utc::now());
        tx.execute("INSERT INTO evidence_receipts(receipt_id,operation_id,data,updated_commit_seq) VALUES(?1,?2,?3,?4)",
            params![ev.receipt_id.to_string(),operation_id.to_string(),serde_json::to_string(&ev)?,seq.0 as i64])?;
        tx.execute("UPDATE operations SET record_revision=?2,data=?3,updated_commit_seq=?4 WHERE operation_id=?1",
            params![operation_id.to_string(),op.record_revision.0 as i64,serde_json::to_string(&op)?,seq.0 as i64])?;
        tx.execute(
            "UPDATE attempts SET data=?2,updated_commit_seq=?3 WHERE attempt_id=?1",
            params![
                attempt_id.to_string(),
                serde_json::to_string(&att)?,
                seq.0 as i64
            ],
        )?;
        tx.execute(
            "INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count) VALUES(?1,?2,?3,3)",
            params![
                commit.ledger_id.to_string(),
                seq.0 as i64,
                self.capabilities.incarnation.to_string()
            ],
        )?;
        tx.commit()?;
        Ok(op)
    }
    pub fn external_operation_attempts(&self, id: EntityId) -> Result<Vec<AttemptRecord>> {
        let c = self.ledger.conn.lock();
        let mut q = c.prepare("SELECT data FROM attempts WHERE operation_id=?1 ORDER BY rowid")?;
        let rows = q
            .query_map([id.to_string()], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|data| serde_json::from_str(&data).map_err(Into::into))
            .collect()
    }

    pub fn external_evidence(&self, id: EntityId) -> Result<EvidenceReceipt> {
        let c = self.ledger.conn.lock();
        let data: String = c.query_row(
            "SELECT data FROM evidence_receipts WHERE receipt_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn external_operation_evidence(&self, id: EntityId) -> Result<Vec<EvidenceReceipt>> {
        let c = self.ledger.conn.lock();
        let mut q =
            c.prepare("SELECT data FROM evidence_receipts WHERE operation_id=?1 ORDER BY rowid")?;
        let rows = q
            .query_map([id.to_string()], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|data| serde_json::from_str(&data).map_err(Into::into))
            .collect()
    }
}
