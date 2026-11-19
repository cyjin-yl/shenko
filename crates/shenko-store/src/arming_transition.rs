//! Canonical T1 for already claimed non-inference native tasks.
//!
//! The original request, operation, attempt and invocation are frozen before
//! choosing an admission. Recovery materializes that same intent, never a new
//! permission to execute it. Publication, model quota and task claim stay in
//! their existing protocols.
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    authority::{CommitAdmission, CommitAuthorityGrant},
    commit::{CommitRef, UpdatedEntity},
    error::ShenkoError,
    ids::{EntityId, RecordRevision},
    object::{Digest, ObjectRef},
    operation::{AttemptDispatchStage, AttemptRecord, OperationRecord, OperationState},
    subject::{SubjectBranchHead, SubjectMode, SubjectStep},
    task::{
        DispatchEligibility, ExecutionAssignment, RunMode, TaskInvocation, TaskRecord, TaskState,
    },
};

use crate::{
    Store,
    authority::{AuthorityReservation, admission_materialized},
    child::{
        COGNITION_IMPORTED, NATIVE_ASSIGNMENT, NATIVE_CHILD, NATIVE_OPERATION_ID,
        NATIVE_SETTLEMENT_ONLY, memo_flag, memo_uuid,
    },
    control_transition::{
        Cell, MAX_IMAGE_BYTES, PreparedTransition, RowTransition, RowTransitionInput,
        TableTransition,
    },
    native_facts::{CanonicalFact, narrow_task_read, validate_task_read},
    store::storage_error,
};

const CODEC: &str = "store-native-arm-transition";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArmPlan {
    invocation: TaskInvocation,
    task: TaskRecord,
    operation: OperationRecord,
    attempt: AttemptRecord,
    native_dispatch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArmTransition {
    rows: RowTransition,
    plan: ArmPlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    canonical_claim: Option<CanonicalFact>,
}

pub(crate) struct NativeArmFacts {
    pub task: TaskRecord,
    pub operation: OperationRecord,
    pub attempt: AttemptRecord,
    pub creator: SubjectStep,
}

pub(crate) enum ArmApplyMode {
    /// Only the live call that inserted the preparation can construct this.
    Current,
    Prepared,
    Learned,
}

pub(crate) struct ArmedMaterialization {
    operation: OperationRecord,
    attempt: AttemptRecord,
    fresh: bool,
}

fn invalid(reason: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::other(format!("invalid canonical native arm: {reason}"))
}

fn conflict(reason: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::conflict(format!("canonical native arm: {reason}"))
}

fn source_error(error: anyhow::Error) -> ShenkoError {
    match error.downcast::<ShenkoError>() {
        Ok(error) => error,
        Err(error) if error.is::<rusqlite::Error>() || error.is::<serde_json::Error>() => {
            storage_error(error)
        }
        Err(error) => conflict(error),
    }
}

fn same(left: &impl Serialize, right: &impl Serialize) -> Result<bool, ShenkoError> {
    Ok(serde_json::to_value(left)? == serde_json::to_value(right)?)
}

impl ArmPlan {
    fn position(&self) -> String {
        format!(
            "task/{}/operation/{}",
            self.task.task_id,
            Digest::blake3(self.operation.logical_key.as_bytes())
        )
    }

    fn input_digest(&self, head: &SubjectBranchHead) -> Result<Digest, ShenkoError> {
        Ok(Digest::blake3(&serde_json::to_vec(&(head, self))?))
    }

    fn validate(&self, head: &SubjectBranchHead) -> Result<(), ShenkoError> {
        let ctx = &self.invocation;
        let task = &self.task;
        let operation = &self.operation;
        let attempt = &self.attempt;
        // These are the version-1 records defined by the durable-state
        // contract. Deserializing familiar fields is not permission to
        // interpret a future record schema as the same arming action.
        if task.record_schema_version != 1
            || operation.record_schema_version != 1
            || attempt.record_schema_version != 1
        {
            return Err(invalid(
                "unsupported task, operation or attempt record schema",
            ));
        }
        if task.agent_id != head.agent_id
            || task.task_id != ctx.task_id
            || task.task_revision != ctx.task_revision
            || task.record_revision != ctx.record_revision
            || task.abort_request.is_some()
            || task.memos.contains_key(COGNITION_IMPORTED)
            || task.memos.contains_key(NATIVE_SETTLEMENT_ONLY)
            || !memo_flag(task, NATIVE_CHILD)
            || task.kind == crate::external::MODEL_CHILD_KIND
            || task.dispatch_eligibility != DispatchEligibility::Ready
            || !matches!(head.mode, SubjectMode::Active | SubjectMode::Paused)
            || ctx.epoch != head.controller_epoch
            || ctx.software_generation != head.software_generation
            || !matches!(task.state, TaskState::Running { invocation_id, epoch, mode: RunMode::Run, .. }
                if invocation_id == ctx.invocation_id && epoch == ctx.epoch)
            || operation.task_id != task.task_id
            || operation.state != OperationState::InFlight
            || operation.outcome.is_some()
            || operation.evidence_through.is_some()
            || attempt.operation_id != operation.operation_id
            || attempt.invocation_id != ctx.invocation_id
            || attempt.worker_id != ctx.worker_id
            || attempt.node_id != ctx.node_id
            || attempt.epoch != ctx.epoch
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
            || attempt.final_receipt_id.is_some()
        {
            return Err(invalid("task and original unarmed request do not agree"));
        }
        let assignment: ExecutionAssignment = serde_json::from_value(
            task.memos
                .get(NATIVE_ASSIGNMENT)
                .cloned()
                .ok_or_else(|| invalid("native assignment missing"))?,
        )
        .map_err(invalid)?;
        if assignment.invocation_id != ctx.invocation_id
            || assignment.epoch != ctx.epoch
            || assignment.worker_id != ctx.worker_id
            || assignment.node_id != ctx.node_id
            || assignment.implementation_ref != task.implementation_ref
        {
            return Err(invalid(
                "original invocation differs from its claimed assignment",
            ));
        }
        if crate::child::has_native_dispatch(task, ctx, operation.operation_id, attempt.attempt_id)
            .map_err(invalid)?
        {
            return Err(invalid("unarmed attempt already has a dispatch decision"));
        }
        Ok(())
    }

    fn projection(
        &self,
        commit: CommitRef,
    ) -> Result<(TaskRecord, OperationRecord, AttemptRecord), ShenkoError> {
        let mut task = self.task.clone();
        let mut operation = self.operation.clone();
        let mut attempt = self.attempt.clone();
        if self.native_dispatch {
            crate::child::insert_native_dispatch_marker(
                &mut task,
                &self.invocation,
                operation.operation_id,
                attempt.attempt_id,
                commit,
            )
            .map_err(invalid)?;
        }
        operation.prepared_commit = commit;
        attempt.armed_commit = commit;
        task.record_revision = RecordRevision(
            task.record_revision
                .0
                .checked_add(1)
                .ok_or_else(|| invalid("task revision exhausted"))?,
        );
        task.updated_commit = commit;
        task.memos.insert(
            "external:last_operation".into(),
            serde_json::json!({
                "operation_id":operation.operation_id,"attempt_id":attempt.attempt_id,
            }),
        );
        Ok((task, operation, attempt))
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

fn json_row<T: serde::de::DeserializeOwned>(
    table: &TableTransition,
    row: &[Cell],
) -> Result<T, ShenkoError> {
    let Cell::Text(data) = column(table, row, "data")? else {
        return Err(invalid("original row is not JSON"));
    };
    serde_json::from_str(data).map_err(invalid)
}

impl ArmTransition {
    fn read_table(&self, name: &str) -> Result<&TableTransition, ShenkoError> {
        let tables = self
            .rows
            .tables
            .iter()
            .filter(|table| table.before.table.name == name)
            .collect::<Vec<_>>();
        let [table] = tables.as_slice() else {
            return Err(invalid(format!("missing unique {name} read condition")));
        };
        if name == "tasks" && self.canonical_claim.is_some() {
            validate_task_read(table, &self.plan.task)?;
        } else if !table.complete_read {
            return Err(invalid(format!("incomplete {name} read condition")));
        }
        Ok(table)
    }

    fn original(
        &self,
        name: &str,
        id_column: &str,
        id: EntityId,
        expected: &impl Serialize,
    ) -> Result<(), ShenkoError> {
        let table = self.read_table(name)?;
        let identity = Cell::Text(id.to_string());
        let rows = table
            .before
            .rows
            .values()
            .filter(|row| column(table, row, id_column).is_ok_and(|cell| cell == &identity))
            .collect::<Vec<_>>();
        let [row] = rows.as_slice() else {
            return Err(invalid("missing unique original row"));
        };
        if json_row::<serde_json::Value>(table, row)? != serde_json::to_value(expected)? {
            return Err(invalid(
                "original row differs from the named T1 predecessor",
            ));
        }
        Ok(())
    }

    fn validate_absence(&self) -> Result<(), ShenkoError> {
        let plan = &self.plan;
        let operations = self.read_table("operations")?;
        for row in operations.before.rows.values() {
            let original: OperationRecord = json_row(operations, row)?;
            if column(operations, row, "operation_id")?
                == &Cell::Text(plan.operation.operation_id.to_string())
                || original.operation_id == plan.operation.operation_id
                || (original.task_id == plan.task.task_id
                    && original.logical_key == plan.operation.logical_key)
            {
                return Err(invalid(
                    "T1 cannot replace an original operation or logical key",
                ));
            }
        }
        let attempts = self.read_table("attempts")?;
        for row in attempts.before.rows.values() {
            let original: AttemptRecord = json_row(attempts, row)?;
            if column(attempts, row, "attempt_id")?
                == &Cell::Text(plan.attempt.attempt_id.to_string())
                || original.attempt_id == plan.attempt.attempt_id
                || original.operation_id == plan.operation.operation_id
            {
                return Err(invalid("T1 cannot replace an original attempt"));
            }
        }
        let intents = self.read_table("subject_intents")?;
        for row in intents.before.rows.values() {
            if column(intents, row, "intent_id")? == &Cell::Text(plan.task.task_id.to_string())
                && column(intents, row, "kind")? == &Cell::Text("publication".into())
            {
                return Err(invalid("publication arming has its own protocol"));
            }
        }
        Ok(())
    }

    /// Only these three named writes are allowed. The sealed request is not a
    /// general SQL capability and cannot update an older attempt or receipt.
    fn validate(&self, admission: &CommitAdmission) -> Result<(), ShenkoError> {
        let rows = &self.rows;
        let head = &rows.original_head;
        let plan = &self.plan;
        plan.validate(head)?;
        if self.canonical_claim.is_some()
            && (!plan.native_dispatch
                || memo_uuid(&plan.task, NATIVE_OPERATION_ID) != Some(plan.operation.operation_id))
        {
            return Err(invalid(
                "direct claim continuation must arm its original operation and dispatch together",
            ));
        }
        let position = plan.position();
        if rows.schema_version != 1
            || rows.action_id != admission.action_id
            || rows.action_id
                != EntityId::derived("commit-admission-action-v1", position.as_bytes())
            || rows.input_digest != plan.input_digest(head)?
            || rows.source_ledger_id != rows.result_commit.ledger_id
            || admission.subject_id != head.agent_id
            || admission.branch_id != head.branch_id
            || admission.controller_epoch != head.controller_epoch
            || admission.software_generation != head.software_generation
            || admission.conflict_key != position
            || admission.expected_parent.is_some()
            || admission.expected_head_revision.is_some()
            || admission.operation_id != Some(plan.operation.operation_id)
            || admission.attempt_id != Some(plan.attempt.attempt_id)
            || !admission
                .conflict_aliases
                .contains(&crate::authority::task_revision_position(&plan.task))
        {
            return Err(invalid("transition does not bind the original T1 position"));
        }
        self.original("subject_branch_heads", "agent_id", head.agent_id, head)?;
        self.original("tasks", "task_id", plan.task.task_id, &plan.task)?;
        self.validate_absence()?;
        let (task, operation, attempt) = plan.projection(rows.result_commit)?;
        let expected_entities = [
            (task.task_id, task.record_revision),
            (operation.operation_id, operation.record_revision),
            (attempt.attempt_id, RecordRevision(0)),
        ];
        if rows.updated.len() != expected_entities.len()
            || rows
                .updated
                .iter()
                .zip(expected_entities)
                .any(|(actual, (id, revision))| {
                    actual.entity_id != id || actual.record_revision != revision
                })
        {
            return Err(invalid(
                "updated entities differ from original T1 identities",
            ));
        }
        let mut changed = BTreeMap::new();
        for table in &rows.tables {
            for change in &table.changes {
                let name = table.before.table.name.as_str();
                let expected = match name {
                    "tasks" => {
                        let before = change
                            .before
                            .as_ref()
                            .ok_or_else(|| invalid("T1 cannot create a task"))?;
                        if column(table, before, "task_id")?
                            != &Cell::Text(task.task_id.to_string())
                        {
                            return Err(invalid("T1 changed another task"));
                        }
                        let mut expected = table
                            .before
                            .table
                            .columns
                            .iter()
                            .cloned()
                            .zip(before.iter().cloned())
                            .collect::<BTreeMap<_, _>>();
                        expected.insert("data".into(), Cell::Text(serde_json::to_string(&task)?));
                        expected.insert(
                            "record_revision".into(),
                            Cell::Integer(i64::try_from(task.record_revision.0).map_err(invalid)?),
                        );
                        expected.insert("updated_commit_seq".into(), Cell::LocalSequence);
                        expected
                    }
                    "operations" => {
                        if change.before.is_some() {
                            return Err(invalid("T1 cannot update an existing operation"));
                        }
                        BTreeMap::from([
                            (
                                "operation_id".into(),
                                Cell::Text(operation.operation_id.to_string()),
                            ),
                            (
                                "record_revision".into(),
                                Cell::Integer(
                                    i64::try_from(operation.record_revision.0).map_err(invalid)?,
                                ),
                            ),
                            (
                                "data".into(),
                                Cell::Text(serde_json::to_string(&operation)?),
                            ),
                            ("updated_commit_seq".into(), Cell::LocalSequence),
                            (
                                "created_at".into(),
                                Cell::Text(rows.created_at.to_rfc3339()),
                            ),
                        ])
                    }
                    "attempts" => {
                        if change.before.is_some() {
                            return Err(invalid("T1 cannot update an existing attempt"));
                        }
                        BTreeMap::from([
                            (
                                "attempt_id".into(),
                                Cell::Text(attempt.attempt_id.to_string()),
                            ),
                            (
                                "operation_id".into(),
                                Cell::Text(operation.operation_id.to_string()),
                            ),
                            ("data".into(), Cell::Text(serde_json::to_string(&attempt)?)),
                            ("updated_commit_seq".into(), Cell::LocalSequence),
                            (
                                "created_at".into(),
                                Cell::Text(rows.created_at.to_rfc3339()),
                            ),
                        ])
                    }
                    _ => return Err(invalid("T1 wrote outside its task, operation and attempt")),
                };
                if expected.len() != table.before.table.columns.len() {
                    return Err(invalid("unexpected T1 result columns"));
                }
                let expected = table
                    .before
                    .table
                    .columns
                    .iter()
                    .map(|name| {
                        expected
                            .get(name)
                            .cloned()
                            .ok_or_else(|| invalid("unknown T1 result column"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if change.after.as_ref() != Some(&expected) {
                    return Err(invalid("T1 write differs from its frozen request"));
                }
                *changed.entry(name).or_insert(0) += 1;
            }
        }
        if changed != BTreeMap::from([("tasks", 1), ("operations", 1), ("attempts", 1)]) {
            return Err(invalid("missing or repeated T1 write"));
        }
        Ok(())
    }
}

impl Store {
    pub(crate) fn canonical_native_arm_applicable(&self, task: EntityId) -> anyhow::Result<bool> {
        if !self.has_commit_authority() {
            return Ok(false);
        }
        let conn = self.ledger.conn.lock();
        let Some(task) = crate::child::read_task(&conn, task)? else {
            return Ok(false);
        };
        if !memo_flag(&task, NATIVE_CHILD) || task.kind == crate::external::MODEL_CHILD_KIND {
            return Ok(false);
        }
        let publication: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM subject_intents WHERE intent_id=?1 AND kind='publication')",
            [task.task_id.to_string()], |row| row.get(0))?;
        Ok(!publication)
    }

    pub(crate) fn canonical_native_arm(
        &self,
        invocation: &TaskInvocation,
        operation: &mut OperationRecord,
        attempt: &mut AttemptRecord,
        native_dispatch: bool,
    ) -> Result<Option<OperationRecord>, ShenkoError> {
        let (head, mut position) =
            self.task_admission_position(invocation.task_id, String::new())?;
        let task = position
            .expected_tasks
            .first()
            .cloned()
            .ok_or_else(|| conflict("task missing"))?;
        let plan = ArmPlan {
            invocation: invocation.clone(),
            task,
            operation: operation.clone(),
            attempt: attempt.clone(),
            native_dispatch,
        };
        plan.validate(&head)
            .map_err(|error| conflict(format!("source precondition changed: {error}")))?;
        position.key = plan.position();
        position.operation_id = Some(plan.operation.operation_id);
        position.attempt_id = Some(plan.attempt.attempt_id);
        let canonical_claim = if native_dispatch
            && memo_uuid(&plan.task, NATIVE_OPERATION_ID) == Some(plan.operation.operation_id)
        {
            let conn = self.ledger.conn.lock();
            self.direct_native_claim(&conn, &plan.task)?
                .map(|(fact, original)| {
                    position.expected_steps.push(original.creator);
                    fact
                })
        } else {
            None
        };
        let expected = position.expected_tasks.clone();
        let steps = position.expected_steps.clone();
        let (prepared, fresh) =
            self.prepare_arm_transition(&head, plan, position, canonical_claim)?;
        let authority = self.reserve_sealed_admission(prepared.admission, expected, steps)?;
        let applied = self.apply_arm_transition(
            &prepared.transition_ref,
            &authority,
            if fresh {
                ArmApplyMode::Current
            } else {
                ArmApplyMode::Prepared
            },
        )?;
        *operation = applied.operation;
        *attempt = applied.attempt;
        Ok((!applied.fresh).then(|| operation.clone()))
    }

    fn prepare_arm_transition(
        &self,
        head: &SubjectBranchHead,
        plan: ArmPlan,
        mut position: crate::authority::AdmissionPosition,
        canonical_claim: Option<CanonicalFact>,
    ) -> Result<(PreparedTransition, bool), ShenkoError> {
        position.aliases.sort();
        position.aliases.dedup();
        position.reads.sort();
        position.reads.dedup();
        let action_id = EntityId::derived("commit-admission-action-v1", position.key.as_bytes());
        let input_digest = plan.input_digest(head)?;
        if let Some(prepared) = self.prepared_transition(action_id, &input_digest)? {
            return Ok((prepared, false));
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
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage_error)?;
            authority.check(&tx, self.capabilities.incarnation, Some(head.agent_id))?;
            if !self.native_invocation_current_tx(&tx, &plan.task, &plan.invocation).map_err(source_error)? {
                return Err(conflict("source invocation was fenced before preparation"));
            }
            if let Some(prepared) = crate::control_transition::prepared_transition_at(&tx, action_id, &input_digest)? { return Ok((prepared, false)); }
            // Local dispatcher/authority checks above do not become canonical
            // facts. The captured transaction contains shared state only.
            let (mut rows, ()) = self.capture_row_transition(&tx, RowTransitionInput { action_id, input_digest: input_digest.clone(), head },
                canonical_claim.as_ref().map(|_| &plan.task),
                |commit, created_at| self.evaluate_arm(&tx, head, &plan, commit, created_at))?;
            if canonical_claim.is_some() { narrow_task_read(&mut rows, &plan.task)?; }
            let transition = ArmTransition { rows, plan: plan.clone(), canonical_claim: canonical_claim.clone() };
            let result_commit = transition.rows.result_commit;
            let bytes = serde_json::to_vec(&transition)?;
            if bytes.len() > MAX_IMAGE_BYTES { return Err(conflict("T1 exceeds the 64 MiB admission budget")); }
            let transition_ref = self.cas.seal_object(&bytes, "application/json", "commit-authority", CODEC,
                if canonical_claim.is_some() { 2 } else { 1 })?.object;
            let payload_ref = self.cas.seal_object(&serde_json::to_vec(&serde_json::json!({
                "schema_version":1,"kind":"store.control_admission","head":head,
                "source_ledger_id":self.ledger.ledger_id(),"predecessor_ref":predecessor_ref,
                "expected_tasks":position.expected_tasks,"snapshot_tasks":position.snapshot_tasks,"expected_steps":position.expected_steps,
                "action":{"kind":"native.arm","task_id":plan.task.task_id,"transition_ref":transition_ref},
            }))?, "application/json", "commit-authority", "commit-admission", 1)?.object;
            let admission = CommitAdmission {
                schema_version:1,subject_id:head.agent_id,branch_id:head.branch_id.clone(),action_id,
                conflict_key:position.key.clone(),conflict_aliases:position.aliases.clone(),required_unconsumed:position.reads.clone(),
                expected_parent:None,expected_head_revision:None,controller_epoch:head.controller_epoch,
                software_generation:head.software_generation.clone(),operation_id:position.operation_id,attempt_id:position.attempt_id,payload_ref,
            };
            transition.validate(&admission)?;
            self.verify_arm_claim(&tx, &CanonicalFact { admission: admission.clone(), transition_ref: transition_ref.clone() }, &transition)?;
            tx.execute("INSERT INTO control_prepared_transitions(action_id,input_digest,admission,result_commit,transition_ref) VALUES(?1,?2,?3,?4,?5)",
                params![action_id.to_string(),input_digest.0,serde_json::to_string(&admission)?,serde_json::to_string(&result_commit)?,serde_json::to_string(&transition_ref)?]).map_err(storage_error)?;
            self.record_subject_commit(&tx, result_commit, 1)?;
            tx.commit().map_err(storage_error)?;
            Ok((PreparedTransition { admission, transition_ref }, true))
        }, |error| matches!(error, ShenkoError::Storage(_)))
    }

    fn evaluate_arm(
        &self,
        tx: &Transaction<'_>,
        head: &SubjectBranchHead,
        plan: &ArmPlan,
        commit: CommitRef,
        created_at: DateTime<Utc>,
    ) -> Result<(Vec<UpdatedEntity>, ()), ShenkoError> {
        if !same(&crate::store::read_subject(tx, head.agent_id)?.0, head)? {
            return Err(conflict("subject changed before T1"));
        }
        let task = crate::child::read_task(tx, plan.task.task_id)
            .map_err(source_error)?
            .ok_or_else(|| conflict("task disappeared before T1"))?;
        if !same(&task, &plan.task)? {
            return Err(conflict("task changed before T1"));
        }
        crate::child::ensure_owner_active(tx, &task, head).map_err(source_error)?;
        let publication: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM subject_intents WHERE intent_id=?1 AND kind='publication')",
            [task.task_id.to_string()], |row| row.get(0)).map_err(storage_error)?;
        if publication {
            return Err(conflict("publication arming has its own protocol"));
        }
        let operation_exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE operation_id=?1 OR (json_extract(data,'$.task_id')=?2 AND json_extract(data,'$.logical_key')=?3))",
            params![plan.operation.operation_id.to_string(),task.task_id.to_string(),plan.operation.logical_key], |row| row.get(0)).map_err(storage_error)?;
        let attempt_exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM attempts WHERE attempt_id=?1 OR operation_id=?2)",
                params![
                    plan.attempt.attempt_id.to_string(),
                    plan.operation.operation_id.to_string()
                ],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        if operation_exists || attempt_exists {
            return Err(conflict("original operation or attempt already exists"));
        }
        let (task, operation, attempt) = plan.projection(commit)?;
        let sequence = i64::try_from(commit.seq.0).map_err(invalid)?;
        tx.execute("INSERT INTO operations(operation_id,record_revision,data,updated_commit_seq,created_at) VALUES(?1,?2,?3,?4,?5)",
            params![operation.operation_id.to_string(),i64::try_from(operation.record_revision.0).map_err(invalid)?,serde_json::to_string(&operation)?,sequence,created_at.to_rfc3339()]).map_err(storage_error)?;
        tx.execute("INSERT INTO attempts(attempt_id,operation_id,data,updated_commit_seq,created_at) VALUES(?1,?2,?3,?4,?5)",
            params![attempt.attempt_id.to_string(),operation.operation_id.to_string(),serde_json::to_string(&attempt)?,sequence,created_at.to_rfc3339()]).map_err(storage_error)?;
        crate::child::write_task(tx, &task).map_err(source_error)?;
        Ok((
            vec![
                UpdatedEntity {
                    entity_id: task.task_id,
                    record_revision: task.record_revision,
                },
                UpdatedEntity {
                    entity_id: operation.operation_id,
                    record_revision: operation.record_revision,
                },
                UpdatedEntity {
                    entity_id: attempt.attempt_id,
                    record_revision: RecordRevision(0),
                },
            ],
            (),
        ))
    }

    fn read_arm_transition(&self, reference: &ObjectRef) -> Result<ArmTransition, ShenkoError> {
        if !matches!(reference.codec.major, 1 | 2) {
            return Err(invalid("unsupported transition type or size"));
        }
        crate::native_facts::typed_json(reference, CODEC, reference.codec.major)?;
        let bytes = self.cas.read_object(&reference.digest)?;
        if bytes.len() as u64 != reference.bytes || Digest::blake3(&bytes) != reference.digest {
            return Err(invalid("transition bytes differ"));
        }
        let transition: ArmTransition = serde_json::from_slice(&bytes).map_err(invalid)?;
        if (reference.codec.major == 2) != transition.canonical_claim.is_some() {
            return Err(invalid(
                "T1 reader version differs from its task dependency proof",
            ));
        }
        self.verify_transition_objects(&transition.rows)?;
        Ok(transition)
    }

    fn verify_arm_claim(
        &self,
        conn: &Connection,
        fact: &CanonicalFact,
        transition: &ArmTransition,
    ) -> Result<Option<SubjectStep>, ShenkoError> {
        let Some(claim) = &transition.canonical_claim else {
            return Ok(None);
        };
        let original = self.verify_native_claim_fact(conn, claim)?;
        if !same(&original.claim.task, &transition.plan.task)?
            || !same(&original.claim.invocation, &transition.plan.invocation)?
            || original.claim.operation_id != transition.plan.operation.operation_id
        {
            return Err(invalid("T1 changed the original first claim"));
        }
        self.verify_direct_native_reads(
            fact,
            &transition.rows,
            &transition.plan.task,
            &original.creator,
            false,
        )?;
        Ok(Some(original.creator))
    }

    pub(crate) fn verify_native_arm_fact(
        &self,
        conn: &Connection,
        fact: &CanonicalFact,
    ) -> Result<NativeArmFacts, ShenkoError> {
        let transition = self.read_arm_transition(&fact.transition_ref)?;
        transition.validate(&fact.admission)?;
        let envelope: serde_json::Value =
            self.read_native_json(&fact.admission.payload_ref, "commit-admission", 1)?;
        if envelope["action"]
            != serde_json::json!({"kind":"native.arm", "task_id":transition.plan.task.task_id,
            "transition_ref":fact.transition_ref})
        {
            return Err(invalid("retained T1 action differs"));
        }
        let creator = self
            .verify_arm_claim(conn, fact, &transition)?
            .ok_or_else(|| conflict("T1 has no original direct claim proof"))?;
        self.verify_local_native_fact(conn, fact, transition.rows.result_commit)?;
        let (task, operation, attempt) =
            transition.plan.projection(transition.rows.result_commit)?;
        Ok(NativeArmFacts {
            task,
            operation,
            attempt,
            creator,
        })
    }

    pub(crate) fn apply_arm_transition(
        &self,
        reference: &ObjectRef,
        authority: &AuthorityReservation,
        mode: ArmApplyMode,
    ) -> Result<ArmedMaterialization, ShenkoError> {
        let admission = authority
            .admission()
            .ok_or_else(|| invalid("missing T1 admission"))?;
        let transition = self.read_arm_transition(reference)?;
        transition.validate(admission)?;
        let envelope: serde_json::Value =
            serde_json::from_slice(&self.cas.read_object(&admission.payload_ref.digest)?)?;
        if envelope["action"]
            != serde_json::json!({"kind":"native.arm","task_id":transition.plan.task.task_id,"transition_ref":reference})
            || envelope["head"] != serde_json::to_value(&transition.rows.original_head)?
            || envelope["source_ledger_id"]
                != serde_json::to_value(transition.rows.source_ledger_id)?
        {
            return Err(invalid("T1 action differs from its admitted envelope"));
        }
        authority.with_mutation(&self.ledger,
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                if admission_materialized(&tx, admission)? {
                    // A late E1 may have advanced these records. Read it, never
                    // restore the frozen InFlight/Armed projection over that fact.
                    let operation: String = tx
                        .query_row(
                            "SELECT data FROM operations WHERE operation_id=?1",
                            [transition.plan.operation.operation_id.to_string()],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(storage_error)?
                        .ok_or_else(|| invalid("locally materialized operation missing"))?;
                    let attempt: String = tx
                        .query_row(
                            "SELECT data FROM attempts WHERE attempt_id=?1",
                            [transition.plan.attempt.attempt_id.to_string()],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(storage_error)?
                        .ok_or_else(|| invalid("locally materialized attempt missing"))?;
                    let operation: OperationRecord = serde_json::from_str(&operation)?;
                    let attempt: AttemptRecord = serde_json::from_str(&attempt)?;
                    if operation.operation_id != transition.plan.operation.operation_id
                        || operation.task_id != transition.plan.task.task_id
                        || operation.prepared_commit != transition.rows.result_commit
                        || attempt.attempt_id != transition.plan.attempt.attempt_id
                        || attempt.operation_id != operation.operation_id
                        || attempt.invocation_id != transition.plan.invocation.invocation_id
                        || attempt.armed_commit != transition.rows.result_commit
                    {
                        return Err(invalid("locally materialized T1 identity differs"));
                    }
                    tx.commit().map_err(storage_error)?;
                    authority.materialized();
                    return Ok(ArmedMaterialization {
                        operation,
                        attempt,
                        fresh: false,
                    });
                }
                match mode {
                    ArmApplyMode::Learned => authority.check_materialization(&tx)?,
                    ArmApplyMode::Prepared => authority.check(
                        &tx,
                        self.capabilities.incarnation,
                        Some(admission.subject_id),
                    )?,
                    ArmApplyMode::Current => {
                        authority.check(
                            &tx,
                            self.capabilities.incarnation,
                            Some(admission.subject_id),
                        )?;
                        if transition.rows.source_ledger_id != self.ledger.ledger_id() {
                            return Err(invalid("fresh T1 is not its original source ledger"));
                        }
                        let task = crate::child::read_task(&tx, transition.plan.task.task_id)
                            .map_err(source_error)?
                            .ok_or_else(|| conflict("current T1 task disappeared"))?;
                        if !self
                            .native_invocation_current_tx(&tx, &task, &transition.plan.invocation)
                            .map_err(source_error)?
                        {
                            return Err(conflict("current T1 invocation was fenced"));
                        }
                    }
                }
                self.verify_arm_claim(
                    &tx,
                    &CanonicalFact {
                        admission: admission.clone(),
                        transition_ref: reference.clone(),
                    },
                    &transition,
                )?;
                let local_commit =
                    CommitRef::new(self.ledger.ledger_id(), self.ledger.allocate_seq(&tx)?);
                transition.rows.apply_rows(&tx, local_commit.seq)?;
                authority.record_canonical_admission(
                    &tx,
                    local_commit,
                    transition.rows.result_commit,
                    reference,
                    transition.rows.created_at,
                )?;
                self.record_subject_commit(&tx, local_commit, 3)?;
                let (_, operation, attempt) =
                    transition.plan.projection(transition.rows.result_commit)?;
                tx.commit().map_err(storage_error)?;
                authority.materialized();
                Ok(ArmedMaterialization {
                    operation,
                    attempt,
                    fresh: matches!(mode, ArmApplyMode::Current)
                        && authority.grant().is_none_or(|grant| !grant.replayed),
                })
            },
            |error| matches!(error, ShenkoError::Storage(_)),
        )
    }

    pub(crate) fn recover_arm_transition(
        &self,
        grant: &CommitAuthorityGrant,
        reference: &ObjectRef,
    ) -> Result<ArmedMaterialization, ShenkoError> {
        let authority = self.reserve_recovered_admission(grant.clone(), Vec::new(), Vec::new())?;
        self.apply_arm_transition(reference, &authority, ArmApplyMode::Learned)
    }
}
