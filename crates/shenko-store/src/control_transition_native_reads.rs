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
use shenko_types::{error::ShenkoError, ids::EntityId, operation::{AttemptRecord, OperationRecord}};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum NativeReadSelector {
    ArmOperation { operation_id: EntityId, task_id: EntityId, logical_key: String },
    ArmAttempt { attempt_id: EntityId, operation_id: EntityId },
    OriginalOperation { operation_id: EntityId },
    OriginalAttempt { attempt_id: EntityId },
    OriginalReceipt { receipt_id: EntityId },
}

impl NativeReadSelector {
    pub(crate) fn table_name(&self) -> &'static str {
        match self {
            Self::ArmOperation { .. } | Self::OriginalOperation { .. } => "operations",
            Self::ArmAttempt { .. } | Self::OriginalAttempt { .. } => "attempts",
            Self::OriginalReceipt { .. } => "evidence_receipts",
        }
    }

    fn matches_row(&self, row: &Row<'_>, table: &Table, remaining: usize) -> Result<bool, ShenkoError> {
        if !matches!(self, Self::ArmOperation { .. } | Self::ArmAttempt { .. }) {
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
            _ => Ok(true),
        }
    }

    pub(super) fn image(&self, conn: &Connection, table: &Table, remaining: &mut usize) -> Result<Image, ShenkoError> {
        if table.name != self.table_name() {
            return Err(invalid("native selector/table mismatch"));
        }
        let (key_column, condition, parameters): (&str, &str, Vec<String>) = match self {
            Self::ArmOperation { .. } => ("operation_id", "1", vec![]),
            Self::ArmAttempt { .. } => ("attempt_id", "1", vec![]),
            Self::OriginalOperation { operation_id } => ("operation_id", "operation_id=?1", vec![operation_id.to_string()]),
            Self::OriginalAttempt { attempt_id } => ("attempt_id", "attempt_id=?1", vec![attempt_id.to_string()]),
            Self::OriginalReceipt { receipt_id } => ("receipt_id", "receipt_id=?1", vec![receipt_id.to_string()]),
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
            if !self.matches_row(row, table, *remaining)? { continue; }
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
