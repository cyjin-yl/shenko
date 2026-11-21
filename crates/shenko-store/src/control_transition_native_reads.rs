//! Typed dependency predicates for direct native arm and original-result T2.
//!
//! These selectors only change which exact rows form a witness. Capture and
//! every materialization query the same predicate, including its empty range;
//! no cache, SQL supplied by a sender, or fresh execution permission is used.
use super::{
    Cell, Image, MAX_CAPTURE_BYTES, ROW_CAPTURE_REFUSAL, Table, charge_image,
    invalid, json_size, key_with_budget, quoted, storage_error,
};
use rusqlite::{Connection, Row, params_from_iter, types::ValueRef};
use serde::{Deserialize, Serialize};
use shenko_types::{error::ShenkoError, ids::EntityId, operation::{AttemptRecord, OperationRecord},
    subject::SubjectBranchHead, task::{TaskOwner, TaskRecord}};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeAncestry {
    pub task_id: EntityId,
    pub agent_id: EntityId,
    pub root_task_id: EntityId,
}

impl NativeAncestry {
    pub(crate) fn new(task: &TaskRecord, head: &SubjectBranchHead) -> Self {
        Self { task_id: task.task_id, agent_id: head.agent_id, root_task_id: head.subject_root_task_id }
    }

    /// Follow the same bounded owner chain as the admission position. The
    /// branch predicate stops before the root, but the Node read fence also
    /// includes that root's complete original row. Only IDs survive each read.
    fn keys(&self, conn: &Connection, remaining: &mut usize) -> Result<AncestryKeys, ShenkoError> {
        let mut tasks = BTreeSet::new();
        let mut steps = BTreeSet::new();
        let mut parents = BTreeSet::new();
        let mut next = self.task_id;
        let mut is_parent = false;
        let mut branch_open = true;
        let mut statement = conn.prepare("SELECT data FROM main.tasks WHERE task_id=?1").map_err(storage_error)?;
        loop {
            let mut query = statement.query([next.to_string()]).map_err(storage_error)?;
            let row = query.next().map_err(storage_error)?
                .ok_or_else(|| ShenkoError::not_found("native ancestry task"))?;
            let ValueRef::Text(data) = row.get_ref(0).map_err(storage_error)? else {
                return Err(invalid("native ancestry task is not JSON text"));
            };
            if data.len() > *remaining { return Err(ShenkoError::conflict(ROW_CAPTURE_REFUSAL)); }
            let task: TaskRecord = serde_json::from_slice(data).map_err(invalid)?;
            if task.task_id != next || task.agent_id != self.agent_id || task.record_schema_version != 1 {
                return Err(invalid("native ancestry physical identity or subject differs"));
            }
            charge_image(remaining, 128)?;
            tasks.insert(next.to_string());
            if branch_open {
                if task.kind == "subject_root" { branch_open = false; }
                else {
                    if let Some(step) = task.created_by_step_id { steps.insert(step.to_string()); }
                    if !matches!(task.owner, TaskOwner::Task { id } if id != self.root_task_id) {
                        branch_open = false;
                    }
                }
            }
            if is_parent && next == self.root_task_id { break; }
            let TaskOwner::Task { id } = task.owner else { break; };
            if !parents.insert(id) || parents.len() > 64 {
                return Err(invalid("native ancestry owner cycle or depth exceeded"));
            }
            next = id;
            is_parent = true;
        }
        Ok(AncestryKeys { tasks, steps })
    }
}

struct AncestryKeys { tasks: BTreeSet<String>, steps: BTreeSet<String> }

fn selected_keys(column: &str, keys: &BTreeSet<String>) -> (String, Vec<String>) {
    if keys.is_empty() { return ("0".into(), Vec::new()); }
    let parameters = keys.iter().cloned().collect::<Vec<_>>();
    let placeholders = (1..=parameters.len()).map(|index| format!("?{index}"))
        .collect::<Vec<_>>().join(",");
    (format!("{} IN ({placeholders})", quoted(column)), parameters)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum NativeReadSelector {
    ArmOperation { operation_id: EntityId, task_id: EntityId, logical_key: String },
    ArmAttempt { attempt_id: EntityId, operation_id: EntityId },
    OriginalOperation { operation_id: EntityId },
    OriginalAttempt { attempt_id: EntityId },
    OriginalReceipt { receipt_id: EntityId },
    TaskAncestry { ancestry: NativeAncestry, ordinary_children: bool },
    AuthoringSteps { ancestry: NativeAncestry },
}

impl NativeReadSelector {
    pub(crate) fn table_name(&self) -> &'static str {
        match self {
            Self::ArmOperation { .. } | Self::OriginalOperation { .. } => "operations",
            Self::ArmAttempt { .. } | Self::OriginalAttempt { .. } => "attempts",
            Self::OriginalReceipt { .. } => "evidence_receipts",
            Self::TaskAncestry { .. } => "tasks",
            Self::AuthoringSteps { .. } => "subject_steps",
        }
    }

    fn matches_row(&self, row: &Row<'_>, table: &Table, remaining: usize,
        ancestry_keys: Option<&AncestryKeys>) -> Result<bool, ShenkoError> {
        if !matches!(self, Self::ArmOperation { .. } | Self::ArmAttempt { .. }
            | Self::TaskAncestry { ordinary_children: true, .. }) {
            return Ok(true);
        }
        let cell = |name: &str| -> Result<ValueRef<'_>, ShenkoError> {
            let index = table.columns.iter().position(|column| column == name)
                .ok_or_else(|| invalid("native predicate column is missing"))?;
            row.get_ref(index).map_err(storage_error)
        };
        let ValueRef::Text(data) = cell("data")? else {
            return Err(invalid("native predicate original is not JSON text"));
        };
        if data.len() > remaining {
            return Err(ShenkoError::conflict(ROW_CAPTURE_REFUSAL));
        }
        // The old absence validator compares deserialized EntityId values,
        // not JSON spellings. Stream and parse one bounded original at a time:
        // UUID aliases (case, simple, braced or URN form) cannot hide a match.
        // Only matching rows are retained; this is still a full-table scan.
        match self {
            Self::ArmOperation { operation_id, task_id, logical_key } => {
                let original: OperationRecord = serde_json::from_slice(data).map_err(invalid)?;
                Ok(cell("operation_id")? == ValueRef::Text(operation_id.to_string().as_bytes())
                    || original.operation_id == *operation_id
                    || (original.task_id == *task_id && original.logical_key == *logical_key))
            }
            Self::ArmAttempt { attempt_id, operation_id } => {
                let original: AttemptRecord = serde_json::from_slice(data).map_err(invalid)?;
                Ok(cell("attempt_id")? == ValueRef::Text(attempt_id.to_string().as_bytes())
                    || original.attempt_id == *attempt_id
                    || cell("operation_id")? == ValueRef::Text(operation_id.to_string().as_bytes())
                    || original.operation_id == *operation_id)
            }
            Self::TaskAncestry { ancestry, ordinary_children: true } => {
                let original: TaskRecord = serde_json::from_slice(data).map_err(invalid)?;
                let ValueRef::Text(key) = cell("task_id")? else {
                    return Err(invalid("native task scope has a non-text primary key"));
                };
                let key = std::str::from_utf8(key).map_err(invalid)?;
                let keys = ancestry_keys.ok_or_else(|| invalid("native task scope lacks its ancestry"))?;
                Ok(keys.tasks.contains(key)
                    || (!original.background && original.owner == (TaskOwner::Task { id: ancestry.task_id })))
            }
            _ => Ok(true),
        }
    }

    pub(super) fn image(&self, conn: &Connection, table: &Table, remaining: &mut usize) -> Result<Image, ShenkoError> {
        if table.name != self.table_name() {
            return Err(invalid("native selector/table mismatch"));
        }
        let ancestry_keys = match self {
            Self::TaskAncestry { ancestry, .. } | Self::AuthoringSteps { ancestry } => Some(ancestry.keys(conn, remaining)?),
            _ => None,
        };
        let (key_column, condition, parameters): (&str, String, Vec<String>) = match self {
            Self::ArmOperation { .. } => ("operation_id", "1".into(), vec![]),
            Self::ArmAttempt { .. } => ("attempt_id", "1".into(), vec![]),
            Self::OriginalOperation { operation_id } => ("operation_id", "operation_id=?1".into(), vec![operation_id.to_string()]),
            Self::OriginalAttempt { attempt_id } => ("attempt_id", "attempt_id=?1".into(), vec![attempt_id.to_string()]),
            Self::OriginalReceipt { receipt_id } => ("receipt_id", "receipt_id=?1".into(), vec![receipt_id.to_string()]),
            Self::TaskAncestry { ordinary_children: true, .. } => ("task_id", "1".into(), vec![]),
            Self::TaskAncestry { ordinary_children: false, .. } => {
                let keys = ancestry_keys.as_ref().ok_or_else(|| invalid("missing native ancestry keys"))?;
                let (condition, parameters) = selected_keys("task_id", &keys.tasks);
                ("task_id", condition, parameters)
            }
            Self::AuthoringSteps { .. } => {
                let keys = ancestry_keys.as_ref().ok_or_else(|| invalid("missing native authoring keys"))?;
                let (condition, parameters) = selected_keys("step_id", &keys.steps);
                ("step_id", condition, parameters)
            }
        };
        let key_index = table.columns.iter().position(|name| name == key_column)
            .ok_or_else(|| invalid("native selector is missing its primary key"))?;
        if table.key != [key_index] {
            return Err(invalid("native selector requires its original single-column primary key"));
        }
        let table_bytes = json_size(table, *remaining, ROW_CAPTURE_REFUSAL)?;
        charge_image(remaining, table_bytes)?;
        let columns = table.columns.iter().map(|column| quoted(column)).collect::<Vec<_>>().join(",");
        let mut statement = conn.prepare(&format!("SELECT {columns} FROM main.{} WHERE {condition}", quoted(&table.name)))
            .map_err(storage_error)?;
        let mut query = statement.query(params_from_iter(parameters)).map_err(storage_error)?;
        let mut rows = BTreeMap::new();
        while let Some(row) = query.next().map_err(storage_error)? {
            if !self.matches_row(row, table, *remaining, ancestry_keys.as_ref())? { continue; }
            charge_image(remaining, 128)?;
            let cells = table.columns.len().checked_mul(std::mem::size_of::<Cell>())
                .ok_or_else(|| ShenkoError::conflict(ROW_CAPTURE_REFUSAL))?;
            charge_image(remaining, cells)?;
            let mut values = Vec::with_capacity(table.columns.len());
            for index in 0..table.columns.len() {
                if table.local_sequence == Some(index) { values.push(Cell::LocalSequence); }
                else {
                    let value = row.get_ref(index).map_err(storage_error)?;
                    charge_image(remaining, match value { ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes.len(), _ => 0 })?;
                    values.push(Cell::read(value)?);
                }
            }
            let key = key_with_budget(table, &values, remaining)?;
            if rows.insert(key, values).is_some() {
                return Err(invalid("native predicate returned duplicate primary keys"));
            }
        }
        Ok(Image { table: table.clone(), rows })
    }

    pub(super) fn current_image(&self, conn: &Connection, table: &Table) -> Result<Image, ShenkoError> {
        let mut remaining = MAX_CAPTURE_BYTES;
        self.image(conn, table, &mut remaining)
    }
}
