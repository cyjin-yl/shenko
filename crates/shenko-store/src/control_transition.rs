//! A chosen S1/native T2 is a frozen state transition, not a recipe to run again.
//!
//! Evaluate the ordinary transaction once in a rollback savepoint. SQLite's
//! authorizer records every table read or written by that evaluation; their
//! complete preimages are the read conditions. Seal the generated write bytes
//! (including random IDs and clock-dependent outcomes) before quorum admission.
//! Applying those bytes performs only SQLite writes, never a provider call.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use parking_lot::Mutex;
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior,
    hooks::{AuthAction, AuthContext, Authorization},
    params, params_from_iter,
    types::{Value, ValueRef},
};
use serde::{Deserialize, Serialize};
use shenko_types::{
    authority::{CommitAdmission, CommitAuthorityGrant},
    commit::{CommitReceipt, CommitRef, UpdatedEntity},
    error::ShenkoError,
    ids::{EntityId, LedgerSeq},
    object::{Digest, ObjectRef},
    subject::{SubjectBranchHead, SubjectStep},
};

use crate::{
    Store,
    authority::{AdmissionPosition, AuthorityReservation, admission_materialized},
    store::{commit_error, storage_error},
};

const CODEC: &str = "store-s1-transition";
pub(crate) const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
// Account while copying rows, before the eventual sealed JSON size is known.
// This bounds the retained pre-capture even when most tables prove untouched.
pub(crate) const MAX_CAPTURE_BYTES: usize = 128 * 1024 * 1024;
// The stable marker of a capture-budget refusal. The frozen pre-capture
// refuses to grow without bound; the marker lets the caller recognize that
// this capacity refusal — and not damage, and not a disagreement about the
// decision — happened, so the decision is kept instead of being lost with the
// transition.
const CAPTURE_BUDGET_MARKER: &str = "capture budget exceeded";

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
    pub before: Image,
    pub changes: Vec<Change>,
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
    /// Check the complete frozen predecessor and apply its exact writes atomically.
    pub(crate) fn apply_rows(
        &self,
        tx: &Transaction<'_>,
        sequence: LedgerSeq,
    ) -> Result<(), ShenkoError> {
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
        tx.execute_batch("PRAGMA defer_foreign_keys=ON")
            .map_err(storage_error)?;
        for table in &self.tables {
            apply_table(tx, table, sequence)?;
        }
        for table in &self.tables {
            let mut expected = table.before.clone();
            for change in &table.changes {
                if let Some(after) = &change.after {
                    expected.rows.insert(change.key.clone(), after.clone());
                } else {
                    expected.rows.remove(&change.key);
                }
            }
            if current_image(tx, table)? != expected {
                return Err(invalid("applied writes differ from frozen result"));
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
    )
}

fn layouts(conn: &Connection) -> Result<BTreeMap<String, Table>, ShenkoError> {
    // table_list distinguishes virtual/shadow FTS projections from base facts.
    // Triggers rebuild those projections from the exact admitted base writes.
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_list WHERE schema='main' AND type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .map_err(storage_error)?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_error)?;
    let mut tables = BTreeMap::new();
    for name in names {
        if local_table(&name) {
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
        let schema = schema
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

fn key(table: &Table, values: &[Cell]) -> Result<String, ShenkoError> {
    if values.len() != table.columns.len() || table.key.is_empty() {
        return Err(invalid(format!(
            "{} has no complete stable primary key",
            table.name
        )));
    }
    let cells = table
        .key
        .iter()
        .map(|&index| {
            values
                .get(index)
                .ok_or_else(|| invalid("key index outside row"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if cells
        .iter()
        .any(|cell| matches!(cell, Cell::Null | Cell::LocalSequence))
    {
        return Err(invalid("null or local primary key"));
    }
    Ok(serde_json::to_string(&cells)?)
}

fn image(conn: &Connection, table: &Table) -> Result<Image, ShenkoError> {
    let mut remaining = MAX_CAPTURE_BYTES;
    image_with_budget(conn, table, &mut remaining)
}

fn charge_image(remaining: &mut usize, bytes: usize) -> Result<(), ShenkoError> {
    *remaining = remaining
        .checked_sub(bytes)
        .ok_or_else(|| conflict(format!(
            "{CAPTURE_BUDGET_MARKER}: row capture exceeds {} MiB memory budget",
            MAX_CAPTURE_BYTES / (1024 * 1024)
        )))?;
    Ok(())
}

/// Whether an error is the refusal of the frozen pre-capture budget.
///
/// This is a capacity fact about this installation, not damage and not a
/// disagreement about the decision. The caller keeps the decision and uses
/// the original-plan path, so an installation whose replicated rows exceed
/// the ceiling keeps committing its steps instead of failing every S1.
pub(crate) fn capture_budget_refusal(error: &ShenkoError) -> bool {
    matches!(error, ShenkoError::Conflict(message) if message.contains(CAPTURE_BUDGET_MARKER))
}

fn image_with_budget(
    conn: &Connection,
    table: &Table,
    remaining: &mut usize,
) -> Result<Image, ShenkoError> {
    charge_image(remaining, serde_json::to_vec(table)?.len())?;
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
        let values = (0..table.columns.len())
            .map(|index| {
                charge_image(remaining, std::mem::size_of::<Cell>())?;
                if Some(index) == table.local_sequence {
                    Ok(Cell::LocalSequence)
                } else {
                    let value = row.get_ref(index).map_err(storage_error)?;
                    let bytes = match value {
                        ValueRef::Text(text) => text.len(),
                        ValueRef::Blob(bytes) => bytes.len(),
                        _ => 0,
                    };
                    charge_image(remaining, bytes)?;
                    Cell::read(value)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let key = key(table, &values)?;
        charge_image(remaining, key.len())?;
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
}

struct Authorizer<'a>(&'a Connection);
impl Drop for Authorizer<'_> {
    fn drop(&mut self) {
        self.0
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    }
}

fn observe(conn: &Connection, accesses: Arc<Mutex<Accesses>>) -> Authorizer<'_> {
    conn.flush_prepared_statement_cache();
    conn.authorizer(Some(move |context: AuthContext<'_>| {
        let mut accesses = accesses.lock();
        match context.action {
            AuthAction::Read { table_name, .. } => {
                accesses.reads.insert(table_name.to_owned());
            }
            AuthAction::Insert { table_name }
            | AuthAction::Delete { table_name }
            | AuthAction::Update { table_name, .. } => {
                accesses.writes.insert(table_name.to_owned());
            }
            _ => {}
        }
        Authorization::Allow
    }));
    Authorizer(conn)
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
    /// Capture one already-authorized factual transaction without retaining its writes.
    /// Local execution guards run before this observation; the evaluated body may
    /// touch only shared facts. No provider calls belong in this callback.
    pub(crate) fn capture_row_transition<T>(
        &self,
        tx: &Transaction<'_>,
        input: RowTransitionInput<'_>,
        evaluate: impl FnOnce(
            CommitRef,
            chrono::DateTime<chrono::Utc>,
        ) -> Result<(Vec<UpdatedEntity>, T), ShenkoError>,
    ) -> Result<(RowTransition, T), ShenkoError> {
        let tables = layouts(tx)?;
        let mut before = BTreeMap::new();
        let mut capture_remaining = MAX_CAPTURE_BYTES;
        for (name, table) in &tables {
            // A table without PK can only be a read projection outside
            // this transition. If the action touches one, the authorizer below
            // refuses preparation rather than inventing rowid identity.
            if !table.key.is_empty() {
                before.insert(
                    name.clone(),
                    image_with_budget(tx, table, &mut capture_remaining)?,
                );
            }
        }
        let result_commit = CommitRef::new(self.ledger.ledger_id(), self.ledger.allocate_seq(tx)?);
        let created_at = chrono::Utc::now();
        tx.execute_batch("SAVEPOINT canonical_row_evaluation")
            .map_err(storage_error)?;
        let accesses = Arc::new(Mutex::new(Accesses::default()));
        let observation = observe(tx, Arc::clone(&accesses));
        let evaluated = evaluate(result_commit, created_at);
        drop(observation);
        let (updated, result) = evaluated?;
        let accesses = accesses.lock();
        let touched = accesses
            .reads
            .union(&accesses.writes)
            .collect::<BTreeSet<_>>();
        let mut transitions = Vec::new();
        for name in touched {
            if local_table(name) {
                return Err(conflict(format!(
                    "transition depended on node-local table {name}"
                )));
            }
            let Some(table) = tables.get(name) else {
                // Virtual/shadow tables are derivations rebuilt by their
                // original triggers. They cannot carry canonical facts.
                continue;
            };
            let mut original = before
                .remove(name)
                .ok_or_else(|| conflict(format!("transition touched unkeyed table {name}")))?;
            let after = image(tx, table)?;
            let keys = original
                .rows
                .keys()
                .chain(after.rows.keys())
                .collect::<BTreeSet<_>>();
            let changes: Vec<Change> = keys
                .into_iter()
                .filter_map(|key| {
                    let old = original.rows.get(key);
                    let new = after.rows.get(key);
                    (old != new).then(|| Change {
                        key: key.clone(),
                        before: old.cloned(),
                        after: new.cloned(),
                    })
                })
                .collect();
            let complete_read = accesses.reads.contains(name);
            if !complete_read {
                original
                    .rows
                    .retain(|key, _| changes.iter().any(|change| &change.key == key));
            }
            transitions.push(TableTransition {
                complete_read,
                before: original,
                changes,
            });
        }
        // Detect writes that escaped the observed transaction body (for
        // example a future helper using cached SQL). Never silently omit
        // a changed base table from the frozen result.
        for (name, original) in before {
            if image(tx, &original.table)? != original {
                return Err(conflict(format!("unobserved transition write to {name}")));
            }
        }
        tx.execute_batch("ROLLBACK TO canonical_row_evaluation; RELEASE canonical_row_evaluation")
            .map_err(storage_error)?;
        let object_roots = self.transition_object_roots(&transitions);
        let (_, missing) = self.collect_closure(&object_roots).map_err(storage_error)?;
        if !missing.is_empty() {
            return Err(ShenkoError::ObjectClosureIncomplete {
                missing_count: missing.len(),
            });
        }
        let transition = RowTransition {
            schema_version: 1,
            action_id: input.action_id,
            input_digest: input.input_digest,
            source_ledger_id: self.ledger.ledger_id(),
            original_head: input.head.clone(),
            result_commit,
            created_at,
            updated,
            object_roots,
            tables: transitions,
        };
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
        self.ledger.with_mutation(|| {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage_error)?;
            authority.check(&tx, self.capabilities.incarnation, Some(head.agent_id))?;
            if let Some(prepared) = prepared_transition_at(&tx, action_id, &input_digest)? { return Ok(prepared); }
            let (transition, ()) = self.capture_row_transition(&tx, RowTransitionInput {
                action_id, input_digest: input_digest.clone(), head,
            }, |commit, _| {
                let receipt = self.apply_step_commit_tx(&tx, step, head, next, commit)?;
                Ok((receipt.updated, ()))
            })?;
            let result_commit = transition.result_commit;
            let bytes = serde_json::to_vec(&transition)?;
            if bytes.len() > MAX_IMAGE_BYTES { return Err(conflict("read/write image exceeds 64 MiB admission budget")); }
            let transition_ref = self.cas.seal_object(&bytes, "application/json", "commit-authority", CODEC, 1)?.object;
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
            tx.commit().map_err(commit_error)?;
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
        let mut rows = Vec::new();
        for transition in tables {
            let table = &transition.before.table;
            for cells in transition.before.rows.values().chain(
                transition
                    .changes
                    .iter()
                    .filter_map(|change| change.after.as_ref()),
            ) {
                rows.push(crate::cognition_archive::CognitionRow {
                    table: table.name.clone(),
                    key: String::new(),
                    values: table
                        .columns
                        .iter()
                        .zip(cells)
                        .filter_map(|(name, cell)| match cell {
                            Cell::Text(text) => Some((name.clone(), Some(text.clone()))),
                            _ => None,
                        })
                        .collect(),
                });
            }
        }
        self.cognition_object_roots(&rows).0
    }

    pub(crate) fn read_step_transition(
        &self,
        reference: &ObjectRef,
    ) -> Result<RowTransition, ShenkoError> {
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
        let transition: RowTransition = serde_json::from_slice(&bytes)?;
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
        let transition = self.read_step_transition(reference)?;
        if transition.schema_version != 1
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
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
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
                tx.commit().map_err(commit_error)?;
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
    let mut image = image(conn, &transition.before.table)?;
    if !transition.complete_read {
        image
            .rows
            .retain(|key, _| transition.changes.iter().any(|change| &change.key == key));
    }
    Ok(image)
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

    /// The capture-budget refusal is a capacity fact, not damage and not a
    /// disagreement about the decision. The caller recognizes it and keeps the
    /// decision through the original-plan path instead of losing it.
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
        let mut remaining = MAX_CAPTURE_BYTES;
        assert!(charge_image(&mut remaining, 1).is_ok());
    }
}
