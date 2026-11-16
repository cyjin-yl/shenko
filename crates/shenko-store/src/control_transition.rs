//! A chosen S1 is a frozen state transition, not a recipe to run again.
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
    store::storage_error,
};

const CODEC: &str = "store-s1-transition";
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum Cell {
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
struct Table {
    name: String,
    schema_digest: Digest,
    columns: Vec<String>,
    key: Vec<usize>,
    local_sequence: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    table: Table,
    rows: BTreeMap<String, Vec<Cell>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    key: String,
    before: Option<Vec<Cell>>,
    after: Option<Vec<Cell>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TableTransition {
    /// Table reads bind a complete range, including absence. Insert-only writes
    /// bind just their target keys, so unrelated parked replica tasks survive.
    complete_read: bool,
    before: Image,
    changes: Vec<Change>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepTransition {
    schema_version: u32,
    action_id: EntityId,
    input_digest: Digest,
    source_ledger_id: EntityId,
    original_head: SubjectBranchHead,
    result_commit: CommitRef,
    created_at: chrono::DateTime<chrono::Utc>,
    updated: Vec<UpdatedEntity>,
    /// Typed references extracted from SQL text/JSON cells. Keeping these
    /// explicit makes the accepted value's ordinary CAS walk complete.
    object_roots: Vec<ObjectRef>,
    tables: Vec<TableTransition>,
}

pub(crate) struct PreparedStep {
    pub admission: CommitAdmission,
    pub transition_ref: ObjectRef,
}

impl StepTransition {
    /// Bind the frozen rows to the public S1 identities, independently of the
    /// producer that captured them. The remaining SQL rows are exact deltas;
    /// head, committed result and successor may never be omitted or renamed.
    fn validate_result(
        &self,
        step: &SubjectStep,
        head: &SubjectBranchHead,
        next: EntityId,
    ) -> Result<(), ShenkoError> {
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
                .ok_or_else(|| conflict("head revision exhausted"))?,
        );
        let successor = SubjectStep {
            step_id: next,
            branch_id: head.branch_id.clone(),
            step_no: step
                .step_no
                .checked_add(1)
                .ok_or_else(|| conflict("step number exhausted"))?,
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
                .ok_or_else(|| conflict("missing S1 result table"))?;
            if !table.complete_read {
                return Err(conflict("S1 predecessor is not a complete read"));
            }
            let column = table
                .before
                .table
                .columns
                .iter()
                .position(|name| name == id_column)
                .ok_or_else(|| conflict("missing identity column"))?;
            let data = table
                .before
                .table
                .columns
                .iter()
                .position(|name| name == "data")
                .ok_or_else(|| conflict("missing result data"))?;
            let change = table
                .changes
                .iter()
                .find(|change| {
                    change.after.as_ref().and_then(|row| row.get(column))
                        == Some(&Cell::Text(id.to_string()))
                })
                .ok_or_else(|| conflict("missing original S1 result identity"))?;
            let Some(Cell::Text(json)) = change.after.as_ref().and_then(|row| row.get(data)) else {
                return Err(conflict("missing frozen result bytes"));
            };
            if change.before.is_none() != inserts
                || serde_json::from_str::<serde_json::Value>(json)? != expected
            {
                return Err(conflict(
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
            return Err(conflict("S1 receipt identities differ"));
        }
        Ok(())
    }
}

fn conflict(reason: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::conflict(format!("canonical S1 transition: {reason}"))
}

fn quoted(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn local_table(name: &str) -> bool {
    matches!(
        name,
        "schema_version"
            | "commit_log"
            | "ledger_identity"
            | "commit_authority_policy"
            | "subject_authority_bindings"
            | "control_prepared_transitions"
            | "control_materializations"
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
        return Err(conflict(format!(
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
                .ok_or_else(|| conflict("key index outside row"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if cells
        .iter()
        .any(|cell| matches!(cell, Cell::Null | Cell::LocalSequence))
    {
        return Err(conflict("null or local primary key"));
    }
    Ok(serde_json::to_string(&cells)?)
}

fn image(conn: &Connection, table: &Table) -> Result<Image, ShenkoError> {
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
        let values = (0..table.columns.len())
            .map(|index| {
                if Some(index) == table.local_sequence {
                    Ok(Cell::LocalSequence)
                } else {
                    Cell::read(row.get_ref(index).map_err(storage_error)?)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        if rows.insert(key(table, &values)?, values).is_some() {
            return Err(conflict("duplicate primary key"));
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
    pub(crate) fn prepare_step_transition(
        &self,
        step: &SubjectStep,
        head: &SubjectBranchHead,
        next: EntityId,
        mut position: AdmissionPosition,
    ) -> Result<PreparedStep, ShenkoError> {
        position.aliases.sort();
        position.aliases.dedup();
        position.reads.sort();
        position.reads.dedup();
        let action_id = EntityId::derived("commit-admission-action-v1", position.key.as_bytes());
        let input_digest = input_digest(step, head, next)?;
        // Retry reads the already frozen plan; no new time, UUID, sequence or
        // snapshot identity is substituted after a chosen-but-unapplied crash.
        if let Some(prepared) = self.prepared_step(action_id, &input_digest)? {
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
            if let Some(prepared) = prepared_step(&tx, action_id, &input_digest)? { return Ok(prepared); }
            let tables = layouts(&tx)?;
            let mut before = BTreeMap::new();
            for (name, table) in &tables {
                // A table without PK can only be a read projection outside
                // this transition. If S1 touches one, the authorizer below
                // refuses preparation rather than inventing rowid identity.
                if !table.key.is_empty() { before.insert(name.clone(), image(&tx, table)?); }
            }
            let result_commit = CommitRef::new(self.ledger.ledger_id(), self.ledger.allocate_seq(&tx)?);
            tx.execute_batch("SAVEPOINT canonical_s1_evaluation").map_err(storage_error)?;
            let accesses = Arc::new(Mutex::new(Accesses::default()));
            let observation = observe(&tx, Arc::clone(&accesses));
            let receipt = self.apply_step_commit_tx(&tx, step, head, next, result_commit);
            drop(observation);
            let receipt = receipt?;
            let accesses = accesses.lock();
            let touched = accesses.reads.union(&accesses.writes).collect::<BTreeSet<_>>();
            let mut transitions = Vec::new();
            for name in touched {
                if local_table(name) { return Err(conflict(format!("S1 depended on node-local table {name}"))); }
                let Some(table) = tables.get(name) else {
                    // Virtual/shadow tables are derivations rebuilt by their
                    // original triggers. They cannot carry canonical facts.
                    continue;
                };
                let mut original = before.remove(name).ok_or_else(|| conflict(format!("S1 touched unkeyed table {name}")))?;
                let after = image(&tx, table)?;
                let keys = original.rows.keys().chain(after.rows.keys()).collect::<BTreeSet<_>>();
                let changes: Vec<Change> = keys.into_iter().filter_map(|key| {
                    let old = original.rows.get(key); let new = after.rows.get(key);
                    (old != new).then(|| Change { key: key.clone(), before: old.cloned(), after: new.cloned() })
                }).collect();
                let complete_read = accesses.reads.contains(name);
                if !complete_read {
                    original.rows.retain(|key, _| changes.iter().any(|change| &change.key == key));
                }
                transitions.push(TableTransition { complete_read, before: original, changes });
            }
            // Detect writes that escaped the observed transaction body (for
            // example a future helper using cached SQL). Never silently omit
            // a changed base table from the frozen result.
            for (name, original) in before {
                if image(&tx, &original.table)? != original {
                    return Err(conflict(format!("unobserved S1 write to {name}")));
                }
            }
            tx.execute_batch("ROLLBACK TO canonical_s1_evaluation; RELEASE canonical_s1_evaluation").map_err(storage_error)?;
            let object_roots = self.transition_object_roots(&transitions);
            let (_, missing) = self.collect_closure(&object_roots).map_err(storage_error)?;
            if !missing.is_empty() { return Err(ShenkoError::ObjectClosureIncomplete { missing_count: missing.len() }); }
            let transition = StepTransition { schema_version: 1, action_id, input_digest: input_digest.clone(),
                source_ledger_id: self.ledger.ledger_id(), original_head: head.clone(), result_commit,
                created_at: chrono::Utc::now(), updated: receipt.updated, object_roots, tables: transitions };
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
            tx.commit().map_err(storage_error)?;
            Ok(PreparedStep { admission, transition_ref })
        }, |error| matches!(error, ShenkoError::Storage(_)))
    }

    fn prepared_step(
        &self,
        action: EntityId,
        input: &Digest,
    ) -> Result<Option<PreparedStep>, ShenkoError> {
        prepared_step(&self.ledger.conn.lock(), action, input)
    }

    /// Resume the crash window after a source sealed S1 but before it learned
    /// whether consensus chose it. Call before activating the new runtime
    /// epoch. Only exact source predecessors qualify; stale plans stay facts.
    /// Preparing the original descriptor either learns its old grant or orders
    /// that same descriptor. It never regenerates a decision or sends an effect.
    pub fn resume_prepared_step_commits(&self, subject: EntityId) -> Result<usize, ShenkoError> {
        if self.ledger.authority.activated() {
            return Err(shenko_types::authority::authority_fenced(
                "prepared recovery requires a new Store incarnation before runtime activation",
            ));
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
        let mut applied = 0;
        for raw in descriptors {
            let admission: CommitAdmission = serde_json::from_str(&raw)?;
            if admission.subject_id != subject || self.control_admission_applied_here(&admission)? {
                continue;
            }
            let envelope: serde_json::Value =
                serde_json::from_slice(&self.cas.read_object(&admission.payload_ref.digest)?)?;
            let head: SubjectBranchHead = serde_json::from_value(envelope["head"].clone())?;
            if envelope["source_ledger_id"] != serde_json::to_value(self.ledger.ledger_id())?
                || serde_json::to_value(self.load_subject(subject).map_err(storage_error)?.0)?
                    != serde_json::to_value(&head)?
            {
                continue;
            }
            let step: SubjectStep = serde_json::from_value(envelope["action"]["step"].clone())?;
            let next: EntityId =
                serde_json::from_value(envelope["action"]["next_step_id"].clone())?;
            let reference: ObjectRef =
                serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
            self.admission_objects(&admission.payload_ref)
                .map_err(storage_error)?;
            let reservation = self
                .reserve_sealed_admission(admission.clone(), Vec::new(), Vec::new())?
                .prepared_recovery();
            self.apply_step_transition(
                &admission,
                &reference,
                &step,
                &head,
                next,
                &reservation,
                false,
            )?;
            applied += 1;
        }
        Ok(applied)
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
    ) -> Result<StepTransition, ShenkoError> {
        if reference.codec.name != CODEC
            || reference.codec.major != 1
            || reference.media_type != "application/json"
            || reference.bytes > MAX_IMAGE_BYTES as u64
        {
            return Err(conflict("unsupported transition type or size"));
        }
        let bytes = self.cas.read_object(&reference.digest)?;
        if bytes.len() as u64 != reference.bytes || Digest::blake3(&bytes) != reference.digest {
            return Err(conflict("transition bytes differ"));
        }
        let transition: StepTransition = serde_json::from_slice(&bytes)?;
        let roots = self.transition_object_roots(&transition.tables);
        let (_, missing) = self.collect_closure(&roots).map_err(storage_error)?;
        if !missing.is_empty() {
            return Err(ShenkoError::ObjectClosureIncomplete {
                missing_count: missing.len(),
            });
        }
        Ok(transition)
    }

    pub(crate) fn apply_step_transition(
        &self,
        admission: &CommitAdmission,
        reference: &ObjectRef,
        step: &SubjectStep,
        head: &SubjectBranchHead,
        next: EntityId,
        authority: &AuthorityReservation,
        learned: bool,
    ) -> Result<CommitReceipt, ShenkoError> {
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
            return Err(conflict(
                "transition is not the admitted S1/result identity",
            ));
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
                let actual_layouts = layouts(&tx)?;
                let mut seen = BTreeSet::new();
                for table in &transition.tables {
                    let expected = &table.before;
                    if !seen.insert(expected.table.name.clone())
                        || actual_layouts.get(&expected.table.name) != Some(&expected.table)
                        || current_image(&tx, table)? != *expected
                    {
                        return Err(conflict(format!(
                            "predecessor/schema differs for {}",
                            expected.table.name
                        )));
                    }
                }
                let sequence = self.ledger.allocate_seq(&tx)?;
                let local_commit = CommitRef::new(self.ledger.ledger_id(), sequence);
                tx.execute_batch("PRAGMA defer_foreign_keys=ON")
                    .map_err(storage_error)?;
                for table in &transition.tables {
                    apply_table(&tx, table, sequence)?;
                }
                for table in &transition.tables {
                    let mut expected = table.before.clone();
                    for change in &table.changes {
                        if let Some(after) = &change.after {
                            expected.rows.insert(change.key.clone(), after.clone());
                        } else {
                            expected.rows.remove(&change.key);
                        }
                    }
                    if current_image(&tx, table)? != expected {
                        return Err(conflict("applied writes differ from frozen result"));
                    }
                }
                authority.record_canonical_admission(
                    &tx,
                    local_commit,
                    transition.result_commit,
                    reference,
                    transition.created_at,
                )?;
                self.record_subject_commit(&tx, local_commit, transition.updated.len() as i64)?;
                tx.commit().map_err(storage_error)?;
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
        self.apply_step_transition(
            &grant.admission,
            reference,
            step,
            head,
            next,
            &reservation,
            true,
        )
    }
}

fn prepared_step(
    conn: &Connection,
    action: EntityId,
    input: &Digest,
) -> Result<Option<PreparedStep>, ShenkoError> {
    let row: Option<(String,String,String)> = conn.query_row(
        "SELECT input_digest,admission,transition_ref FROM control_prepared_transitions WHERE action_id=?1",
        [action.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(storage_error)?;
    row.map(|(original, admission, reference)| {
        if original != input.0 {
            return Err(conflict(
                "another S1 already reserved this successor position",
            ));
        }
        let admission: CommitAdmission = serde_json::from_str(&admission)?;
        if admission.action_id != action {
            return Err(conflict("stored prepared action identity differs"));
        }
        Ok(PreparedStep {
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
    for change in &transition.changes {
        if change.before == change.after
            || transition.before.rows.get(&change.key) != change.before.as_ref()
        {
            return Err(conflict("invalid row change preimage"));
        }
        for values in [&change.before, &change.after].into_iter().flatten() {
            if key(table, values)? != change.key
                || values.iter().enumerate().any(|(index, value)| {
                    matches!(value, Cell::LocalSequence) != (Some(index) == table.local_sequence)
                })
            {
                return Err(conflict("row change key or sequence projection differs"));
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
                .ok_or_else(|| conflict("delete without preimage"))?;
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
            return Err(conflict("row mutation did not affect exactly one identity"));
        }
    }
    Ok(())
}
