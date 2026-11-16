//! First claim of a native delegation born in a locally materialized canonical
//! S1, admitted by quorum choice or the existing local availability authority.
//! A grant-free local admission is not proof of quorum choice. Learned claim
//! application still requires a verified chosen grant and this Store's S1 fact.
//!
//! This narrow reader knows that the original delegation is directly owned by
//! the subject root. Its shared task dependency is exactly that delegation;
//! the root revision remains a Node read fence and a source-only live check.
//! Imported parking is never undone. A learned or prepared Running projection
//! is a fact, not a permit to start the original worker.
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    authority::{CommitAdmission, CommitAuthorityGrant},
    commit::{CommitRef, UpdatedEntity},
    error::ShenkoError,
    ids::{EntityId, RecordRevision},
    object::{Digest, ObjectRef},
    subject::{StepDecision, SubjectBranchHead, SubjectMode, SubjectStep, SubjectStepState},
    task::{
        DispatchEligibility, ExecutionAssignment, ImplementationRef, RunMode, TaskInvocation,
        TaskOwner, TaskRecord, TaskState,
    },
};

use crate::{
    NativeTaskClaim, Store,
    authority::{
        AuthorityReservation, admission_applied, admission_materialized, task_revision_position,
    },
    child::{
        COGNITION_IMPORTED, NATIVE_ASSIGNMENT, NATIVE_CHILD, NATIVE_INPUT, NATIVE_OPERATION_ID,
        NATIVE_RESULT_ID, native_claim_ids,
    },
    control_transition::{
        Cell, MAX_IMAGE_BYTES, PreparedTransition, RowTransition, RowTransitionInput,
        TableTransition,
    },
    store::storage_error,
};

const CODEC: &str = "store-native-claim-transition";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct S1Birth {
    admission: CommitAdmission,
    transition_ref: ObjectRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimPlan {
    task: TaskRecord,
    /// Source read fence only. The root is the structural owner boundary, not
    /// ordinary owned work; the receiving installation may retain it parked.
    root: TaskRecord,
    creator: SubjectStep,
    implementation: ImplementationRef,
    invocation: TaskInvocation,
    birth: S1Birth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimTransition {
    rows: RowTransition,
    plan: ClaimPlan,
}

pub(crate) enum ClaimApplyMode {
    Current,
    Prepared,
    Learned,
}

fn invalid(reason: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::other(format!("invalid canonical native claim: {reason}"))
}

fn conflict(reason: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::conflict(format!("canonical native claim: {reason}"))
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

fn admission_type(reference: &ObjectRef) -> Result<(), ShenkoError> {
    if reference.codec.name != "commit-admission"
        || reference.codec.major != 1
        || reference.codec.digest != Digest::blake3(b"commit-admission")
        || reference.store_namespace != "commit-authority"
        || reference.media_type != "application/json"
        || reference.bytes > MAX_IMAGE_BYTES as u64
    {
        return Err(invalid("unsupported claim or birth admission type or size"));
    }
    Ok(())
}

/// All other tasks keep their existing protocol. In particular this does not
/// claim recovery work, child-created tools, models, publications or parked
/// tasks, even if some of their fields resemble a new delegation.
fn first_delegation(task: &TaskRecord) -> bool {
    task.record_schema_version == 1
        && task.record_revision.0 == 0
        && task.task_revision.0 == 0
        && task.created_commit == task.updated_commit
        && task.created_by_step_id.is_some()
        && !task.background
        && task.kind != crate::external::MODEL_CHILD_KIND
        && !matches!(
            task.kind.as_str(),
            "subject_root" | "subject_publication" | "delegation"
        )
        && task.first_run_at.is_none()
        && task.terminal_at.is_none()
        && task.abort_request.is_none()
        && task.no_progress_crashes == 0
        && task.dispatch_eligibility == DispatchEligibility::Ready
        && matches!(
            task.state,
            TaskState::Pending {
                checkpoint_ref: None
            }
        )
        && task
            .memos
            .get(NATIVE_INPUT)
            .and_then(serde_json::Value::as_str)
            == Some("subject_decision")
        && task.memos.len() == 2
        && task.memos.get("intent_id") == Some(&serde_json::json!(task.task_id))
}

impl ClaimPlan {
    fn position(&self) -> String {
        format!(
            "task/{}/claim/{}",
            self.task.task_id, self.task.record_revision.0
        )
    }

    fn input_digest(&self, head: &SubjectBranchHead) -> Result<Digest, ShenkoError> {
        Ok(Digest::blake3(&serde_json::to_vec(&(head, self))?))
    }

    fn validate(&self, head: &SubjectBranchHead) -> Result<(), ShenkoError> {
        let task = &self.task;
        let ctx = &self.invocation;
        if !first_delegation(task)
            || task.memos.contains_key(COGNITION_IMPORTED)
            || task.agent_id != head.agent_id
            || task.conversation_id != head.agent_id
            || task.owner
                != (TaskOwner::Task {
                    id: head.subject_root_task_id,
                })
            || self.root.task_id != head.subject_root_task_id
            || self.root.agent_id != head.agent_id
            || self.root.record_schema_version != 1
            || self.root.kind != "subject_root"
            || head.mode != SubjectMode::Active
            || task.created_by_step_id != Some(self.creator.step_id)
            || self.creator.branch_id != head.branch_id
            || self.creator.state != SubjectStepState::Committed
            || self.creator.commit_ref != Some(task.created_commit)
            || self.creator.decision_ref.as_ref() != Some(&task.input_ref)
            || ctx.task_id != task.task_id
            || ctx.task_revision != task.task_revision
            || ctx.record_revision != RecordRevision(1)
            || ctx.invocation_id != native_claim_ids(task).2
            || ctx.epoch != head.controller_epoch
            || ctx.software_generation != head.software_generation
        {
            return Err(invalid("claim is not the first direct-root S1 delegation"));
        }
        Ok(())
    }

    fn projection(
        &self,
        commit: CommitRef,
        created_at: DateTime<Utc>,
    ) -> Result<NativeTaskClaim, ShenkoError> {
        let mut task = self.task.clone();
        let ctx = self.invocation.clone();
        let (operation_id, result_id, _) = native_claim_ids(&task);
        task.state = TaskState::Running {
            checkpoint_ref: None,
            invocation_id: ctx.invocation_id,
            mode: RunMode::Run,
            epoch: ctx.epoch,
        };
        task.record_revision = ctx.record_revision;
        task.updated_commit = commit;
        task.first_run_at = Some(created_at);
        task.implementation_ref = self.implementation.clone();
        task.memos.insert(NATIVE_CHILD.into(), true.into());
        task.memos
            .insert(NATIVE_OPERATION_ID.into(), serde_json::json!(operation_id));
        task.memos
            .insert(NATIVE_RESULT_ID.into(), serde_json::json!(result_id));
        task.memos.insert(
            NATIVE_ASSIGNMENT.into(),
            serde_json::to_value(ExecutionAssignment {
                worker_id: ctx.worker_id,
                node_id: ctx.node_id,
                invocation_id: ctx.invocation_id,
                epoch: ctx.epoch,
                implementation_ref: self.implementation.clone(),
                assigned_commit: commit,
            })?,
        );
        Ok(NativeTaskClaim {
            task,
            invocation: ctx.clone(),
            operation_id,
            result_id,
            software_generation: ctx.software_generation,
            settlement_only: false,
        })
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
        return Err(invalid("row is not JSON"));
    };
    serde_json::from_str(data).map_err(invalid)
}

fn unique_table<'a>(
    rows: &'a RowTransition,
    name: &str,
) -> Result<&'a TableTransition, ShenkoError> {
    let mut tables = rows
        .tables
        .iter()
        .filter(|table| table.before.table.name == name);
    let table = tables
        .next()
        .ok_or_else(|| invalid(format!("missing {name} dependency")))?;
    if tables.next().is_some() {
        return Err(invalid("repeated dependency table"));
    }
    Ok(table)
}

impl ClaimTransition {
    fn original(
        &self,
        name: &str,
        id_column: &str,
        id: EntityId,
        expected: &impl Serialize,
    ) -> Result<(), ShenkoError> {
        let table = unique_table(&self.rows, name)?;
        if !table.complete_read {
            return Err(invalid(format!("incomplete {name} dependency")));
        }
        let mut rows = table.before.rows.values().filter(|row| {
            column(table, row, id_column).is_ok_and(|value| value == &Cell::Text(id.to_string()))
        });
        let row = rows
            .next()
            .ok_or_else(|| invalid("original dependency is missing"))?;
        if rows.next().is_some()
            || json_row::<serde_json::Value>(table, row)? != serde_json::to_value(expected)?
        {
            return Err(invalid("original dependency differs"));
        }
        Ok(())
    }

    fn validate(&self, admission: &CommitAdmission) -> Result<(), ShenkoError> {
        let rows = &self.rows;
        let plan = &self.plan;
        let head = &rows.original_head;
        plan.validate(head)?;
        let position = plan.position();
        if rows.schema_version != 1
            || admission.schema_version != 1
            || rows.action_id != admission.action_id
            || rows.action_id
                != EntityId::derived("commit-admission-action-v1", position.as_bytes())
            || rows.input_digest != plan.input_digest(head)?
            || rows.source_ledger_id != rows.result_commit.ledger_id
            || plan.invocation.node_id != rows.source_ledger_id
            || admission.subject_id != head.agent_id
            || admission.branch_id != head.branch_id
            || admission.controller_epoch != head.controller_epoch
            || admission.software_generation != head.software_generation
            || admission.conflict_key != position
            || admission.expected_parent.is_some()
            || admission.expected_head_revision.is_some()
            || admission.operation_id.is_some()
            || admission.attempt_id.is_some()
            || admission.conflict_aliases != vec![task_revision_position(&plan.task)]
            || admission.required_unconsumed != vec![task_revision_position(&plan.root)]
        {
            return Err(invalid("claim admission or invocation identity differs"));
        }
        self.original("subject_branch_heads", "agent_id", head.agent_id, head)?;
        self.original(
            "subject_steps",
            "step_id",
            plan.creator.step_id,
            &plan.creator,
        )?;
        let operations = unique_table(rows, "operations")?;
        if !operations.complete_read {
            return Err(invalid("operation absence is not frozen"));
        }
        let operation_id = native_claim_ids(&plan.task).0;
        for row in operations.before.rows.values() {
            if column(operations, row, "operation_id")? == &Cell::Text(operation_id.to_string()) {
                return Err(invalid("first claim already has an original operation"));
            }
        }
        // This closed shape proves there is no omitted intermediate ancestor:
        // the task is directly root-owned, and the root is the structural
        // boundary in task_in_current_branch/task_owner_active. No range query
        // or ordinary child query can be represented by this targeted read.
        let tasks = unique_table(rows, "tasks")?;
        if tasks.complete_read || tasks.before.rows.len() != 1 || tasks.changes.len() != 1 {
            return Err(invalid(
                "first-claim task dependency must be exactly its target",
            ));
        }
        let change = &tasks.changes[0];
        let before = change
            .before
            .as_ref()
            .ok_or_else(|| invalid("claim cannot create a task"))?;
        if tasks.before.rows.get(&change.key) != Some(before)
            || column(tasks, before, "task_id")? != &Cell::Text(plan.task.task_id.to_string())
            || json_row::<serde_json::Value>(tasks, before)? != serde_json::to_value(&plan.task)?
        {
            return Err(invalid(
                "claim target differs from the original Pending task",
            ));
        }
        let claimed = plan.projection(rows.result_commit, rows.created_at)?;
        let mut after = before.clone();
        for (index, name) in tasks.before.table.columns.iter().enumerate() {
            match name.as_str() {
                "data" => after[index] = Cell::Text(serde_json::to_string(&claimed.task)?),
                "record_revision" => after[index] = Cell::Integer(1),
                "updated_commit_seq" => after[index] = Cell::LocalSequence,
                _ => {}
            }
        }
        if change.after.as_ref() != Some(&after)
            || rows
                .tables
                .iter()
                .any(|table| table.before.table.name != "tasks" && !table.changes.is_empty())
            || rows.updated.len() != 1
            || rows.updated[0].entity_id != plan.task.task_id
            || rows.updated[0].record_revision != RecordRevision(1)
        {
            return Err(invalid(
                "claim may only install the exact original Running task",
            ));
        }
        Ok(())
    }
}

impl Store {
    fn claim_birth(
        &self,
        conn: &Connection,
        task: &TaskRecord,
    ) -> Result<Option<S1Birth>, ShenkoError> {
        let mut statement = conn.prepare(
            "SELECT data FROM control_materializations WHERE json_extract(data,'$.logical_result.ledger_id')=?1
             AND json_extract(data,'$.logical_result.seq')=?2",
        ).map_err(storage_error)?;
        let records = statement
            .query_map(
                params![
                    task.created_commit.ledger_id.to_string(),
                    i64::try_from(task.created_commit.seq.0).map_err(invalid)?
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        let Some(raw) = records.first() else {
            return Ok(None);
        };
        if records.len() != 1 {
            return Err(invalid("multiple local receipts claim the task's S1 birth"));
        }
        let value: serde_json::Value = serde_json::from_str(raw).map_err(invalid)?;
        let birth = S1Birth {
            admission: serde_json::from_value(value["admission"].clone()).map_err(invalid)?,
            transition_ref: serde_json::from_value(value["transition_ref"].clone())
                .map_err(invalid)?,
        };
        self.validate_local_claim_birth(conn, task, &birth)?;
        Ok(Some(birth))
    }

    /// Only this Store's nonreplicated application plus its physical commit
    /// proves local S1 birth. An imported applied event or source receipt is
    /// insufficient. Same-installation backups keep the old physical receipt;
    /// accepting that history does not give the copied worker a fresh permit.
    fn validate_local_claim_birth(
        &self,
        conn: &Connection,
        task: &TaskRecord,
        birth: &S1Birth,
    ) -> Result<(), ShenkoError> {
        let row: Option<(String, String, i64)> = conn.query_row(
            "SELECT action_id,data,updated_commit_seq FROM control_materializations WHERE conflict_key=?1",
            [&birth.admission.conflict_key], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().map_err(storage_error)?;
        let Some((action, raw, sequence)) = row else {
            return Err(conflict(
                "the task's canonical S1 has not been materialized on this installation",
            ));
        };
        let value: serde_json::Value = serde_json::from_str(&raw).map_err(invalid)?;
        let admission: CommitAdmission =
            serde_json::from_value(value["admission"].clone()).map_err(invalid)?;
        let physical: CommitRef =
            serde_json::from_value(value["committed_at"].clone()).map_err(invalid)?;
        if action != birth.admission.action_id.to_string()
            || admission != birth.admission
            || value["logical_result"] != serde_json::to_value(task.created_commit)?
            || value["transition_ref"] != serde_json::to_value(&birth.transition_ref)?
            || i64::try_from(physical.seq.0).ok() != Some(sequence)
            || !admission_applied(conn, &birth.admission)?
        {
            return Err(invalid(
                "local S1 birth receipt differs from its original admission/result",
            ));
        }
        let committed: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM commit_log WHERE ledger_id=?1 AND seq=?2)",
                params![physical.ledger_id.to_string(), sequence],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        if !committed {
            return Err(invalid("local S1 birth has no physical commit receipt"));
        }
        let event_id = EntityId::derived(
            "commit-authority-admission-v1",
            birth.admission.conflict_key.as_bytes(),
        );
        let event: String = conn
            .query_row(
                "SELECT data FROM inbox_events WHERE event_id=?1",
                [event_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let event: serde_json::Value = serde_json::from_str(&event).map_err(invalid)?;
        if event["payload"]["committed_at"] != serde_json::to_value(task.created_commit)?
            || event["payload"]["transition_ref"] != serde_json::to_value(&birth.transition_ref)?
        {
            return Err(invalid(
                "shared S1 birth fact has a different logical result",
            ));
        }
        Ok(())
    }

    /// Verify the source's frozen S1 and authored delegation, independently of
    /// the caller asking to claim it. No SQL writes or decision re-evaluation.
    fn verify_claim_birth(
        &self,
        task: &TaskRecord,
        birth: &S1Birth,
    ) -> Result<SubjectStep, ShenkoError> {
        admission_type(&birth.admission.payload_ref)?;
        self.admission_objects(&birth.admission.payload_ref)
            .map_err(invalid)?;
        let envelope: serde_json::Value =
            serde_json::from_slice(&self.cas.read_object(&birth.admission.payload_ref.digest)?)
                .map_err(invalid)?;
        let mut step: SubjectStep =
            serde_json::from_value(envelope["action"]["step"].clone()).map_err(invalid)?;
        let head: SubjectBranchHead =
            serde_json::from_value(envelope["head"].clone()).map_err(invalid)?;
        let next: EntityId =
            serde_json::from_value(envelope["action"]["next_step_id"].clone()).map_err(invalid)?;
        if birth.transition_ref.store_namespace != "commit-authority"
            || birth.transition_ref.codec.digest != Digest::blake3(b"store-s1-transition")
        {
            return Err(invalid(
                "unsupported original S1 transition namespace or codec digest",
            ));
        }
        let rows = self.read_step_transition(&birth.transition_ref)?;
        let admission = &birth.admission;
        if admission.schema_version != 1
            || envelope["schema_version"] != 1
            || envelope["kind"] != "store.control_admission"
            || envelope["action"]["kind"] != "subject.step_commit"
            || envelope["action"]["transition_ref"] != serde_json::to_value(&birth.transition_ref)?
            || envelope["action"]["expected_head"] != serde_json::to_value(&head)?
            || envelope["source_ledger_id"] != serde_json::to_value(rows.source_ledger_id)?
            || rows.schema_version != 1
            || rows.action_id != admission.action_id
            || admission.action_id
                != EntityId::derived(
                    "commit-admission-action-v1",
                    admission.conflict_key.as_bytes(),
                )
            || admission.conflict_key
                != format!(
                    "branch/{}/successor/{}:{}",
                    head.branch_id.0, head.committed_at.ledger_id, head.committed_at.seq.0
                )
            || rows.input_digest
                != Digest::blake3(&serde_json::to_vec(
                    &serde_json::json!({"step":step,"head":head,"next":next}),
                )?)
            || !same(&rows.original_head, &head)?
            || rows.source_ledger_id != rows.result_commit.ledger_id
            || rows.result_commit != task.created_commit
            || admission.subject_id != task.agent_id
            || admission.subject_id != head.agent_id
            || admission.branch_id != head.branch_id
            || admission.expected_parent != Some(head.committed_at)
            || admission.expected_head_revision != Some(head.record_revision)
            || admission.controller_epoch != head.controller_epoch
            || admission.software_generation != head.software_generation
            || admission.operation_id.is_some()
            || admission.attempt_id.is_some()
            || task.created_by_step_id != Some(step.step_id)
            || step.decision_ref.as_ref() != Some(&task.input_ref)
        {
            return Err(invalid("task birth is not its original canonical S1"));
        }
        rows.validate_result(&step, &head, next)?;
        let tasks = unique_table(&rows, "tasks")?;
        let matches = tasks
            .changes
            .iter()
            .filter(|change| {
                change.after.as_ref().is_some_and(|after| {
                    column(tasks, after, "task_id")
                        .is_ok_and(|id| id == &Cell::Text(task.task_id.to_string()))
                })
            })
            .collect::<Vec<_>>();
        let [change] = matches.as_slice() else {
            return Err(invalid("S1 did not create this unique task"));
        };
        if change.before.is_some()
            || json_row::<serde_json::Value>(
                tasks,
                change
                    .after
                    .as_ref()
                    .ok_or_else(|| invalid("missing birth result"))?,
            )? != serde_json::to_value(task)?
        {
            return Err(invalid("Pending task differs from its immutable S1 birth"));
        }
        let decision_bytes = self.cas.read_object(&task.input_ref.digest)?;
        if decision_bytes.len() as u64 != task.input_ref.bytes
            || Digest::blake3(&decision_bytes) != task.input_ref.digest
        {
            return Err(invalid("original delegation decision content differs"));
        }
        let decision: StepDecision = serde_json::from_slice(&decision_bytes).map_err(invalid)?;
        let definitions = decision
            .delegated_task_intents
            .iter()
            .filter(|intent| intent.intent_id == task.task_id)
            .collect::<Vec<_>>();
        let [intent] = definitions.as_slice() else {
            return Err(invalid("task has no unique authored delegation"));
        };
        if !crate::channel::ADMITTED_DECISION_SCHEMA_VERSIONS.contains(&decision.schema_version)
            || decision.step_id != step.step_id
            || decision.successor_step_id != next
            || intent.definition.as_ref().is_none_or(|definition| {
                definition.kind != task.kind
                    || definition.definition_version != task.definition_version
            })
            || decision
                .publication_intents
                .iter()
                .any(|intent| intent.intent_id == task.task_id)
        {
            return Err(invalid(
                "claim is not the original non-model native delegation",
            ));
        }
        step.commit_ref = Some(rows.result_commit);
        Ok(step)
    }

    pub(crate) fn canonical_native_claim_applicable(
        &self,
        task_id: EntityId,
    ) -> anyhow::Result<bool> {
        if !self.has_commit_authority() {
            return Ok(false);
        }
        let conn = self.ledger.conn.lock();
        let Some(task) = crate::child::read_task(&conn, task_id)? else {
            return Ok(false);
        };
        if !first_delegation(&task) {
            return Ok(false);
        }
        // A source fact can route to strict validation but cannot authorize a
        // claim: missing local materialization must refuse, never fall through
        // to a legacy claim of a canonical task whose proof was lost.
        let known: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM inbox_events WHERE source='commit-authority'
             AND json_extract(data,'$.payload.committed_at.ledger_id')=?1
             AND json_extract(data,'$.payload.committed_at.seq')=?2
             AND json_extract(data,'$.payload.transition_ref') IS NOT NULL)",
            params![
                task.created_commit.ledger_id.to_string(),
                i64::try_from(task.created_commit.seq.0)?
            ],
            |row| row.get(0),
        )?;
        Ok(known || self.claim_birth(&conn, &task)?.is_some())
    }

    pub(crate) fn canonical_native_claim(
        &self,
        task_id: EntityId,
        implementation: ImplementationRef,
    ) -> Result<Option<NativeTaskClaim>, ShenkoError> {
        let (head, mut position) = self.task_admission_position(task_id, String::new())?;
        let [task, root] = position.expected_tasks.as_slice() else {
            return Err(conflict(
                "first claim requires its direct subject-root owner",
            ));
        };
        let task = task.clone();
        let root = root.clone();
        let birth = {
            let conn = self.ledger.conn.lock();
            self.claim_birth(&conn, &task)?
                .ok_or_else(|| conflict("canonical S1 birth is not locally materialized"))?
        };
        let creator = self.verify_claim_birth(&task, &birth)?;
        let invocation = TaskInvocation {
            task_id,
            task_revision: task.task_revision,
            record_revision: RecordRevision(1),
            invocation_id: native_claim_ids(&task).2,
            epoch: head.controller_epoch,
            software_generation: head.software_generation.clone(),
            worker_id: self.capabilities.incarnation,
            node_id: self.ledger.ledger_id(),
        };
        let plan = ClaimPlan {
            task,
            root,
            creator,
            implementation,
            invocation,
            birth,
        };
        plan.validate(&head)
            .map_err(|error| conflict(format!("source precondition changed: {error}")))?;
        position.key = plan.position();
        position.expected_steps = vec![plan.creator.clone()];
        let tasks = position.expected_tasks.clone();
        let steps = position.expected_steps.clone();
        let (prepared, fresh) = self.prepare_claim_transition(&head, plan, position)?;
        let authority = self.reserve_sealed_admission(prepared.admission, tasks, steps)?;
        self.apply_claim_transition(
            &prepared.transition_ref,
            &authority,
            if fresh {
                ClaimApplyMode::Current
            } else {
                ClaimApplyMode::Prepared
            },
        )
    }

    fn prepare_claim_transition(
        &self,
        head: &SubjectBranchHead,
        plan: ClaimPlan,
        mut position: crate::authority::AdmissionPosition,
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
        self.ledger.with_mutation(|| {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage_error)?;
            authority.check(&tx, self.capabilities.incarnation, Some(head.agent_id))?;
            self.check_dispatch_owner(&tx).map_err(source_error)?;
            self.validate_local_claim_birth(&tx, &plan.task, &plan.birth)?;
            if let Some(prepared) = crate::control_transition::prepared_transition_at(&tx, action_id, &input_digest)? {
                return Ok((prepared, false));
            }
            let (mut rows, ()) = self.capture_row_transition(&tx, RowTransitionInput {
                action_id, input_digest: input_digest.clone(), head,
            }, |commit, created_at| self.evaluate_claim(&tx, head, &plan, commit, created_at))?;
            // Only this independently validated direct-root action may reduce
            // its task read range. Generic RowTransition behavior is unchanged.
            let table = rows.tables.iter_mut().find(|table| table.before.table.name == "tasks")
                .ok_or_else(|| invalid("claim capture omitted its task"))?;
            let [change] = table.changes.as_slice() else { return Err(invalid("claim capture changed more than its target")); };
            let key = change.key.clone();
            table.before.rows.retain(|candidate, _| candidate == &key);
            table.complete_read = false;
            let transition = ClaimTransition { rows, plan: plan.clone() };
            let result_commit = transition.rows.result_commit;
            let bytes = serde_json::to_vec(&transition)?;
            if bytes.len() > MAX_IMAGE_BYTES { return Err(conflict("claim exceeds the 64 MiB admission budget")); }
            let transition_ref = self.cas.seal_object(&bytes, "application/json", "commit-authority", CODEC, 1)?.object;
            let payload_ref = self.cas.seal_object(&serde_json::to_vec(&serde_json::json!({
                "schema_version":1,"kind":"store.control_admission","head":head,
                "source_ledger_id":self.ledger.ledger_id(),"predecessor_ref":predecessor_ref,
                "expected_tasks":position.expected_tasks,"snapshot_tasks":position.snapshot_tasks,"expected_steps":position.expected_steps,
                "action":{"kind":"native.claim","task_id":plan.task.task_id,"transition_ref":transition_ref},
            }))?, "application/json", "commit-authority", "commit-admission", 1)?.object;
            let admission = CommitAdmission { schema_version:1,subject_id:head.agent_id,branch_id:head.branch_id.clone(),
                action_id,conflict_key:position.key.clone(),conflict_aliases:position.aliases.clone(),required_unconsumed:position.reads.clone(),
                expected_parent:None,expected_head_revision:None,controller_epoch:head.controller_epoch,
                software_generation:head.software_generation.clone(),operation_id:None,attempt_id:None,payload_ref };
            transition.validate(&admission)?;
            tx.execute("INSERT INTO control_prepared_transitions(action_id,input_digest,admission,result_commit,transition_ref) VALUES(?1,?2,?3,?4,?5)",
                params![action_id.to_string(),input_digest.0,serde_json::to_string(&admission)?,serde_json::to_string(&result_commit)?,serde_json::to_string(&transition_ref)?])
                .map_err(storage_error)?;
            self.record_subject_commit(&tx, result_commit, 1)?;
            tx.commit().map_err(storage_error)?;
            Ok((PreparedTransition { admission, transition_ref }, true))
        }, |error| matches!(error, ShenkoError::Storage(_)))
    }

    fn evaluate_claim(
        &self,
        tx: &Transaction<'_>,
        head: &SubjectBranchHead,
        plan: &ClaimPlan,
        commit: CommitRef,
        created_at: DateTime<Utc>,
    ) -> Result<(Vec<UpdatedEntity>, ()), ShenkoError> {
        if !same(&crate::store::read_subject(tx, head.agent_id)?.0, head)? {
            return Err(conflict("subject changed before claim"));
        }
        let task = crate::child::read_task(tx, plan.task.task_id)
            .map_err(source_error)?
            .ok_or_else(|| conflict("task disappeared before claim"))?;
        if !same(&task, &plan.task)? {
            return Err(conflict("Pending task changed before claim"));
        }
        crate::child::ensure_owner_active(tx, &task, head).map_err(source_error)?;
        let raw: String = tx
            .query_row(
                "SELECT data FROM subject_steps WHERE step_id=?1",
                [plan.creator.step_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let creator: SubjectStep = serde_json::from_str(&raw)?;
        if !same(&creator, &plan.creator)? {
            return Err(conflict("original authoring step changed"));
        }
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE operation_id=?1)",
                [native_claim_ids(&task).0.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        if exists {
            return Err(conflict("first claim already has an original operation"));
        }
        let claimed = plan.projection(commit, created_at)?;
        crate::child::write_task(tx, &claimed.task).map_err(source_error)?;
        Ok((
            vec![UpdatedEntity {
                entity_id: task.task_id,
                record_revision: claimed.task.record_revision,
            }],
            (),
        ))
    }

    fn read_claim_transition(&self, reference: &ObjectRef) -> Result<ClaimTransition, ShenkoError> {
        if reference.codec.name != CODEC
            || reference.codec.major != 1
            || reference.codec.digest != Digest::blake3(CODEC.as_bytes())
            || reference.store_namespace != "commit-authority"
            || reference.media_type != "application/json"
            || reference.bytes > MAX_IMAGE_BYTES as u64
        {
            return Err(invalid("unsupported claim transition type or size"));
        }
        let bytes = self.cas.read_object(&reference.digest)?;
        if bytes.len() as u64 != reference.bytes || Digest::blake3(&bytes) != reference.digest {
            return Err(invalid("claim transition content differs"));
        }
        let transition: ClaimTransition = serde_json::from_slice(&bytes).map_err(invalid)?;
        self.verify_transition_objects(&transition.rows)?;
        let creator = self.verify_claim_birth(&transition.plan.task, &transition.plan.birth)?;
        if !same(&creator, &transition.plan.creator)? {
            return Err(invalid("claim replaced its original authoring step"));
        }
        Ok(transition)
    }

    pub(crate) fn apply_claim_transition(
        &self,
        reference: &ObjectRef,
        authority: &AuthorityReservation,
        mode: ClaimApplyMode,
    ) -> Result<Option<NativeTaskClaim>, ShenkoError> {
        let admission = authority
            .admission()
            .ok_or_else(|| invalid("missing claim admission"))?;
        admission_type(&admission.payload_ref)?;
        let transition = self.read_claim_transition(reference)?;
        transition.validate(admission)?;
        let bytes = self.cas.read_object(&admission.payload_ref.digest)?;
        if bytes.len() as u64 != admission.payload_ref.bytes
            || Digest::blake3(&bytes) != admission.payload_ref.digest
        {
            return Err(invalid("claim admission content differs"));
        }
        let envelope: serde_json::Value = serde_json::from_slice(&bytes).map_err(invalid)?;
        let tasks = [transition.plan.task.clone(), transition.plan.root.clone()];
        let steps = [transition.plan.creator.clone()];
        if envelope["schema_version"] != 1
            || envelope["kind"] != "store.control_admission"
            || envelope["action"]
                != serde_json::json!({"kind":"native.claim","task_id":transition.plan.task.task_id,"transition_ref":reference})
            || envelope["head"] != serde_json::to_value(&transition.rows.original_head)?
            || envelope["source_ledger_id"]
                != serde_json::to_value(transition.rows.source_ledger_id)?
            || envelope["expected_tasks"] != serde_json::to_value(&tasks)?
            || envelope["expected_steps"] != serde_json::to_value(&steps)?
            || envelope["snapshot_tasks"] != serde_json::json!([])
        {
            return Err(invalid("claim action differs from its admitted envelope"));
        }
        let predecessor_ref: ObjectRef =
            serde_json::from_value(envelope["predecessor_ref"].clone()).map_err(invalid)?;
        let predecessor = self.inspect_control_predecessor_for_admission(
            &predecessor_ref,
            admission,
            &tasks,
            &steps,
        )?;
        if predecessor.source_ledger_id != transition.rows.source_ledger_id
            || !same(&predecessor.original_head, &transition.rows.original_head)?
        {
            return Err(invalid(
                "claim reads differ from their original predecessor",
            ));
        }
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                if admission_materialized(&tx, admission)? {
                    return Ok(None);
                }
                match mode {
                    ClaimApplyMode::Learned => authority.check_materialization(&tx)?,
                    ClaimApplyMode::Prepared => authority.check(
                        &tx,
                        self.capabilities.incarnation,
                        Some(admission.subject_id),
                    )?,
                    ClaimApplyMode::Current => {
                        authority.check(
                            &tx,
                            self.capabilities.incarnation,
                            Some(admission.subject_id),
                        )?;
                        self.check_dispatch_owner(&tx).map_err(source_error)?;
                        if transition.rows.source_ledger_id != self.ledger.ledger_id()
                            || transition.plan.invocation.node_id != self.ledger.ledger_id()
                            || transition.plan.invocation.worker_id != self.capabilities.incarnation
                        {
                            return Err(conflict("the original claim worker is no longer current"));
                        }
                    }
                }
                self.validate_local_claim_birth(
                    &tx,
                    &transition.plan.task,
                    &transition.plan.birth,
                )?;
                let local = CommitRef::new(self.ledger.ledger_id(), self.ledger.allocate_seq(&tx)?);
                transition.rows.apply_rows(&tx, local.seq)?;
                authority.record_canonical_admission(
                    &tx,
                    local,
                    transition.rows.result_commit,
                    reference,
                    transition.rows.created_at,
                )?;
                self.record_subject_commit(&tx, local, 1)?;
                tx.commit().map_err(storage_error)?;
                authority.materialized();
                if matches!(mode, ClaimApplyMode::Current)
                    && authority.grant().is_none_or(|grant| !grant.replayed)
                {
                    Ok(Some(transition.plan.projection(
                        transition.rows.result_commit,
                        transition.rows.created_at,
                    )?))
                } else {
                    Ok(None)
                }
            },
            |error| matches!(error, ShenkoError::Storage(_)),
        )
    }

    pub(crate) fn recover_claim_transition(
        &self,
        grant: &CommitAuthorityGrant,
        reference: &ObjectRef,
    ) -> Result<Option<NativeTaskClaim>, ShenkoError> {
        let authority = self.reserve_recovered_admission(grant.clone(), Vec::new(), Vec::new())?;
        self.apply_claim_transition(reference, &authority, ClaimApplyMode::Learned)
    }
}
