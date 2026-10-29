//! Durable finite native-child dispatch on the existing task/operation ledger.
//! No second task platform: S1 owns intent, this module owns CAS transitions,
//! and native handlers own real execution plus E1 evidence.
use crate::Store;
use anyhow::{Context as _, Result, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    commit::CommitRef,
    evidence::EvidenceReceipt,
    ids::*,
    object::{Digest, ObjectRef},
    operation::*,
    subject::{SubjectBranchHead, SubjectMode},
    task::*,
};
use std::collections::{HashMap, HashSet};

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS task_results(
 result_id TEXT PRIMARY KEY, task_id TEXT NOT NULL UNIQUE, agent_id TEXT NOT NULL,
 kind TEXT NOT NULL, data TEXT NOT NULL, commit_seq INTEGER NOT NULL,
 created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS task_results_agent ON task_results(agent_id,commit_seq,result_id);
CREATE INDEX IF NOT EXISTS inbox_task_result_ready ON inbox_events(agent_id,kind,consumed,event_id);
"#;

pub const NATIVE_INPUT: &str = "native_input";
pub const NATIVE_CHILD: &str = "native_child";
pub const NATIVE_OPERATION_ID: &str = "native_operation_id";
pub const NATIVE_RESULT_ID: &str = "native_result_id";
pub const NATIVE_RECOVERY: &str = "native_recovery";
pub const NATIVE_PARK: &str = "native_park";
pub const NATIVE_COMPLETION: &str = "native_completion";
pub const NATIVE_ASSIGNMENT: &str = "native_execution_assignment";
pub const NATIVE_SETTLEMENT_ONLY: &str = "native_settlement_only";
pub const COGNITION_IMPORTED: &str = "cognition_imported";

#[derive(Debug, Clone)]
pub struct NativeTaskClaim {
    pub task: TaskRecord,
    pub invocation: crate::external::TaskInvocation,
    pub operation_id: EntityId,
    pub result_id: EntityId,
    pub software_generation: RuntimeGeneration,
    pub settlement_only: bool,
}

#[derive(Debug, Clone, Default)]
pub struct TaskCancellation {
    pub cancelled: Vec<EntityId>,
    pub already_terminal: Vec<EntityId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompletionMemo {
    operation_id: EntityId,
    receipt_id: EntityId,
    invocation_id: EntityId,
    outcome: TaskOutcome,
    result: Option<serde_json::Value>,
    evidence_ref: ObjectRef,
}

pub fn memo_uuid(task: &TaskRecord, key: &str) -> Option<EntityId> {
    task.memos
        .get(key)
        .and_then(serde_json::Value::as_str)
        .and_then(|value| value.parse().ok())
}

pub fn memo_flag(task: &TaskRecord, key: &str) -> bool {
    task.memos
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

pub fn park_reason(task: &TaskRecord) -> Option<&str> {
    task.memos.get(NATIVE_PARK).and_then(|value| value.as_str())
}

impl Store {
    /// Recover only finite native work. Old unbound running delegations are
    /// quarantined as pending/parked; their raw input and effects stay intact.
    pub fn recover_native_tasks(&self) -> Result<usize> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                self.check_dispatch_owner(&tx)?;
                let mut changed = 0;
                let mut commit = None;
                let mut after = String::new();
                loop {
                    let tasks = read_recovery_candidates_tx(&tx, &after)?;
                    if tasks.is_empty() {
                        break;
                    }
                    for (key, mut task) in tasks {
                        ensure!(task.task_id.to_string() == key, "recovery task identity mismatch");
                        after = key;
                        let commit_ref = match commit {
                            Some(commit_ref) => commit_ref,
                            None => {
                                let seq = self.ledger.allocate_seq(&tx)?;
                                let commit_ref = CommitRef::new(self.ledger.ledger_id(), seq);
                                commit = Some(commit_ref);
                                commit_ref
                            }
                        };
                        let native = memo_flag(&task, NATIVE_CHILD);
                        let checkpoint = pending_checkpoint(&task.state);
                        task.state = TaskState::Pending {
                            checkpoint_ref: checkpoint,
                        };
                        task.record_revision = task.record_revision.next();
                        if native {
                            task.memos.insert(NATIVE_RECOVERY.into(), true.into());
                            task.dispatch_eligibility = if task.abort_request.is_some() {
                                parked(task.task_id, "cancel_requested", None)
                            } else {
                                DispatchEligibility::Ready
                            };
                        } else {
                            task.memos.insert(
                                NATIVE_PARK.into(),
                                serde_json::Value::String("legacy_unbound".into()),
                            );
                            task.dispatch_eligibility = parked(task.task_id, "legacy_unbound", None);
                        }
                        task.updated_commit = commit_ref;
                        write_task(&tx, &task)?;
                        changed += 1;
                    }
                }
                let Some(commit) = commit else { return Ok(0) };
                child_log(&tx, self, commit, changed as i64)?;
                tx.commit()?;
                Ok(changed)
            },
            storage_failure,
        )
    }

    /// Read one durable task record without claiming it. Returns `None` for
    /// an unknown id instead of a storage error.
    pub fn get_task(&self, id: EntityId) -> Result<Option<TaskRecord>> {
        let conn = self.ledger.conn.lock();
        read_task(&conn, id)
    }

    /// One bounded page of dispatchable work strictly after `after`, ordered by
    /// the same stable key as recovery. `after` is a scan position, not a
    /// completion watermark: callers wrap to the beginning when a page is short.
    pub fn native_task_candidates_after(
        &self,
        after: Option<(&str, EntityId)>,
        limit: usize,
    ) -> Result<Vec<(String, TaskRecord)>> {
        let conn = self.ledger.conn.lock();
        read_candidates_tx(&conn, after, limit)
    }

    /// Last dispatchable candidate in scan order. A dispatcher records it when
    /// a round starts so continuously arriving later work cannot postpone the
    /// wrap past older tasks that become eligible meanwhile.
    pub fn native_task_scan_upper(&self) -> Result<Option<(String, EntityId)>> {
        let conn = self.ledger.conn.lock();
        read_scan_upper_tx(&conn)
    }

    /// Park is a durable, diagnosable non-terminal state. A missing definition,
    /// handler failure, cancellation race or unknown effect never becomes success.
    pub fn park_native_task(
        &self,
        task_id: EntityId,
        expected_revision: RecordRevision,
        reason: &str,
        detail: &str,
        retryable: bool,
    ) -> Result<bool> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                self.check_dispatch_owner(&tx)?;
                let Some(mut task) = read_task(&tx, task_id)? else {
                    return Ok(false);
                };
                if task.is_terminal()
                    || matches!(task.state, TaskState::Completing { .. })
                    || task.record_revision != expected_revision
                {
                    return Ok(false);
                }
                if park_reason(&task) == Some(reason)
                    && matches!(
                        task.dispatch_eligibility,
                        DispatchEligibility::Parked { .. }
                    )
                    && matches!(task.state, TaskState::Pending { .. })
                {
                    return Ok(false);
                }
                task.state = TaskState::Pending {
                    checkpoint_ref: pending_checkpoint(&task.state),
                };
                task.record_revision = task.record_revision.next();
                task.memos
                    .insert(NATIVE_PARK.into(), serde_json::Value::String(reason.into()));
                let detail: String = detail.chars().take(600).collect();
                task.memos.insert(
                    "native_diagnostic".into(),
                    serde_json::json!({"reason":reason,"detail":detail}),
                );
                if memo_uuid(&task, NATIVE_OPERATION_ID).is_some() {
                    task.memos.insert(NATIVE_RECOVERY.into(), true.into());
                }
                let until = retryable.then(|| chrono::Utc::now() + chrono::Duration::seconds(1));
                task.dispatch_eligibility = parked(task_id, reason, until);
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                task.updated_commit = commit;
                write_task(&tx, &task)?;
                child_log(&tx, self, commit, 1)?;
                tx.commit()?;
                Ok(true)
            },
            storage_failure,
        )
    }

    /// Atomically bind a registered implementation and create a fresh invocation.
    /// Recovery settlement gets no T1 authority; a missing armed record is the
    /// only case where the same stable operation identity may be admitted again.
    pub fn claim_native_task(
        &self,
        task_id: EntityId,
        implementation: ImplementationRef,
        settlement_only: bool,
    ) -> Result<Option<NativeTaskClaim>> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                self.check_dispatch_owner(&tx)?;
                let Some(mut task) = read_task(&tx, task_id)? else {
                    return Ok(None);
                };
                if task.memos.contains_key(COGNITION_IMPORTED) {
                    // Replication is not a continuation decision. This fence
                    // covers fresh execution and settlement/reconciliation.
                    return Ok(None);
                }
                if task.is_terminal()
                    || task.abort_request.is_some()
                    || !matches!(task.state, TaskState::Pending { .. })
                    || !task.memos.contains_key(NATIVE_INPUT)
                {
                    return Ok(None);
                }
                if memo_flag(&task, NATIVE_CHILD) {
                    ensure!(
                        task.implementation_ref.plugin_id == implementation.plugin_id
                            && task.implementation_ref.artifact_digest
                                == implementation.artifact_digest,
                        "native_implementation_unavailable"
                    );
                }
                let bound_implementation = if memo_flag(&task, NATIVE_CHILD) {
                    task.implementation_ref.clone()
                } else {
                    implementation.clone()
                };
                let operation_id =
                    memo_uuid(&task, NATIVE_OPERATION_ID).unwrap_or_else(EntityId::new);
                let result_id = memo_uuid(&task, NATIVE_RESULT_ID).unwrap_or_else(EntityId::new);
                let operation_exists = operation_exists(&tx, operation_id)?;
                let recovery = memo_flag(&task, NATIVE_RECOVERY);
                if settlement_only {
                    ensure!(
                        recovery && operation_exists,
                        "settlement_requires_original_operation"
                    );
                } else {
                    let eligible = match &task.dispatch_eligibility {
                        DispatchEligibility::Ready => true,
                        DispatchEligibility::Parked { .. } => matches!(
                            park_reason(&task),
                            Some("handler_unavailable" | "reconciling")
                        ),
                        DispatchEligibility::Blocked { .. } => false,
                    };
                    if !eligible || (recovery && operation_exists) {
                        return Ok(None);
                    }
                }
                let head = read_head(&tx, task.agent_id)?;
                // A subject-only pause closes new claims, not evidence from
                // work that was admitted before the boundary.
                if head.mode != SubjectMode::Active {
                    return Ok(None);
                }
                ensure_owner_active(&tx, &task, &head)?;
                if task.owner
                    != (TaskOwner::Conversation {
                        id: task.conversation_id,
                    })
                {
                    ensure!(
                        !task.background,
                        "background_task_must_be_conversation_owned"
                    );
                }

                let old_revision = task.record_revision;
                let invocation_id = EntityId::new();
                let checkpoint = pending_checkpoint(&task.state);
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let invocation = crate::external::TaskInvocation {
                    task_id,
                    task_revision: task.task_revision,
                    record_revision: old_revision.next(),
                    invocation_id,
                    epoch: head.controller_epoch,
                    worker_id: self.capabilities.incarnation,
                    node_id: self.ledger.ledger_id(),
                };
                task.state = TaskState::Running {
                    checkpoint_ref: checkpoint,
                    invocation_id,
                    mode: RunMode::Run,
                    epoch: head.controller_epoch,
                };
                task.record_revision = old_revision.next();
                task.updated_commit = commit;
                task.first_run_at.get_or_insert_with(chrono::Utc::now);
                task.implementation_ref = bound_implementation.clone();
                task.dispatch_eligibility = DispatchEligibility::Ready;
                task.memos.insert(NATIVE_CHILD.into(), true.into());
                task.memos.insert(
                    NATIVE_OPERATION_ID.into(),
                    serde_json::Value::String(operation_id.to_string()),
                );
                task.memos.insert(
                    NATIVE_RESULT_ID.into(),
                    serde_json::Value::String(result_id.to_string()),
                );
                task.memos.remove(NATIVE_PARK);
                task.memos.remove("native_diagnostic");
                let assignment = ExecutionAssignment {
                    worker_id: invocation.worker_id,
                    node_id: invocation.node_id,
                    invocation_id,
                    epoch: invocation.epoch,
                    implementation_ref: bound_implementation,
                    assigned_commit: commit,
                };
                task.memos
                    .insert(NATIVE_ASSIGNMENT.into(), serde_json::to_value(&assignment)?);
                if settlement_only {
                    task.memos
                        .insert(NATIVE_SETTLEMENT_ONLY.into(), true.into());
                } else {
                    task.memos.remove(NATIVE_SETTLEMENT_ONLY);
                }
                write_task(&tx, &task)?;
                child_log(&tx, self, commit, 1)?;
                tx.commit()?;
                Ok(Some(NativeTaskClaim {
                    task,
                    invocation,
                    operation_id,
                    result_id,
                    software_generation: head.software_generation,
                    settlement_only,
                }))
            },
            storage_failure,
        )
    }

    /// Admit a fresh native task durably outside a subject S1 decision.
    ///
    /// This is the same task ledger dispatch already scans — no second
    /// queue. Callers that produce deterministic work ids (Playground
    /// backend actions keyed by (artifact_id, request_id)) get either the
    /// inserted task or the stored task back; dedup/conflict semantics on
    /// the stored input belong to the caller, which reads the returned
    /// record. The frozen input object must already be sealed in CAS, the
    /// owning subject head must exist, and the record must be a Pending,
    /// Ready native task. The input event commits in the SAME transaction,
    /// before any accepted response. Claims still require the dispatcher
    /// owner; admission alone never authorizes a guest/process effect.
    /// Returns `Some(existing)` when the id is already committed with the same
    /// sealed input; a committed id never loses to a later binding or handler
    /// availability check. A committed id with a DIFFERENT sealed input is a
    /// conflict — deterministic task ids must never admit divergent inputs.
    /// Typed validation (record shape, canonical event JSON, CAS digest,
    /// target head existence) runs BEFORE the mutation gate so an invalid
    /// admission can never poison the writer incarnation.
    pub fn admit_native_task(&self, task: &TaskRecord, input_event: &shenko_types::store::RawInboxEvent) -> Result<Option<TaskRecord>> {
        ensure!(
            matches!(task.state, TaskState::Pending { .. })
                && matches!(task.dispatch_eligibility, DispatchEligibility::Ready)
                && task.abort_request.is_none()
                && task.record_revision == RecordRevision(0)
                && task.task_revision == TaskRevision(0)
                && task.memos.contains_key(NATIVE_INPUT)
                && task.memos.contains_key("intent_id")
                && !task.memos.contains_key(NATIVE_CHILD)
                && input_event.agent_id == task.agent_id
                && !input_event.kind.is_empty()
                && !input_event.source.is_empty(),
            "native_task_not_admissible"
        );
        // Canonical event JSON is validated BEFORE the writer is claimed:
        // malformed payload_json would otherwise abort the mutation closure
        // inside the transaction and be misclassified as storage failure.
        let event: serde_json::Value = serde_json::from_str(&input_event.payload_json)
            .map_err(|_| shenko_types::error::ShenkoError::conflict(
                "native admission input payload must be a complete canonical event JSON"))?;
        crate::external::validate_original_event(
            input_event.event_id, &input_event.kind, &input_event.source, &event)?;
        // The frozen input object is part of admission: a task whose CAS
        // input cannot be resolved would die parked as input_unavailable.
        // Digest and declared length are verified BEFORE the object is ever
        // trusted (read_object already refuses digest path traversal).
        let bytes = self.cas.read_object(&task.input_ref.digest)?;
        ensure!(
            bytes.len() as u64 == task.input_ref.bytes
                && Digest::blake3(&bytes) == task.input_ref.digest,
            "native_input_object_mismatch"
        );
        // Target subject head existence is validated on the read path too:
        // missing head is a typed rejection, not a transaction failure.
        {
            let conn = self.ledger.conn.lock();
            read_head(&conn, task.agent_id)?;
            let event_exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM inbox_events WHERE event_id=?1)",
                [input_event.event_id.to_string()], |row| row.get(0),
            )?;
            if event_exists && read_task(&conn, task.task_id)?.is_none() {
                return Err(shenko_types::error::ShenkoError::conflict(
                    "native admission event identity already committed").into());
            }
        }
        self.ledger.with_mutation(|| self.try_admit_native_task(task, input_event), storage_failure)
    }

    fn try_admit_native_task(&self, task: &TaskRecord, input_event: &shenko_types::store::RawInboxEvent) -> Result<Option<TaskRecord>> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = read_task(&tx, task.task_id)? {
            // The committed id wins unconditionally: identical sealed input is
            // a replay of the original admission, a different one is a
            // cross-node/payload conflict — never a silent false ACK.
            if existing.input_ref.digest != task.input_ref.digest
                || existing.input_ref.bytes != task.input_ref.bytes
                || existing.agent_id != task.agent_id
            {
                return Err(shenko_types::error::ShenkoError::conflict(
                    "native task identity already committed with different input").into());
            }
            tx.commit()?;
            return Ok(Some(existing));
        }
        // The subject head must exist or this task would be undispatchable
        // forever: claims resolve the owner chain against the live head.
        read_head(&tx, task.agent_id)?;
        // Repeat under BEGIN IMMEDIATE: a competing event admission may have
        // won since preflight. A reused caller identity is not disk failure
        // and MUST NOT close the writer incarnation.
        let event_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM inbox_events WHERE event_id=?1)",
            [input_event.event_id.to_string()], |row| row.get(0),
        )?;
        if event_exists {
            return Err(shenko_types::error::ShenkoError::conflict(
                "native admission event identity already committed").into());
        }
        let seq = self.ledger.allocate_seq(&tx)?;
        let commit = CommitRef::new(self.ledger.ledger_id(), seq);
        let mut admitted = task.clone();
        admitted.created_commit = commit;
        admitted.updated_commit = commit;
        tx.execute(
            "INSERT INTO tasks(task_id,record_revision,data,updated_commit_seq) VALUES(?1,?2,?3,?4)",
            params![
                admitted.task_id.to_string(),
                admitted.record_revision.0 as i64,
                serde_json::to_string(&admitted)?,
                seq.0 as i64,
            ],
        )?;
        tx.execute(
            "INSERT INTO inbox_events(event_id,agent_id,kind,source,consumed,data) VALUES(?1,?2,?3,?4,0,?5)",
            params![input_event.event_id.to_string(), input_event.agent_id.to_string(),
                input_event.kind, input_event.source, input_event.payload_json],
        )?;
        child_log(&tx, self, commit, 2)?;
        tx.commit()?;
        Ok(None)
    }

    /// Durable cancellation fences the old task revision before any signal.
    /// Late E1 remains legal, but it cannot settle the cancelled task.
    pub fn request_native_cancellation(
        &self,
        task_id: EntityId,
        cancel_id: EntityId,
        decision_ref: EntityId,
        reason: &str,
        scope: AbortScope,
    ) -> Result<TaskCancellation> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                self.check_dispatch_owner(&tx)?;
                let all = read_all_tasks(&tx)?;
                let by_id: HashMap<_, _> = all
                    .iter()
                    .map(|task| (task.task_id, task.clone()))
                    .collect();
                ensure!(by_id.contains_key(&task_id), "task_not_found");
                let mut summary = TaskCancellation::default();
                let mut selected = Vec::new();
                for task in all {
                    if !in_cancel_scope(&by_id, task.task_id, task_id, scope) {
                        continue;
                    }
                    if task.is_terminal() {
                        summary.already_terminal.push(task.task_id);
                        continue;
                    }
                    if task.abort_request.is_some() {
                        continue;
                    }
                    selected.push(task);
                }
                if selected.is_empty() {
                    return Ok(summary);
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                for mut task in selected {
                    task.task_revision = task.task_revision.next();
                    task.record_revision = task.record_revision.next();
                    task.abort_request = Some(AbortRequest {
                        cancel_id,
                        decision_ref,
                        requested_commit: commit,
                        reason: reason.into(),
                        scope,
                    });
                    task.memos.insert(
                        NATIVE_PARK.into(),
                        serde_json::Value::String("cancel_requested".into()),
                    );
                    task.dispatch_eligibility = parked(task.task_id, "cancel_requested", None);
                    task.updated_commit = commit;
                    write_task(&tx, &task)?;
                    summary.cancelled.push(task.task_id);
                }
                child_log(&tx, self, commit, summary.cancelled.len() as i64)?;
                tx.commit()?;
                Ok(summary)
            },
            storage_failure,
        )
    }

    /// T2 binds an actually resolved operation and its sealed E1 to a durable
    /// result entry and subject-ready inbox projection in one SQL transaction.
    pub fn complete_native_task(
        &self,
        claim: &NativeTaskClaim,
        operation: &OperationRecord,
    ) -> Result<Option<TaskResult>> {
        let completion = self.validate_completion(claim.operation_id, operation)?;
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                self.check_dispatch_owner(&tx)?;
                let Some(mut task) = read_task(&tx, claim.task.task_id)? else {
                    return Ok(None);
                };
                if task.is_terminal() {
                    return read_result_for_task(&tx, task.task_id);
                }
                if task.abort_request.is_some()
                    || task.task_revision != claim.invocation.task_revision
                    || !matches!(task.state, TaskState::Running { invocation_id, epoch, .. }
                        if invocation_id == claim.invocation.invocation_id
                            && epoch == claim.invocation.epoch)
                {
                    return Ok(None);
                }
                ensure_current_operation(&tx, &completion)?;
                if unfinished_children(&tx, task.task_id)? {
                    task.state = TaskState::Completing {
                        outcome_ref: completion.evidence_ref.clone(),
                    };
                    task.record_revision = task.record_revision.next();
                    task.memos
                        .insert(NATIVE_COMPLETION.into(), serde_json::to_value(&completion)?);
                    let seq = self.ledger.allocate_seq(&tx)?;
                    let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                    task.updated_commit = commit;
                    write_task(&tx, &task)?;
                    child_log(&tx, self, commit, 1)?;
                    tx.commit()?;
                    return Ok(None);
                }
                let result = finalize_task(&tx, self, task, completion)?;
                tx.commit()?;
                Ok(Some(result))
            },
            storage_failure,
        )
    }

    /// Completing is reopened from its memo, not from an in-memory callback.
    pub fn finish_native_completing(&self, task_id: EntityId) -> Result<Option<TaskResult>> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                self.check_dispatch_owner(&tx)?;
                let Some(task) = read_task(&tx, task_id)? else {
                    return Ok(None);
                };
                if task.is_terminal() {
                    return read_result_for_task(&tx, task.task_id);
                }
                if !matches!(task.state, TaskState::Completing { .. })
                    || unfinished_children(&tx, task_id)?
                {
                    return Ok(None);
                }
                let completion: CompletionMemo = serde_json::from_value(
                    task.memos
                        .get(NATIVE_COMPLETION)
                        .cloned()
                        .context("missing native completion memo")?,
                )?;
                ensure_current_operation(&tx, &completion)?;
                let result = finalize_task(&tx, self, task, completion)?;
                tx.commit()?;
                Ok(Some(result))
            },
            storage_failure,
        )
    }

    fn validate_completion(
        &self,
        expected_operation: EntityId,
        operation: &OperationRecord,
    ) -> Result<CompletionMemo> {
        ensure!(
            operation.operation_id == expected_operation
                && operation.state == OperationState::Resolved
                && operation.outcome.is_some()
                && !operation.outcome_conflict
                && operation.effect_status != EffectStatus::Unknown,
            "operation_not_conclusively_resolved"
        );
        let attempt = self.external_operation_attempt(operation.operation_id)?;
        ensure!(
            attempt.operation_id == operation.operation_id
                && attempt.dispatch_stage == AttemptDispatchStage::Ended,
            "attempt_not_ended"
        );
        let receipt_id = attempt.final_receipt_id.context("missing final receipt")?;
        let evidence = self.external_evidence(receipt_id)?;
        validate_evidence(self, operation.operation_id, attempt.attempt_id, &evidence)?;
        let result = evidence
            .normalized_ref
            .as_ref()
            .map(|object| read_normalized_result(self, object))
            .transpose()?;
        let outcome = match operation.outcome.unwrap() {
            OperationOutcome::Succeeded => TaskOutcome::Succeeded,
            OperationOutcome::Failed => TaskOutcome::Failed {
                reason: "native operation failed; inspect sealed evidence".into(),
            },
            OperationOutcome::Cancelled => TaskOutcome::Cancelled,
            OperationOutcome::Interrupted => TaskOutcome::Interrupted,
        };
        Ok(CompletionMemo {
            operation_id: operation.operation_id,
            receipt_id,
            invocation_id: attempt.invocation_id,
            outcome,
            result,
            evidence_ref: evidence.raw_ref,
        })
    }

    pub fn native_task_result(&self, task_id: EntityId) -> Result<Option<TaskResult>> {
        let conn = self.ledger.conn.lock();
        read_result_for_task(&conn, task_id)
    }

    /// Durable ready projection: S1 dispositions, not an in-memory broadcast or
    /// a single high-water cursor, decide whether a result still needs delivery.
    pub fn ready_native_results(
        &self,
        agent_id: EntityId,
        limit: usize,
    ) -> Result<Vec<TaskResult>> {
        let conn = self.ledger.conn.lock();
        let mut query = conn.prepare(
            "SELECT i.event_id,i.data FROM inbox_events i
             JOIN subject_branch_heads h ON h.agent_id=i.agent_id
             LEFT JOIN subject_event_dispositions d
               ON d.branch_id=h.branch_id AND d.event_id=i.event_id
             WHERE i.agent_id=?1 AND i.kind='task_result' AND i.source='native-child'
               AND (d.disposition IS NULL OR d.disposition='\"deferred\"')
             ORDER BY i.rowid LIMIT ?2",
        )?;
        let rows = query
            .query_map(params![agent_id.to_string(), limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(event_id, data)| {
                let result: TaskResult = serde_json::from_str(&data)?;
                ensure!(
                    result.result_id == event_id.parse()?,
                    "result_event_identity_mismatch"
                );
                Ok(result)
            })
            .collect()
    }
}

fn validate_evidence(
    store: &Store,
    operation_id: EntityId,
    attempt_id: EntityId,
    evidence: &EvidenceReceipt,
) -> Result<()> {
    ensure!(
        evidence.operation_id == Some(operation_id) && evidence.attempt_id == Some(attempt_id),
        "receipt_identity_mismatch"
    );
    let bytes = store.cas.read_object(&evidence.raw_ref.digest)?;
    ensure!(
        bytes.len() as u64 == evidence.raw_ref.bytes
            && Digest::blake3(&bytes) == evidence.raw_ref.digest,
        "result_evidence_closure_incomplete"
    );
    Ok(())
}

fn read_normalized_result(store: &Store, object: &ObjectRef) -> Result<serde_json::Value> {
    const MAX_NORMALIZED_RESULT_BYTES: u64 = 512 * 1024;
    ensure!(
        object.bytes <= MAX_NORMALIZED_RESULT_BYTES,
        "normalized_result_too_large"
    );
    let bytes = store.cas.read_object(&object.digest)?;
    ensure!(
        bytes.len() as u64 == object.bytes && Digest::blake3(&bytes) == object.digest,
        "normalized_result_closure_incomplete"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

fn ensure_current_operation(tx: &Transaction<'_>, completion: &CompletionMemo) -> Result<()> {
    let operation: OperationRecord = read_json(
        tx,
        "SELECT data FROM operations WHERE operation_id=?1",
        completion.operation_id,
    )?
    .context("operation_missing")?;
    ensure!(
        operation.state == OperationState::Resolved
            && operation.outcome.is_some()
            && !operation.outcome_conflict
            && operation.effect_status != EffectStatus::Unknown,
        "operation_changed_before_t2"
    );
    let attempt: AttemptRecord = read_json(
        tx,
        "SELECT data FROM attempts WHERE attempt_id=(
           SELECT attempt_id FROM attempts WHERE operation_id=?1 ORDER BY rowid LIMIT 1)",
        completion.operation_id,
    )?
    .context("attempt_missing")?;
    ensure!(
        attempt.dispatch_stage == AttemptDispatchStage::Ended
            && attempt.final_receipt_id == Some(completion.receipt_id)
            && attempt.invocation_id == completion.invocation_id,
        "attempt_receipt_changed_before_t2"
    );
    Ok(())
}

fn finalize_task(
    tx: &Transaction<'_>,
    store: &Store,
    mut task: TaskRecord,
    completion: CompletionMemo,
) -> Result<TaskResult> {
    let result_id = memo_uuid(&task, NATIVE_RESULT_ID).context("missing stable result id")?;
    let seq = store.ledger.allocate_seq(tx)?;
    let commit = CommitRef::new(store.ledger.ledger_id(), seq);
    task.state = TaskState::Terminal {
        outcome_ref: completion.evidence_ref.clone(),
    };
    task.record_revision = task.record_revision.next();
    task.updated_commit = commit;
    task.terminal_at = Some(chrono::Utc::now());
    task.memos.remove(NATIVE_RECOVERY);
    task.memos.remove(NATIVE_SETTLEMENT_ONLY);
    task.memos.remove(NATIVE_PARK);
    task.memos.remove("native_diagnostic");
    let result = TaskResult {
        schema_version: 1,
        result_id,
        task_id: task.task_id,
        task_revision: task.task_revision,
        task_record_revision: task.record_revision,
        invocation_id: completion.invocation_id,
        operation_id: completion.operation_id,
        receipt_id: completion.receipt_id,
        outcome: completion.outcome,
        result: completion.result,
        evidence_ref: completion.evidence_ref,
        commit_ref: commit,
    };
    let data = serde_json::to_string(&result)?;
    write_task(tx, &task)?;
    tx.execute(
        "INSERT INTO task_results(result_id,task_id,agent_id,kind,data,commit_seq)
         VALUES(?1,?2,?3,'native_child',?4,?5)",
        params![
            result_id.to_string(),
            task.task_id.to_string(),
            task.agent_id.to_string(),
            data,
            commit.seq.0 as i64
        ],
    )?;
    tx.execute(
        "INSERT INTO inbox_events(event_id,agent_id,kind,source,consumed,data)
         VALUES(?1,?2,'task_result','native-child',0,?3)",
        params![result_id.to_string(), task.agent_id.to_string(), data],
    )?;
    child_log(tx, store, commit, 3)?;
    Ok(result)
}

fn ensure_owner_active(
    tx: &Transaction<'_>,
    task: &TaskRecord,
    head: &SubjectBranchHead,
) -> Result<()> {
    let mut owner = task.owner.clone();
    let mut seen = HashSet::new();
    for _ in 0..64 {
        match owner {
            TaskOwner::Conversation { id } => {
                ensure!(id == task.conversation_id, "conversation_owner_mismatch");
                return Ok(());
            }
            TaskOwner::Task { id } => {
                if id == head.subject_root_task_id {
                    return Ok(());
                }
                ensure!(seen.insert(id), "task_owner_cycle");
                let parent = read_task(tx, id)?.context("task_owner_missing")?;
                ensure!(
                    parent.abort_request.is_none() && !parent.is_terminal(),
                    "task_owner_inactive_or_cancelled"
                );
                owner = parent.owner;
            }
        }
    }
    anyhow::bail!("task_owner_depth_exceeded")
}

fn in_cancel_scope(
    tasks: &HashMap<EntityId, TaskRecord>,
    candidate: EntityId,
    target: EntityId,
    scope: AbortScope,
) -> bool {
    if candidate == target {
        return true;
    }
    if scope == AbortScope::Task {
        return false;
    }
    let mut current = candidate;
    let mut seen = HashSet::new();
    for _ in 0..64 {
        let Some(task) = tasks.get(&current) else {
            return false;
        };
        if scope == AbortScope::SubtreeOrdinary && task.background {
            return false;
        }
        match task.owner {
            TaskOwner::Conversation { .. } => return false,
            TaskOwner::Task { id } => {
                if id == target {
                    return true;
                }
                if !seen.insert(id) {
                    return false;
                }
                current = id;
            }
        }
    }
    false
}

fn pending_checkpoint(state: &TaskState) -> Option<ObjectRef> {
    match state {
        TaskState::Pending { checkpoint_ref }
        | TaskState::Running { checkpoint_ref, .. }
        | TaskState::Waiting { checkpoint_ref, .. } => checkpoint_ref.clone(),
        _ => None,
    }
}

fn parked(
    task_id: EntityId,
    _reason: &str,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> DispatchEligibility {
    // The task's own native_diagnostic memo holds bounded reason detail. This
    // stable reference is never a hash of a natural-language goal or outcome.
    DispatchEligibility::Parked {
        reason_ref: task_id,
        until_utc: until,
    }
}

fn read_candidates_tx(
    conn: &rusqlite::Connection,
    after: Option<(&str, EntityId)>,
    limit: usize,
) -> Result<Vec<(String, TaskRecord)>> {
    let (created_after, id_after) = after
        .map(|(created, id)| (created.to_string(), id.to_string()))
        .unwrap_or_default();
    let mut query = conn.prepare(
        "SELECT data, created_at FROM tasks
         WHERE json_extract(data,'$.kind')!='subject_publication'
           AND json_extract(data,'$.memos.cognition_imported') IS NULL
           AND json_extract(data,'$.state.status') IN ('pending','running','completing')
           AND (?1='' OR created_at>?1 OR (created_at=?1 AND task_id>?2))
         ORDER BY created_at,task_id LIMIT ?3",
    )?;
    let rows = query
        .query_map(rusqlite::params![created_after, id_after, limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(data, created)| {
            Ok((created, serde_json::from_str(&data)?))
        })
        .collect()
}

fn read_scan_upper_tx(conn: &rusqlite::Connection) -> Result<Option<(String, EntityId)>> {
    let row = conn.query_row(
        "SELECT data, created_at FROM tasks
         WHERE json_extract(data,'$.kind')!='subject_publication'
           AND json_extract(data,'$.memos.cognition_imported') IS NULL
           AND json_extract(data,'$.state.status') IN ('pending','running','completing')
         ORDER BY created_at DESC, task_id DESC LIMIT 1",
        [],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ).optional()?;
    row.map(|(data, created)| {
        let task: TaskRecord = serde_json::from_str(&data)?;
        Ok((created, task.task_id))
    }).transpose()
}

fn read_recovery_candidates_tx(conn: &rusqlite::Connection, after: &str) -> Result<Vec<(String, TaskRecord)>> {
    // Recovery is a complete startup pass, not one dispatch page. Unrelated
    // pending work must not hide a crashed running task, nor may page 257 wait
    // for another startup that will never happen in this runtime.
    let mut query = conn.prepare(
        "SELECT task_id,data FROM tasks
         WHERE task_id>?1 AND json_extract(data,'$.kind')!='subject_publication'
           AND json_extract(data,'$.memos.cognition_imported') IS NULL
           AND json_extract(data,'$.state.status')='running'
           AND (json_extract(data,'$.memos.native_child')=1
                OR json_extract(data,'$.kind')='delegation')
         ORDER BY task_id LIMIT 256",
    )?;
    let rows = query.query_map([after], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    rows.map(|row| {
        let (key, data) = row?;
        Ok((key, serde_json::from_str(&data)?))
    }).collect()
}

fn read_all_tasks(conn: &rusqlite::Connection) -> Result<Vec<TaskRecord>> {
    let mut query = conn.prepare("SELECT data FROM tasks")?;
    let rows = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|data| serde_json::from_str(&data).map_err(Into::into))
        .collect()
}

fn read_task(conn: &rusqlite::Connection, id: EntityId) -> Result<Option<TaskRecord>> {
    read_json(conn, "SELECT data FROM tasks WHERE task_id=?1", id)
}

fn read_head(conn: &rusqlite::Connection, agent: EntityId) -> Result<SubjectBranchHead> {
    read_json(
        conn,
        "SELECT data FROM subject_branch_heads WHERE agent_id=?1",
        agent,
    )?
    .context("subject_head_missing")
}

fn read_json<T: serde::de::DeserializeOwned>(
    conn: &rusqlite::Connection,
    sql: &str,
    id: EntityId,
) -> Result<Option<T>> {
    let data: Option<String> = conn
        .query_row(sql, [id.to_string()], |row| row.get(0))
        .optional()?;
    data.map(|data| serde_json::from_str(&data).map_err(Into::into))
        .transpose()
}

fn read_result_for_task(
    conn: &rusqlite::Connection,
    task_id: EntityId,
) -> Result<Option<TaskResult>> {
    read_json(
        conn,
        "SELECT data FROM task_results WHERE task_id=?1",
        task_id,
    )
}

fn write_task(tx: &Transaction<'_>, task: &TaskRecord) -> Result<()> {
    let changed = tx.execute(
        "UPDATE tasks SET record_revision=?2,data=?3,updated_commit_seq=?4
         WHERE task_id=?1 AND record_revision=?5",
        params![
            task.task_id.to_string(),
            task.record_revision.0 as i64,
            serde_json::to_string(task)?,
            task.updated_commit.seq.0 as i64,
            (task.record_revision.0 - 1) as i64
        ],
    )?;
    ensure!(changed == 1, "stale_task_record");
    Ok(())
}

fn operation_exists(conn: &rusqlite::Connection, id: EntityId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM operations WHERE operation_id=?1",
            [id.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn unfinished_children(conn: &rusqlite::Connection, task_id: EntityId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM tasks
             WHERE json_extract(data,'$.owner.kind')='task'
               AND json_extract(data,'$.owner.id')=?1
               AND json_extract(data,'$.background')=0
               AND json_extract(data,'$.state.status')!='terminal' LIMIT 1",
            [task_id.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn child_log(tx: &Transaction<'_>, store: &Store, commit: CommitRef, count: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count) VALUES(?1,?2,?3,?4)",
        params![
            commit.ledger_id.to_string(),
            commit.seq.0 as i64,
            store.capabilities.incarnation.to_string(),
            count
        ],
    )?;
    Ok(())
}

fn storage_failure(error: &anyhow::Error) -> bool {
    error.is::<rusqlite::Error>() || error.is::<serde_json::Error>()
}
