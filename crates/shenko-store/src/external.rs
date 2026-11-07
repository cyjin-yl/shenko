//! Conditional T1 and append-only E1 for native external executors.
//! No network I/O occurs under these transactions; task completion is separate.
use crate::{InterventionReminder, Store};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::evidence::EvidenceReceipt;
use shenko_types::{commit::CommitRef, ids::*, operation::*, task::*};

/// One model step of a finite model child: one broker send with its own
/// T1/E1/T2 on the shared inference ledger.
pub const MODEL_CHILD_STEP_OPERATION: &str = "model.child.generate.v1";
/// The model child's own run operation. It carries the child's result, not a
/// provider send, and is bound to the same sealed delegation contract.
pub const MODEL_CHILD_RESULT_OPERATION: &str = "model.child.result.v1";
/// The native definition kind of a model child task.
pub const MODEL_CHILD_KIND: &str = "model.child";

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

/// Result of a durable inbox admission: the caller learns whether its row
/// was new or already admitted identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxAdmission {
    Inserted,
    Replay,
}

pub(crate) fn validate_original_event(
    event_id: EntityId, kind: &str, source: &str, data: &serde_json::Value,
) -> Result<()> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Original {
        event_id: EntityId,
        kind: String,
        source: String,
        payload: serde_json::Map<String, serde_json::Value>,
        created_at: chrono::DateTime<chrono::Utc>,
    }
    let original: Original = serde_json::from_value(data.clone())
        .map_err(|_| shenko_types::error::ShenkoError::conflict("invalid complete inbox event"))?;
    let _ = (&original.payload, original.created_at);
    if original.event_id != event_id || original.kind != kind || original.source != source
        || kind.is_empty() || source.is_empty()
    {
        return Err(shenko_types::error::ShenkoError::conflict("inbox event identity mismatch").into());
    }
    // The bare native T2 lane (kind=task_result, source=native-child, data =
    // TaskResult) is written by finalize_task internally. A complete
    // InboxEvent envelope can never be decoded by ready_native_results'
    // bare-TaskResult reader, so the public admission API refuses the kind
    // outright rather than publishing a row that breaks the dispatcher.
    if kind == "task_result" {
        return Err(shenko_types::error::ShenkoError::conflict(
            "task_result is reserved for the internal bare native-child T2 lane, not envelope admission").into());
    }
    Ok(())
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

    /// Latest appended receipt per operation, globally newest first. The
    /// cursor resumes strictly below (observed_at, receipt_id), with missing
    /// timestamps last. This bounds returned metadata, not SQLite's query
    /// work; it neither reads CAS payloads nor dispatches another attempt.
    pub fn external_observations_before(
        &self,
        names: &[&str],
        since: Option<chrono::DateTime<chrono::Utc>>,
        before: Option<(Option<chrono::DateTime<chrono::Utc>>, EntityId)>,
        limit: usize,
    ) -> Result<Vec<(OperationRecord, EvidenceReceipt)>> {
        if limit == 0 { return Ok(Vec::new()); }
        let time_key = |time: chrono::DateTime<chrono::Utc>| {
            time.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                .trim_end_matches('Z').to_owned()
        };
        let since = since.map(time_key);
        let before_id = before.as_ref().map(|(_, id)| id.to_string());
        let before_time = before.and_then(|(time, _)| time).map(time_key);
        let conn = self.ledger.conn.lock();
        // EvidenceReceipt serializes DateTime<Utc>: remove the UTC suffix,
        // then pad fractional seconds to nine digits before comparison.
        // Raw RFC3339 lexical order is wrong when fraction widths differ.
        let mut query = conn.prepare(
            "WITH latest AS (
                SELECT o.data AS operation_data, e.data AS receipt_data, e.receipt_id,
                    replace(replace(json_extract(e.data,'$.observed_at'),'Z',''),'+00:00','') AS observed
                FROM operations o JOIN evidence_receipts e ON e.operation_id=o.operation_id
                WHERE json_extract(o.data,'$.operation_name') IN (SELECT value FROM json_each(?1))
                  AND e.rowid=(SELECT rowid FROM evidence_receipts
                               WHERE operation_id=o.operation_id ORDER BY rowid DESC LIMIT 1)
             ), chronological AS (
                SELECT operation_data, receipt_data, receipt_id,
                    CASE WHEN observed IS NULL THEN NULL ELSE
                        substr(observed,1,19) || '.' || substr(
                            CASE WHEN substr(observed,20,1)='.' THEN substr(observed,21) ELSE '' END
                            || '000000000',1,9)
                    END AS observed_key
                FROM latest
             )
             SELECT operation_data, receipt_data FROM chronological
             WHERE (?2 IS NULL OR observed_key>=?2)
               AND (?3 IS NULL
                    OR (?4 IS NOT NULL AND (observed_key<?4
                        OR (observed_key=?4 AND receipt_id<?3) OR observed_key IS NULL))
                    OR (?4 IS NULL AND observed_key IS NULL AND receipt_id<?3))
             ORDER BY observed_key DESC, receipt_id DESC LIMIT ?5",
        )?;
        let rows = query.query_map(
            params![serde_json::to_string(names)?, since, before_id, before_time, i64::try_from(limit)?],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        rows.map(|row| {
            let (operation, receipt) = row?;
            Ok((serde_json::from_str(&operation)?, serde_json::from_str(&receipt)?))
        }).collect()
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
            |e: &anyhow::Error| e.is::<rusqlite::Error>() || e.is::<serde_json::Error>(),
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
        if task.kind == MODEL_CHILD_KIND {
            let read = |object: &shenko_types::object::ObjectRef| -> Result<serde_json::Value> {
                let bytes = self.cas.read_object(&object.digest)?;
                ensure!(bytes.len() as u64 == object.bytes
                    && shenko_types::object::Digest::blake3(&bytes) == object.digest,
                    "inference T1 input/binding closure mismatch");
                Ok(serde_json::from_slice(&bytes)?)
            };
            // The sealed model child contract this task is bound to. A child
            // the subject delegated carries the contract inside its frozen S1
            // decision; a nested or continued child carries the very same
            // contract shape as its own sealed task input. Both are bound to
            // the same T1 rules.
            let sealed: serde_json::Value = match task
                .memos
                .get(crate::child::NATIVE_INPUT)
                .and_then(|value| value.as_str())
            {
                Some("object") => read(&task.input_ref)?,
                _ => {
                    let decision = read(&task.input_ref)?;
                    let intent = decision.get("delegated_task_intents").and_then(|value| value.as_array())
                        .and_then(|intents| intents.iter().find(|intent| intent.get("intent_id") == Some(&serde_json::json!(task.task_id))))
                        .ok_or_else(|| anyhow::anyhow!("model inference has no frozen S1 delegation"))?;
                    intent.pointer("/definition/input").cloned()
                        .ok_or_else(|| anyhow::anyhow!("model child delegation has no frozen definition input"))?
                }
            };
            let sealed_digest = shenko_types::object::Digest::blake3(&serde_json::to_vec(&sealed)?);
            match op.operation_name.as_str() {
                MODEL_CHILD_STEP_OPERATION => {
                    let binding = inference.ok_or_else(|| anyhow::anyhow!("model inference must use quota-bound T1 admission"))?;
                    ensure!(binding.schema_version == 1, "unsupported inference send binding");
                    let step = read(&op.final_input_ref)?;
                    ensure!(op.input_digest == op.final_input_ref.digest,
                        "model child step input is not its own closure");
                    ensure!(op.input_schema_version == 2
                        && step.get("schema_version").and_then(|value| value.as_u64()) == Some(2),
                        "unsupported model child step input");
                    let plan = step.get("contribution_plan")
                        .ok_or_else(|| anyhow::anyhow!("model child step lacks its contribution plan"))?;
                    let sent: Vec<shenko_types::plugin_runtime::ContextContribution> =
                        serde::Deserialize::deserialize(&plan["contributions"])?;
                    let _: Vec<shenko_types::plugin_runtime::ContributionOmission> =
                        serde::Deserialize::deserialize(&plan["omitted"])?;
                    ensure!(plan["missing_required"].as_array().is_some_and(Vec::is_empty)
                        && sent.iter().all(|item| item.scope.scope_id == task.task_id
                            && item.scope.task_id == Some(task.task_id) && item.tokens <= item.budget_tokens)
                        && sent.iter().try_fold(0u64, |total, item| total.checked_add(item.tokens))
                            .is_some_and(|tokens| plan["total_tokens"].as_u64() == Some(tokens)),
                        "model child contribution scope/token evidence mismatch");
                    for item in &sent { read(&item.data)?; }
                    let sealed_digest_text = sealed_digest.to_string();
                    ensure!(step.get("contract_digest").and_then(|value| value.as_str())
                        == Some(sealed_digest_text.as_str()),
                        "model child step does not descend from the sealed delegation");
                    ensure!(step.get("request_id") == Some(&serde_json::to_value(binding.request_id)?)
                        && step.get("provider_binding") == sealed.get("provider_binding"),
                        "model child step request/transport differs from the sealed delegation");
                    ensure!(read(att.transport_ref.as_ref().ok_or_else(|| anyhow::anyhow!("inference attempt lacks selected send binding"))?)?
                        == serde_json::to_value(binding)?,
                        "inference T1 request/payload/selected-send mismatch");
                    if crate::quota::quota_gate_open(&tx)? {
                        let send_id = binding.send_id.ok_or_else(|| anyhow::anyhow!("configured quota requires exact selected send"))?;
                        ensure!(crate::quota::quota_claim_send(&tx, binding.request_id, send_id, seq)?,
                            "inference selected send is not admitted, armed and unclaimed for this request");
                    } else {
                        ensure!(binding.send_id.is_none(), "unexpected quota send without a recorded policy");
                    }
                }
                MODEL_CHILD_RESULT_OPERATION => {
                    ensure!(inference.is_none(), "the model child run operation is not a provider send");
                    ensure!(op.input_digest == op.final_input_ref.digest
                        && op.final_input_ref.digest == sealed_digest,
                        "model child run operation does not bind the sealed delegation contract");
                }
                other => {
                    anyhow::bail!("unsupported model child operation: {other}");
                }
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

    /// Durably admit one subject inbox event from an external producer.
    ///
    /// This is the pending durable row the subject drains each step; the
    /// caller's separate volatile wake (try_send) is not the admission. A
    /// replay of the same event_id with the same facts returns
    /// [`InboxAdmission::Replay`]; a different fact under the same event_id
    /// is a conflict, matching raw-session rules. The subject head must
    /// exist so the event has a real drain owner.
    pub fn admit_inbox_event(
        &self,
        event_id: EntityId,
        agent_id: EntityId,
        kind: &str,
        source: &str,
        data: &serde_json::Value,
    ) -> Result<InboxAdmission> {
        // Typed validation BEFORE the mutation gate: an invalid admission must
        // fail before the writer incarnation is claimed, never poison it.
        validate_original_event(event_id, kind, source, data)?;
        let serialized = serde_json::to_string(data)?;
        // Target agent head must exist for the event to have a drain owner;
        // a missing head is a typed rejection, not a row failure inside the
        // writer transaction.
        {
            let conn = self.ledger.conn.lock();
            let head_exists: Option<String> = conn
                .query_row(
                    "SELECT data FROM subject_branch_heads WHERE agent_id=?1",
                    [agent_id.to_string()],
                    |r| r.get(0),
                )
                .optional()?;
            ensure!(head_exists.is_some(), "subject head missing for inbox event agent");
        }
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let existing: Option<(String, String, String, String)> = tx
                    .query_row(
                        "SELECT agent_id, kind, source, data FROM inbox_events WHERE event_id=?1",
                        [event_id.to_string()],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()?;
                if let Some((stored_agent, stored_kind, stored_source, stored_data)) = existing {
                    if stored_agent != agent_id.to_string() || stored_kind != kind
                        || stored_source != source || stored_data != serialized
                    {
                        return Err(shenko_types::error::ShenkoError::conflict(
                            "inbox_event_conflict: same event_id with different facts").into());
                    }
                    tx.commit()?;
                    return Ok(InboxAdmission::Replay);
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                tx.execute(
                    "INSERT INTO inbox_events(event_id,agent_id,kind,source,consumed,data)
                     VALUES(?1,?2,?3,?4,0,?5)",
                    params![
                        event_id.to_string(),
                        agent_id.to_string(),
                        kind,
                        source,
                        serialized,
                    ],
                )?;
                tx.execute(
                    "INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count) VALUES(?1,?2,?3,1)",
                    params![
                        commit.ledger_id.to_string(),
                        commit.seq.0 as i64,
                        self.capabilities.incarnation.to_string()
                    ],
                )?;
                tx.commit()?;
                Ok(InboxAdmission::Inserted)
            },
            |e: &anyhow::Error| e.is::<rusqlite::Error>() || e.is::<serde_json::Error>(),
        )
    }

    /// Evidence may arrive after cancellation/epoch change. This method does
    /// not advance a task, create children, or grant the old callback authority.
    ///
    /// A receipt is admitted once (SPEC X04): the identical fact again is a
    /// replay that returns the durable projection and writes nothing, while a
    /// different original under the same receipt identity or the same
    /// `(source_id, source_instance_id, source_event_key)` scope is refused as
    /// `RECEIPT_KEY_CONFLICT` and preserved as its own conflict fact. Neither
    /// answer re-folds the operation, duplicates its evidence or closes the
    /// single writer.
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
            |e: &anyhow::Error| e.is::<rusqlite::Error>() || e.is::<serde_json::Error>(),
        )
    }

    /// The provider's answer of one bounded intervention attempt becomes
    /// durable evidence on its own.
    ///
    /// An answer is not yet a decision: the policy still has to read it.
    /// Until the decision commits, nothing may claim that the intervention
    /// succeeded, so the operation stays unresolved and no settlement path
    /// can complete the task from an answer alone.
    pub fn record_intervention_provider_evidence(
        &self,
        operation_id: EntityId,
        attempt_id: EntityId,
        ev: EvidenceReceipt,
    ) -> Result<OperationRecord> {
        self.ledger.with_mutation(
            || self.try_record_intervention_provider_evidence(operation_id, attempt_id, ev),
            |e: &anyhow::Error| e.is::<rusqlite::Error>() || e.is::<serde_json::Error>(),
        )
    }

    /// The decision of one bounded intervention attempt and the settlement
    /// that accepts it commit together.
    ///
    /// The decision IS the effect of this operation, so a successful
    /// intervention and its durable decision are one fact: neither can exist
    /// without the other. A bounded answer that carries no usable decision
    /// settles as a failure and never as a fabricated silence.
    pub fn record_intervention_settlement(
        &self,
        operation_id: EntityId,
        attempt_id: EntityId,
        ev: EvidenceReceipt,
        outcome: OperationOutcome,
        effect: EffectStatus,
        decision: Option<&InterventionReminder>,
    ) -> Result<OperationRecord> {
        ensure!(
            matches!(outcome, OperationOutcome::Succeeded) == decision.is_some(),
            "an intervention outcome and its decision must agree"
        );
        ensure!(
            decision.is_none() || effect == EffectStatus::Complete,
            "a recorded intervention decision is a complete effect"
        );
        self.ledger.with_mutation(
            || {
                self.try_record_intervention_settlement(
                    operation_id, attempt_id, ev, outcome, effect, decision,
                )
            },
            |e: &anyhow::Error| e.is::<rusqlite::Error>() || e.is::<serde_json::Error>()
                || e.downcast_ref::<shenko_types::error::ShenkoError>()
                    .is_some_and(crate::research::research_storage_failure),
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
        let (mut op, mut att) =
            Self::load_external_records(&tx, operation_id, attempt_id, &ev)?;
        match crate::receipt_admission::decide(&tx, &ev)? {
            crate::receipt_admission::Admission::New => {}
            // The same fact is already durable: the ledger's own projection is
            // the answer, not a second commit. It may only be confirmed — a
            // different settlement for one fact is a divergence, and the
            // durable projection is never rewritten to match it.
            crate::receipt_admission::Admission::Replay { .. } => {
                if let Some(observed) = outcome {
                    ensure!(
                        op.outcome == Some(observed) && op.effect_status == effect,
                        "receipt replay carries a different settlement than the durable fact"
                    );
                }
                return Ok(op);
            }
            crate::receipt_admission::Admission::Conflict(conflict) => {
                let refusal =
                    crate::receipt_admission::refuse(&self.ledger, tx, &ev, conflict)?;
                return Err(refusal.into());
            }
        }
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
        // An operation that just became (or stayed) reconciling has a durable
        // reconciliation case: the uncertainty is a fact with its own
        // supervisor and evidence, not a property of the caller's timeout.
        // Nothing is resent; the case is what a later receipt or an explicit
        // subject decision acts on (SPEC §41.4 X11).
        if op.state == OperationState::Reconciling {
            let reason = if op.outcome_conflict {
                crate::reconcile::REASON_CONTRADICTORY_EVIDENCE
            } else {
                crate::reconcile::REASON_EFFECT_UNKNOWN
            };
            self.ensure_unknown_effect_case(&tx, &op, ev.receipt_id, reason, commit)?;
        } else if op.state == OperationState::Resolved
            && op.effect_status != EffectStatus::Unknown
        {
            // Definite evidence arrived after an earlier unknown: the case
            // becomes resolved, and the original unknown receipts are still
            // there. This never rewrites the case row's earlier state in
            // place — the row keeps its id and gains the new reference.
            self.resolve_unknown_effect_cases(&tx, operation_id, ev.receipt_id, commit)?;
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
            "INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count) VALUES(?1,?2,?3,?4)",
            params![
                commit.ledger_id.to_string(),
                seq.0 as i64,
                self.capabilities.incarnation.to_string(),
                3
            ],
        )?;
        tx.commit()?;
        Ok(op)
    }
    /// The agent one durable task row records, or None when the row is gone.
    fn task_agent(tx: &Transaction<'_>, task_id: EntityId) -> Result<Option<String>> {
        Ok(tx
            .query_row(
                "SELECT json_extract(data,'$.agent_id') FROM tasks WHERE task_id=?1",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// The operation and the attempt one receipt belongs to.
    fn load_external_records(
        tx: &Transaction<'_>,
        operation_id: EntityId,
        attempt_id: EntityId,
        ev: &EvidenceReceipt,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let data: String = tx.query_row(
            "SELECT data FROM operations WHERE operation_id=?1",
            [operation_id.to_string()],
            |r| r.get(0),
        )?;
        let op: OperationRecord = serde_json::from_str(&data)?;
        let data: String = tx.query_row(
            "SELECT data FROM attempts WHERE attempt_id=?1",
            [attempt_id.to_string()],
            |r| r.get(0),
        )?;
        let att: AttemptRecord = serde_json::from_str(&data)?;
        ensure!(
            att.operation_id == operation_id
                && ev.operation_id == Some(operation_id)
                && ev.attempt_id == Some(attempt_id),
            "receipt_identity_mismatch"
        );
        Ok((op, att))
    }

    /// Read one sealed object and parse it as JSON.
    fn read_sealed_json(
        &self,
        reference: &shenko_types::object::ObjectRef,
    ) -> Result<serde_json::Value> {
        let bytes = self.cas.read_object(&reference.digest)?;
        ensure!(
            bytes.len() as u64 == reference.bytes
                && shenko_types::object::Digest::blake3(&bytes) == reference.digest,
            "sealed intervention evidence closure mismatch"
        );
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// The provider's answer of one bounded attempt becomes durable, and the
    /// operation stays unresolved until its decision commits.
    fn try_record_intervention_provider_evidence(
        &self,
        operation_id: EntityId,
        attempt_id: EntityId,
        mut ev: EvidenceReceipt,
    ) -> Result<OperationRecord> {
        let mut c = self.ledger.conn.lock();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut op, mut att) =
            Self::load_external_records(&tx, operation_id, attempt_id, &ev)?;
        match crate::receipt_admission::decide(&tx, &ev)? {
            crate::receipt_admission::Admission::New => {}
            // The answer is already durable; replaying it writes nothing and
            // never promotes it to a decision.
            crate::receipt_admission::Admission::Replay { .. } => return Ok(op),
            crate::receipt_admission::Admission::Conflict(conflict) => {
                let refusal =
                    crate::receipt_admission::refuse(&self.ledger, tx, &ev, conflict)?;
                return Err(refusal.into());
            }
        }
        ensure!(
            op.outcome.is_none() && !op.outcome_conflict,
            "the intervention operation was already settled"
        );
        let seq = self.ledger.allocate_seq(&tx)?;
        let commit = CommitRef::new(self.ledger.ledger_id(), seq);
        ev.received_commit = commit;
        // The answer is the original fact; the decision is not made yet, so
        // the operation keeps no outcome and no effect.
        op.record_revision = RecordRevision(op.record_revision.0 + 1);
        op.evidence_through = Some(commit);
        att.dispatch_stage = AttemptDispatchStage::Ended;
        att.final_receipt_id = Some(ev.receipt_id);
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
            "INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count) VALUES(?1,?2,?3,?4)",
            params![
                commit.ledger_id.to_string(),
                seq.0 as i64,
                self.capabilities.incarnation.to_string(),
                3
            ],
        )?;
        tx.commit()?;
        Ok(op)
    }

    /// The decision of one bounded attempt and the settlement that accepts it
    /// commit together: the decision row becomes visible in the same
    /// transaction that resolves the operation.
    fn try_record_intervention_settlement(
        &self,
        operation_id: EntityId,
        attempt_id: EntityId,
        mut ev: EvidenceReceipt,
        outcome: OperationOutcome,
        effect: EffectStatus,
        decision: Option<&InterventionReminder>,
    ) -> Result<OperationRecord> {
        let mut c = self.ledger.conn.lock();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut op, mut att) =
            Self::load_external_records(&tx, operation_id, attempt_id, &ev)?;
        if let Some(reminder) = decision {
            // A decision belongs to one research epoch, and an epoch that
            // records its owning task belongs to that task's agent. Settling
            // another agent's operation with it would publish one agent's
            // decision into another agent's ledger, so the durable boundary
            // refuses the settlement before any evidence, outcome or decision
            // row is written — including on a replay, which must not launder
            // a mismatched decision.
            //
            // An epoch that records NO owning task (the column is nullable and
            // some lineages are opened without one) makes no agent claim, so
            // there is nothing to contradict; the operation's own frozen
            // request still binds that epoch to this task.
            let epoch_task: Option<Option<String>> = tx
                .query_row(
                    "SELECT task_id FROM research_epochs WHERE epoch_id=?1",
                    [reminder.epoch_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            ensure!(
                epoch_task.is_some(),
                "intervention decision names no research epoch"
            );
            if let Some(task_id) = epoch_task.flatten() {
                let epoch_agent = Self::task_agent(&tx, task_id.parse::<EntityId>()?)?;
                ensure!(
                    epoch_agent.is_some()
                        && epoch_agent == Self::task_agent(&tx, op.task_id)?,
                    "intervention decision epoch belongs to a different agent than the settled task"
                );
            }
        }
        match crate::receipt_admission::decide(&tx, &ev)? {
            crate::receipt_admission::Admission::New => {}
            crate::receipt_admission::Admission::Replay { .. } => {
                // The settlement is already durable. A replay may only confirm
                // it: a different outcome or a decision this settlement never
                // committed is a divergence, not a second settlement.
                ensure!(
                    op.outcome == Some(outcome) && op.effect_status == effect,
                    "intervention replay carries a different settlement"
                );
                if let Some(reminder) = decision {
                    ensure!(
                        !Self::insert_intervention(&tx, reminder).map_err(anyhow::Error::from)?,
                        "intervention replay carries a decision this settlement never committed"
                    );
                }
                return Ok(op);
            }
            crate::receipt_admission::Admission::Conflict(conflict) => {
                let refusal =
                    crate::receipt_admission::refuse(&self.ledger, tx, &ev, conflict)?;
                return Err(refusal.into());
            }
        }
        ensure!(
            op.outcome.is_none() && !op.outcome_conflict,
            "the intervention operation was already settled"
        );
        if let Some(reminder) = decision {
            // The decision must be the decision the frozen request and the
            // original evidence describe.
            let frozen = self.read_sealed_json(&op.final_input_ref)?;
            for (field, expected) in [
                ("task_id", op.task_id),
                ("request_id", reminder.reminder_id),
                ("epoch_id", reminder.epoch_id),
                ("lineage_id", reminder.lineage_id),
            ] {
                ensure!(
                    frozen[field].as_str().map(str::parse::<EntityId>).transpose()? == Some(expected),
                    "intervention decision differs from frozen {field}"
                );
            }
            ensure!(
                frozen["policy_name"].as_str() == Some(reminder.policy.as_str())
                    && frozen["policy_revision"].as_str() == Some(reminder.policy_revision.as_str()),
                "intervention decision policy differs from frozen request"
            );
            let normalized = self.read_sealed_json(
                ev.normalized_ref.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("intervention decision lacks normalized evidence")
                })?,
            )?;
            let decision_ref: shenko_types::object::ObjectRef =
                serde::Deserialize::deserialize(&normalized["decision_ref"])?;
            ensure!(normalized["schema_version"] == 2
                && normalized["task_id"] == serde_json::to_value(op.task_id)?
                && normalized["request_id"] == serde_json::to_value(reminder.reminder_id)?
                && normalized["reminder_id"] == serde_json::to_value(reminder.reminder_id)?
                && normalized["decision"] == serde_json::to_value(reminder.decision)?
                && normalized["scope"] == reminder.scope
                && self.read_sealed_json(&decision_ref)? == serde_json::to_value(reminder)?,
                "intervention decision differs from its original evidence"
            );
            ensure!(
                reminder.used_step_id.is_none() && reminder.used_at_ms.is_none(),
                "intervention acceptance cannot fabricate consumption"
            );
        }
        let seq = self.ledger.allocate_seq(&tx)?;
        let commit = CommitRef::new(self.ledger.ledger_id(), seq);
        ev.received_commit = commit;
        let decision_inserted = match decision {
            Some(reminder) => {
                Self::insert_intervention(&tx, reminder).map_err(anyhow::Error::from)?
            }
            None => false,
        };
        op.state = OperationState::Resolved;
        op.outcome = Some(outcome);
        op.effect_status = effect;
        op.record_revision = RecordRevision(op.record_revision.0 + 1);
        op.evidence_through = Some(commit);
        att.dispatch_stage = AttemptDispatchStage::Ended;
        att.final_receipt_id = Some(ev.receipt_id);
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
            "INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count) VALUES(?1,?2,?3,?4)",
            params![
                commit.ledger_id.to_string(),
                seq.0 as i64,
                self.capabilities.incarnation.to_string(),
                3 + i64::from(decision_inserted)
            ],
        )?;
        tx.commit()?;
        Ok(op)
    }
    /// Every operation of one task under a given operation name, in commit
    /// order. A finite model child uses it to rebuild its own step history
    /// from the ledger after a restart, without a second index.
    pub fn external_task_operations(
        &self,
        task_id: EntityId,
        operation_name: &str,
    ) -> Result<Vec<OperationRecord>> {
        let c = self.ledger.conn.lock();
        let mut q = c.prepare("SELECT data FROM operations WHERE json_extract(data,'$.task_id')=?1 AND json_extract(data,'$.operation_name')=?2 ORDER BY rowid")?;
        let rows = q
            .query_map(params![task_id.to_string(), operation_name], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|data| serde_json::from_str(&data).map_err(Into::into))
            .collect()
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
