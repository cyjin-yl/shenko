//! Independent readers for bounded native task ownership, not direct-S1 proof.
use std::collections::{BTreeMap, BTreeSet};

use shenko_types::{authority::CommitAdmission, error::ShenkoError, ids::EntityId,
    subject::SubjectStep, task::{TaskOwner, TaskRecord}};

use crate::{authority::task_revision_position,
    control_transition::{Cell, NATIVE_ANCESTRY_READ_SCHEMA, NativeAncestry, NativeReadSelector,
        RowTransition, TableTransition}};
use super::{column, same};

fn invalid(reason: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::other(format!("invalid native ancestry: {reason}"))
}

fn selected_table<'a>(rows: &'a RowTransition, selector: &NativeReadSelector) -> Result<&'a TableTransition, ShenkoError> {
    let matches = rows.tables.iter().filter(|table| table.before.table.name == selector.table_name())
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [table] if !table.complete_read && table.read_range.is_none()
            && table.read_selector.as_ref() == Some(selector) => Ok(table),
        _ => Err(invalid("missing exact structural selector")),
    }
}

/// Recover the ordered target/ancestor read fence from the frozen physical
/// rows themselves. A sender cannot omit an ancestor, substitute its JSON
/// identity, or supply a free-form list of supposedly relevant keys.
pub(crate) fn validate_native_ancestry(
    rows: &RowTransition, task: &TaskRecord, ordinary_children: bool,
) -> Result<Vec<TaskRecord>, ShenkoError> {
    if rows.schema_version != NATIVE_ANCESTRY_READ_SCHEMA {
        return Err(invalid("structural readers require row schema 5"));
    }
    let head = &rows.original_head;
    let ancestry = NativeAncestry::new(task, head);
    let tasks = selected_table(rows, &NativeReadSelector::TaskAncestry {
        ancestry: ancestry.clone(), ordinary_children,
    })?;
    let steps = selected_table(rows, &NativeReadSelector::AuthoringSteps { ancestry })?;
    if !steps.changes.is_empty() { return Err(invalid("native action may not write its authoring steps")); }

    let mut records = BTreeMap::new();
    for row in tasks.before.rows.values() {
        let Cell::Text(data) = column(tasks, row, "data")? else {
            return Err(invalid("task predecessor is not JSON text"));
        };
        let original: TaskRecord = serde_json::from_str(data).map_err(invalid)?;
        if column(tasks, row, "task_id")? != &Cell::Text(original.task_id.to_string())
            || records.insert(original.task_id, original).is_some()
        {
            return Err(invalid("task physical and JSON identity differ or repeat"));
        }
    }
    let mut authoring = BTreeMap::new();
    for row in steps.before.rows.values() {
        let Cell::Text(data) = column(steps, row, "data")? else {
            return Err(invalid("authoring predecessor is not JSON text"));
        };
        let step: SubjectStep = serde_json::from_str(data).map_err(invalid)?;
        if column(steps, row, "step_id")? != &Cell::Text(step.step_id.to_string())
            || authoring.insert(step.step_id, step).is_some()
        {
            return Err(invalid("authoring physical and JSON identity differ or repeat"));
        }
    }
    let target = records.get(&task.task_id).ok_or_else(|| invalid("missing target task"))?;
    if !same(target, task)? { return Err(invalid("target differs from the native plan")); }
    let mut chain = vec![target.clone()];
    let mut parent_ids = BTreeSet::new();
    let mut owner = target.owner.clone();
    while let TaskOwner::Task { id } = owner {
        if !parent_ids.insert(id) || parent_ids.len() > 64 {
            return Err(invalid("owner cycle or depth exceeded"));
        }
        let parent = records.get(&id).ok_or_else(|| invalid("missing original task ancestor"))?;
        chain.push(parent.clone());
        owner = parent.owner.clone();
        if id == head.subject_root_task_id { break; }
    }
    for (index, original) in chain.iter().enumerate() {
        if original.record_schema_version != 1 || original.agent_id != head.agent_id {
            return Err(invalid("ancestor schema or subject differs"));
        }
        // The root is the structural boundary, as in task_owner_active. Its
        // revision is still an authority read; a parked root is not silently
        // replaced with a new receiving installation's row.
        if index > 0 && original.task_id != head.subject_root_task_id
            && (original.abort_request.is_some() || original.is_terminal())
        {
            return Err(invalid("original task ancestor is inactive"));
        }
    }

    // Mirror task_in_current_branch against the complete frozen chain. Keep
    // each actually consulted creation step, not the mutable current S0 step.
    let mut current = target;
    let mut bound = false;
    let mut complete = false;
    let mut step_ids = BTreeSet::new();
    for _ in 0..64 {
        if current.kind == "subject_root" {
            if current.task_id != head.subject_root_task_id {
                return Err(invalid("foreign subject root in ancestry"));
            }
            complete = true;
            break;
        }
        if let Some(id) = current.created_by_step_id {
            let step = authoring.get(&id).ok_or_else(|| invalid("missing original authoring step"))?;
            if step.branch_id != head.branch_id { return Err(invalid("authoring step is on another branch")); }
            step_ids.insert(id);
            bound = true;
        }
        if let Some(branch) = current.memos.get("subject_branch_id") {
            let branch: EntityId = serde_json::from_value(branch.clone()).map_err(invalid)?;
            if branch.0 != head.branch_id.0 { return Err(invalid("task is bound to another branch")); }
            bound = true;
        }
        match current.owner {
            TaskOwner::Conversation { id } => {
                if id != task.conversation_id || (!bound && head.branch_origin.is_some()) {
                    return Err(invalid("conversation owner has no original branch binding"));
                }
                complete = true;
                break;
            }
            TaskOwner::Task { id } if id == head.subject_root_task_id => {
                complete = true;
                break;
            }
            TaskOwner::Task { id } => current = records.get(&id)
                .ok_or_else(|| invalid("missing branch ancestor"))?,
        }
    }
    if !complete || authoring.keys().copied().collect::<BTreeSet<_>>() != step_ids {
        return Err(invalid("authoring read set is incomplete or contains another step"));
    }
    let mut expected_tasks = chain.iter().map(|ancestor| ancestor.task_id).collect::<BTreeSet<_>>();
    if ordinary_children {
        expected_tasks.extend(records.values().filter(|record|
            !record.background && record.owner == (TaskOwner::Task { id: task.task_id }))
            .map(|record| record.task_id));
    }
    if records.keys().copied().collect::<BTreeSet<_>>() != expected_tasks {
        return Err(invalid("task read set contains a non-ancestor or unrelated child"));
    }
    Ok(chain)
}

pub(crate) fn validate_native_ancestry_fences(
    admission: &CommitAdmission, tasks: &[TaskRecord],
) -> Result<(), ShenkoError> {
    let target = tasks.first().ok_or_else(|| invalid("empty task authority chain"))?;
    let ancestors = tasks.iter().skip(1).map(task_revision_position)
        .collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
    if admission.schema_version != 1
        || admission.conflict_aliases != vec![task_revision_position(target)]
        || admission.required_unconsumed != ancestors
    {
        return Err(invalid("Node revision fences differ from the original complete ancestry"));
    }
    Ok(())
}

pub(crate) fn validate_native_ancestry_envelope(
    envelope: &serde_json::Value, tasks: &[TaskRecord],
) -> Result<(), ShenkoError> {
    if envelope["schema_version"] != 1 || envelope["kind"] != "store.control_admission"
        || envelope["expected_tasks"] != serde_json::to_value(tasks)?
        || envelope["snapshot_tasks"] != serde_json::json!([])
        || envelope["expected_steps"] != serde_json::json!([])
    {
        return Err(invalid("admission envelope differs from its original complete ancestry"));
    }
    Ok(())
}
