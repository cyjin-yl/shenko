//! Canonical native dispatch records a single original send decision.
//!
//! Materialization is never a send permit. Only the live caller that created
//! this preparation may receive `Won`, after a fresh admission and a final
//! current invocation/dispatcher check. Prepared and learned plans retain the
//! original marker and source commit without running any provider.
use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    authority::{CommitAdmission, CommitAuthorityGrant},
    commit::{CommitRef, UpdatedEntity},
    error::ShenkoError,
    ids::{EntityId, RecordRevision},
    object::{Digest, ObjectRef},
    operation::{AttemptDispatchStage, AttemptRecord, OperationRecord, OperationState},
    subject::{SubjectBranchHead, SubjectMode},
    task::{RunMode, TaskInvocation, TaskRecord, TaskState},
};

use crate::{
    Store,
    authority::{AuthorityReservation, admission_materialized},
    child::{COGNITION_IMPORTED, NATIVE_SETTLEMENT_ONLY, NativeDispatchTransition, memo_flag},
    control_transition::{
        Cell, MAX_IMAGE_BYTES, PreparedTransition, RowTransition, RowTransitionInput,
        TableTransition,
    },
    store::storage_error,
};

const CODEC: &str = "store-native-dispatch-transition";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchPlan {
    invocation: TaskInvocation,
    task: TaskRecord,
    operation: OperationRecord,
    attempt: AttemptRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchTransition {
    rows: RowTransition,
    plan: DispatchPlan,
}

pub(crate) enum DispatchApplyMode {
    /// Constructed only when this same live call inserted the preparation.
    Current,
    Prepared,
    Learned,
}

fn invalid(reason: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::other(format!("invalid canonical native dispatch: {reason}"))
}

fn conflict(reason: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::conflict(format!("canonical native dispatch: {reason}"))
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

impl DispatchPlan {
    fn position(&self) -> String {
        format!("attempt/{}/dispatch", self.attempt.attempt_id)
    }

    fn input_digest(&self, head: &SubjectBranchHead) -> Result<Digest, ShenkoError> {
        Ok(Digest::blake3(&serde_json::to_vec(&(head, self))?))
    }

    fn validate(&self, head: &SubjectBranchHead) -> Result<(), ShenkoError> {
        let ctx = &self.invocation;
        let task = &self.task;
        let operation = &self.operation;
        let attempt = &self.attempt;
        if task.agent_id != head.agent_id
            || task.task_id != ctx.task_id
            || task.task_revision != ctx.task_revision
            || task.abort_request.is_some()
            || task.memos.contains_key(COGNITION_IMPORTED)
            || memo_flag(task, NATIVE_SETTLEMENT_ONLY)
            || !matches!(head.mode, SubjectMode::Active | SubjectMode::Paused)
            || ctx.epoch != head.controller_epoch
            || ctx.software_generation != head.software_generation
            || !matches!(task.state, TaskState::Running { invocation_id, epoch, mode: RunMode::Run, .. }
                if invocation_id == ctx.invocation_id && epoch == ctx.epoch)
            || operation.task_id != task.task_id
            || operation.state != OperationState::InFlight
            || attempt.operation_id != operation.operation_id
            || attempt.invocation_id != ctx.invocation_id
            || attempt.worker_id != ctx.worker_id
            || attempt.node_id != ctx.node_id
            || attempt.epoch != ctx.epoch
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
            || attempt.final_receipt_id.is_some()
        {
            return Err(invalid("task and original armed attempt do not agree"));
        }
        if crate::child::has_native_dispatch(task, ctx, operation.operation_id, attempt.attempt_id)
            .map_err(invalid)?
        {
            return Err(invalid(
                "the original attempt already has a dispatch decision",
            ));
        }
        Ok(())
    }

    fn projection(&self, commit: CommitRef) -> Result<TaskRecord, ShenkoError> {
        let mut task = self.task.clone();
        crate::child::insert_native_dispatch_marker(
            &mut task,
            &self.invocation,
            self.operation.operation_id,
            self.attempt.attempt_id,
            commit,
        )
        .map_err(invalid)?;
        task.record_revision = RecordRevision(
            task.record_revision
                .0
                .checked_add(1)
                .ok_or_else(|| invalid("task revision exhausted"))?,
        );
        task.updated_commit = commit;
        Ok(task)
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

impl DispatchTransition {
    fn original(
        &self,
        name: &str,
        id_column: &str,
        id: EntityId,
        expected: &impl Serialize,
    ) -> Result<(), ShenkoError> {
        let tables = self
            .rows
            .tables
            .iter()
            .filter(|table| table.before.table.name == name)
            .collect::<Vec<_>>();
        let [table] = tables.as_slice() else {
            return Err(invalid(format!("missing unique {name} read condition")));
        };
        if !table.complete_read {
            return Err(invalid(format!("incomplete {name} read condition")));
        }
        let identity = Cell::Text(id.to_string());
        let originals = table
            .before
            .rows
            .values()
            .filter(|row| column(table, row, id_column).is_ok_and(|cell| cell == &identity))
            .collect::<Vec<_>>();
        let [row] = originals.as_slice() else {
            return Err(invalid("missing unique original row"));
        };
        let Cell::Text(data) = column(table, row, "data")? else {
            return Err(invalid("original row is not JSON"));
        };
        if serde_json::from_str::<serde_json::Value>(data).map_err(invalid)?
            != serde_json::to_value(expected)?
        {
            return Err(invalid(
                "original row differs from the named dispatch predecessor",
            ));
        }
        Ok(())
    }

    /// A producer cannot smuggle an arbitrary row delta into a send decision.
    /// Exactly the named task receives its original attempt marker; the
    /// operation, attempt, receipts, ownership and invocation remain unchanged.
    fn validate(&self, admission: &CommitAdmission) -> Result<(), ShenkoError> {
        let rows = &self.rows;
        let head = &rows.original_head;
        let plan = &self.plan;
        plan.validate(head)?;
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
            return Err(invalid(
                "transition does not bind the original dispatch position",
            ));
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
        let task = plan.projection(rows.result_commit)?;
        if rows.updated.len() != 1
            || rows.updated[0].entity_id != task.task_id
            || rows.updated[0].record_revision != task.record_revision
        {
            return Err(invalid("updated entities differ from the dispatch task"));
        }
        let mut changed = 0;
        for table in &rows.tables {
            for change in &table.changes {
                if table.before.table.name != "tasks" {
                    return Err(invalid("dispatch wrote outside its original task"));
                }
                let before = change
                    .before
                    .as_ref()
                    .ok_or_else(|| invalid("dispatch cannot create a task"))?;
                if column(table, before, "task_id")? != &Cell::Text(task.task_id.to_string()) {
                    return Err(invalid("dispatch changed another task"));
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
                let expected = table
                    .before
                    .table
                    .columns
                    .iter()
                    .map(|name| {
                        expected
                            .get(name)
                            .cloned()
                            .ok_or_else(|| invalid("unknown dispatch result column"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if change.after.as_ref() != Some(&expected) {
                    return Err(invalid("dispatch write differs from its named marker"));
                }
                changed += 1;
            }
        }
        if changed != 1 {
            return Err(invalid("missing or repeated dispatch task write"));
        }
        Ok(())
    }
}

impl Store {
    pub(crate) fn canonical_native_dispatch(
        &self,
        invocation: &TaskInvocation,
        operation: OperationRecord,
        attempt: AttemptRecord,
    ) -> Result<NativeDispatchTransition, ShenkoError> {
        let _phase = crate::timing::Phase::bound("native.dispatch_commit", crate::timing::Binding {
            task_id: Some(invocation.task_id), operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id), ..Default::default()
        });
        let (head, mut position) =
            self.task_admission_position(invocation.task_id, String::new())?;
        let task = position
            .expected_tasks
            .first()
            .cloned()
            .ok_or_else(|| conflict("task missing"))?;
        let plan = DispatchPlan {
            invocation: invocation.clone(),
            task,
            operation,
            attempt,
        };
        plan.validate(&head)
            .map_err(|error| conflict(format!("source precondition changed: {error}")))?;
        position.key = plan.position();
        position.operation_id = Some(plan.operation.operation_id);
        position.attempt_id = Some(plan.attempt.attempt_id);
        let expected = position.expected_tasks.clone();
        let (prepared, fresh_preparation) =
            self.prepare_dispatch_transition(&head, plan, position)?;
        let authority = self.reserve_sealed_admission(prepared.admission, expected, Vec::new())?;
        self.apply_dispatch_transition(
            &prepared.transition_ref,
            &authority,
            if fresh_preparation {
                DispatchApplyMode::Current
            } else {
                DispatchApplyMode::Prepared
            },
        )
    }

    /// The boolean belongs to this live call, not to the durable plan or the
    /// provider grant. A crash/retry cannot recover it from `replayed=false`.
    fn prepare_dispatch_transition(
        &self,
        head: &SubjectBranchHead,
        plan: DispatchPlan,
        mut position: crate::authority::AdmissionPosition,
    ) -> Result<(PreparedTransition, bool), ShenkoError> {
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
            return Ok((prepared, false));
        }
        let predecessor_ref =
            self.prepare_control_predecessor_for_admission(head, &position.expected_tasks, &[])?;
        let authority = self
            .reserve_authority()?
            .with_read_conditions(position.expected_tasks.clone(), Vec::new());
        authority.with_mutation(&self.ledger, || {
            let mut conn = crate::timing::measure("sqlite.mutex_wait", || self.ledger.conn.lock());
            let tx = crate::timing::immediate(&mut conn).map_err(storage_error)?;
            authority.check(&tx, self.capabilities.incarnation, Some(head.agent_id))?;
            if !self.native_invocation_current_tx(&tx, &plan.task, &plan.invocation).map_err(source_error)? {
                return Err(conflict("source invocation was fenced before preparation"));
            }
            if let Some(prepared) = crate::control_transition::prepared_transition_at(&tx, action_id, &input_digest)? {
                return Ok((prepared, false));
            }
            // Dispatcher ownership is local and was checked above. Only the
            // shared factual transaction below contributes to the read set.
            let (rows, ()) = self.capture_row_transition(&tx, RowTransitionInput {
                action_id, input_digest: input_digest.clone(), head,
            }, None, |commit, _| self.evaluate_dispatch(&tx, head, &plan, commit))?;
            let transition = DispatchTransition { rows, plan: plan.clone() };
            let result_commit = transition.rows.result_commit;
            let bytes = serde_json::to_vec(&transition)?;
            if bytes.len() > MAX_IMAGE_BYTES { return Err(conflict("dispatch exceeds the 64 MiB admission budget")); }
            let transition_ref = self.cas.seal_object(&bytes, "application/json", "commit-authority", CODEC, 1)?.object;
            let payload_ref = self.cas.seal_object(&serde_json::to_vec(&serde_json::json!({
                "schema_version":1,"kind":"store.control_admission","head":head,
                "source_ledger_id":self.ledger.ledger_id(),"predecessor_ref":predecessor_ref,
                "expected_tasks":position.expected_tasks,"snapshot_tasks":position.snapshot_tasks,"expected_steps":[],
                "action":{"kind":"native.dispatch","task_id":plan.task.task_id,"transition_ref":transition_ref},
            }))?, "application/json", "commit-authority", "commit-admission", 1)?.object;
            let admission = CommitAdmission {
                schema_version:1, subject_id:head.agent_id, branch_id:head.branch_id.clone(), action_id,
                conflict_key:position.key.clone(), conflict_aliases:position.aliases.clone(), required_unconsumed:position.reads.clone(),
                expected_parent:None, expected_head_revision:None, controller_epoch:head.controller_epoch,
                software_generation:head.software_generation.clone(), operation_id:position.operation_id,
                attempt_id:position.attempt_id, payload_ref,
            };
            transition.validate(&admission)?;
            tx.execute("INSERT INTO control_prepared_transitions(action_id,input_digest,admission,result_commit,transition_ref) VALUES(?1,?2,?3,?4,?5)",
                params![action_id.to_string(),input_digest.0,serde_json::to_string(&admission)?,serde_json::to_string(&result_commit)?,serde_json::to_string(&transition_ref)?])
                .map_err(storage_error)?;
            self.record_subject_commit(&tx, result_commit, 1)?;
            crate::timing::measure("sqlite.commit", || tx.commit()).map_err(storage_error)?;
            Ok((PreparedTransition { admission, transition_ref }, true))
        }, |error| matches!(error, ShenkoError::Storage(_)))
    }

    fn evaluate_dispatch(
        &self,
        tx: &Transaction<'_>,
        head: &SubjectBranchHead,
        plan: &DispatchPlan,
        commit: CommitRef,
    ) -> Result<(Vec<UpdatedEntity>, ()), ShenkoError> {
        let current = crate::store::read_subject(tx, head.agent_id)?.0;
        if !same(&current, head)? {
            return Err(conflict("subject changed before dispatch"));
        }
        let task = crate::child::read_task(tx, plan.task.task_id)
            .map_err(source_error)?
            .ok_or_else(|| conflict("task disappeared before dispatch"))?;
        if !same(&task, &plan.task)? {
            return Err(conflict("task changed before dispatch"));
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
                return Err(conflict(
                    "original operation/attempt changed before dispatch",
                ));
            }
        }
        let task = plan.projection(commit)?;
        crate::child::write_task(tx, &task).map_err(source_error)?;
        Ok((
            vec![UpdatedEntity {
                entity_id: task.task_id,
                record_revision: task.record_revision,
            }],
            (),
        ))
    }

    fn read_dispatch_transition(
        &self,
        reference: &ObjectRef,
    ) -> Result<DispatchTransition, ShenkoError> {
        if reference.codec.name != CODEC
            || reference.codec.major != 1
            || reference.media_type != "application/json"
            || reference.bytes > MAX_IMAGE_BYTES as u64
        {
            return Err(invalid("unsupported transition type or size"));
        }
        let bytes = self.cas.read_object(&reference.digest)?;
        if bytes.len() as u64 != reference.bytes || Digest::blake3(&bytes) != reference.digest {
            return Err(invalid("transition bytes differ"));
        }
        let transition: DispatchTransition = serde_json::from_slice(&bytes).map_err(invalid)?;
        self.verify_transition_objects(&transition.rows)?;
        Ok(transition)
    }

    pub(crate) fn apply_dispatch_transition(
        &self,
        reference: &ObjectRef,
        authority: &AuthorityReservation,
        mode: DispatchApplyMode,
    ) -> Result<NativeDispatchTransition, ShenkoError> {
        let admission = authority
            .admission()
            .ok_or_else(|| invalid("missing dispatch admission"))?;
        let _phase = crate::timing::Phase::bound("canonical.apply", crate::timing::Binding {
            subject_id: Some(admission.subject_id), action_id: Some(admission.action_id),
            operation_id: admission.operation_id, attempt_id: admission.attempt_id,
            ..Default::default()
        });
        let transition = self.read_dispatch_transition(reference)?;
        transition.validate(admission)?;
        let envelope: serde_json::Value =
            serde_json::from_slice(&self.cas.read_object(&admission.payload_ref.digest)?)?;
        if envelope["action"]
            != serde_json::json!({"kind":"native.dispatch","task_id":transition.plan.task.task_id,"transition_ref":reference})
            || envelope["head"] != serde_json::to_value(&transition.rows.original_head)?
            || envelope["source_ledger_id"]
                != serde_json::to_value(transition.rows.source_ledger_id)?
        {
            return Err(invalid(
                "dispatch action differs from its admitted envelope",
            ));
        }
        authority.with_mutation(&self.ledger,
            || {
                let mut conn = crate::timing::measure("sqlite.mutex_wait", || self.ledger.conn.lock());
                let tx = crate::timing::immediate(&mut conn)
                    .map_err(storage_error)?;
                if admission_materialized(&tx, admission)? {
                    crate::timing::measure("sqlite.commit", || tx.commit()).map_err(storage_error)?;
                    authority.materialized();
                    return Ok(NativeDispatchTransition::AlreadyCommitted);
                }
                match mode {
                    DispatchApplyMode::Learned => authority.check_materialization(&tx)?,
                    DispatchApplyMode::Prepared => authority.check(
                        &tx,
                        self.capabilities.incarnation,
                        Some(admission.subject_id),
                    )?,
                    DispatchApplyMode::Current => {
                        authority.check(
                            &tx,
                            self.capabilities.incarnation,
                            Some(admission.subject_id),
                        )?;
                        if transition.rows.source_ledger_id != self.ledger.ledger_id() {
                            return Err(invalid(
                                "fresh dispatch is not its original source ledger",
                            ));
                        }
                        let task = crate::child::read_task(&tx, transition.plan.task.task_id)
                            .map_err(source_error)?
                            .ok_or_else(|| conflict("current dispatch task disappeared"))?;
                        if !self
                            .native_invocation_current_tx(&tx, &task, &transition.plan.invocation)
                            .map_err(source_error)?
                        {
                            return Ok(NativeDispatchTransition::Fenced);
                        }
                    }
                }
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
                self.record_subject_commit(&tx, local_commit, 1)?;
                crate::timing::measure("sqlite.commit", || tx.commit()).map_err(storage_error)?;
                authority.materialized();
                Ok(
                    if matches!(mode, DispatchApplyMode::Current)
                        && authority.grant().is_none_or(|grant| !grant.replayed)
                    {
                        NativeDispatchTransition::Won
                    } else {
                        NativeDispatchTransition::AlreadyCommitted
                    },
                )
            },
            |error| matches!(error, ShenkoError::Storage(_)),
        )
    }

    pub(crate) fn recover_dispatch_transition(
        &self,
        grant: &CommitAuthorityGrant,
        reference: &ObjectRef,
    ) -> Result<NativeDispatchTransition, ShenkoError> {
        let authority = self.reserve_recovered_admission(grant.clone(), Vec::new(), Vec::new())?;
        self.apply_dispatch_transition(reference, &authority, DispatchApplyMode::Learned)
    }
}
