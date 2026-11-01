//! Durable activation records and the activation selector — SPEC §41.12.
//!
//! The selector row is the linearization point of a software-generation
//! hand-over: a successful conditional update of `selector_revision` (with a
//! strictly monotonic `active_epoch`) is the only thing that transfers the
//! control authority. A file rename, a symlink or a running PID never does.
//!
//! Both tables live in the same SQLite file as the ledger, so the first
//! selector can be created in the same transaction as the genesis, the root
//! task, the branch head and the first step (§44.6).

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use shenko_types::activation::{ActivationPhase, ActivationRecord, ActivationSelector};
use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::ids::{ControllerEpoch, EntityId, RecordRevision};
use shenko_types::object::ObjectRef;
use shenko_types::subject::{SubjectBranchHead, SubjectMode};

use crate::Store;
use crate::store::storage_error;

/// The activation tables. Created on first use inside the writing
/// transaction, so the ledger's own migration file stays untouched.
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS activation_launch_receipts (
    launch_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    selector_revision INTEGER NOT NULL,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS activation_records (
    activation_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    phase TEXT NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_activation_records_agent
    ON activation_records(agent_id, phase);

CREATE TABLE IF NOT EXISTS activation_selectors (
    agent_id TEXT PRIMARY KEY,
    selector_revision INTEGER NOT NULL,
    active_epoch INTEGER NOT NULL,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

/// Create the activation tables if they do not exist yet.
pub fn ensure_activation_schema(conn: &Connection) -> Result<(), ShenkoError> {
    conn.execute_batch(SCHEMA).map_err(storage_error)
}

impl crate::store::GenesisExtension for ActivationSelector {
    fn extend_genesis(
        &self,
        store: &Store,
        tx: &Transaction<'_>,
        root_commit: CommitRef,
    ) -> Result<u32, ShenkoError> {
        let raw: String = tx
            .query_row(
                "SELECT data FROM subject_branch_heads WHERE agent_id=?1",
                [self.agent_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let head: SubjectBranchHead = serde_json::from_str(&raw).map_err(storage_error)?;
        let origin_matches = match &self.origin {
            shenko_types::activation::ActivationOrigin::Genesis { genesis_ref } => {
                genesis_ref.genesis_id == head.genesis_ref.genesis_id
                    && genesis_ref.ledger_id == head.genesis_ref.ledger_id
                    && genesis_ref.ledger_id == store.ledger.ledger_id()
            }
            shenko_types::activation::ActivationOrigin::Activation { .. } => false,
        };
        if !origin_matches
            || self.generation != head.software_generation
            || self.selector_revision != 0
            || self.active_epoch == 0
            || self.active_epoch != head.controller_epoch.0
        {
            return Err(ShenkoError::conflict(
                "initial activation selector does not bind this genesis/head",
            ));
        }
        write_initial_selector(tx, self, root_commit)?;
        Ok(1)
    }
}

/// Write the *first* selector of an installation inside the caller's
/// transaction.
///
/// This is the conditional "expected selector does not exist" write of
/// §41.12 X05: an installation that already has a selector is never
/// overwritten by a bootstrap or by an import.
pub fn write_initial_selector(
    tx: &Transaction<'_>,
    selector: &ActivationSelector,
    root_commit: CommitRef,
) -> Result<(), ShenkoError> {
    tx.execute_batch(SCHEMA).map_err(storage_error)?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT agent_id FROM activation_selectors WHERE agent_id=?1",
            params![selector.agent_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    if existing.is_some() {
        return Err(ShenkoError::conflict(
            "activation selector already exists; bootstrap must not overwrite it",
        ));
    }
    if selector.selector_revision != 0 {
        return Err(ShenkoError::conflict(
            "the first activation selector must start at revision 0",
        ));
    }
    tx.execute(
        "INSERT INTO activation_selectors
            (agent_id, selector_revision, active_epoch, data, updated_commit_seq)
         VALUES (?1,?2,?3,?4,?5)",
        params![
            selector.agent_id.to_string(),
            selector.selector_revision as i64,
            selector.active_epoch as i64,
            serde_json::to_string(selector).map_err(storage_error)?,
            root_commit.seq.0 as i64,
        ],
    )
    .map_err(storage_error)?;
    Ok(())
}

/// Read the current selector of a subject.
pub fn read_selector(
    conn: &Connection,
    agent_id: EntityId,
) -> Result<Option<ActivationSelector>, ShenkoError> {
    let row: Option<String> = conn
        .query_row(
            "SELECT data FROM activation_selectors WHERE agent_id=?1",
            params![agent_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    match row {
        None => Ok(None),
        Some(json) => Ok(Some(
            serde_json::from_str(&json).map_err(storage_error)?,
        )),
    }
}

/// Compare-and-swap the selector inside the caller's transaction.
///
/// The update succeeds only when the stored revision is exactly
/// `expected_revision`. `active_epoch` is strictly monotonic: a stored epoch
/// that is not smaller than the requested one is refused instead of being
/// reused.
pub fn cas_selector(
    tx: &Transaction<'_>,
    expected_revision: u64,
    next: &ActivationSelector,
) -> Result<ActivationSelector, ShenkoError> {
    tx.execute_batch(SCHEMA).map_err(storage_error)?;
    let current: Option<(u64, u64)> = tx
        .query_row(
            "SELECT selector_revision, active_epoch FROM activation_selectors WHERE agent_id=?1",
            params![next.agent_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(storage_error)?;
    let Some((current_revision, current_epoch)) = current else {
        return Err(ShenkoError::conflict(
            "activation selector does not exist; a hand-over needs an existing selector",
        ));
    };
    if current_revision != expected_revision {
        return Err(ShenkoError::conflict(format!(
            "SELECTOR_REVISION_CONFLICT: expected {expected_revision}, actual {current_revision}"
        )));
    }
    if next.active_epoch <= current_epoch {
        return Err(ShenkoError::conflict(format!(
            "activation epoch must increase: current {current_epoch}, requested {}",
            next.active_epoch
        )));
    }
    let updated = tx.execute(
        "UPDATE activation_selectors
            SET selector_revision=?2, active_epoch=?3, data=?4, updated_commit_seq=?5
          WHERE agent_id=?1 AND selector_revision=?6",
        params![
            next.agent_id.to_string(),
            next.selector_revision as i64,
            next.active_epoch as i64,
            serde_json::to_string(next).map_err(storage_error)?,
            0i64,
            expected_revision as i64,
        ],
    )
    .map_err(storage_error)?;
    if updated != 1 {
        return Err(ShenkoError::conflict(
            "SELECTOR_REVISION_CONFLICT: the selector changed during the update",
        ));
    }
    Ok(next.clone())
}

/// Insert or update one activation record.
///
/// `expected_revision` is the record revision the caller read. `None` means
/// "this record must not exist yet".
pub fn write_activation_record(
    tx: &Transaction<'_>,
    record: &ActivationRecord,
    expected_revision: Option<RecordRevision>,
) -> Result<ActivationRecord, ShenkoError> {
    tx.execute_batch(SCHEMA).map_err(storage_error)?;
    let stored: Option<u64> = tx
        .query_row(
            "SELECT record_revision FROM activation_records WHERE activation_id=?1",
            params![record.activation_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    match (stored, expected_revision) {
        (None, None) => {}
        (None, Some(_)) => {
            return Err(ShenkoError::conflict(
                "activation record does not exist; the expected revision cannot match",
            ));
        }
        (Some(revision), Some(expected)) => {
            if revision != expected.0 {
                return Err(ShenkoError::conflict(format!(
                    "ACTIVATION_REVISION_CONFLICT: expected {}, actual {revision}",
                    expected.0
                )));
            }
        }
        (Some(_), None) => {
            return Err(ShenkoError::conflict(
                "activation record already exists; a new activation needs its own identity",
            ));
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO activation_records
            (activation_id, agent_id, phase, record_revision, data, updated_commit_seq)
         VALUES (?1,?2,?3,?4,?5,0)",
        params![
            record.activation_id.to_string(),
            record.agent_id.to_string(),
            record.phase.name(),
            record.record_revision as i64,
            serde_json::to_string(record).map_err(storage_error)?,
        ],
    )
    .map_err(storage_error)?;
    Ok(record.clone())
}

/// Read one activation record.
pub fn read_activation_record(
    conn: &Connection,
    activation_id: EntityId,
) -> Result<Option<ActivationRecord>, ShenkoError> {
    let row: Option<String> = conn
        .query_row(
            "SELECT data FROM activation_records WHERE activation_id=?1",
            params![activation_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    match row {
        None => Ok(None),
        Some(json) => Ok(Some(
            serde_json::from_str(&json).map_err(storage_error)?,
        )),
    }
}

/// Every activation record of a subject, oldest first.
pub fn list_activation_records(
    conn: &Connection,
    agent_id: EntityId,
) -> Result<Vec<ActivationRecord>, ShenkoError> {
    let mut statement = conn
        .prepare(
            "SELECT data FROM activation_records WHERE agent_id=?1 ORDER BY created_at, activation_id",
        )
        .map_err(storage_error)?;
    let rows = statement
        .query_map(params![agent_id.to_string()], |row| {
            row.get::<_, String>(0)
        })
        .map_err(storage_error)?;
    let mut records = Vec::new();
    for row in rows {
        let json = row.map_err(storage_error)?;
        records.push(serde_json::from_str(&json).map_err(storage_error)?);
    }
    Ok(records)
}

/// The one activation that is still running for this subject, if any.
pub fn read_open_activation(
    conn: &Connection,
    agent_id: EntityId,
) -> Result<Option<ActivationRecord>, ShenkoError> {
    Ok(list_activation_records(conn, agent_id)?
        .into_iter()
        .rev()
        .find(|record| matches!(record.phase, ActivationPhase::Prepared | ActivationPhase::Copying
            | ActivationPhase::CatchUp | ActivationPhase::Smoke | ActivationPhase::Handoff)))
}

/// The phase of an activation, read without decoding the whole record.
pub fn read_activation_phase(
    conn: &Connection,
    activation_id: EntityId,
) -> Result<Option<ActivationPhase>, ShenkoError> {
    let row: Option<String> = conn
        .query_row(
            "SELECT phase FROM activation_records WHERE activation_id=?1",
            params![activation_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    match row {
        None => Ok(None),
        Some(name) => Ok(Some(match name.as_str() {
            "prepared" => ActivationPhase::Prepared,
            "copying" => ActivationPhase::Copying,
            "catch_up" => ActivationPhase::CatchUp,
            "smoke" => ActivationPhase::Smoke,
            "handoff" => ActivationPhase::Handoff,
            "activated" => ActivationPhase::Activated,
            "observing" => ActivationPhase::Observing,
            "retired" => ActivationPhase::Retired,
            "abandoned" => ActivationPhase::Abandoned,
            other => {
                return Err(ShenkoError::storage(format!(
                    "unknown activation phase {other:?}"
                )));
            }
        })),
    }
}

impl Store {
    pub fn activation_launch_for_selector(&self, agent: EntityId, revision: u64) -> Result<Option<serde_json::Value>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let raw: Option<String> = conn.query_row("SELECT data FROM activation_launch_receipts WHERE agent_id=?1 AND selector_revision=?2 ORDER BY updated_commit_seq DESC LIMIT 1",
            params![agent.to_string(), revision as i64], |row| row.get(0)).optional().map_err(storage_error)?;
        raw.map(|raw| serde_json::from_str(&raw).map_err(storage_error)).transpose()
    }
    pub fn activation_launch(&self, launch_id: EntityId) -> Result<Option<serde_json::Value>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let raw: Option<String> = conn.query_row("SELECT data FROM activation_launch_receipts WHERE launch_id=?1",
            [launch_id.to_string()], |row| row.get(0)).optional().map_err(storage_error)?;
        raw.map(|raw| serde_json::from_str(&raw).map_err(storage_error)).transpose()
    }

    pub fn record_activation_launch(&self, launch_id: EntityId, selector: &ActivationSelector,
        receipt: &serde_json::Value) -> Result<(), ShenkoError> {
        self.ledger.with_mutation(|| {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage_error)?;
            tx.execute_batch(SCHEMA).map_err(storage_error)?;
            if read_selector(&tx, selector.agent_id)?.as_ref() != Some(selector) {
                return Err(ShenkoError::conflict("launch selector changed before receipt commit"));
            }
            let raw: Option<String> = tx.query_row("SELECT data FROM activation_launch_receipts WHERE launch_id=?1",
                [launch_id.to_string()], |row| row.get(0)).optional().map_err(storage_error)?;
            if let Some(raw) = raw {
                if serde_json::from_str::<serde_json::Value>(&raw).map_err(storage_error)? != *receipt {
                    return Err(ShenkoError::conflict("launch identity reused with different receipt"));
                }
                return Ok(());
            }
            let seq = self.ledger.allocate_seq(&tx)?;
            let commit = CommitRef::new(self.ledger.ledger_id(), seq);
            tx.execute("INSERT INTO activation_launch_receipts(launch_id,agent_id,selector_revision,data,updated_commit_seq) VALUES(?1,?2,?3,?4,?5)",
                params![launch_id.to_string(), selector.agent_id.to_string(), selector.selector_revision as i64,
                    serde_json::to_string(receipt).map_err(storage_error)?, seq.0 as i64]).map_err(storage_error)?;
            self.record_subject_commit(&tx, commit, 1)?;
            tx.commit().map_err(storage_error)?;
            Ok(())
        }, |error| matches!(error, ShenkoError::Storage(_)))
    }

    /// Installation identity comes from the subject's persisted bootstrap,
    /// not the current ledger writer identity (which changes across copies).
    pub fn installation_id_for_subject(&self, agent: EntityId) -> Result<EntityId, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let raw: String = conn.query_row("SELECT data FROM subject_genesis WHERE subject_id=?1",
            [agent.to_string()], |row| row.get(0)).map_err(storage_error)?;
        let genesis: shenko_types::subject::SubjectGenesis = serde_json::from_str(&raw).map_err(storage_error)?;
        let (installation, data): (String, String) = conn.query_row(
            "SELECT installation_id,data FROM bootstrap_records WHERE bootstrap_id=?1",
            [genesis.bootstrap_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?))).map_err(storage_error)?;
        let installation = installation.parse::<EntityId>().map_err(storage_error)?;
        let value: serde_json::Value = serde_json::from_str(&data).map_err(storage_error)?;
        if value.get("installation_id").and_then(|value| value.as_str()) != Some(installation.to_string().as_str()) {
            return Err(ShenkoError::conflict("bootstrap installation identity differs from indexed binding"));
        }
        Ok(installation)
    }

    /// Explicit same-installation migration, NOT peer election authority.
    /// The caller owns an offline candidate directory. Only a validated
    /// local paused boundary is installed; the declared archive importer
    /// subsequently verifies/applies all remaining typed fact closures.
    pub fn copy_paused_activation_boundary(&self, source: &Store, agent: EntityId) -> Result<(), ShenkoError> {
        let (head, step, requests, selector, activations) = {
            let conn = source.ledger.conn.lock();
            let raw: String = conn.query_row("SELECT data FROM subject_branch_heads WHERE agent_id=?1", [agent.to_string()], |row| row.get(0)).map_err(storage_error)?;
            let head: SubjectBranchHead = serde_json::from_str(&raw).map_err(storage_error)?;
            if head.mode != SubjectMode::Paused { return Ok(()); }
            crate::runtime_boundary::validate_paused_head(&conn, &head)?;
            let step = crate::store::read_step(&conn, &head.branch_id, head.next_step_id)?
                .ok_or_else(|| ShenkoError::conflict("paused successor missing"))?;
            crate::store::validate_step_binding(&head, &step)?;
            if step.state != shenko_types::subject::SubjectStepState::Planned
                || step.active_request_id.is_some() || step.active_attempt_id.is_some() {
                return Err(ShenkoError::conflict("migration boundary is not clean"));
            }
            let requests = {
                let mut stmt = conn.prepare("SELECT data FROM subject_quiesce_requests WHERE agent_id=?1 AND state='paused'").map_err(storage_error)?;
                stmt.query_map([agent.to_string()], |row| row.get::<_, String>(0))
                    .map_err(storage_error)?.collect::<Result<Vec<_>, _>>().map_err(storage_error)?
            };
            let selector = read_selector(&conn, agent)?
                .ok_or_else(|| ShenkoError::conflict("paused migration source selector missing"))?;
            if selector.generation != head.software_generation {
                return Err(ShenkoError::conflict("source selector does not own the paused subject generation"));
            }
            let activations = list_activation_records(&conn, agent)?;
            (head, step, requests, selector, activations)
        };
        self.ledger.with_mutation(|| {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage_error)?;
            let raw: String = tx.query_row("SELECT data FROM subject_branch_heads WHERE agent_id=?1", [agent.to_string()], |row| row.get(0)).map_err(storage_error)?;
            let old: SubjectBranchHead = serde_json::from_str(&raw).map_err(storage_error)?;
            if old.genesis_ref != head.genesis_ref || old.agent_id != head.agent_id || old.branch_id != head.branch_id {
                return Err(ShenkoError::conflict("migration boundary belongs to another installation/branch"));
            }
            let seq = self.ledger.allocate_seq(&tx)?;
            let boundary_commit = CommitRef::new(self.ledger.ledger_id(), seq);
            let local_selector = read_selector(&tx, agent)?
                .ok_or_else(|| ShenkoError::conflict("candidate selector missing"))?;
            if local_selector.selector_revision > selector.selector_revision
                || (local_selector.selector_revision == selector.selector_revision && local_selector != selector) {
                return Err(ShenkoError::conflict("candidate selector diverged from its owned source; no peer promotion"));
            }
            // This inactive copy receives its CURRENT source's control
            // state before archive import. It acquires no launch/dispatch
            // authority here: its slot manifest still differs from this
            // source selector's route, until activate_successor CAS commits.
            tx.execute("UPDATE activation_selectors SET selector_revision=?2,active_epoch=?3,data=?4,updated_commit_seq=?5 WHERE agent_id=?1",
                params![agent.to_string(), selector.selector_revision as i64, selector.active_epoch as i64,
                    serde_json::to_string(&selector).map_err(storage_error)?, seq.0 as i64]).map_err(storage_error)?;
            for incoming in &activations {
                let existing = read_activation_record(&tx, incoming.activation_id)?;
                if let Some(existing) = &existing {
                    if existing.agent_id != incoming.agent_id || existing.from_generation != incoming.from_generation
                        || existing.to_generation != incoming.to_generation
                        || existing.record_revision > incoming.record_revision
                        || (existing.record_revision == incoming.record_revision && existing != incoming) {
                        return Err(ShenkoError::conflict("owned migration activation history diverged; retain both closures"));
                    }
                }
                write_activation_record(&tx, incoming, existing.as_ref().map(|record| RecordRevision(record.record_revision)))?;
                tx.execute("UPDATE activation_records SET updated_commit_seq=?2 WHERE activation_id=?1",
                    params![incoming.activation_id.to_string(), seq.0 as i64]).map_err(storage_error)?;
            }
            let previous = {
                let mut stmt = tx.prepare("SELECT data FROM subject_quiesce_requests WHERE agent_id=?1 AND state IN ('requested','paused')").map_err(storage_error)?;
                stmt.query_map([agent.to_string()], |row| row.get::<_, String>(0))
                    .map_err(storage_error)?.collect::<Result<Vec<_>, _>>().map_err(storage_error)?
            };
            for raw in previous {
                let mut request: crate::runtime_boundary::SubjectQuiesce = serde_json::from_str(&raw).map_err(storage_error)?;
                request.state = crate::runtime_boundary::SubjectQuiesceState::Resumed;
                request.closed_at = Some(boundary_commit);
                tx.execute("UPDATE subject_quiesce_requests SET state='resumed',data=?2,updated_commit_seq=?3 WHERE request_id=?1",
                    params![request.request_id.to_string(), serde_json::to_string(&request).map_err(storage_error)?, seq.0 as i64]).map_err(storage_error)?;
            }
            tx.execute("UPDATE subject_branch_heads SET committed_step_no=?2,committed_step_id=?3,next_step_id=?4,mode='paused',record_revision=?5,data=?6 WHERE agent_id=?1",
                params![agent.to_string(), head.committed_step_no as i64, head.committed_step_id.map(|id| id.to_string()),
                    head.next_step_id.to_string(), head.record_revision.0 as i64, serde_json::to_string(&head).map_err(storage_error)?]).map_err(storage_error)?;
            tx.execute("INSERT INTO subject_steps(step_id,branch_id,step_no,state,record_revision,data,updated_commit_seq) VALUES(?1,?2,?3,'planned',?4,?5,0) ON CONFLICT(step_id) DO UPDATE SET state='planned',record_revision=excluded.record_revision,data=excluded.data",
                params![step.step_id.to_string(), step.branch_id.0.to_string(), step.step_no as i64,
                    step.record_revision.0 as i64, serde_json::to_string(&step).map_err(storage_error)?]).map_err(storage_error)?;
            for raw in &requests {
                let request: crate::runtime_boundary::SubjectQuiesce = serde_json::from_str(raw).map_err(storage_error)?;
                tx.execute("INSERT INTO subject_quiesce_requests(request_id,agent_id,state,data,updated_commit_seq) VALUES(?1,?2,'paused',?3,0) ON CONFLICT(request_id) DO UPDATE SET state='paused',data=excluded.data",
                    params![request.request_id.to_string(), agent.to_string(), raw]).map_err(storage_error)?;
            }
            self.record_subject_commit(&tx, boundary_commit, 3)?;
            tx.commit().map_err(storage_error)?;
            Ok(())
        }, |error| matches!(error, ShenkoError::Storage(_)))
    }

    /// The current activation selector of a subject, if this installation has
    /// one. A missing selector is a real state, not an error.
    pub fn activation_selector(
        &self,
        agent_id: EntityId,
    ) -> Result<Option<ActivationSelector>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        read_selector(&conn, agent_id)
    }

    /// Every activation record of a subject, oldest first.
    pub fn activation_records(
        &self,
        agent_id: EntityId,
    ) -> Result<Vec<ActivationRecord>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        list_activation_records(&conn, agent_id)
    }

    /// One activation record by identity.
    pub fn activation_record(
        &self,
        activation_id: EntityId,
    ) -> Result<Option<ActivationRecord>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        read_activation_record(&conn, activation_id)
    }

    /// The activation that is still running for this subject, if any.
    pub fn open_activation(
        &self,
        agent_id: EntityId,
    ) -> Result<Option<ActivationRecord>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        read_open_activation(&conn, agent_id)
    }

    /// Commit a new activation record. The identity must be new: an
    /// activation never overwrites another activation's history.
    pub fn create_activation_record(
        &self,
        record: &ActivationRecord,
    ) -> Result<ActivationRecord, ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit_ref = CommitRef::new(self.ledger.ledger_id(), seq);
                let mut stored = record.clone();
                stored.record_revision = 0;
                write_activation_record(&tx, &stored, None)?;
                tx.execute(
                    "UPDATE activation_records SET updated_commit_seq=?2 WHERE activation_id=?1",
                    params![stored.activation_id.to_string(), seq.0 as i64],
                )
                .map_err(storage_error)?;
                self.record_subject_commit(&tx, commit_ref, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(stored)
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Advance an activation record under a revision CAS. The phase change
    /// itself is checked by the caller against the phase machine; this
    /// method only guarantees that no other writer moved the record.
    pub fn update_activation_record(
        &self,
        record: &ActivationRecord,
        expected: RecordRevision,
    ) -> Result<ActivationRecord, ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit_ref = CommitRef::new(self.ledger.ledger_id(), seq);
                let mut stored = record.clone();
                stored.record_revision = expected.next().0;
                write_activation_record(&tx, &stored, Some(expected))?;
                tx.execute(
                    "UPDATE activation_records SET updated_commit_seq=?2 WHERE activation_id=?1",
                    params![stored.activation_id.to_string(), seq.0 as i64],
                )
                .map_err(storage_error)?;
                self.record_subject_commit(&tx, commit_ref, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(stored)
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Swap the activation selector — the linearization point of an
    /// activation (§41.12 X17 step 4).
    ///
    /// The selector's expected revision, the monotonic epoch and the
    /// activation record's `activated` phase are committed in one ledger
    /// transaction. Before this commit the candidate has no execution
    /// authority; after it the previous generation has none.
    pub fn swap_activation_selector(
        &self,
        expected_revision: u64,
        next: &ActivationSelector,
        activated: &ActivationRecord,
    ) -> Result<(ActivationSelector, ActivationRecord, CommitRef), ShenkoError> {
        self.swap_activation_selector_inner(expected_revision, next, activated, false)
    }

    /// Candidate-only commit: selector and the single clean successor step
    /// acquire the new controller epoch together. Historical in-flight
    /// tasks/attempts retain their original generation and debt.
    pub fn activate_successor(
        &self,
        expected_revision: u64,
        next: &ActivationSelector,
        activated: &ActivationRecord,
    ) -> Result<(ActivationSelector, ActivationRecord, CommitRef), ShenkoError> {
        self.swap_activation_selector_inner(expected_revision, next, activated, true)
    }

    fn swap_activation_selector_inner(
        &self,
        expected_revision: u64,
        next: &ActivationSelector,
        activated: &ActivationRecord,
        successor: bool,
    ) -> Result<(ActivationSelector, ActivationRecord, CommitRef), ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit_ref = CommitRef::new(self.ledger.ledger_id(), seq);
                let swapped = cas_selector(&tx, expected_revision, next)?;
                if successor {
                    let raw: String = tx.query_row("SELECT data FROM subject_branch_heads WHERE agent_id=?1",
                        [next.agent_id.to_string()], |row| row.get(0)).map_err(storage_error)?;
                    let mut head: SubjectBranchHead = serde_json::from_str(&raw).map_err(storage_error)?;
                    if head.mode != SubjectMode::Paused {
                        return Err(ShenkoError::conflict("successor requires a paused clean subject boundary"));
                    }
                    let raw: String = tx.query_row("SELECT data FROM subject_steps WHERE step_id=?1",
                        [head.next_step_id.to_string()], |row| row.get(0)).map_err(storage_error)?;
                    let mut step: shenko_types::subject::SubjectStep = serde_json::from_str(&raw).map_err(storage_error)?;
                    if step.state != shenko_types::subject::SubjectStepState::Planned
                        || step.active_request_id.is_some() || step.active_attempt_id.is_some()
                        || step.input_plan_ref.is_some() {
                        return Err(ShenkoError::conflict("successor step has frozen or active work"));
                    }
                    crate::store::validate_step_binding(&head, &step)?;
                    head.controller_epoch = ControllerEpoch(head.controller_epoch.0.checked_add(1)
                        .ok_or_else(|| ShenkoError::conflict("controller epoch exhausted"))?);
                    head.software_generation = next.generation.clone();
                    head.mode = SubjectMode::Active;
                    head.mode_reason_ref = Some(next.route_authority_ref.clone());
                    head.record_revision = head.record_revision.next();
                    step.controller_epoch = head.controller_epoch;
                    step.software_generation = head.software_generation.clone();
                    step.record_revision = step.record_revision.next();
                    tx.execute("UPDATE subject_branch_heads SET mode='active',record_revision=?2,data=?3,updated_commit_seq=?4 WHERE agent_id=?1",
                        params![next.agent_id.to_string(), head.record_revision.0 as i64,
                            serde_json::to_string(&head).map_err(storage_error)?, seq.0 as i64]).map_err(storage_error)?;
                    tx.execute("UPDATE subject_steps SET record_revision=?2,data=?3,updated_commit_seq=?4 WHERE step_id=?1",
                        params![step.step_id.to_string(), step.record_revision.0 as i64,
                            serde_json::to_string(&step).map_err(storage_error)?, seq.0 as i64]).map_err(storage_error)?;
                    let requests = {
                        let mut stmt = tx.prepare("SELECT data FROM subject_quiesce_requests WHERE agent_id=?1 AND state='paused'").map_err(storage_error)?;
                        stmt.query_map([next.agent_id.to_string()], |row| row.get::<_, String>(0))
                            .map_err(storage_error)?.collect::<Result<Vec<_>, _>>().map_err(storage_error)?
                    };
                    for raw in requests {
                        let mut request: crate::runtime_boundary::SubjectQuiesce = serde_json::from_str(&raw).map_err(storage_error)?;
                        request.state = crate::runtime_boundary::SubjectQuiesceState::Resumed;
                        request.closed_at = Some(commit_ref);
                        tx.execute("UPDATE subject_quiesce_requests SET state='resumed',data=?2,updated_commit_seq=?3 WHERE request_id=?1",
                            params![request.request_id.to_string(), serde_json::to_string(&request).map_err(storage_error)?, seq.0 as i64]).map_err(storage_error)?;
                    }
                }
                let mut stored = activated.clone();
                stored.phase = ActivationPhase::Activated;
                stored.record_revision = activated.record_revision.checked_add(1)
                    .ok_or_else(|| ShenkoError::conflict("activation revision exhausted"))?;
                write_activation_record(&tx, &stored, Some(RecordRevision(activated.record_revision)))?;
                tx.execute(
                    "UPDATE activation_records SET phase=?2, updated_commit_seq=?3 WHERE activation_id=?1",
                    params![
                        stored.activation_id.to_string(),
                        stored.phase.name(),
                        seq.0 as i64
                    ],
                )
                .map_err(storage_error)?;
                tx.execute(
                    "UPDATE activation_selectors SET updated_commit_seq=?2 WHERE agent_id=?1",
                    params![swapped.agent_id.to_string(), seq.0 as i64],
                )
                .map_err(storage_error)?;
                self.record_subject_commit(&tx, commit_ref, 2)?;
                tx.commit().map_err(storage_error)?;
                Ok((swapped, stored, commit_ref))
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Fence the retired generation's writer.
    ///
    /// The head moves to `Handoff`, its controller epoch increases and the
    /// sealed reason (activation id and new epoch) is attached. Every step
    /// write validates the epoch and the mode against the registered head,
    /// so the retired generation can no longer commit a decision or admit a
    /// dispatch. The epoch is never reused: the successor starts from the
    /// fenced epoch.
    pub fn fence_head_for_handoff(
        &self,
        head: &SubjectBranchHead,
        expected_revision: RecordRevision,
        reason_ref: &ObjectRef,
    ) -> Result<SubjectBranchHead, ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit_ref = CommitRef::new(self.ledger.ledger_id(), seq);
                let mut fenced = head.clone();
                fenced.mode = SubjectMode::Handoff;
                fenced.controller_epoch = ControllerEpoch(head.controller_epoch.0 + 1);
                fenced.mode_reason_ref = Some(reason_ref.clone());
                fenced.record_revision = expected_revision.next();
                let changed = tx.execute(
                    "UPDATE subject_branch_heads SET mode='handoff', record_revision=?2, data=?3, updated_commit_seq=?4
                      WHERE agent_id=?5 AND branch_id=?6 AND record_revision=?7 AND next_step_id=?8",
                    params![
                        fenced.record_revision.0 as i64,
                        serde_json::to_string(&fenced).map_err(storage_error)?,
                        seq.0 as i64,
                        fenced.agent_id.to_string(),
                        fenced.branch_id.0.to_string(),
                        expected_revision.0 as i64,
                        fenced.next_step_id.to_string(),
                    ],
                )
                .map_err(storage_error)?;
                if changed != 1 {
                    return Err(ShenkoError::conflict(
                        "stale subject head; the fence was not applied",
                    ));
                }
                self.record_subject_commit(&tx, commit_ref, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(fenced)
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }
}
