//! Local application proofs and exact source reads for the direct native chain.
//!
//! A copied applied event is not this installation's materialization. The
//! physical receipt can belong to an earlier writer of the same installation;
//! it never transfers that writer's execution permission.
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    authority::CommitAdmission,
    commit::CommitRef,
    error::ShenkoError,
    ids::EntityId,
    object::{Digest, ObjectRef},
    subject::SubjectStep,
    task::{TaskOwner, TaskRecord},
};

use crate::{
    Store,
    authority::{admission_applied, task_revision_position},
    control_transition::{Cell, MAX_IMAGE_BYTES, RowTransition, TableTransition},
    store::storage_error,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CanonicalFact {
    pub admission: CommitAdmission,
    pub transition_ref: ObjectRef,
}

pub(crate) fn invalid(reason: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::other(format!("invalid direct native fact: {reason}"))
}

pub(crate) fn conflict(reason: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::conflict(format!("direct native fact: {reason}"))
}

pub(crate) fn same(left: &impl Serialize, right: &impl Serialize) -> Result<bool, ShenkoError> {
    Ok(serde_json::to_value(left)? == serde_json::to_value(right)?)
}

pub(crate) fn typed_json(
    reference: &ObjectRef,
    codec: &str,
    major: u32,
) -> Result<(), ShenkoError> {
    if reference.codec.name != codec
        || reference.codec.major != major
        || reference.codec.digest != Digest::blake3(codec.as_bytes())
        || reference.store_namespace != "commit-authority"
        || reference.media_type != "application/json"
        || reference.bytes > MAX_IMAGE_BYTES as u64
    {
        return Err(invalid("unsupported typed object or size"));
    }
    Ok(())
}

pub(crate) fn column<'a>(
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

/// This is usable only after the caller independently proves the action's
/// direct-root ancestry and, for completion, its ordinary-child absence.
pub(crate) fn narrow_task_read(
    rows: &mut RowTransition,
    task: &TaskRecord,
) -> Result<(), ShenkoError> {
    let table = rows
        .tables
        .iter_mut()
        .find(|table| table.before.table.name == "tasks")
        .ok_or_else(|| invalid("missing native task read"))?;
    let [change] = table.changes.as_slice() else {
        return Err(invalid("direct action must change exactly its task"));
    };
    let key = change.key.clone();
    table.before.rows.retain(|candidate, _| candidate == &key);
    table.complete_read = false;
    validate_task_read(table, task)
}

pub(crate) fn validate_task_read(
    table: &TableTransition,
    task: &TaskRecord,
) -> Result<(), ShenkoError> {
    if table.complete_read || table.before.rows.len() != 1 || table.changes.len() != 1 {
        return Err(invalid("direct action must read exactly its target task"));
    }
    let change = &table.changes[0];
    let before = change
        .before
        .as_ref()
        .ok_or_else(|| invalid("direct action cannot create its task"))?;
    let Cell::Text(data) = column(table, before, "data")? else {
        return Err(invalid("native task body is not JSON"));
    };
    if table.before.rows.get(&change.key) != Some(before)
        || column(table, before, "task_id")? != &Cell::Text(task.task_id.to_string())
        || serde_json::from_str::<serde_json::Value>(data).map_err(invalid)?
            != serde_json::to_value(task)?
    {
        return Err(invalid(
            "direct action task differs from its frozen predecessor",
        ));
    }
    Ok(())
}

pub(crate) fn has_ordinary_children(
    conn: &Connection,
    task: EntityId,
) -> Result<bool, ShenkoError> {
    let mut statement = conn
        .prepare("SELECT data FROM tasks")
        .map_err(storage_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(storage_error)?;
    for row in rows {
        let child: TaskRecord =
            serde_json::from_str(&row.map_err(storage_error)?).map_err(invalid)?;
        if child.record_schema_version != 1 {
            return Err(invalid(
                "unsupported task schema in ordinary-child absence range",
            ));
        }
        if child.owner == (TaskOwner::Task { id: task }) && !child.background {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Store {
    pub(crate) fn read_native_json<T: serde::de::DeserializeOwned>(
        &self,
        reference: &ObjectRef,
        codec: &str,
        major: u32,
    ) -> Result<T, ShenkoError> {
        typed_json(reference, codec, major)?;
        let bytes = self.cas.read_object(&reference.digest)?;
        if bytes.len() as u64 != reference.bytes || Digest::blake3(&bytes) != reference.digest {
            return Err(invalid("typed object bytes differ"));
        }
        serde_json::from_slice(&bytes).map_err(invalid)
    }

    pub(crate) fn local_native_fact(
        &self,
        conn: &Connection,
        logical: CommitRef,
        kind: &str,
    ) -> Result<Option<CanonicalFact>, ShenkoError> {
        let mut statement = conn.prepare("SELECT data FROM control_materializations
            WHERE json_extract(data,'$.logical_result.ledger_id')=?1 AND json_extract(data,'$.logical_result.seq')=?2")
            .map_err(storage_error)?;
        let records = statement
            .query_map(
                params![
                    logical.ledger_id.to_string(),
                    i64::try_from(logical.seq.0).map_err(invalid)?
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
            return Err(invalid(
                "multiple local applications share a logical result",
            ));
        }
        let value: serde_json::Value = serde_json::from_str(raw).map_err(invalid)?;
        let fact = CanonicalFact {
            admission: serde_json::from_value(value["admission"].clone()).map_err(invalid)?,
            transition_ref: serde_json::from_value(value["transition_ref"].clone())
                .map_err(invalid)?,
        };
        self.verify_local_native_fact(conn, &fact, logical)?;
        let envelope: serde_json::Value =
            self.read_native_json(&fact.admission.payload_ref, "commit-admission", 1)?;
        if envelope["action"]["kind"] != kind {
            return Ok(None);
        }
        Ok(Some(fact))
    }

    pub(crate) fn verify_local_native_fact(
        &self,
        conn: &Connection,
        fact: &CanonicalFact,
        logical: CommitRef,
    ) -> Result<(), ShenkoError> {
        let row: Option<(String, String, i64)> = conn.query_row(
            "SELECT action_id,data,updated_commit_seq FROM control_materializations WHERE conflict_key=?1",
            [&fact.admission.conflict_key], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().map_err(storage_error)?;
        let Some((action, raw, sequence)) = row else {
            return Err(conflict(
                "original control fact has no local application receipt",
            ));
        };
        let value: serde_json::Value = serde_json::from_str(&raw).map_err(invalid)?;
        let physical: CommitRef =
            serde_json::from_value(value["committed_at"].clone()).map_err(invalid)?;
        if action != fact.admission.action_id.to_string()
            || value["admission"] != serde_json::to_value(&fact.admission)?
            || value["logical_result"] != serde_json::to_value(logical)?
            || value["transition_ref"] != serde_json::to_value(&fact.transition_ref)?
            || i64::try_from(physical.seq.0).ok() != Some(sequence)
            || !admission_applied(conn, &fact.admission)?
        {
            return Err(invalid(
                "local control receipt differs from the original fact",
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
            return Err(invalid("local control receipt has no physical commit"));
        }
        let event_id = EntityId::derived(
            "commit-authority-admission-v1",
            fact.admission.conflict_key.as_bytes(),
        );
        let event: String = conn
            .query_row(
                "SELECT data FROM inbox_events WHERE event_id=?1",
                [event_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let event: serde_json::Value = serde_json::from_str(&event).map_err(invalid)?;
        if event["payload"]["committed_at"] != serde_json::to_value(logical)?
            || event["payload"]["transition_ref"] != serde_json::to_value(&fact.transition_ref)?
        {
            return Err(invalid(
                "shared applied fact has a different original result",
            ));
        }
        Ok(())
    }

    /// Retain the complete source root/creator relationship and independently
    /// check it against the immutable predecessor, without installing it.
    pub(crate) fn verify_direct_native_reads(
        &self,
        fact: &CanonicalFact,
        rows: &RowTransition,
        task: &TaskRecord,
        creator: &SubjectStep,
        childless: bool,
    ) -> Result<(), ShenkoError> {
        let envelope: serde_json::Value =
            self.read_native_json(&fact.admission.payload_ref, "commit-admission", 1)?;
        let expected: Vec<TaskRecord> =
            serde_json::from_value(envelope["expected_tasks"].clone()).map_err(invalid)?;
        let [target, root] = expected.as_slice() else {
            return Err(invalid("direct native reads omitted their source root"));
        };
        let head = &rows.original_head;
        if fact.admission.schema_version != 1
            || envelope["schema_version"] != 1
            || envelope["kind"] != "store.control_admission"
            || !same(target, task)?
            || task.owner
                != (TaskOwner::Task {
                    id: head.subject_root_task_id,
                })
            || root.task_id != head.subject_root_task_id
            || root.agent_id != task.agent_id
            || root.record_schema_version != 1
            || root.kind != "subject_root"
            || task.created_by_step_id != Some(creator.step_id)
            || creator.branch_id != head.branch_id
            || creator.commit_ref != Some(task.created_commit)
            || creator.decision_ref.as_ref() != Some(&task.input_ref)
            || envelope["expected_steps"] != serde_json::to_value([creator])?
            || envelope["snapshot_tasks"] != serde_json::json!([])
            || envelope["head"] != serde_json::to_value(head)?
            || envelope["source_ledger_id"] != serde_json::to_value(rows.source_ledger_id)?
            || fact.admission.conflict_aliases != vec![task_revision_position(task)]
            || fact.admission.required_unconsumed != vec![task_revision_position(root)]
        {
            return Err(invalid("direct native source ancestry differs"));
        }
        let reference: ObjectRef =
            serde_json::from_value(envelope["predecessor_ref"].clone()).map_err(invalid)?;
        let predecessor = self.inspect_direct_native_predecessor(
            &reference,
            &fact.admission,
            &expected,
            std::slice::from_ref(creator),
            childless.then_some(task.task_id),
        )?;
        if predecessor.source_ledger_id != rows.source_ledger_id
            || !same(&predecessor.original_head, head)?
        {
            return Err(invalid(
                "direct native reads differ from the sealed predecessor",
            ));
        }
        Ok(())
    }
}
