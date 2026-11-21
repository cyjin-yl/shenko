//! A chosen S1/native T2 is a frozen state transition, not a recipe to run again.
//!
//! Evaluate the ordinary transaction once in a rollback savepoint. SQLite's
//! authorizer records every table read by that evaluation and preupdate hooks
//! record actual writes. S1 v2 freezes whole-range digests and exact changed
//! rows; native v1 retains its complete witnesses for independent validation.
//! Seal the generated write bytes
//! (including random IDs and clock-dependent outcomes) before quorum admission.
//! Applying those bytes performs only SQLite writes, never a provider call.
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
    sync::Arc,
};

use parking_lot::Mutex;
use rusqlite::{
    Connection, OptionalExtension, Transaction,
    hooks::{AuthAction, AuthContext, Authorization},
    params, params_from_iter,
    types::{Value, ValueRef},
};
#[cfg(test)]
use rusqlite::TransactionBehavior;
use serde::{Deserialize, Serialize};
use serde::ser::SerializeSeq;
use shenko_types::{
    authority::{CommitAdmission, CommitAuthorityGrant},
    commit::{CommitReceipt, CommitRef, UpdatedEntity},
    error::ShenkoError,
    ids::{EntityId, LedgerSeq},
    object::{Digest, ObjectRef},
    subject::{SubjectBranchHead, SubjectStep},
    task::TaskRecord,
};

use crate::{
    Store,
    authority::{AdmissionPosition, AuthorityReservation, admission_materialized},
    store::{commit_error, storage_error},
};

const CODEC: &str = "store-s1-transition";
#[path = "control_transition_capture.rs"]
mod physical;
pub(crate) const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
// Account all retained before/after rows and duplicated change cells together,
// before the eventual sealed JSON size is known. Untouched tables are not
// scanned. S1 complete-read ranges stream borrowed values into fixed digests.
pub(crate) const MAX_CAPTURE_BYTES: usize = 128 * 1024 * 1024;
const ROW_CAPTURE_REFUSAL: &str = "canonical Store transition: capture budget exceeded: row capture exceeds 128 MiB memory budget";
const IMAGE_CAPTURE_REFUSAL: &str = "canonical Store transition: read/write image exceeds 64 MiB admission budget";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub(crate) enum Cell {
    Null,
    Integer(i64),
    /// Preserve the exact SQLite floating-point representation.
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
    LocalSequence,
}

impl Cell {
    fn read(value: ValueRef<'_>) -> Result<Self, ShenkoError> {
        Ok(match value {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(value) => Self::Integer(value),
            ValueRef::Real(value) => Self::Real(value.to_bits()),
            ValueRef::Text(value) => Self::Text(
                std::str::from_utf8(value)
                    .map_err(|_| conflict("non-UTF8 SQLite text"))?
                    .to_owned(),
            ),
            ValueRef::Blob(value) => Self::Blob(value.to_vec()),
        })
    }

    fn sql(&self, sequence: LedgerSeq) -> Result<Value, ShenkoError> {
        Ok(match self {
            Self::Null => Value::Null,
            Self::Integer(value) => Value::Integer(*value),
            Self::Real(value) => Value::Real(f64::from_bits(*value)),
            Self::Text(value) => Value::Text(value.clone()),
            Self::Blob(value) => Value::Blob(value.clone()),
            Self::LocalSequence => Value::Integer(
                i64::try_from(sequence.0)
                    .map_err(|_| conflict("local sequence exceeds SQLite integer range"))?,
            ),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Table {
    pub name: String,
    pub schema_digest: Digest,
    pub columns: Vec<String>,
    pub key: Vec<usize>,
    pub local_sequence: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Image {
    pub table: Table,
    pub rows: BTreeMap<String, Vec<Cell>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Change {
    pub key: String,
    pub before: Option<Vec<Cell>>,
    pub after: Option<Vec<Cell>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TableTransition {
    /// Table reads bind a complete range, including absence. Insert-only writes
    /// bind just their target keys, so unrelated parked replica tasks survive.
    pub complete_read: bool,
    /// S1 v2 retains a streaming digest of a complete read range. It has the
    /// same conservative absence/range semantics as v1, without serializing
    /// unrelated historical rows. Native v1 witnesses are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_range: Option<RangeImage>,
    pub before: Image,
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RangeImage {
    pub before: Digest,
    pub after: Digest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RowTransition {
    pub(crate) schema_version: u32,
    pub(crate) action_id: EntityId,
    pub(crate) input_digest: Digest,
    pub(crate) source_ledger_id: EntityId,
    pub(crate) original_head: SubjectBranchHead,
    pub(crate) result_commit: CommitRef,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    pub(crate) updated: Vec<UpdatedEntity>,
    /// Typed references extracted from SQL text/JSON cells. Keeping these
    /// explicit makes the accepted value's ordinary CAS walk complete.
    object_roots: Vec<ObjectRef>,
    pub(crate) tables: Vec<TableTransition>,
}

pub(crate) struct RowTransitionInput<'a> {
    pub action_id: EntityId,
    pub input_digest: Digest,
    pub head: &'a SubjectBranchHead,
}

pub(crate) struct PreparedTransition {
    pub admission: CommitAdmission,
    pub transition_ref: ObjectRef,
}

impl RowTransition {
    fn validate_capture_shape(&self) -> Result<(), ShenkoError> {
        match self.schema_version {
            1 if self.tables.iter().all(|table| table.read_range.is_none()) => Ok(()),
            2 if self.tables.iter().all(|table| table.complete_read == table.read_range.is_some()
                && table.before.rows.keys().all(|key| table.changes.iter().any(|change| &change.key == key))) => Ok(()),
            _ => Err(invalid("unsupported row capture schema or read witness")),
        }
    }

    /// Check the complete frozen predecessor and apply its exact writes atomically.
    pub(crate) fn apply_rows(
        &self,
        tx: &Transaction<'_>,
        sequence: LedgerSeq,
    ) -> Result<(), ShenkoError> {
        let _phase = crate::timing::Phase::bound("materialize.rows", crate::timing::Binding {
            subject_id: Some(self.original_head.agent_id), action_id: Some(self.action_id),
            ..Default::default()
        });
        let _preconditions = crate::timing::Phase::new("materialize.preconditions");
        self.validate_capture_shape()?;
        let actual_layouts = layouts(tx)?;
        let mut seen = BTreeSet::new();
        for table in &self.tables {
            let expected = &table.before;
            if !seen.insert(expected.table.name.clone()) {
                return Err(invalid("duplicate transition table"));
            }
            if actual_layouts.get(&expected.table.name) != Some(&expected.table)
                || current_image(tx, table)? != *expected
            {
                return Err(conflict(format!(
                    "predecessor/schema differs for {}",
                    expected.table.name
                )));
            }
        }
        // Preserve an actual read/hash error, rather than treating it as a
        // matching predecessor or silently weakening the range condition.
        for table in &self.tables {
            if let Some(range) = &table.read_range
                && range_digest(tx, &table.before.table)? != range.before
            {
                return Err(conflict(format!("read range differs for {}", table.before.table.name)));
            }
        }
        drop(_preconditions);
        let _writes = crate::timing::Phase::new("materialize.writes");
        tx.execute_batch("PRAGMA defer_foreign_keys=ON")
            .map_err(storage_error)?;
        for table in &self.tables {
            apply_table(tx, table, sequence)?;
        }
        drop(_writes);
        let _postconditions = crate::timing::Phase::new("materialize.postconditions");
        for table in &self.tables {
            let mut expected = table.before.clone();
            for change in &table.changes {
                if let Some(after) = &change.after {
                    expected.rows.insert(change.key.clone(), after.clone());
                } else {
                    expected.rows.remove(&change.key);
                }
            }
            if current_image(tx, table)? != expected
                || match &table.read_range { Some(range) => range_digest(tx, &expected.table)? != range.after, None => false } {
                return Err(invalid(format!("applied writes differ from frozen result for {}", expected.table.name)));
            }
        }
        Ok(())
    }

    /// Bind the frozen rows to the public S1 identities, independently of the
    /// producer that captured them. The remaining SQL rows are exact deltas;
    /// head, committed result and successor may never be omitted or renamed.
    pub(crate) fn validate_result(
        &self,
        step: &SubjectStep,
        head: &SubjectBranchHead,
        next: EntityId,
    ) -> Result<(), ShenkoError> {
        if step.state != shenko_types::subject::SubjectStepState::Committed
            || step.step_id != head.next_step_id
            || step.branch_id != head.branch_id
            || step.expected_parent_commit != head.committed_at
            || step.controller_epoch != head.controller_epoch
            || step.software_generation != head.software_generation
            || next == step.step_id
        {
            return Err(invalid("result is not the original committed S1 position"));
        }
        let mut committed = step.clone();
        committed.commit_ref = Some(self.result_commit);
        let mut advanced = head.clone();
        advanced.committed_step_no = step.step_no;
        advanced.committed_step_id = Some(step.step_id);
        advanced.committed_at = self.result_commit;
        advanced.next_step_id = next;
        advanced.record_revision = shenko_types::ids::RecordRevision(
            head.record_revision
                .0
                .checked_add(1)
                .ok_or_else(|| invalid("head revision exhausted"))?,
        );
        let successor = SubjectStep {
            step_id: next,
            branch_id: head.branch_id.clone(),
            step_no: step
                .step_no
                .checked_add(1)
                .ok_or_else(|| invalid("step number exhausted"))?,
            expected_parent_commit: self.result_commit,
            controller_epoch: head.controller_epoch,
            software_generation: head.software_generation.clone(),
            state: shenko_types::subject::SubjectStepState::Planned,
            input_plan_ref: None,
            context_revision: None,
            active_request_id: None,
            active_attempt_id: None,
            model_result_ref: None,
            model_outcome_ref: None,
            decision_ref: None,
            commit_ref: None,
            retry_reason_ref: None,
            record_revision: shenko_types::ids::RecordRevision(0),
            created_by_step_id: Some(step.step_id),
        };
        for (table_name, id_column, id, expected, inserts) in [
            (
                "subject_branch_heads",
                "agent_id",
                head.agent_id,
                serde_json::to_value(&advanced)?,
                false,
            ),
            (
                "subject_steps",
                "step_id",
                step.step_id,
                serde_json::to_value(&committed)?,
                false,
            ),
            (
                "subject_steps",
                "step_id",
                next,
                serde_json::to_value(&successor)?,
                true,
            ),
        ] {
            let table = self
                .tables
                .iter()
                .find(|table| table.before.table.name == table_name)
                .ok_or_else(|| invalid("missing S1 result table"))?;
            if !table.complete_read {
                return Err(invalid("S1 predecessor is not a complete read"));
            }
            let column = table
                .before
                .table
                .columns
                .iter()
                .position(|name| name == id_column)
                .ok_or_else(|| invalid("missing identity column"))?;
            let data = table
                .before
                .table
                .columns
                .iter()
                .position(|name| name == "data")
                .ok_or_else(|| invalid("missing result data"))?;
            let change = table
                .changes
                .iter()
                .find(|change| {
                    change.after.as_ref().and_then(|row| row.get(column))
                        == Some(&Cell::Text(id.to_string()))
                })
                .ok_or_else(|| invalid("missing original S1 result identity"))?;
            let Some(Cell::Text(json)) = change.after.as_ref().and_then(|row| row.get(data)) else {
                return Err(invalid("missing frozen result bytes"));
            };
            if change.before.is_none() != inserts
                || serde_json::from_str::<serde_json::Value>(json)? != expected
            {
                return Err(invalid(
                    "S1 result rows differ from their admitted identities",
                ));
            }
        }
        let updated = self
            .updated
            .iter()
            .map(|entity| (entity.entity_id, entity.record_revision))
            .collect::<Vec<_>>();
        if updated
            != vec![
                (step.step_id, step.record_revision),
                (head.agent_id, advanced.record_revision),
                (next, successor.record_revision),
            ]
        {
            return Err(invalid("S1 receipt identities differ"));
        }
        Ok(())
    }
}

fn invalid(reason: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::other(format!("invalid canonical Store transition: {reason}"))
}

fn conflict(reason: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::conflict(format!("canonical Store transition: {reason}"))
}

fn quoted(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

pub(crate) fn local_table(name: &str) -> bool {
    matches!(
        name,
        "schema_version"
            | "commit_log"
            | "ledger_identity"
            | "commit_authority_policy"
            | "subject_authority_bindings"
            | "subject_dispatch_owner"
            | "control_prepared_transitions"
            | "control_materializations"
            | "native_evidence_materializations"
            | "native_evidence_seal_debts"
            | "native_evidence_deliveries"
            | "local_installation_provisioning"
            | "local_model_health"
    )
}

/// These two tables invalidate a local context cache. They are rebuilt by
/// factual row triggers and their counters have no cross-ledger meaning.
fn execution_projection(name: &str) -> bool {
    matches!(name, "context_execution_clock" | "context_execution_revisions")
}

fn execution_projection_trigger(accessor: Option<&str>) -> bool {
    accessor.and_then(|name| name.strip_prefix("context_revision_"))
        .and_then(|name| name.rsplit_once('_'))
        .is_some_and(|(table, operation)| matches!(table,
            "subject_intents" | "tasks" | "memory_intents" | "task_results" | "operations")
            && matches!(operation, "insert" | "update" | "delete"))
}

fn verified_projection_triggers(conn: &Connection) -> Result<BTreeMap<String, String>, ShenkoError> {
    let mut stmt = conn
        .prepare(
            "SELECT name, tbl_name, sql FROM main.sqlite_master \
             WHERE type='trigger' AND name LIKE 'context_revision_%' AND sql IS NOT NULL",
        )
        .map_err(storage_error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_error)?;
    let mut verified = BTreeMap::new();
    for (name, tbl_name, sql) in rows {
        if execution_projection_trigger(Some(&name)) {
            if !crate::subject_state::canonical_projection_trigger(&name, &tbl_name, &sql) {
                return Err(conflict("non-canonical execution projection trigger"));
            }
            verified.insert(name, tbl_name);
        }
    }
    Ok(verified)
}

fn layouts(conn: &Connection) -> Result<BTreeMap<String, Table>, ShenkoError> {
    // table_list distinguishes virtual/shadow FTS projections from base facts.
    // Triggers rebuild those and the local execution invalidation index from
    // the exact admitted base writes; freezing the index would apply it twice.
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_list WHERE schema='main' AND type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .map_err(storage_error)?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_error)?;
    let mut tables = BTreeMap::new();
    for name in names {
        if local_table(&name) || execution_projection(&name) {
            continue;
        }
        let mut columns = conn
            .prepare(&format!("PRAGMA table_info({})", quoted(&name)))
            .map_err(storage_error)?;
        let columns = columns
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
            })
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        let mut key = columns
            .iter()
            .enumerate()
            .filter(|(_, (_, ordinal))| *ordinal > 0)
            .map(|(index, (_, ordinal))| (*ordinal, index))
            .collect::<Vec<_>>();
        key.sort_unstable();
        let mut schema = conn.prepare("SELECT type,name,sql FROM sqlite_master WHERE tbl_name=?1 AND sql IS NOT NULL ORDER BY type,name")
            .map_err(storage_error)?;
        let mut schema = schema
            .query_map([&name], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        if schema.iter().any(|(kind, name, _)| kind == "trigger"
            && execution_projection_trigger(Some(name)))
        {
            // The receiver regenerates this local index through these row
            // triggers. Bind its schema too: a new secondary trigger on the
            // projection must not introduce writes outside the frozen result.
            let mut projection_schema = conn.prepare("SELECT type,name,sql FROM sqlite_master
                WHERE tbl_name IN ('context_execution_clock','context_execution_revisions')
                AND sql IS NOT NULL ORDER BY type,name").map_err(storage_error)?;
            schema.extend(projection_schema.query_map([], |row| Ok((row.get::<_, String>(0)?,
                row.get::<_, String>(1)?, row.get::<_, String>(2)?))).map_err(storage_error)?
                .collect::<rusqlite::Result<Vec<_>>>().map_err(storage_error)?);
            schema.sort();
        }
        let local_sequence = crate::cognition::control_local_sequence(&name)
            .and_then(|local| columns.iter().position(|(column, _)| column == local));
        tables.insert(
            name.clone(),
            Table {
                name,
                schema_digest: Digest::blake3(&serde_json::to_vec(&schema)?),
                columns: columns.into_iter().map(|(name, _)| name).collect(),
                key: key.into_iter().map(|(_, index)| index).collect(),
                local_sequence,
            },
        );
    }
    Ok(tables)
}

struct KeyCells<'a> {
    table: &'a Table,
    values: &'a [Cell],
}

impl Serialize for KeyCells<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.table.key.len()))?;
        for &index in &self.table.key {
            let cell = self.values.get(index)
                .ok_or_else(|| <S::Error as serde::ser::Error>::custom("key index outside row"))?;
            sequence.serialize_element(cell)?;
        }
        sequence.end()
    }
}

/// Serde may expand a text/blob key many times beyond its SQL byte length.
/// Count through a bounded sink before allocating the exact output buffer.
/// The same writer bounds the second, purely representational serialization;
/// neither pass calls the transaction's semantic evaluation callback.
struct JsonWriter<W> {
    inner: W,
    limit: usize,
    written: usize,
    exceeded: bool,
}

impl<W: Write> Write for JsonWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.written) {
            self.exceeded = true;
            return Err(io::Error::other("canonical JSON budget exhausted"));
        }
        let written = self.inner.write(bytes)?;
        self.written += written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn json_size<T: Serialize + ?Sized>(
    value: &T,
    limit: usize,
    refusal: &'static str,
) -> Result<usize, ShenkoError> {
    let mut writer = JsonWriter { inner: io::sink(), limit, written: 0, exceeded: false };
    let result = serde_json::to_writer(&mut writer, value);
    if writer.exceeded {
        return Err(ShenkoError::conflict(refusal));
    }
    result?;
    Ok(writer.written)
}

fn json_bytes<T: Serialize + ?Sized>(value: &T, size: usize) -> Result<Vec<u8>, ShenkoError> {
    let mut writer = JsonWriter {
        inner: Vec::with_capacity(size), limit: size, written: 0, exceeded: false,
    };
    let result = serde_json::to_writer(&mut writer, value);
    if writer.exceeded {
        return Err(invalid("immutable JSON changed after measurement"));
    }
    result?;
    if writer.written != size {
        return Err(invalid("immutable JSON changed after measurement"));
    }
    Ok(writer.inner)
}

fn key_with_budget(
    table: &Table,
    values: &[Cell],
    remaining: &mut usize,
) -> Result<String, ShenkoError> {
    if values.len() != table.columns.len() || table.key.is_empty() {
        return Err(invalid(format!(
            "{} has no complete stable primary key",
            table.name
        )));
    }
    for &index in &table.key {
        let cell = values.get(index).ok_or_else(|| invalid("key index outside row"))?;
        if matches!(cell, Cell::Null | Cell::LocalSequence) {
            return Err(invalid("null or local primary key"));
        }
    }
    let cells = KeyCells { table, values };
    let size = json_size(&cells, *remaining, ROW_CAPTURE_REFUSAL)?;
    charge_image(remaining, size)?;
    String::from_utf8(json_bytes(&cells, size)?)
        .map_err(|error| invalid(format!("JSON key is not UTF-8: {error}")))
}

fn key(table: &Table, values: &[Cell]) -> Result<String, ShenkoError> {
    let mut remaining = MAX_CAPTURE_BYTES;
    key_with_budget(table, values, &mut remaining)
}

fn image(conn: &Connection, table: &Table) -> Result<Image, ShenkoError> {
    let mut remaining = MAX_CAPTURE_BYTES;
    image_with_budget(conn, table, &mut remaining)
}

fn charge_image(remaining: &mut usize, bytes: usize) -> Result<(), ShenkoError> {
    *remaining = remaining
        .checked_sub(bytes)
        .ok_or_else(|| ShenkoError::conflict(ROW_CAPTURE_REFUSAL))?;
    Ok(())
}

fn sealed_image_bytes<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, ShenkoError> {
    let size = json_size(value, MAX_IMAGE_BYTES, IMAGE_CAPTURE_REFUSAL)?;
    json_bytes(value, size)
}

/// These two pre-preparation capacity refusals preserve a precise diagnostic.
/// S1 alone may then attempt its original plan through ordinary ordered
/// admission. Prepared/chosen transitions and native execution never use it.
pub(crate) fn capture_budget_refusal(error: &ShenkoError) -> bool {
    matches!(error, ShenkoError::Conflict(message)
        if message == ROW_CAPTURE_REFUSAL || message == IMAGE_CAPTURE_REFUSAL)
}

fn image_with_budget(
    conn: &Connection,
    table: &Table,
    remaining: &mut usize,
) -> Result<Image, ShenkoError> {
    let table_bytes = json_size(table, *remaining, ROW_CAPTURE_REFUSAL)?;
    charge_image(remaining, table_bytes)?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {} FROM {}",
            table
                .columns
                .iter()
                .map(|column| quoted(column))
                .collect::<Vec<_>>()
                .join(","),
            quoted(&table.name)
        ))
        .map_err(storage_error)?;
    let mut query = stmt.query([]).map_err(storage_error)?;
    let mut rows = BTreeMap::new();
    while let Some(row) = query.next().map_err(storage_error)? {
        // Row containers and the BTree node have overhead even for empty
        // values. Check variable-size SQLite values before making owned copies.
        charge_image(remaining, 128)?;
        let cell_bytes = table.columns.len().checked_mul(std::mem::size_of::<Cell>())
            .ok_or_else(|| ShenkoError::conflict(ROW_CAPTURE_REFUSAL))?;
        charge_image(remaining, cell_bytes)?;
        let mut values = Vec::with_capacity(table.columns.len());
        for index in 0..table.columns.len() {
            if Some(index) == table.local_sequence {
                values.push(Cell::LocalSequence);
            } else {
                let value = row.get_ref(index).map_err(storage_error)?;
                let bytes = match value {
                    ValueRef::Text(text) => text.len(),
                    ValueRef::Blob(bytes) => bytes.len(),
                    _ => 0,
                };
                charge_image(remaining, bytes)?;
                values.push(Cell::read(value)?);
            }
        }
        let key = key_with_budget(table, &values, remaining)?;
        if rows.insert(key, values).is_some() {
            return Err(invalid("duplicate primary key"));
        }
    }
    Ok(Image {
        table: table.clone(),
        rows,
    })
}

#[derive(Default)]
struct Accesses {
    reads: BTreeSet<String>,
    writes: BTreeSet<String>,
    refusal: Option<&'static str>,
}

struct Authorizer<'a>(&'a Connection);
impl Drop for Authorizer<'_> {
    fn drop(&mut self) {
        self.0
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    }
}

fn observe(
    conn: &Connection,
    accesses: Arc<Mutex<Accesses>>,
    main_objects: BTreeSet<String>,
    verified_projection_triggers: BTreeMap<String, String>,
) -> Authorizer<'_> {
    conn.flush_prepared_statement_cache();
    conn.authorizer(Some(move |context: AuthContext<'_>| {
        let mut accesses = accesses.lock();
        match context.action {
            AuthAction::Read { table_name, column_name } => {
                // SQLite's no-column read (for example unqualified count(*))
                // reports the FROM item's optional database qualifier, not
                // its resolved database. None is main only because the
                // same-transaction preflight excluded every other schema's
                // objects before this observer was installed. Never infer
                // main merely from a matching table name in the main schema.
                let unqualified_main = context.database_name.is_none()
                    && column_name.is_empty() && main_objects.contains(table_name);
                if context.database_name != Some("main") && !unqualified_main {
                    accesses.refusal = Some("transition accessed a non-main database");
                    return Authorization::Deny;
                }
                if execution_projection(table_name)
                    && !context
                        .accessor
                        .is_some_and(|accessor| verified_projection_triggers.contains_key(accessor))
                {
                    accesses.refusal = Some("transition directly accessed a local execution projection");
                    return Authorization::Deny;
                }
                // NEW/OLD column references belong to this exact changed row,
                // not a semantic read of every unrelated row in its table.
                let verified_trigger_own_table = context.accessor.is_some_and(|accessor| {
                    verified_projection_triggers
                        .get(accessor)
                        .is_some_and(|target| target == table_name)
                });
                if !verified_trigger_own_table {
                    accesses.reads.insert(table_name.to_owned());
                }
            }
            AuthAction::Insert { table_name }
            | AuthAction::Delete { table_name }
            | AuthAction::Update { table_name, .. } => {
                if context.database_name != Some("main") {
                    accesses.refusal = Some("transition accessed a non-main database");
                    return Authorization::Deny;
                }
                if execution_projection(table_name)
                    && !context
                        .accessor
                        .is_some_and(|accessor| verified_projection_triggers.contains_key(accessor))
                {
                    accesses.refusal = Some("transition directly accessed a local execution projection");
                    return Authorization::Deny;
                }
                accesses.writes.insert(table_name.to_owned());
            }
            AuthAction::Select | AuthAction::Function { .. } | AuthAction::Recursive => {}
            // FTS checks this read-only main-database counter while updating
            // its derived index. No argument does not make other PRAGMAs
            // read-only (for example optimize or incremental_vacuum).
            AuthAction::Pragma { pragma_name, pragma_value: None }
                if pragma_name.eq_ignore_ascii_case("data_version")
                    && context.database_name == Some("main") => {}
            // Row transitions cannot describe DDL, attachments, connection
            // changes or an escape from their enclosing transaction/savepoint.
            // Do not let a newly created table disappear into the projection
            // exception below. Schema comparison also catches an unobserved DDL.
            _ => {
                accesses.refusal = Some("transition attempted non-row SQL");
                return Authorization::Deny;
            }
        }
        Authorization::Allow
    }));
    Authorizer(conn)
}

/// Unambiguous cell framing, without allocating an owned SQL value. The caller
/// explicitly substitutes the local-sequence marker for logical read ranges;
/// actual sequence-only writes are still seen by the physical preupdate hook.
fn hash_cell(hasher: &mut blake3::Hasher, value: ValueRef<'_>) {
    match value {
        ValueRef::Null => { hasher.update(&[0]); }
        ValueRef::Integer(value) => {
            hasher.update(&[1]);
            hasher.update(&value.to_le_bytes());
        }
        ValueRef::Real(value) => {
            hasher.update(&[2]);
            hasher.update(&value.to_bits().to_le_bytes());
        }
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            hasher.update(&[if matches!(value, ValueRef::Text(_)) { 3 } else { 4 }]);
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
    }
}

fn hash_query(
    conn: &Connection,
    sql: &str,
    hasher: &mut blake3::Hasher,
) -> Result<(), ShenkoError> {
    let mut stmt = conn.prepare(sql).map_err(storage_error)?;
    let columns = stmt.column_count();
    hasher.update(&(columns as u64).to_le_bytes());
    let mut rows = stmt.query([]).map_err(storage_error)?;
    while let Some(row) = rows.next().map_err(storage_error)? {
        hasher.update(&[1]);
        for index in 0..columns {
            hash_cell(hasher, row.get_ref(index).map_err(storage_error)?);
        }
    }
    hasher.update(&[0]);
    Ok(())
}

/// Stream the complete logical range without retaining its historical cells.
/// Local materialization sequence numbers use the same projection as Image.
/// This remains a whole-table dependency, including empty ranges and absence.
fn range_digest(conn: &Connection, table: &Table) -> Result<Digest, ShenkoError> {
    let columns = table.columns.iter().map(|column| quoted(column)).collect::<Vec<_>>();
    let order = table.key.iter().copied()
        .chain((0..columns.len()).filter(|index| !table.key.contains(index)))
        .map(|index| format!("{} COLLATE BINARY", columns[index]))
        .collect::<Vec<_>>().join(",");
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"shenko-canonical-read-range-v2\0");
    hasher.update(table.schema_digest.0.as_bytes());
    hasher.update(&(columns.len() as u64).to_le_bytes());
    let mut statement = conn.prepare(&format!("SELECT {} FROM main.{} ORDER BY {order}",
        columns.join(","), quoted(&table.name))).map_err(storage_error)?;
    let mut rows = statement.query([]).map_err(storage_error)?;
    while let Some(row) = rows.next().map_err(storage_error)? {
        hasher.update(&[1]);
        for index in 0..columns.len() {
            if table.local_sequence == Some(index) { hasher.update(&[5]); }
            else { hash_cell(&mut hasher, row.get_ref(index).map_err(storage_error)?); }
        }
    }
    hasher.update(&[0]);
    Ok(Digest(format!("blake3:{}", hasher.finalize().to_hex())))
}

/// Source-local structural guard, not a replicated physical-layout identity.
/// Include every existing schema, its version and objects (including unkeyed,
/// virtual/shadow and local tables) so new/disappearing tables or DDL are never
/// silently treated as a derived projection. The callback itself denies
/// ATTACH/DETACH and non-main factual accesses before they can escape rollback.
fn schema_fingerprint(conn: &Connection) -> Result<blake3::Hash, ShenkoError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"shenko-canonical-schema-guard-v1\0");
    hash_query(conn, "SELECT seq,name,file FROM pragma_database_list ORDER BY name COLLATE BINARY", &mut hasher)?;
    let mut stmt = conn.prepare("SELECT name FROM pragma_database_list ORDER BY name COLLATE BINARY")
        .map_err(storage_error)?;
    let names = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(storage_error)?;
    drop(stmt);
    for name in names {
        hash_cell(&mut hasher, ValueRef::Text(name.as_bytes()));
        hash_query(conn, &format!("PRAGMA {}.schema_version", quoted(&name)), &mut hasher)?;
        hash_query(conn, &format!("SELECT type,name,tbl_name,rootpage,sql FROM {}.sqlite_schema ORDER BY type COLLATE BINARY,name COLLATE BINARY", quoted(&name)), &mut hasher)?;
    }
    Ok(hasher.finalize())
}

fn projection_tables(conn: &Connection) -> Result<BTreeSet<String>, ShenkoError> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_list WHERE schema='main' AND
        (type!='table' OR name LIKE 'sqlite_%' OR name IN ('context_execution_clock','context_execution_revisions'))")
        .map_err(storage_error)?;
    let names = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(storage_error)?
        .collect::<rusqlite::Result<BTreeSet<_>>>().map_err(storage_error)?;
    Ok(names)
}

/// A no-column SQLITE_READ can omit its resolved database name. Freeze an
/// unambiguous namespace before observation: main may contain facts, while
/// temp/attached databases must have no schema objects at all. ATTACH/DDL stay
/// denied during evaluation, and the independent schema fingerprint checks
/// the same boundary again before and after rollback.
fn require_main_factual_schema(conn: &Connection) -> Result<(), ShenkoError> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_database_list WHERE name!='main'")
        .map_err(storage_error)?;
    let names = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(storage_error)?;
    drop(stmt);
    for name in names {
        let has_objects: bool = conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {}.sqlite_schema)", quoted(&name)),
            [], |row| row.get(0),
        ).map_err(storage_error)?;
        if has_objects {
            return Err(conflict("capture requires no non-main schema objects"));
        }
    }
    Ok(())
}

fn charge_row_copy(remaining: &mut usize, values: &[Cell]) -> Result<(), ShenkoError> {
    for value in values {
        charge_image(remaining, std::mem::size_of::<Cell>())?;
        charge_image(remaining, match value {
            Cell::Text(text) => text.len(),
            Cell::Blob(bytes) => bytes.len(),
            _ => 0,
        })?;
    }
    Ok(())
}

fn row_changes(
    before: &Image,
    mut after: Image,
    remaining: &mut usize,
) -> Result<Vec<Change>, ShenkoError> {
    let mut changes = Vec::new();
    for (key, old) in &before.rows {
        let new = after.rows.remove(key);
        if new.as_ref() == Some(old) { continue; }
        reserve_change(&mut changes, remaining)?;
        charge_image(remaining, key.len())?;
        charge_row_copy(remaining, old)?;
        changes.push(Change { key: key.clone(), before: Some(old.clone()), after: new });
    }
    // Move already charged postimages into inserts rather than copying them.
    for (key, new) in after.rows {
        reserve_change(&mut changes, remaining)?;
        changes.push(Change { key, before: None, after: Some(new) });
    }
    Ok(changes)
}

fn reserve_change(changes: &mut Vec<Change>, remaining: &mut usize) -> Result<(), ShenkoError> {
    if changes.len() == changes.capacity() {
        let capacity = changes.capacity().max(1).checked_mul(2)
            .ok_or_else(|| ShenkoError::conflict(ROW_CAPTURE_REFUSAL))?;
        let bytes = capacity.checked_mul(std::mem::size_of::<Change>())
            .ok_or_else(|| ShenkoError::conflict(ROW_CAPTURE_REFUSAL))?;
        // Charge the entire new allocation before reserving. The old buffer's
        // earlier charge stays consumed, also accounting for a copying grow.
        charge_image(remaining, bytes)?;
        changes.try_reserve_exact(capacity - changes.len())
            .map_err(|error| invalid(format!("cannot allocate row changes: {error}")))?;
    }
    Ok(())
}

fn input_digest(
    step: &SubjectStep,
    head: &SubjectBranchHead,
    next: EntityId,
) -> Result<Digest, ShenkoError> {
    Ok(Digest::blake3(&serde_json::to_vec(
        &serde_json::json!({"step":step,"head":head,"next":next}),
    )?))
}

impl Store {
    /// Native v1 retains full read witnesses used by its typed validators.
    /// A direct action may supply the same independently proven target that
    /// its native validator already permits it to narrow after capture. The
    /// physical observer still retains every changed row, and the exact target
    /// shape is checked before this capture can become an admission.
    pub(crate) fn capture_row_transition<T>(
        &self, tx: &Transaction<'_>, input: RowTransitionInput<'_>,
        direct_task: Option<&TaskRecord>,
        evaluate: impl FnOnce(CommitRef, chrono::DateTime<chrono::Utc>)
            -> Result<(Vec<UpdatedEntity>, T), ShenkoError>,
    ) -> Result<(RowTransition, T), ShenkoError> {
        self.capture_row_transition_inner(tx, input, false, direct_task, evaluate)
    }

    /// S1 v2 freezes the same conservative complete read ranges as streamed
    /// digests. Only physically touched rows become retained write witnesses.
    fn capture_row_transition_with_ranges<T>(
        &self, tx: &Transaction<'_>, input: RowTransitionInput<'_>,
        evaluate: impl FnOnce(CommitRef, chrono::DateTime<chrono::Utc>)
            -> Result<(Vec<UpdatedEntity>, T), ShenkoError>,
    ) -> Result<(RowTransition, T), ShenkoError> {
        self.capture_row_transition_inner(tx, input, true, None, evaluate)
    }

    /// Evaluate exactly once in a rollback savepoint. No provider call belongs
    /// in this callback; generated IDs/time and the real write bytes are frozen.
    fn capture_row_transition_inner<T>(
        &self, tx: &Transaction<'_>, input: RowTransitionInput<'_>, summarize_reads: bool,
        direct_task: Option<&TaskRecord>,
        evaluate: impl FnOnce(CommitRef, chrono::DateTime<chrono::Utc>)
            -> Result<(Vec<UpdatedEntity>, T), ShenkoError>,
    ) -> Result<(RowTransition, T), ShenkoError> {
        let _phase = crate::timing::Phase::bound("capture.rows", crate::timing::Binding {
            subject_id: Some(input.head.agent_id), action_id: Some(input.action_id),
            ..Default::default()
        });
        let _schema = crate::timing::Phase::new("capture.schema");
        // Authorizer installation cannot observe an already active cursor.
        if tx.is_busy() { return Err(conflict("capture began with an active SQLite statement")); }
        require_main_factual_schema(tx)?;
        let verified_projection_triggers = verified_projection_triggers(tx)?;
        let tables = layouts(tx)?;
        let projections = projection_tables(tx)?;
        let main_objects = tables.keys().chain(projections.iter()).cloned().collect();
        let schema = schema_fingerprint(tx)?;
        drop(_schema);
        let result_commit = CommitRef::new(self.ledger.ledger_id(), self.ledger.allocate_seq(tx)?);
        let created_at = chrono::Utc::now();
        tx.execute_batch("SAVEPOINT canonical_row_evaluation").map_err(storage_error)?;
        let accesses = Arc::new(Mutex::new(Accesses::default()));
        let physical = physical::Observer::start(tx, tables.clone(), projections.clone(), true);
        let observation = observe(
            tx,
            Arc::clone(&accesses),
            main_objects,
            verified_projection_triggers,
        );
        let evaluated = crate::timing::measure("capture.evaluate", || evaluate(result_commit, created_at));
        drop(observation);
        let mut physical = physical.finish();
        let accesses = std::mem::take(&mut *accesses.lock());
        let captured = crate::timing::measure("capture.postimages", || -> Result<_, ShenkoError> {
            if let Some(refusal) = accesses.refusal { return Err(conflict(refusal)); }
            let (updated, result) = evaluated?;
            if layouts(tx)? != tables || schema_fingerprint(tx)? != schema {
                return Err(conflict("transition changed a schema or table layout"));
            }
            if let Some(error) = physical.refusal.take() { return Err(error); }
            // Actual writes are independently observed at execution time,
            // including a cached statement or an observer removed by a helper.
            // Unrelated retained tables are never scanned to discover writes.
            for name in &physical.tables {
                if (!projections.contains(name) || execution_projection(name)) && !accesses.writes.contains(name) {
                    return Err(conflict(format!("unobserved transition write to {name}")));
                }
            }
            let mut remaining = physical.remaining;
            let mut after = BTreeMap::new();
            let mut range_after = BTreeMap::new();
            for name in accesses.reads.union(&accesses.writes) {
                if local_table(name) { return Err(conflict(format!("transition depended on node-local table {name}"))); }
                let Some(table) = tables.get(name) else {
                    if projections.contains(name) { continue; }
                    return Err(conflict(format!("transition touched unknown table {name}")));
                };
                if table.key.is_empty() { return Err(conflict(format!("transition touched unkeyed table {name}"))); }
                let full_witness = !summarize_reads && accesses.reads.contains(name)
                    && !(name == "tasks" && direct_task.is_some());
                let postimage = if !full_witness {
                    if summarize_reads && accesses.reads.contains(name) {
                        range_after.insert(name.clone(), range_digest(tx, table)?);
                    }
                    image_for_keys_with_budget(tx, table,
                        physical.rows.get(name).into_iter().flat_map(|rows| rows.values())
                            .map(|row| row.key_values.as_slice()), &mut remaining)?
                } else { image_with_budget(tx, table, &mut remaining)? };
                after.insert(name.clone(), postimage);
            }
            Ok((updated, result, after, range_after, accesses, remaining))
        });
        // Even a refused/failed evaluation is rolled back before returning.
        tx.execute_batch("ROLLBACK TO canonical_row_evaluation; RELEASE canonical_row_evaluation")
            .map_err(storage_error)?;
        let (updated, result, after, range_after, accesses, mut remaining) = captured?;
        // FTS rollback closes its own incremental reader. An application-owned
        // cursor must not escape the one captured evaluation either.
        if tx.is_busy() { return Err(conflict("capture ended with an active SQLite statement")); }
        if layouts(tx)? != tables || schema_fingerprint(tx)? != schema {
            return Err(invalid("rollback did not restore the captured schema"));
        }
        let _preimages = crate::timing::Phase::new("capture.preimages");
        let mut transitions = Vec::new();
        for (name, after) in after {
            let complete_read = accesses.reads.contains(&name)
                && !(name == "tasks" && direct_task.is_some());
            let full_witness = !summarize_reads && complete_read;
            let mut original = if !full_witness {
                Image { table: tables[&name].clone(), rows: physical.rows.remove(&name)
                    .unwrap_or_default().into_iter().filter_map(|(key, row)| row.before.map(|row| (key, row))).collect() }
            } else { image_with_budget(tx, &tables[&name], &mut remaining)? };
            let changes = row_changes(&original, after, &mut remaining)?;
            if summarize_reads || !complete_read {
                original.rows.retain(|key, _| changes.iter().any(|change| &change.key == key));
            }
            let read_range = if summarize_reads && complete_read {
                Some(RangeImage { before: range_digest(tx, &tables[&name])?,
                    after: range_after.get(&name).cloned().ok_or_else(|| invalid("missing captured post-range"))? })
            } else { None };
            transitions.push(TableTransition { complete_read, read_range, before: original, changes });
        }
        if let Some(task) = direct_task {
            let table = transitions.iter().find(|table| table.before.table.name == "tasks")
                .ok_or_else(|| invalid("direct capture omitted its target task"))?;
            crate::native_facts::validate_task_read(table, task)?;
        }
        drop(_preimages);
        let object_roots = self.transition_object_roots(&transitions);
        let (_, missing) = crate::timing::measure("capture.closure", || self.collect_closure(&object_roots)).map_err(storage_error)?;
        if !missing.is_empty() {
            return Err(ShenkoError::ObjectClosureIncomplete { missing_count: missing.len() });
        }
        let transition = RowTransition {
            schema_version: if summarize_reads { 2 } else { 1 },
            action_id: input.action_id, input_digest: input.input_digest,
            source_ledger_id: self.ledger.ledger_id(), original_head: input.head.clone(),
            result_commit, created_at, updated, object_roots, tables: transitions,
        };
        transition.validate_capture_shape()?;
        Ok((transition, result))
    }

    pub(crate) fn prepare_step_transition(
        &self,
        step: &SubjectStep,
        head: &SubjectBranchHead,
        next: EntityId,
        mut position: AdmissionPosition,
    ) -> Result<PreparedTransition, ShenkoError> {
        position.aliases.sort();
        position.aliases.dedup();
        position.reads.sort();
        position.reads.dedup();
        let action_id = EntityId::derived("commit-admission-action-v1", position.key.as_bytes());
        let _phase = crate::timing::Phase::bound("subject.prepare_s1", crate::timing::Binding {
            subject_id: Some(head.agent_id), action_id: Some(action_id), ..Default::default()
        });
        let input_digest = input_digest(step, head, next)?;
        // Retry reads the already frozen plan; no new time, UUID, sequence or
        // snapshot identity is substituted after a chosen-but-unapplied crash.
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
            if let Some(prepared) = prepared_transition_at(&tx, action_id, &input_digest)? { return Ok(prepared); }
            let (transition, ()) = self.capture_row_transition_with_ranges(&tx, RowTransitionInput {
                action_id, input_digest: input_digest.clone(), head,
            }, |commit, _| {
                let receipt = self.apply_step_commit_tx(&tx, step, head, next, commit)?;
                Ok((receipt.updated, ()))
            })?;
            let result_commit = transition.result_commit;
            let bytes = sealed_image_bytes(&transition)?;
            let transition_ref = self.cas.seal_object(&bytes, "application/json", "commit-authority", CODEC, transition.schema_version)?.object;
            let payload_ref = self.cas.seal_object(&serde_json::to_vec(&serde_json::json!({
                "schema_version":1,"kind":"store.control_admission","head":head,
                "source_ledger_id":self.ledger.ledger_id(),"predecessor_ref":predecessor_ref,
                "expected_tasks":position.expected_tasks,"snapshot_tasks":position.snapshot_tasks,
                "expected_steps":position.expected_steps,
                "action":{"kind":"subject.step_commit","step":step,"expected_head":head,
                    "next_step_id":next,"transition_ref":transition_ref},
            }))?, "application/json", "commit-authority", "commit-admission", 1)?.object;
            let admission = CommitAdmission { schema_version:1, subject_id:head.agent_id, branch_id:head.branch_id.clone(),
                conflict_key:position.key.clone(), conflict_aliases:position.aliases.clone(), required_unconsumed:position.reads.clone(),
                action_id, expected_parent:Some(head.committed_at), expected_head_revision:Some(head.record_revision),
                controller_epoch:head.controller_epoch, software_generation:head.software_generation.clone(),
                operation_id:None, attempt_id:None, payload_ref };
            tx.execute("INSERT INTO control_prepared_transitions(action_id,input_digest,admission,result_commit,transition_ref) VALUES(?1,?2,?3,?4,?5)",
                params![action_id.to_string(),input_digest.0,serde_json::to_string(&admission)?,serde_json::to_string(&result_commit)?,serde_json::to_string(&transition_ref)?])
                .map_err(storage_error)?;
            // This is a durable reservation of the source result coordinate,
            // not an S1 success. The later physical materialization is another
            // local receipt; foreign ledgers never mint a replacement result.
            self.record_subject_commit(&tx, result_commit, 1)?;
            crate::timing::measure("sqlite.commit", || tx.commit()).map_err(commit_error)?;
            Ok(PreparedTransition { admission, transition_ref })
        }, |error| matches!(error, ShenkoError::Storage(_)))
    }

    pub(crate) fn prepared_transition(
        &self,
        action: EntityId,
        input: &Digest,
    ) -> Result<Option<PreparedTransition>, ShenkoError> {
        prepared_transition_at(&self.ledger.conn.lock(), action, input)
    }

    /// Resume after a source sealed S1/native T1/dispatch/T2 but before it learned
    /// whether consensus chose it. Call before activating the new runtime
    /// epoch. Only exact source predecessors qualify; stale plans stay facts.
    /// Preparing the original descriptor either learns its old grant or orders
    /// that same descriptor. It never regenerates a decision or sends an effect.
    pub fn resume_prepared_step_commits(
        &self,
        subject: EntityId,
    ) -> anyhow::Result<crate::PreparedStepRecovery> {
        if self.ledger.authority.activated() {
            return Err(shenko_types::authority::authority_fenced(
                "prepared recovery requires a new Store incarnation before runtime activation",
            )
            .into());
        }
        let descriptors: Vec<String> = {
            let conn = self.ledger.conn.lock();
            let mut statement = conn
                .prepare("SELECT admission FROM control_prepared_transitions ORDER BY rowid")
                .map_err(storage_error)?;
            statement
                .query_map([], |row| row.get(0))
                .map_err(storage_error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(storage_error)?
        };
        let mut recovery = crate::PreparedStepRecovery::default();
        for raw in descriptors {
            let admission: CommitAdmission = serde_json::from_str(&raw)?;
            if admission.subject_id != subject || self.control_admission_applied_here(&admission)? {
                continue;
            }
            let envelope: serde_json::Value =
                serde_json::from_slice(&self.cas.read_object(&admission.payload_ref.digest)?)?;
            let head: SubjectBranchHead = serde_json::from_value(envelope["head"].clone())?;
            if envelope["source_ledger_id"] != serde_json::to_value(self.ledger.ledger_id())?
                || serde_json::to_value(self.load_subject(subject)?.0)?
                    != serde_json::to_value(&head)?
            {
                continue;
            }
            let reference: ObjectRef =
                serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
            self.admission_objects(&admission.payload_ref)?;
            let reservation = self
                .reserve_sealed_admission(admission.clone(), Vec::new(), Vec::new())?
                .prepared_recovery();
            let grant = reservation.grant().cloned();
            let result = match envelope["action"]["kind"].as_str() {
                Some("native.claim") => self
                    .apply_claim_transition(
                        &reference,
                        &reservation,
                        crate::claim_transition::ClaimApplyMode::Prepared,
                    )
                    .map(|_| ()),
                Some("subject.step_commit") => {
                    let step: SubjectStep =
                        serde_json::from_value(envelope["action"]["step"].clone())?;
                    let next: EntityId =
                        serde_json::from_value(envelope["action"]["next_step_id"].clone())?;
                    self.apply_step_transition(&reference, &step, &head, next, &reservation, false)
                        .map(|_| ())
                }
                Some("native.complete" | "native.finish") => self
                    .apply_native_transition(
                        &reference,
                        &reservation,
                        crate::native_transition::NativeApplyMode::Prepared,
                    )
                    .map(|_| ()),
                Some("native.arm") => self
                    .apply_arm_transition(
                        &reference,
                        &reservation,
                        crate::arming_transition::ArmApplyMode::Prepared,
                    )
                    .map(|_| ()),
                Some("native.dispatch") => self
                    .apply_dispatch_transition(
                        &reference,
                        &reservation,
                        crate::dispatch_transition::DispatchApplyMode::Prepared,
                    )
                    .map(|_| ()),
                _ => Err(invalid("unsupported prepared control action")),
            };
            drop(reservation);
            match result {
                Ok(_) => recovery.applied += 1,
                Err(error)
                    if !shenko_types::authority::is_authority_fenced(&error)
                        && matches!(
                            error,
                            ShenkoError::Conflict(_)
                                | ShenkoError::NotFound(_)
                                | ShenkoError::ResnapshotRequired { .. }
                        ) =>
                {
                    self.retain_prepared_step_debt(&admission, grant.as_ref(), &error.to_string())?;
                    recovery.unresolved.push(crate::PreparedStepDebt {
                        action_id: admission.action_id,
                        grant_id: grant.map(|grant| grant.grant_id),
                    });
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(recovery)
    }

    pub(crate) fn verify_transition_objects(
        &self,
        transition: &RowTransition,
    ) -> Result<(), ShenkoError> {
        let _phase = crate::timing::Phase::new("materialize.closure");
        let roots = self.transition_object_roots(&transition.tables);
        let (_, missing) = self.collect_closure(&roots).map_err(storage_error)?;
        if !missing.is_empty() {
            return Err(ShenkoError::ObjectClosureIncomplete {
                missing_count: missing.len(),
            });
        }
        Ok(())
    }

    fn transition_object_roots(&self, tables: &[TableTransition]) -> Vec<ObjectRef> {
        self.cognition_text_object_roots(tables.iter().flat_map(|transition| {
            let table = &transition.before.table;
            transition.before.rows.values().chain(
                transition
                    .changes
                    .iter()
                    .filter_map(|change| change.after.as_ref()),
            ).flat_map(move |cells| {
                table.columns.iter().zip(cells).filter_map(move |(name, cell)| match cell {
                    Cell::Text(text) => Some((table.name.as_str(), name.as_str(), text.as_str())),
                    _ => None,
                })
            })
        })).0
    }

    pub(crate) fn read_step_transition(
        &self,
        reference: &ObjectRef,
    ) -> Result<RowTransition, ShenkoError> {
        if reference.codec.name != CODEC
            || !matches!(reference.codec.major, 1 | 2)
            || reference.media_type != "application/json"
            || reference.bytes > MAX_IMAGE_BYTES as u64
        {
            return Err(invalid("unsupported transition type or size"));
        }
        let bytes = self.cas.read_object(&reference.digest)?;
        if bytes.len() as u64 != reference.bytes || Digest::blake3(&bytes) != reference.digest {
            return Err(invalid("transition bytes differ"));
        }
        let transition: RowTransition = serde_json::from_slice(&bytes)?;
        if transition.schema_version != reference.codec.major {
            return Err(invalid("row capture schema differs from its typed codec"));
        }
        transition.validate_capture_shape()?;
        self.verify_transition_objects(&transition)?;
        Ok(transition)
    }

    pub(crate) fn apply_step_transition(
        &self,
        reference: &ObjectRef,
        step: &SubjectStep,
        head: &SubjectBranchHead,
        next: EntityId,
        authority: &AuthorityReservation,
        learned: bool,
    ) -> Result<CommitReceipt, ShenkoError> {
        let admission = authority
            .admission()
            .ok_or_else(|| invalid("materialization has no reserved admission"))?;
        let _phase = crate::timing::Phase::bound("subject.apply_s1", crate::timing::Binding {
            subject_id: Some(admission.subject_id), action_id: Some(admission.action_id),
            ..Default::default()
        });
        let transition = self.read_step_transition(reference)?;
        if !matches!(transition.schema_version, 1 | 2)
            || transition.action_id != admission.action_id
            || transition.input_digest != input_digest(step, head, next)?
            || serde_json::to_value(&transition.original_head)? != serde_json::to_value(head)?
            || transition.source_ledger_id != transition.result_commit.ledger_id
            || admission.subject_id != head.agent_id
            || admission.branch_id != head.branch_id
            || admission.expected_parent != Some(head.committed_at)
            || admission.expected_head_revision != Some(head.record_revision)
            || admission.controller_epoch != head.controller_epoch
            || admission.software_generation != head.software_generation
            || admission.operation_id.is_some()
            || admission.attempt_id.is_some()
        {
            return Err(invalid("transition is not the admitted S1/result identity"));
        }
        transition.validate_result(step, head, next)?;
        authority.with_mutation(&self.ledger,
            || {
                let mut conn = crate::timing::measure("sqlite.mutex_wait", || self.ledger.conn.lock());
                let tx = crate::timing::immediate(&mut conn)
                    .map_err(storage_error)?;
                if learned {
                    authority.check_materialization(&tx)?;
                } else {
                    authority.check(&tx, self.capabilities.incarnation, Some(head.agent_id))?;
                }
                if admission_materialized(&tx, admission)? {
                    return Err(conflict("transition already materialized"));
                }
                let sequence = self.ledger.allocate_seq(&tx)?;
                let local_commit = CommitRef::new(self.ledger.ledger_id(), sequence);
                transition.apply_rows(&tx, sequence)?;
                authority.record_canonical_admission(
                    &tx,
                    local_commit,
                    transition.result_commit,
                    reference,
                    transition.created_at,
                )?;
                self.record_subject_commit(&tx, local_commit, transition.updated.len() as i64)?;
                crate::timing::measure("sqlite.commit", || tx.commit()).map_err(commit_error)?;
                authority.materialized();
                Ok(self.subject_receipt(local_commit, transition.updated.clone()))
            },
            |error| matches!(error, ShenkoError::Storage(_)),
        )
    }

    pub(crate) fn recover_step_transition(
        &self,
        grant: &CommitAuthorityGrant,
        reference: &ObjectRef,
        step: &SubjectStep,
        head: &SubjectBranchHead,
        next: EntityId,
    ) -> Result<CommitReceipt, ShenkoError> {
        let reservation =
            self.reserve_recovered_admission(grant.clone(), Vec::new(), Vec::new())?;
        self.apply_step_transition(reference, step, head, next, &reservation, true)
    }
}

pub(crate) fn prepared_transition_at(
    conn: &Connection,
    action: EntityId,
    input: &Digest,
) -> Result<Option<PreparedTransition>, ShenkoError> {
    let row: Option<(String,String,String)> = conn.query_row(
        "SELECT input_digest,admission,transition_ref FROM control_prepared_transitions WHERE action_id=?1",
        [action.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(storage_error)?;
    row.map(|(original, admission, reference)| {
        if original != input.0 {
            return Err(conflict(
                "another transition already reserved this action position",
            ));
        }
        let admission: CommitAdmission = serde_json::from_str(&admission)?;
        if admission.action_id != action {
            return Err(conflict("stored prepared action identity differs"));
        }
        Ok(PreparedTransition {
            admission,
            transition_ref: serde_json::from_str(&reference)?,
        })
    })
    .transpose()
}

fn current_image(conn: &Connection, transition: &TableTransition) -> Result<Image, ShenkoError> {
    if transition.complete_read && transition.read_range.is_none() {
        return image(conn, &transition.before.table);
    }
    let mut remaining = MAX_CAPTURE_BYTES;
    image_for_keys_with_budget(conn, &transition.before.table,
        transition.changes.iter().filter_map(|change|
            change.before.as_deref().or(change.after.as_deref())), &mut remaining)
}

/// Read only named primary keys for partial witnesses. Unlike selecting the
/// whole table and filtering afterward, this work grows with the actual delta.
fn image_for_keys_with_budget<'a>(
    conn: &Connection, table: &Table, keys: impl IntoIterator<Item = &'a [Cell]>,
    remaining: &mut usize,
) -> Result<Image, ShenkoError> {
    let table_bytes = json_size(table, *remaining, ROW_CAPTURE_REFUSAL)?;
    charge_image(remaining, table_bytes)?;
    let condition = table.key.iter().enumerate().map(|(position, index)|
        format!("{} IS ?{}", quoted(&table.columns[*index]), position + 1)).collect::<Vec<_>>().join(" AND ");
    let columns = table.columns.iter().map(|column| quoted(column)).collect::<Vec<_>>().join(",");
    let mut statement = conn.prepare(&format!("SELECT {columns} FROM main.{} WHERE {condition}", quoted(&table.name)))
        .map_err(storage_error)?;
    let mut rows = BTreeMap::new();
    for key_cells in keys {
        let parameters = table.key.iter().map(|index| key_cells.get(*index)
            .ok_or_else(|| invalid("partial witness has an incomplete primary key"))?.sql(LedgerSeq(0)))
            .collect::<Result<Vec<_>, _>>()?;
        let mut query = statement.query(params_from_iter(parameters)).map_err(storage_error)?;
        let Some(row) = query.next().map_err(storage_error)? else { continue; };
        charge_image(remaining, 128)?;
        charge_image(remaining, table.columns.len().saturating_mul(std::mem::size_of::<Cell>()))?;
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
        if rows.insert(key, values).is_some() || query.next().map_err(storage_error)?.is_some() {
            return Err(invalid("partial witness does not name a unique primary key"));
        }
    }
    Ok(Image { table: table.clone(), rows })
}

fn apply_table(
    tx: &Transaction<'_>,
    transition: &TableTransition,
    sequence: LedgerSeq,
) -> Result<(), ShenkoError> {
    let table = &transition.before.table;
    let columns = table
        .columns
        .iter()
        .map(|column| quoted(column))
        .collect::<Vec<_>>()
        .join(",");
    let mut changed_keys = BTreeSet::new();
    for change in &transition.changes {
        if !changed_keys.insert(&change.key)
            || change.before == change.after
            || transition.before.rows.get(&change.key) != change.before.as_ref()
        {
            return Err(invalid("invalid row change preimage"));
        }
        for values in [&change.before, &change.after].into_iter().flatten() {
            if key(table, values).map_err(invalid)? != change.key
                || values.iter().enumerate().any(|(index, value)| {
                    matches!(value, Cell::LocalSequence) != (Some(index) == table.local_sequence)
                })
            {
                return Err(invalid("row change key or sequence projection differs"));
            }
        }
        let changed = if let Some(after) = &change.after {
            let values = after
                .iter()
                .map(|cell| cell.sql(sequence))
                .collect::<Result<Vec<_>, _>>()?;
            if change.before.is_none() {
                let placeholders = (1..=values.len())
                    .map(|index| format!("?{index}"))
                    .collect::<Vec<_>>()
                    .join(",");
                tx.execute(
                    &format!(
                        "INSERT INTO {} ({columns}) VALUES ({placeholders})",
                        quoted(&table.name)
                    ),
                    params_from_iter(values),
                )
                .map_err(storage_error)?
            } else {
                let assignments = table
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(index, column)| format!("{}=?{}", quoted(column), index + 1))
                    .collect::<Vec<_>>()
                    .join(",");
                let condition = table
                    .key
                    .iter()
                    .map(|&index| format!("{} IS ?{}", quoted(&table.columns[index]), index + 1))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                tx.execute(
                    &format!(
                        "UPDATE {} SET {assignments} WHERE {condition}",
                        quoted(&table.name)
                    ),
                    params_from_iter(values),
                )
                .map_err(storage_error)?
            }
        } else {
            let before = change
                .before
                .as_ref()
                .ok_or_else(|| invalid("delete without preimage"))?;
            let values = table
                .key
                .iter()
                .map(|&index| before[index].sql(sequence))
                .collect::<Result<Vec<_>, _>>()?;
            let condition = table
                .key
                .iter()
                .enumerate()
                .map(|(parameter, &index)| {
                    format!("{} IS ?{}", quoted(&table.columns[index]), parameter + 1)
                })
                .collect::<Vec<_>>()
                .join(" AND ");
            tx.execute(
                &format!("DELETE FROM {} WHERE {condition}", quoted(&table.name)),
                params_from_iter(values),
            )
            .map_err(storage_error)?
        };
        if changed != 1 {
            return Err(invalid("row mutation did not affect exactly one identity"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    /// Capacity retains an exact diagnostic and never authorizes a second
    /// semantic evaluation or weaker noncanonical admission.
    #[test]
    fn the_capture_budget_refusal_is_recognizable_and_typed() {
        let mut remaining = 0usize;
        let Err(error) = charge_image(&mut remaining, 1) else {
            panic!("an exhausted capture budget must refuse the capture");
        };
        assert!(capture_budget_refusal(&error));
        assert!(matches!(error, ShenkoError::Conflict(_)));
        assert!(!capture_budget_refusal(&ShenkoError::conflict("another conflict")));
        assert!(!capture_budget_refusal(&ShenkoError::storage("disk damage")));
        assert!(capture_budget_refusal(&ShenkoError::conflict(IMAGE_CAPTURE_REFUSAL)));
        assert!(!capture_budget_refusal(&ShenkoError::conflict(format!("{ROW_CAPTURE_REFUSAL}: another cause"))));
        assert!(!capture_budget_refusal(&ShenkoError::storage(IMAGE_CAPTURE_REFUSAL)));
        let mut remaining = MAX_CAPTURE_BYTES;
        assert!(charge_image(&mut remaining, 1).is_ok());
    }

    /// One row-engine journey: held cached SQL and FTS, an absent range, an
    /// already stepped cursor, writes hidden from the observer, hidden DDL,
    /// and a real expanding BLOB primary key. Public S1 below separately
    /// exercises the real subject/authority path; this head is only the row
    /// engine's carried metadata, not a claim of controller authority.
    #[test]
    fn capture_observes_cached_sql_and_refuses_unrepresented_state() -> anyhow::Result<()> {
        use shenko_types::{ids::{BranchId, ControllerEpoch, RecordRevision, RuntimeGeneration},
            subject::{GenesisRef, SubjectMode}};
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("capture.db"), dir.path().join("cas"))?;
        let head = SubjectBranchHead {
            branch_origin: None, agent_id: EntityId::new(), branch_id: BranchId::new(),
            subject_root_task_id: EntityId::new(), controller_epoch: ControllerEpoch(1),
            software_generation: RuntimeGeneration::default(),
            genesis_ref: GenesisRef { ledger_id: store.ledger.ledger_id(), genesis_id: EntityId::new() },
            committed_step_no: 0, committed_step_id: None,
            committed_at: CommitRef::new(store.ledger.ledger_id(), LedgerSeq(0)),
            next_step_id: EntityId::new(), mode: SubjectMode::Active,
            mode_reason_ref: None, record_revision: RecordRevision(0),
        };
        let input = || RowTransitionInput {
            action_id: head.next_step_id, input_digest: Digest::blake3(b"row engine journey"), head: &head,
        };
        let mut conn = store.ledger.conn.lock();
        conn.execute_batch("CREATE TABLE capture_rows(id INTEGER PRIMARY KEY,body TEXT,payload BLOB);
            CREATE TABLE capture_absent(id INTEGER PRIMARY KEY);
            CREATE TABLE capture_range(id INTEGER PRIMARY KEY,payload BLOB);
            WITH RECURSIVE sequence(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM sequence WHERE n<4096)
                INSERT INTO capture_range SELECT n,zeroblob(1024) FROM sequence;
            CREATE VIRTUAL TABLE capture_search USING fts5(body,content='capture_rows',content_rowid='id');
            CREATE TRIGGER capture_insert AFTER INSERT ON capture_rows BEGIN
                INSERT INTO capture_search(rowid,body) VALUES(new.id,new.body); END;
            CREATE TRIGGER capture_update AFTER UPDATE ON capture_rows BEGIN
                INSERT INTO capture_search(capture_search,rowid,body) VALUES('delete',old.id,old.body);
                INSERT INTO capture_search(rowid,body) VALUES(new.id,new.body); END;
            INSERT INTO capture_rows VALUES(1,'before',X'00ff');
            INSERT INTO subject_head_history VALUES('guard','row-agent','row-branch','{}',7);")?;
        {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // Hold both cached statements across authorizer installation. A
            // cache flush alone cannot observe these already checked-out SQLs.
            let mut update = tx.prepare_cached("UPDATE capture_rows SET body=?1,payload=?2 WHERE id=1")?;
            let mut absence = tx.prepare_cached("SELECT count(*) FROM capture_absent")?;
            let mut evaluations = 0;
            let (transition, (generated, original_commit, original_time)) = store.capture_row_transition_with_ranges(
                &tx, input(), |commit, at| {
                    evaluations += 1;
                    let id = EntityId::new();
                    let absent: i64 = absence.query_row([], |row| row.get(0)).map_err(storage_error)?;
                    assert_eq!(absent, 0);
                    let historical: i64 = tx.query_row("SELECT count(*) FROM capture_range", [], |row| row.get(0))
                        .map_err(storage_error)?;
                    assert_eq!(historical, 4096);
                    update.execute(params![format!("captured {id} {at}"), [1u8, 2, 255].as_slice()])
                        .map_err(storage_error)?;
                    Ok((Vec::new(), (id, commit, at)))
                })?;
            drop(absence);
            drop(update);
            assert_eq!(evaluations, 1);
            assert_eq!(transition.schema_version, 2);
            let range = transition.tables.iter().find(|table| table.before.table.name == "capture_range")
                .context("the actual large read remains a range dependency")?;
            assert!(range.before.rows.is_empty() && range.read_range.is_some());
            assert!(serde_json::to_vec(&transition)?.len() < 64 * 1024,
                "historical read content is hashed, not copied into the chosen S1");
            assert_eq!(transition.result_commit, original_commit);
            assert_eq!(transition.created_at, original_time);
            let untouched: (String, Vec<u8>) = tx.query_row(
                "SELECT body,payload FROM capture_rows WHERE id=1", [], |row| Ok((row.get(0)?, row.get(1)?)))?;
            assert_eq!(untouched, ("before".into(), vec![0, 255]), "evaluation is rolled back before admission");
            let absence = transition.tables.iter().find(|table| table.before.table.name == "capture_absent")
                .context("a cached absent read must remain a complete range")?;
            assert!(absence.complete_read && absence.before.rows.is_empty());
            assert!(transition.tables.iter().any(|table| table.before.table.name == "capture_rows" && table.complete_read));
            tx.execute("INSERT INTO capture_absent VALUES(8)", [])?;
            assert!(matches!(transition.apply_rows(&tx, original_commit.seq), Err(ShenkoError::Conflict(_))),
                "a newly present row fences the same frozen result");
            assert_eq!(tx.query_row("SELECT body FROM capture_rows WHERE id=1", [], |row| row.get::<_, String>(0))?, "before");
            tx.execute("DELETE FROM capture_absent WHERE id=8", [])?;
            transition.apply_rows(&tx, original_commit.seq)?;
            let applied: (String, Vec<u8>) = tx.query_row(
                "SELECT body,payload FROM capture_rows WHERE id=1", [], |row| Ok((row.get(0)?, row.get(1)?)))?;
            assert_eq!(applied, (format!("captured {generated} {original_time}"), vec![1, 2, 255]));
            assert_eq!(evaluations, 1, "application never evaluates the callback again");
            assert_eq!(tx.query_row("SELECT count(*) FROM capture_search WHERE capture_search MATCH 'captured'", [], |row| row.get::<_, i64>(0))?, 1);
            assert_eq!(tx.query_row("SELECT count(*) FROM capture_search WHERE capture_search MATCH 'before'", [], |row| row.get::<_, i64>(0))?, 0);
        }
        {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut statement = tx.prepare("SELECT body FROM capture_rows")?;
            let mut cursor = statement.query([])?;
            assert!(cursor.next()?.is_some());
            let mut evaluations = 0;
            let result = store.capture_row_transition_with_ranges(&tx, input(), |_, _| {
                evaluations += 1;
                Ok((Vec::new(), ()))
            });
            assert!(matches!(result, Err(ShenkoError::Conflict(message)) if message.contains("began with an active SQLite statement")));
            assert_eq!(evaluations, 0, "an active pre-observer cursor is not an observed read");
        }
        {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let result = store.capture_row_transition_with_ranges(&tx, input(), |_, _| {
                tx.execute("UPDATE capture_rows SET body='must roll back' WHERE id=1", []).map_err(storage_error)?;
                // Deliberately remove the observer to test the independent
                // physical guard, including a column that image() normalizes.
                tx.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
                tx.execute("UPDATE subject_head_history SET received_commit_seq=8 WHERE snapshot_id='guard'", []).map_err(storage_error)?;
                Ok((Vec::new(), ()))
            });
            assert!(matches!(result, Err(ShenkoError::Conflict(message)) if message.contains("unobserved transition write to subject_head_history")));
            assert_eq!(tx.query_row("SELECT received_commit_seq FROM subject_head_history WHERE snapshot_id='guard'", [], |row| row.get::<_, i64>(0))?, 7);
            assert_eq!(tx.query_row("SELECT body FROM capture_rows WHERE id=1", [], |row| row.get::<_, String>(0))?, "before");
            assert_eq!(tx.query_row("SELECT count(*) FROM capture_search WHERE capture_search MATCH 'before'", [], |row| row.get::<_, i64>(0))?, 1);
        }
        {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let result = store.capture_row_transition_with_ranges(&tx, input(), |_, _| {
                tx.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
                tx.execute_batch("CREATE TABLE capture_surprise(id INTEGER PRIMARY KEY); INSERT INTO capture_surprise VALUES(1);")
                    .map_err(storage_error)?;
                Ok((Vec::new(), ()))
            });
            assert!(matches!(result, Err(ShenkoError::Conflict(message)) if message.contains("changed a schema or table layout")));
            assert_eq!(tx.query_row("SELECT count(*) FROM sqlite_schema WHERE name='capture_surprise'", [], |row| row.get::<_, i64>(0))?, 0);
        }
        // Unlike update_hook, preupdate observes both an implicit REPLACE
        // deletion and a WITHOUT ROWID table. The frozen change keeps the
        // original row even though the SQL statement itself was an INSERT.
        conn.execute_batch("CREATE TABLE capture_replace(id TEXT PRIMARY KEY,body TEXT) WITHOUT ROWID;
            INSERT INTO capture_replace VALUES('original','before');")?;
        {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (transition, ()) = store.capture_row_transition_with_ranges(&tx, input(), |_, _| {
                tx.execute("INSERT OR REPLACE INTO capture_replace VALUES('original','after')", [])
                    .map_err(storage_error)?;
                Ok((Vec::new(), ()))
            })?;
            let table = transition.tables.iter().find(|table| table.before.table.name == "capture_replace")
                .context("physical replacement witness")?;
            assert_eq!(table.changes.len(), 1);
            assert!(table.changes[0].before.is_some() && table.changes[0].after.is_some());
            assert_eq!(tx.query_row("SELECT body FROM capture_replace", [], |row| row.get::<_, String>(0))?, "before");
            transition.apply_rows(&tx, LedgerSeq(1))?;
            assert_eq!(tx.query_row("SELECT body FROM capture_replace", [], |row| row.get::<_, String>(0))?, "after");
        }
        // Native v1 keeps its original wire/validator contract. A write-only
        // table and an already-proven direct task must not first copy unrelated
        // history into the capture budget and only discard it afterward.
        conn.execute_batch("CREATE TABLE capture_write_only(id INTEGER PRIMARY KEY,payload BLOB);
            INSERT INTO capture_write_only VALUES(1,zeroblob(129*1024*1024));")?;
        let seed = store.cas.seal_object(b"capture-only task fixture", "text/plain", "test", "raw", 1)?.object;
        let target_task = TaskRecord::subject_root(EntityId::new(), head.agent_id, seed.clone(), head.committed_at);
        let history_task = TaskRecord::subject_root(EntityId::new(), head.agent_id, seed, head.committed_at);
        conn.execute("INSERT INTO tasks(task_id,record_revision,data,updated_commit_seq) VALUES(?1,0,?2,0)",
            params![target_task.task_id.to_string(), serde_json::to_string(&target_task)?])?;
        conn.execute("INSERT INTO tasks(task_id,record_revision,data,updated_commit_seq)
            VALUES(?1,0,json_set(?2,'$.memos.retained_history',printf('%0*d',66*1024*1024,0)),0)",
            params![history_task.task_id.to_string(), serde_json::to_string(&history_task)?])?;
        {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let projection_before: u64 = tx.query_row(
                "SELECT revision FROM context_execution_clock WHERE singleton=1", [], |row| row.get(0))?;
            let (transition, ()) = store.capture_row_transition(&tx, input(), Some(&target_task), |commit, _| {
                let current = crate::child::read_task(&tx, target_task.task_id)
                    .map_err(|error| invalid(error.to_string()))?.ok_or_else(|| invalid("target absent"))?;
                let mut updated = current;
                updated.record_revision = RecordRevision(1);
                updated.updated_commit = commit;
                crate::child::write_task(&tx, &updated).map_err(|error| invalid(error.to_string()))?;
                tx.execute("INSERT INTO capture_write_only VALUES(2,X'01')", []).map_err(storage_error)?;
                Ok((Vec::new(), ()))
            })?;
            assert_eq!(transition.schema_version, 1);
            assert!(serde_json::to_vec(&transition)?.len() < 64 * 1024);
            let tasks = transition.tables.iter().find(|table| table.before.table.name == "tasks")
                .context("bounded native task witness")?;
            crate::native_facts::validate_task_read(tasks, &target_task)?;
            let writes = transition.tables.iter().find(|table| table.before.table.name == "capture_write_only")
                .context("write-only key witness")?;
            assert!(!writes.complete_read && writes.before.rows.is_empty() && writes.changes.len() == 1);
            assert!(transition.tables.iter().all(|table| !execution_projection(&table.before.table.name)),
                "local invalidation counters must not become canonical facts");
            assert_eq!(tx.query_row("SELECT revision FROM context_execution_clock WHERE singleton=1", [],
                |row| row.get::<_, u64>(0))?, projection_before, "evaluation rolls back trigger projections");
            tx.execute_batch("CREATE TRIGGER capture_projection_changed AFTER UPDATE ON context_execution_clock BEGIN
                UPDATE capture_rows SET body='outside frozen writes' WHERE id=1; END;")?;
            assert!(matches!(transition.apply_rows(&tx, transition.result_commit.seq),
                Err(ShenkoError::Conflict(message)) if message.contains("predecessor/schema differs for tasks")),
                "the receiver must reject changed projection triggers before any factual writes");
            assert_eq!(tx.query_row("SELECT revision FROM context_execution_clock WHERE singleton=1", [],
                |row| row.get::<_, u64>(0))?, projection_before);
            assert_eq!(tx.query_row("SELECT count(*) FROM capture_write_only WHERE id=2", [],
                |row| row.get::<_, u64>(0))?, 0);
            tx.execute_batch("DROP TRIGGER capture_projection_changed")?;
            transition.apply_rows(&tx, transition.result_commit.seq)?;
            assert_eq!(tx.query_row("SELECT revision FROM context_execution_clock WHERE singleton=1", [],
                |row| row.get::<_, u64>(0))?, projection_before + 1, "one factual task update invalidates once");
            assert_eq!(tx.query_row("SELECT revision FROM context_execution_revisions WHERE binding_id=?1",
                [target_task.task_id.to_string()], |row| row.get::<_, u64>(0))?, projection_before + 1);
            assert_eq!(tx.query_row("SELECT length(payload) FROM capture_write_only WHERE id=1", [], |row| row.get::<_, i64>(0))?, 129*1024*1024);
            assert_eq!(tx.query_row("SELECT length(json_extract(data,'$.memos.retained_history')) FROM tasks WHERE task_id=?1",
                [history_task.task_id.to_string()], |row| row.get::<_, i64>(0))?, 66*1024*1024);
        }
        // A local rebuildable index cannot silently become a semantic read
        // or a new source write. Only the known factual-row triggers maintain
        // it, and the physical observer independently rejects a hidden write.
        for sql in ["SELECT revision FROM context_execution_clock",
            "UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1"]
        {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let result = store.capture_row_transition_with_ranges(&tx, input(), |_, _| {
                tx.execute_batch(sql).map_err(storage_error)?;
                Ok((Vec::new(), ()))
            });
            assert!(matches!(result, Err(ShenkoError::Conflict(message))
                if message.contains("directly accessed a local execution projection")));
        }
        {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let before: u64 = tx.query_row("SELECT revision FROM context_execution_clock WHERE singleton=1", [],
                |row| row.get(0))?;
            let result = store.capture_row_transition_with_ranges(&tx, input(), |_, _| {
                tx.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
                tx.execute("UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1", [])
                    .map_err(storage_error)?;
                Ok((Vec::new(), ()))
            });
            assert!(matches!(result, Err(ShenkoError::Conflict(message))
                if message.contains("direct physical write to a local execution projection")));
            assert_eq!(tx.query_row("SELECT revision FROM context_execution_clock WHERE singleton=1", [],
                |row| row.get::<_, u64>(0))?, before);
        }
        conn.execute_batch("CREATE TABLE capture_keys(id BLOB PRIMARY KEY,value INTEGER);
            INSERT INTO capture_keys VALUES(zeroblob(48*1024*1024),1);")?;
        {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let result = store.capture_row_transition_with_ranges(&tx, input(), |_, _| {
                tx.execute("UPDATE capture_keys SET value=2", []).map_err(storage_error)?;
                Ok((Vec::new(), ()))
            });
            let error = result.err().context("expanded JSON key must exceed the remaining capture budget")?;
            assert!(matches!(&error, ShenkoError::Conflict(message) if message == ROW_CAPTURE_REFUSAL));
            assert!(capture_budget_refusal(&error));
            assert_eq!(tx.query_row("SELECT value FROM capture_keys", [], |row| row.get::<_, i64>(0))?, 1);
            assert_eq!(tx.query_row("SELECT count(*) FROM commit_log", [], |row| row.get::<_, i64>(0))?, 0);
            assert_eq!(tx.query_row("SELECT count(*) FROM control_prepared_transitions", [], |row| row.get::<_, i64>(0))?, 0);
        }
        Ok(())
    }

    #[test]
    fn insert_only_tasks_with_verified_projection_trigger_omits_complete_read_and_refuses_tampering() -> anyhow::Result<()> {
        use shenko_types::{
            ids::{BranchId, ControllerEpoch, RecordRevision, RuntimeGeneration},
            subject::{GenesisRef, SubjectMode},
        };
        let src_dir = tempfile::tempdir()?;
        let dst_dir = tempfile::tempdir()?;
        let source = Store::open(src_dir.path().join("source.db"), src_dir.path().join("cas"))?;
        let replica = Store::open(dst_dir.path().join("replica.db"), dst_dir.path().join("cas"))?;
        let agent_id = EntityId::new();
        let branch_id = BranchId::new();
        let head = SubjectBranchHead {
            branch_origin: None,
            agent_id,
            branch_id,
            subject_root_task_id: EntityId::new(),
            controller_epoch: ControllerEpoch(1),
            software_generation: RuntimeGeneration::default(),
            genesis_ref: GenesisRef { ledger_id: source.ledger.ledger_id(), genesis_id: EntityId::new() },
            committed_step_no: 0,
            committed_step_id: None,
            committed_at: CommitRef::new(source.ledger.ledger_id(), LedgerSeq(0)),
            next_step_id: EntityId::new(),
            mode: SubjectMode::Active,
            mode_reason_ref: None,
            record_revision: RecordRevision(0),
        };
        let seed_src = source.cas.seal_object(b"insert-only task", "text/plain", "test", "raw", 1)?.object;
        let seed_dst = replica.cas.seal_object(b"parked task", "text/plain", "test", "raw", 1)?.object;
        let mut parked_task = TaskRecord::subject_root(head.subject_root_task_id, agent_id, seed_dst, head.committed_at);
        parked_task.dispatch_eligibility = shenko_types::task::DispatchEligibility::Parked {
            reason_ref: EntityId::new(),
            until_utc: None,
        };
        parked_task.record_revision = RecordRevision(1);
        {
            let dst_conn = replica.ledger.conn.lock();
            dst_conn.execute(
                "INSERT INTO tasks(task_id,record_revision,data,updated_commit_seq) VALUES(?1,?2,?3,0)",
                params![
                    parked_task.task_id.to_string(),
                    parked_task.record_revision.0 as i64,
                    serde_json::to_string(&parked_task)?
                ],
            )?;
        }
        let inserted_task = TaskRecord::subject_root(EntityId::new(), agent_id, seed_src, head.committed_at);
        let transition = {
            let mut src_conn = source.ledger.conn.lock();
            let tx = src_conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (transition, ()) = source.capture_row_transition_with_ranges(
                &tx,
                RowTransitionInput {
                    action_id: head.next_step_id,
                    input_digest: Digest::blake3(b"insert-only task"),
                    head: &head,
                },
                |_, _| {
                    tx.execute(
                        "INSERT INTO tasks(task_id,record_revision,data,updated_commit_seq) VALUES(?1,0,?2,0)",
                        params![
                            inserted_task.task_id.to_string(),
                            serde_json::to_string(&inserted_task).map_err(|error| invalid(error.to_string()))?
                        ],
                    )
                    .map_err(storage_error)?;
                    Ok((Vec::new(), ()))
                },
            )?;
            transition
        };
        let tasks = transition
            .tables
            .iter()
            .find(|table| table.before.table.name == "tasks")
            .context("tasks transition missing")?;
        assert!(!tasks.complete_read, "verified context_revision_tasks_insert must not mark tasks as complete_read");
        assert!(tasks.read_range.is_none(), "insert-only write to tasks must omit read_range");
        assert_eq!(tasks.changes.len(), 1);
        {
            let mut dst_conn = replica.ledger.conn.lock();
            let tx = dst_conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transition.apply_rows(&tx, LedgerSeq(1))?;
            tx.commit()?;
        }
        assert!(replica.get_task(inserted_task.task_id)?.is_some());
        assert!(replica.get_task(parked_task.task_id)?.is_some());
        let mut src_conn = source.ledger.conn.lock();
        src_conn.execute_batch(
            "DROP TRIGGER context_revision_tasks_insert;\
             CREATE TRIGGER context_revision_tasks_insert AFTER INSERT ON tasks BEGIN \
               UPDATE context_execution_clock SET revision=revision+1 WHERE singleton=1 AND EXISTS (SELECT 1 FROM tasks);\
             END;",
        )?;
        let tx = src_conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let tampered = source.capture_row_transition_with_ranges(
            &tx,
            RowTransitionInput {
                action_id: EntityId::new(),
                input_digest: Digest::blake3(b"tampered trigger"),
                head: &head,
            },
            |_, _| {
                let another = TaskRecord::subject_root(EntityId::new(), agent_id, inserted_task.input_ref.clone(), head.committed_at);
                tx.execute(
                    "INSERT INTO tasks(task_id,record_revision,data,updated_commit_seq) VALUES(?1,0,?2,0)",
                    params![
                        another.task_id.to_string(),
                        serde_json::to_string(&another).map_err(|error| invalid(error.to_string()))?
                    ],
                )
                .map_err(storage_error)?;
                Ok((Vec::new(), ()))
            },
        );
        assert!(
            matches!(&tampered, Err(ShenkoError::Conflict(message)) if message.contains("non-canonical execution projection trigger")),
            "tampered context_revision trigger must be refused"
        );
        Ok(())
    }
}
