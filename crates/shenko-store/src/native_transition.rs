//! Canonical native T2: apply one frozen completion, never execute its attempt.
//!
//! Only `native.complete` and `native.finish` are admitted here. The original
//! operation, attempt and E1 are read conditions; the only writes are the named
//! task and, after its ordinary children converge, its result and inbox entry.
use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    authority::{CommitAdmission, CommitAuthorityGrant},
    commit::{CommitRef, UpdatedEntity},
    error::ShenkoError,
    evidence::EvidenceReceipt,
    ids::{EntityId, RecordRevision},
    object::{Digest, ObjectRef},
    operation::{
        AttemptDispatchStage, AttemptRecord, EffectStatus, OperationOutcome, OperationRecord,
        OperationState,
    },
    subject::{SubjectBranchHead, SubjectMode},
    task::{RunMode, TaskInvocation, TaskOutcome, TaskOwner, TaskRecord, TaskResult, TaskState},
};

use crate::{
    Store,
    authority::{AuthorityReservation, admission_materialized},
    child::{
        COGNITION_IMPORTED, CompletionMemo, NATIVE_COMPLETION, NATIVE_OPERATION_ID, NATIVE_PARK,
        NATIVE_RECOVERY, NATIVE_RESULT_ID, NATIVE_SETTLEMENT_ONLY, memo_flag, memo_uuid,
    },
    control_transition::{
        Cell, MAX_IMAGE_BYTES, NATIVE_ANCESTRY_READ_SCHEMA, NATIVE_DIRECT_COMPLETION_READ_SCHEMA, NATIVE_OPERATION_READ_SCHEMA,
        NATIVE_SUBJECT_READ_SCHEMA, NativeAncestry, NativeReadSelector, PreparedTransition,
        RowTransition, RowTransitionInput, TableTransition,
    },
    native_facts::{CanonicalFact, narrow_task_read, validate_task_read},
    store::{commit_error, storage_error},
};

pub(crate) const CODEC: &str = "store-native-t2-transition";
pub(crate) const DIRECT_COMPLETION_CODEC_MAJOR: u32 = 6;

pub(crate) fn is_direct_completion(reference: &ObjectRef) -> bool {
    reference.codec.name == CODEC && reference.codec.major == DIRECT_COMPLETION_CODEC_MAJOR
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(crate) enum NativeT2Phase {
    #[serde(rename = "native.complete")]
    Complete { invocation: TaskInvocation },
    #[serde(rename = "native.finish")]
    Finish,
}

impl NativeT2Phase {
    fn kind(&self) -> &'static str {
        match self {
            Self::Complete { .. } => "native.complete",
            Self::Finish => "native.finish",
        }
    }

    fn position(&self, task: &TaskRecord) -> String {
        let phase = match self {
            Self::Complete { .. } => "complete",
            Self::Finish => "finish",
        };
        format!("task/{}/{phase}/{}", task.task_id, task.record_revision.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeT2Plan {
    phase: NativeT2Phase,
    task: TaskRecord,
    completion: CompletionMemo,
    operation: OperationRecord,
    attempt: AttemptRecord,
    evidence: EvidenceReceipt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeT2Transition {
    rows: RowTransition,
    plan: NativeT2Plan,
    result: Option<TaskResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence_fact: Option<ObjectRef>,
}

pub(crate) enum NativeApplyMode {
    Current,
    Prepared,
    Learned,
}

fn invalid(reason: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::other(format!("invalid canonical native T2: {reason}"))
}

fn conflict(reason: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::conflict(format!("canonical native T2: {reason}"))
}

fn same(left: &impl Serialize, right: &impl Serialize) -> Result<bool, ShenkoError> {
    Ok(serde_json::to_value(left)? == serde_json::to_value(right)?)
}

fn task_outcome(outcome: OperationOutcome) -> TaskOutcome {
    match outcome {
        OperationOutcome::Succeeded => TaskOutcome::Succeeded,
        OperationOutcome::Failed => TaskOutcome::Failed {
            reason: "native operation failed; inspect sealed evidence".into(),
        },
        OperationOutcome::Cancelled => TaskOutcome::Cancelled,
        OperationOutcome::Interrupted => TaskOutcome::Interrupted,
    }
}

impl NativeT2Plan {
    fn read_selectors(&self) -> [NativeReadSelector; 3] {
        [
            NativeReadSelector::OriginalOperation { operation_id: self.operation.operation_id },
            NativeReadSelector::OriginalAttempt { attempt_id: self.attempt.attempt_id },
            NativeReadSelector::OriginalReceipt { receipt_id: self.evidence.receipt_id },
        ]
    }

    fn direct_read_selectors(&self, head: &SubjectBranchHead) -> [NativeReadSelector; 4] {
        let [operation, attempt, receipt] = self.read_selectors();
        [operation, attempt, receipt, NativeReadSelector::DirectCompletionHead { agent_id: head.agent_id }]
    }

    fn generic_read_selectors(&self, head: &SubjectBranchHead) -> Vec<NativeReadSelector> {
        let ancestry = NativeAncestry::new(&self.task, head);
        let mut selectors = self.read_selectors().to_vec();
        selectors.extend([
            NativeReadSelector::TaskAncestry { ancestry: ancestry.clone(), ordinary_children: true },
            NativeReadSelector::AuthoringSteps { ancestry },
        ]);
        selectors
    }

    fn input_digest(&self, head: &SubjectBranchHead) -> Result<Digest, ShenkoError> {
        Ok(Digest::blake3(&serde_json::to_vec(&(head, self))?))
    }

    /// Verify the named original; no receiver-local row ordering selects an attempt.
    fn validate(&self, head: &SubjectBranchHead) -> Result<(), ShenkoError> {
        let task = &self.task;
        let operation = &self.operation;
        let attempt = &self.attempt;
        let evidence = &self.evidence;
        let completion = &self.completion;
        if task.agent_id != head.agent_id
            || task.is_terminal()
            || task.abort_request.is_some()
            || task.memos.contains_key(COGNITION_IMPORTED)
            || !matches!(head.mode, SubjectMode::Active | SubjectMode::Paused)
            || operation.task_id != task.task_id
            || memo_uuid(task, NATIVE_OPERATION_ID) != Some(operation.operation_id)
            || memo_uuid(task, NATIVE_RESULT_ID).is_none()
            || operation.operation_id != completion.operation_id
            || operation.state != OperationState::Resolved
            || operation.outcome_conflict
            || operation.effect_status == EffectStatus::Unknown
            || operation.outcome.map(task_outcome).as_ref() != Some(&completion.outcome)
            || attempt.operation_id != operation.operation_id
            || attempt.dispatch_stage != AttemptDispatchStage::Ended
            || attempt.final_receipt_id != Some(evidence.receipt_id)
            || evidence.receipt_id != completion.receipt_id
            || evidence.operation_id != Some(operation.operation_id)
            || evidence.attempt_id != Some(attempt.attempt_id)
            || completion.invocation_id != attempt.invocation_id
            || completion.evidence_ref != evidence.raw_ref
        {
            return Err(invalid(
                "task, operation, attempt and original E1 do not agree",
            ));
        }
        match &self.phase {
            NativeT2Phase::Complete { invocation } => {
                // A current settlement invocation may consume an older
                // attempt's E1. Its authority never renames that attempt or
                // the result's original invocation.
                let settlement =
                    memo_flag(task, NATIVE_SETTLEMENT_ONLY) && memo_flag(task, NATIVE_RECOVERY);
                if invocation.task_id != task.task_id
                    || invocation.task_revision != task.task_revision
                    || (!settlement
                        && (invocation.invocation_id != attempt.invocation_id
                            || invocation.epoch != attempt.epoch
                            || invocation.worker_id != attempt.worker_id
                            || invocation.node_id != attempt.node_id))
                    || invocation.epoch != head.controller_epoch
                    || invocation.software_generation != head.software_generation
                    || !matches!(task.state, TaskState::Running { invocation_id, epoch, mode: RunMode::Run, .. }
                        if invocation_id == invocation.invocation_id && epoch == invocation.epoch)
                {
                    return Err(invalid("completion is not the original current invocation"));
                }
            }
            NativeT2Phase::Finish => {
                if !matches!(&task.state, TaskState::Completing { outcome_ref } if *outcome_ref == completion.evidence_ref)
                    || task.memos.get(NATIVE_COMPLETION) != Some(&serde_json::to_value(completion)?)
                {
                    return Err(invalid(
                        "finish does not retain the original completion memo",
                    ));
                }
            }
        }
        Ok(())
    }

    fn projection(
        &self,
        commit: CommitRef,
        at: chrono::DateTime<chrono::Utc>,
        unfinished: bool,
    ) -> Result<(TaskRecord, Option<TaskResult>), ShenkoError> {
        let mut task = self.task.clone();
        task.record_revision = RecordRevision(
            task.record_revision
                .0
                .checked_add(1)
                .ok_or_else(|| invalid("task revision exhausted"))?,
        );
        task.updated_commit = commit;
        if unfinished {
            if matches!(&self.phase, NativeT2Phase::Finish) {
                return Err(invalid("finish has unfinished ordinary children"));
            }
            task.state = TaskState::Completing {
                outcome_ref: self.completion.evidence_ref.clone(),
            };
            task.memos.insert(
                NATIVE_COMPLETION.into(),
                serde_json::to_value(&self.completion)?,
            );
            return Ok((task, None));
        }
        task.state = TaskState::Terminal {
            outcome_ref: self.completion.evidence_ref.clone(),
        };
        task.terminal_at = Some(at);
        for key in [
            NATIVE_RECOVERY,
            NATIVE_SETTLEMENT_ONLY,
            NATIVE_PARK,
            "native_diagnostic",
        ] {
            task.memos.remove(key);
        }
        let result = TaskResult {
            schema_version: 1,
            result_id: memo_uuid(&task, NATIVE_RESULT_ID)
                .ok_or_else(|| invalid("missing original result identity"))?,
            task_id: task.task_id,
            task_revision: task.task_revision,
            task_record_revision: task.record_revision,
            invocation_id: self.attempt.invocation_id,
            operation_id: self.operation.operation_id,
            receipt_id: self.evidence.receipt_id,
            outcome: self.completion.outcome.clone(),
            result: self.completion.result.clone(),
            evidence_ref: self.evidence.raw_ref.clone(),
            commit_ref: commit,
        };
        Ok((task, Some(result)))
    }
}

fn column<'a>(
    table: &TableTransition,
    row: &'a [Cell],
    name: &str,
) -> Result<&'a Cell, ShenkoError> {
    table
        .before
        .table
        .columns
        .iter()
        .position(|column| column == name)
        .and_then(|index| row.get(index))
        .ok_or_else(|| invalid(format!("missing {}.{name}", table.before.table.name)))
}

fn json<T: serde::de::DeserializeOwned>(cell: &Cell) -> Result<T, ShenkoError> {
    let Cell::Text(data) = cell else {
        return Err(invalid("original row is not JSON text"));
    };
    serde_json::from_str(data).map_err(invalid)
}

impl NativeT2Transition {
    fn read_selectors(&self) -> Vec<NativeReadSelector> {
        match self.rows.schema_version {
            NATIVE_ANCESTRY_READ_SCHEMA => self.plan.generic_read_selectors(&self.rows.original_head),
            NATIVE_DIRECT_COMPLETION_READ_SCHEMA => self.plan.direct_read_selectors(&self.rows.original_head).to_vec(),
            _ => self.plan.read_selectors().to_vec(),
        }
    }

    fn read_table(&self, name: &str) -> Result<&TableTransition, ShenkoError> {
        let matches = self
            .rows
            .tables
            .iter()
            .filter(|table| table.before.table.name == name)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [table] if name == "tasks" && self.evidence_fact.is_some() => {
                validate_task_read(table, &self.plan.task)?;
                Ok(table)
            }
            [table] if table.complete_read => Ok(table),
            [table] if matches!(self.rows.schema_version, NATIVE_OPERATION_READ_SCHEMA | NATIVE_ANCESTRY_READ_SCHEMA | NATIVE_DIRECT_COMPLETION_READ_SCHEMA)
                && self.read_selectors().iter().any(|selector| table.read_selector.as_ref() == Some(selector)) => Ok(table),
            _ => Err(invalid(format!("missing complete {name} read condition"))),
        }
    }

    fn original(
        &self,
        table: &str,
        id_column: &str,
        id: EntityId,
        expected: &impl Serialize,
    ) -> Result<(), ShenkoError> {
        let table = self.read_table(table)?;
        let identity = Cell::Text(id.to_string());
        let originals = table
            .before
            .rows
            .values()
            .filter(|row| column(table, row, id_column).is_ok_and(|cell| cell == &identity))
            .collect::<Vec<_>>();
        let [row] = originals.as_slice() else {
            return Err(invalid("missing unique original row identity"));
        };
        let value: serde_json::Value = json(column(table, row, "data")?)?;
        if value != serde_json::to_value(expected)? {
            return Err(invalid("original row differs from the admitted identity"));
        }
        Ok(())
    }

    /// Independently constrain every changed cell to this named T2 result.
    /// An extra write, another task or a rewritten E1 is not a native completion.
    fn validate(&self, admission: &CommitAdmission) -> Result<(), ShenkoError> {
        let rows = &self.rows;
        let head = &rows.original_head;
        let plan = &self.plan;
        plan.validate(head)?;
        rows.validate_native_read_selectors(&self.read_selectors())?;
        if self.evidence_fact.is_some()
            && (!matches!(plan.phase, NativeT2Phase::Complete { .. }) || self.result.is_none())
        {
            return Err(invalid(
                "direct native continuation must be a childless original completion",
            ));
        }
        let key = plan.phase.position(&plan.task);
        if !matches!(rows.schema_version, 1 | NATIVE_SUBJECT_READ_SCHEMA | NATIVE_OPERATION_READ_SCHEMA
            | NATIVE_ANCESTRY_READ_SCHEMA | NATIVE_DIRECT_COMPLETION_READ_SCHEMA)
            || (matches!(rows.schema_version, NATIVE_SUBJECT_READ_SCHEMA | NATIVE_OPERATION_READ_SCHEMA
                | NATIVE_DIRECT_COMPLETION_READ_SCHEMA) && self.evidence_fact.is_none())
            || (rows.schema_version == NATIVE_ANCESTRY_READ_SCHEMA && self.evidence_fact.is_some())
            || rows.action_id != admission.action_id
            || rows.action_id != EntityId::derived("commit-admission-action-v1", key.as_bytes())
            || rows.input_digest != plan.input_digest(head)?
            || rows.source_ledger_id != rows.result_commit.ledger_id
            || admission.subject_id != head.agent_id
            || admission.branch_id != head.branch_id
            || admission.controller_epoch != head.controller_epoch
            || admission.software_generation != head.software_generation
            || admission.conflict_key != key
            || admission.expected_parent.is_some()
            || admission.expected_head_revision.is_some()
            || admission.operation_id != Some(plan.operation.operation_id)
            || admission.attempt_id != Some(plan.attempt.attempt_id)
            || !admission
                .conflict_aliases
                .contains(&crate::authority::task_revision_position(&plan.task))
        {
            return Err(invalid("transition does not bind the admitted T2 position"));
        }
        if rows.schema_version == NATIVE_ANCESTRY_READ_SCHEMA {
            let ancestry = crate::native_facts::validate_native_ancestry(rows, &plan.task, true)?;
            crate::native_facts::validate_native_ancestry_fences(admission, &ancestry)?;
        }
        self.original("subject_branch_heads", "agent_id", head.agent_id, head)?;
        self.original("tasks", "task_id", plan.task.task_id, &plan.task)?;
        self.original(
            "operations",
            "operation_id",
            plan.operation.operation_id,
            &plan.operation,
        )?;
        self.original(
            "attempts",
            "attempt_id",
            plan.attempt.attempt_id,
            &plan.attempt,
        )?;
        self.original(
            "evidence_receipts",
            "receipt_id",
            plan.evidence.receipt_id,
            &plan.evidence,
        )?;
        let tasks = self.read_table("tasks")?;
        let mut unfinished = false;
        for row in tasks.before.rows.values() {
            let task: TaskRecord = json(column(tasks, row, "data")?)?;
            unfinished |= task.owner
                == (TaskOwner::Task {
                    id: plan.task.task_id,
                })
                && !task.background
                && !task.is_terminal();
        }
        let (task, result) = plan.projection(rows.result_commit, rows.created_at, unfinished)?;
        if result != self.result
            || rows.updated.len() != 1
            || rows.updated[0].entity_id != task.task_id
            || rows.updated[0].record_revision != task.record_revision
        {
            return Err(invalid(
                "frozen task/result differs from the original completion",
            ));
        }
        let mut remaining = if result.is_some() { 3 } else { 1 };
        for table in &rows.tables {
            for change in &table.changes {
                let expected = match table.before.table.name.as_str() {
                    "tasks" => {
                        let before = change
                            .before
                            .as_ref()
                            .ok_or_else(|| invalid("T2 cannot create a task"))?;
                        if column(table, before, "task_id")?
                            != &Cell::Text(task.task_id.to_string())
                        {
                            return Err(invalid("T2 changed another task"));
                        }
                        let mut values = table
                            .before
                            .table
                            .columns
                            .iter()
                            .cloned()
                            .zip(before.iter().cloned())
                            .collect::<BTreeMap<_, _>>();
                        values.insert("data".into(), Cell::Text(serde_json::to_string(&task)?));
                        values.insert(
                            "record_revision".into(),
                            Cell::Integer(integer(task.record_revision.0)?),
                        );
                        values.insert("updated_commit_seq".into(), Cell::LocalSequence);
                        values
                    }
                    "task_results" | "inbox_events" => {
                        if change.before.is_some() {
                            return Err(invalid("T2 result must be a new original fact"));
                        }
                        let result = result
                            .as_ref()
                            .ok_or_else(|| invalid("Completing cannot publish a result"))?;
                        result_cells(
                            table.before.table.name.as_str(),
                            plan.task.agent_id,
                            result,
                            rows.created_at,
                        )?
                    }
                    _ => return Err(invalid("T2 wrote outside its task/result/inbox")),
                };
                let actual = change
                    .after
                    .as_ref()
                    .ok_or_else(|| invalid("T2 cannot delete a fact"))?;
                let expected = table
                    .before
                    .table
                    .columns
                    .iter()
                    .map(|name| {
                        expected
                            .get(name)
                            .cloned()
                            .ok_or_else(|| invalid("unknown T2 result column"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if *actual != expected {
                    return Err(invalid("T2 write differs from its named result"));
                }
                remaining -= 1;
            }
        }
        if remaining != 0 {
            return Err(invalid("missing or duplicated T2 result rows"));
        }
        Ok(())
    }
}

fn integer(value: u64) -> Result<i64, ShenkoError> {
    i64::try_from(value).map_err(|_| invalid("sequence/revision exceeds SQLite range"))
}

fn result_cells(
    table: &str,
    agent: EntityId,
    result: &TaskResult,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<BTreeMap<String, Cell>, ShenkoError> {
    let text = |value: String| Cell::Text(value);
    let mut cells = BTreeMap::from([
        ("agent_id".into(), text(agent.to_string())),
        ("data".into(), text(serde_json::to_string(result)?)),
        (
            "created_at".into(),
            text(at.format("%Y-%m-%d %H:%M:%S").to_string()),
        ),
    ]);
    if table == "task_results" {
        cells.extend([
            ("result_id".into(), text(result.result_id.to_string())),
            ("task_id".into(), text(result.task_id.to_string())),
            ("kind".into(), text("native_child".into())),
            (
                "commit_seq".into(),
                Cell::Integer(integer(result.commit_ref.seq.0)?),
            ),
        ]);
    } else {
        cells.extend([
            ("event_id".into(), text(result.result_id.to_string())),
            ("kind".into(), text("task_result".into())),
            ("source".into(), text("native-child".into())),
            ("consumed".into(), Cell::Integer(0)),
        ]);
    }
    Ok(cells)
}

/// Logical precondition failures stay conflicts. SQLite/JSON failures retain
/// their storage classification so the writer closes on uncertain settlement.
fn source_error(error: anyhow::Error) -> ShenkoError {
    match error.downcast::<ShenkoError>() {
        Ok(error) => error,
        Err(error) if error.is::<rusqlite::Error>() || error.is::<serde_json::Error>() => {
            storage_error(error)
        }
        Err(error) => conflict(error),
    }
}

impl Store {
    pub(crate) fn canonical_native_completion(
        &self,
        task_id: EntityId,
        phase: NativeT2Phase,
        completion: CompletionMemo,
    ) -> Result<Option<TaskResult>, ShenkoError> {
        let _phase = crate::timing::Phase::bound("native.t2", crate::timing::Binding {
            task_id: Some(task_id), operation_id: Some(completion.operation_id), ..Default::default()
        });
        let (head, mut position) = self.task_admission_position(task_id, String::new())?;
        let task = position
            .expected_tasks
            .first()
            .cloned()
            .ok_or_else(|| conflict("task missing"))?;
        let evidence = self
            .external_evidence(completion.receipt_id)
            .map_err(source_error)?;
        let attempt_id = evidence
            .attempt_id
            .ok_or_else(|| conflict("original E1 has no attempt identity"))?;
        let plan = NativeT2Plan {
            phase,
            task,
            operation: self
                .external_operation(completion.operation_id)
                .map_err(source_error)?,
            attempt: self.external_attempt(attempt_id).map_err(source_error)?,
            evidence,
            completion,
        };
        // The caller can become stale between its initial read and this
        // current snapshot. That is a source precondition conflict, whereas
        // an already sealed plan failing the same validation is corruption.
        plan.validate(&head)
            .map_err(|error| conflict(format!("source precondition changed: {error}")))?;
        self.verify_native_evidence(&plan)?;
        position.key = plan.phase.position(&plan.task);
        position.operation_id = Some(plan.operation.operation_id);
        position.attempt_id = Some(plan.attempt.attempt_id);
        let evidence_fact = if matches!(plan.phase, NativeT2Phase::Complete { .. }) {
            let conn = crate::timing::measure("sqlite.mutex_wait", || self.ledger.conn.lock());
            let reference =
                self.native_completion_evidence(&conn, &plan.task, &plan.operation, &plan.attempt)?;
            if let Some(reference) = &reference {
                let (_, original) = self.read_native_evidence_fact(&conn, reference)?;
                position.expected_steps.push(original.creator);
            }
            reference
        } else {
            None
        };
        let expected = position.expected_tasks.clone();
        let steps = position.expected_steps.clone();
        let prepared = self.prepare_native_transition(&head, plan, position, evidence_fact)?;
        let authority = self.reserve_sealed_admission(prepared.admission, expected, steps)?;
        self.apply_native_transition(
            &prepared.transition_ref,
            &authority,
            NativeApplyMode::Current,
        )
    }

    fn verify_native_evidence(&self, plan: &NativeT2Plan) -> Result<(), ShenkoError> {
        let raw = &plan.evidence.raw_ref;
        let bytes = self.cas.read_object(&raw.digest)?;
        if bytes.len() as u64 != raw.bytes || Digest::blake3(&bytes) != raw.digest {
            return Err(invalid("original E1 bytes differ"));
        }
        let normalized = match &plan.evidence.normalized_ref {
            Some(reference) => {
                if reference.bytes > 512 * 1024 {
                    return Err(invalid("normalized E1 exceeds native result bound"));
                }
                let bytes = self.cas.read_object(&reference.digest)?;
                if bytes.len() as u64 != reference.bytes
                    || Digest::blake3(&bytes) != reference.digest
                {
                    return Err(invalid("normalized E1 bytes differ"));
                }
                Some(serde_json::from_slice::<serde_json::Value>(&bytes).map_err(invalid)?)
            }
            None => None,
        };
        if normalized != plan.completion.result {
            return Err(invalid("completion substituted the original E1 result"));
        }
        Ok(())
    }

    fn prepare_native_transition(
        &self,
        head: &SubjectBranchHead,
        plan: NativeT2Plan,
        mut position: crate::authority::AdmissionPosition,
        evidence_fact: Option<ObjectRef>,
    ) -> Result<PreparedTransition, ShenkoError> {
        position.aliases.sort();
        position.aliases.dedup();
        position.reads.sort();
        position.reads.dedup();
        let action_id = EntityId::derived("commit-admission-action-v1", position.key.as_bytes());
        let _phase = crate::timing::Phase::bound("canonical.prepare", crate::timing::Binding {
            subject_id: Some(head.agent_id), task_id: Some(plan.task.task_id), action_id: Some(action_id),
            operation_id: Some(plan.operation.operation_id), attempt_id: Some(plan.attempt.attempt_id),
        });
        let input_digest = plan.input_digest(head)?;
        if let Some(prepared) = self.prepared_transition(action_id, &input_digest)? {
            return Ok(prepared);
        }
        let predecessor_ref = self.prepare_control_predecessor_for_admission(
            head,
            &position.expected_tasks,
            &position.expected_steps,
        )?;
        let authority = self.reserve_authority()?.with_read_conditions(
            position.expected_tasks.clone(),
            position.expected_steps.clone(),
        );
        authority.with_mutation(&self.ledger, || {
            let mut conn = crate::timing::measure("sqlite.mutex_wait", || self.ledger.conn.lock());
            let tx = crate::timing::immediate(&mut conn).map_err(storage_error)?;
            authority.check(&tx, self.capabilities.incarnation, Some(head.agent_id))?;
            self.check_dispatch_owner(&tx).map_err(source_error)?;
            if let Some(prepared) = crate::control_transition::prepared_transition_at(&tx, action_id, &input_digest)? { return Ok(prepared); }
            // The local execution guard is checked in this transaction, outside
            // the shared read set. A follower never copies dispatcher ownership.
            let input = RowTransitionInput { action_id, input_digest: input_digest.clone(), head };
            let evaluate = |commit, at| self.evaluate_native_t2(&tx, head, &plan, commit, at);
            let (mut rows, result) = if evidence_fact.is_some() {
                let [creator] = position.expected_steps.as_slice() else {
                    return Err(invalid("direct T2 requires its original creator"));
                };
                self.capture_direct_native_transition(&tx, input, &plan.task, creator, &plan.direct_read_selectors(head), evaluate)?
            } else {
                self.capture_native_ancestry_transition(&tx, input, &plan.generic_read_selectors(head), evaluate)?
            };
            if evidence_fact.is_some() { narrow_task_read(&mut rows, &plan.task)?; }
            let transition = NativeT2Transition { rows, plan: plan.clone(), result, evidence_fact: evidence_fact.clone() };
            let result_commit = transition.rows.result_commit;
            let bytes = serde_json::to_vec(&transition)?;
            if bytes.len() > MAX_IMAGE_BYTES { return Err(conflict("native T2 exceeds the 64 MiB admission budget")); }
            let transition_ref = self.cas.seal_object(&bytes, "application/json", "commit-authority", CODEC,
                if evidence_fact.is_some() { DIRECT_COMPLETION_CODEC_MAJOR } else { 5 })?.object;
            let payload_ref = self.cas.seal_object(&serde_json::to_vec(&serde_json::json!({
                "schema_version":1,"kind":"store.control_admission","head":head,
                "source_ledger_id":self.ledger.ledger_id(),"predecessor_ref":predecessor_ref,
                "expected_tasks":position.expected_tasks,"snapshot_tasks":position.snapshot_tasks,"expected_steps":position.expected_steps,
                "action":{"kind":plan.phase.kind(),"task_id":plan.task.task_id,"transition_ref":transition_ref},
            }))?, "application/json", "commit-authority", "commit-admission", 1)?.object;
            let admission = CommitAdmission {
                schema_version:1, subject_id:head.agent_id, branch_id:head.branch_id.clone(), action_id,
                conflict_key:position.key.clone(), conflict_aliases:position.aliases.clone(), required_unconsumed:position.reads.clone(),
                expected_parent:None, expected_head_revision:None, controller_epoch:head.controller_epoch,
                software_generation:head.software_generation.clone(), operation_id:position.operation_id,
                attempt_id:position.attempt_id, payload_ref,
            };
            transition.validate(&admission)?;
            self.verify_native_continuation(&tx, &CanonicalFact { admission: admission.clone(), transition_ref: transition_ref.clone() }, &transition)?;
            tx.execute("INSERT INTO control_prepared_transitions(action_id,input_digest,admission,result_commit,transition_ref) VALUES(?1,?2,?3,?4,?5)",
                params![action_id.to_string(),input_digest.0,serde_json::to_string(&admission)?,serde_json::to_string(&result_commit)?,serde_json::to_string(&transition_ref)?])
                .map_err(storage_error)?;
            self.record_subject_commit(&tx, result_commit, 1)?;
            crate::timing::measure("sqlite.commit", || tx.commit()).map_err(commit_error)?;
            Ok(PreparedTransition { admission, transition_ref })
        }, |error| matches!(error, ShenkoError::Storage(_)))
    }

    /// This is the sole source evaluation. Its SQL reads are captured before
    /// rollback; receivers apply the validated result bytes without this code.
    fn evaluate_native_t2(
        &self,
        tx: &Transaction<'_>,
        head: &SubjectBranchHead,
        plan: &NativeT2Plan,
        commit: CommitRef,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(Vec<UpdatedEntity>, Option<TaskResult>), ShenkoError> {
        let current = crate::store::read_subject(tx, head.agent_id)?.0;
        if !same(&current, head)? {
            return Err(conflict("subject predecessor changed before T2"));
        }
        let task = crate::child::read_task(tx, plan.task.task_id)
            .map_err(source_error)?
            .ok_or_else(|| conflict("task disappeared before T2"))?;
        if !same(&task, &plan.task)? {
            return Err(conflict("task changed before T2"));
        }
        crate::child::ensure_owner_active(tx, &task, head).map_err(source_error)?;
        for (sql, id, original) in [
            (
                "SELECT data FROM operations WHERE operation_id=?1",
                plan.operation.operation_id,
                serde_json::to_value(&plan.operation)?,
            ),
            (
                "SELECT data FROM attempts WHERE attempt_id=?1",
                plan.attempt.attempt_id,
                serde_json::to_value(&plan.attempt)?,
            ),
            (
                "SELECT data FROM evidence_receipts WHERE receipt_id=?1",
                plan.evidence.receipt_id,
                serde_json::to_value(&plan.evidence)?,
            ),
        ] {
            let stored: Option<String> = tx
                .query_row(sql, [id.to_string()], |row| row.get(0))
                .optional()
                .map_err(storage_error)?;
            if stored
                .map(|data| serde_json::from_str::<serde_json::Value>(&data))
                .transpose()?
                != Some(original)
            {
                return Err(conflict("original operation/attempt/E1 changed before T2"));
            }
        }
        let unfinished =
            crate::child::unfinished_children(tx, task.task_id).map_err(source_error)?;
        if unfinished && matches!(&plan.phase, NativeT2Phase::Finish) {
            return Err(conflict("ordinary child changed before finish was frozen"));
        }
        let (task, result) = plan.projection(commit, at, unfinished)?;
        crate::child::write_task(tx, &task).map_err(source_error)?;
        if let Some(result) = &result {
            let data = serde_json::to_string(result)?;
            let created_at = at.format("%Y-%m-%d %H:%M:%S").to_string();
            tx.execute("INSERT INTO task_results(result_id,task_id,agent_id,kind,data,commit_seq,created_at) VALUES(?1,?2,?3,'native_child',?4,?5,?6)",
                params![result.result_id.to_string(),task.task_id.to_string(),task.agent_id.to_string(),data,integer(commit.seq.0)?,created_at]).map_err(storage_error)?;
            tx.execute("INSERT INTO inbox_events(event_id,agent_id,kind,source,consumed,data,created_at) VALUES(?1,?2,'task_result','native-child',0,?3,?4)",
                params![result.result_id.to_string(),task.agent_id.to_string(),data,created_at]).map_err(storage_error)?;
        }
        Ok((
            vec![UpdatedEntity {
                entity_id: task.task_id,
                record_revision: task.record_revision,
            }],
            result,
        ))
    }

    fn read_native_transition(
        &self,
        reference: &ObjectRef,
    ) -> Result<NativeT2Transition, ShenkoError> {
        if !matches!(reference.codec.major, 1..=6) {
            return Err(invalid("unsupported transition type or size"));
        }
        crate::native_facts::typed_json(reference, CODEC, reference.codec.major)?;
        let bytes = self.cas.read_object(&reference.digest)?;
        if bytes.len() as u64 != reference.bytes || Digest::blake3(&bytes) != reference.digest {
            return Err(invalid("transition bytes differ"));
        }
        let transition: NativeT2Transition = serde_json::from_slice(&bytes).map_err(invalid)?;
        if matches!(reference.codec.major, 2..=4 | 6) != transition.evidence_fact.is_some()
            || transition.rows.schema_version != match reference.codec.major {
                6 => NATIVE_DIRECT_COMPLETION_READ_SCHEMA,
                5 => NATIVE_ANCESTRY_READ_SCHEMA,
                4 => NATIVE_OPERATION_READ_SCHEMA,
                3 => NATIVE_SUBJECT_READ_SCHEMA,
                _ => 1,
            }
        {
            return Err(invalid(
                "T2 reader version differs from its original E1 proof",
            ));
        }
        self.verify_transition_objects(&transition.rows)?;
        self.verify_native_evidence(&transition.plan)?;
        Ok(transition)
    }

    fn verify_native_continuation(
        &self,
        conn: &Connection,
        fact: &CanonicalFact,
        transition: &NativeT2Transition,
    ) -> Result<(), ShenkoError> {
        let Some(reference) = &transition.evidence_fact else {
            return Ok(());
        };
        let (evidence, original) = self.read_native_evidence_fact(conn, reference)?;
        if !same(&original.task, &transition.plan.task)?
            || !same(&evidence.operation, &transition.plan.operation)?
            || !same(&evidence.attempt, &transition.plan.attempt)?
            || evidence.receipt != transition.plan.evidence
        {
            return Err(invalid(
                "direct completion differs from the original paired T1 and E1",
            ));
        }
        self.verify_direct_native_reads(
            fact,
            &transition.rows,
            &transition.plan.task,
            &original.creator,
            true,
        )?;
        Ok(())
    }

    pub(crate) fn apply_native_transition(
        &self,
        reference: &ObjectRef,
        authority: &AuthorityReservation,
        mode: NativeApplyMode,
    ) -> Result<Option<TaskResult>, ShenkoError> {
        let admission = authority
            .admission()
            .ok_or_else(|| invalid("missing reserved T2 admission"))?;
        let _phase = crate::timing::Phase::bound("canonical.apply", crate::timing::Binding {
            subject_id: Some(admission.subject_id), action_id: Some(admission.action_id),
            operation_id: admission.operation_id, attempt_id: admission.attempt_id,
            ..Default::default()
        });
        let transition = self.read_native_transition(reference)?;
        transition.validate(admission)?;
        let envelope: serde_json::Value =
            serde_json::from_slice(&self.cas.read_object(&admission.payload_ref.digest)?)?;
        if envelope["action"]
            != serde_json::json!({"kind":transition.plan.phase.kind(),"task_id":transition.plan.task.task_id,"transition_ref":reference})
            || envelope["head"] != serde_json::to_value(&transition.rows.original_head)?
            || envelope["source_ledger_id"]
                != serde_json::to_value(transition.rows.source_ledger_id)?
        {
            return Err(invalid("T2 action differs from its admitted envelope"));
        }
        if transition.rows.schema_version == NATIVE_ANCESTRY_READ_SCHEMA {
            let ancestry = crate::native_facts::validate_native_ancestry(&transition.rows, &transition.plan.task, true)?;
            crate::native_facts::validate_native_ancestry_envelope(&envelope, &ancestry)?;
        }
        authority.with_mutation(&self.ledger,
            || {
                let mut conn = crate::timing::measure("sqlite.mutex_wait", || self.ledger.conn.lock());
                let tx = crate::timing::immediate(&mut conn)
                    .map_err(storage_error)?;
                match mode {
                    NativeApplyMode::Learned => authority.check_materialization(&tx)?,
                    NativeApplyMode::Current => {
                        authority.check(
                            &tx,
                            self.capabilities.incarnation,
                            Some(admission.subject_id),
                        )?;
                        self.check_dispatch_owner(&tx).map_err(source_error)?;
                    }
                    NativeApplyMode::Prepared => authority.check(
                        &tx,
                        self.capabilities.incarnation,
                        Some(admission.subject_id),
                    )?,
                }
                if admission_materialized(&tx, admission)? {
                    return Err(conflict("T2 already materialized"));
                }
                if transition.rows.schema_version == NATIVE_ANCESTRY_READ_SCHEMA {
                    crate::child::ensure_owner_active(&tx, &transition.plan.task, &transition.rows.original_head)
                        .map_err(source_error)?;
                }
                self.verify_native_continuation(
                    &tx,
                    &CanonicalFact {
                        admission: admission.clone(),
                        transition_ref: reference.clone(),
                    },
                    &transition,
                )?;
                if transition.evidence_fact.is_some()
                    && crate::native_facts::has_ordinary_children(
                        &tx,
                        transition.plan.task.task_id,
                    )?
                {
                    return Err(conflict("direct completion has local ordinary owned work"));
                }
                let local_commit =
                    CommitRef::new(self.ledger.ledger_id(), self.ledger.allocate_seq(&tx)?);
                let evidence_written = if let Some(reference) = &transition.evidence_fact {
                    self.materialize_native_evidence(&tx, reference, local_commit)?
                } else {
                    false
                };
                transition.rows.apply_rows(&tx, local_commit.seq)?;
                authority.record_canonical_admission(
                    &tx,
                    local_commit,
                    transition.rows.result_commit,
                    reference,
                    transition.rows.created_at,
                )?;
                self.record_subject_commit(
                    &tx,
                    local_commit,
                    (if transition.result.is_some() { 3 } else { 1 })
                        + if evidence_written { 3 } else { 0 },
                )?;
                crate::timing::measure("sqlite.commit", || tx.commit()).map_err(commit_error)?;
                authority.materialized();
                Ok(transition.result.clone())
            },
            |error| matches!(error, ShenkoError::Storage(_)),
        )
    }

    pub(crate) fn recover_native_transition(
        &self,
        grant: &CommitAuthorityGrant,
        reference: &ObjectRef,
    ) -> Result<Option<TaskResult>, ShenkoError> {
        let authority = self.reserve_recovered_admission(grant.clone(), Vec::new(), Vec::new())?;
        self.apply_native_transition(reference, &authority, NativeApplyMode::Learned)
    }
}
