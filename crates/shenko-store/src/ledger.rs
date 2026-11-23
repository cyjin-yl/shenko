//! Ledger — single mutation line per local store.
use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::sync::Arc;
use tracing::instrument;

use shenko_types::commit::{CommitReceipt, CommitRef, DurabilityProfile, UpdatedEntity};
use shenko_types::error::ShenkoError;
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::store::WriteBatch;
use shenko_types::installation::{InstallationDomain, InstallationPermit};
use std::path::PathBuf;

pub struct Ledger {
    pub(crate) conn: Arc<Mutex<Connection>>,
    /// Shared CAS directory; acquire its publication lease before any mutation.
    cas_base: PathBuf,
    incarnation: EntityId,
    ledger_id: EntityId,
    mutation_open: Mutex<bool>,
    pub(crate) authority: crate::authority::AuthorityRuntime,
    pub(crate) installation: Option<(InstallationDomain, PathBuf, PathBuf)>,
}

impl Ledger {
    pub(crate) fn new(conn: Arc<Mutex<Connection>>, incarnation: EntityId, cas_base: PathBuf,
        installation: Option<(InstallationDomain, PathBuf, PathBuf)>) -> Result<Self> {
        let ledger_id = {
            let mut c = conn.lock();
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // 旧库沿用第一条真实提交的身份；新库在首笔业务写入前固定身份。
            let previous: Option<String> = tx
                .query_row(
                    "SELECT ledger_id FROM commit_log ORDER BY id LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            let candidate = previous.unwrap_or_else(|| EntityId::new().to_string());
            tx.execute(
                "INSERT OR IGNORE INTO ledger_identity (singleton, ledger_id) VALUES (1, ?1)",
                [&candidate],
            )?;
            let id: String = tx.query_row(
                "SELECT ledger_id FROM ledger_identity WHERE singleton=1",
                [],
                |r| r.get(0),
            )?;
            let id = id.parse::<EntityId>()?;
            tx.commit()?;
            id
        };
        Ok(Self {
            conn,
            cas_base,
            incarnation,
            ledger_id,
            mutation_open: Mutex::new(true),
            authority: crate::authority::AuthorityRuntime::default(),
            installation,
        })
    }

    // 必须在同一 IMMEDIATE 写事务内调用，不能先分配再等待写锁。
    pub(crate) fn allocate_seq(
        &self,
        tx: &rusqlite::Transaction<'_>,
    ) -> Result<LedgerSeq, ShenkoError> {
        let seq: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM commit_log",
                [],
                |r| r.get(0),
            )
            .map_err(|e| ShenkoError::storage(e.to_string()))?;
        Ok(LedgerSeq(seq as u64))
    }

    /// Record one committed mutation line in commit_log inside the caller's
    /// transaction. Dedicated state mutations (e.g. media ingress) and the
    /// generic WriteBatch share this line so commit_ref means the same thing.
    pub(crate) fn record_commit(
        &self,
        tx: &rusqlite::Transaction<'_>,
        seq: LedgerSeq,
        entity_count: usize,
    ) -> Result<(), ShenkoError> {
        tx.execute(
            "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                self.ledger_id.to_string(),
                seq.0 as i64,
                self.incarnation.to_string(),
                entity_count as i64,
            ],
        )
        .map_err(|e| ShenkoError::storage(e.to_string()))?;
        Ok(())
    }

    #[instrument(skip(self, batch))]
    pub fn commit(&self, mut batch: WriteBatch) -> Result<CommitReceipt, ShenkoError> {
        let factual = batch.task_upserts.is_empty() && batch.operation_upserts.is_empty()
            && batch.attempt_upserts.is_empty();
        self.with_installation_mutation(None, !factual,
            || match self.try_commit(&mut batch) {
                // The batch is refused whole, so nothing it carried is
                // durable. The receipt it contradicted is preserved on its
                // own committed line before the caller learns of the
                // conflict; a rollback must not erase that evidence.
                Err(error) if crate::receipt_admission::is_key_conflict(&error) => {
                    self.preserve_receipt_key_conflicts(&batch)?;
                    Err(error)
                }
                other => other,
            },
            // A contradiction inside the caller's own facts is not storage
            // damage: the single writer stays usable so the caller can record
            // the correct fact instead of reopening the ledger.
            |error| !crate::receipt_admission::is_key_conflict(error)
                && !shenko_types::authority::is_authority_fenced(error)
                && !matches!(error, ShenkoError::RuntimeAdmissionClosed { .. }),
        )
    }

    /// Preserve every receipt of a refused batch that still contradicts a
    /// durable original. Runs after the batch rolled back, so the conflict
    /// fact never depends on the refused write's atomicity (SPEC X04).
    fn preserve_receipt_key_conflicts(&self, batch: &WriteBatch) -> Result<(), ShenkoError> {
        if batch.evidence_upserts.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let mut refused = Vec::new();
        for receipt in &batch.evidence_upserts {
            if let crate::receipt_admission::Admission::Conflict(conflict) =
                crate::receipt_admission::decide(&tx, receipt)
                    .map_err(|error| ShenkoError::storage(error.to_string()))?
            {
                refused.push((receipt, conflict));
            }
        }
        if refused.is_empty() {
            return Ok(());
        }
        let seq = self.allocate_seq(&tx)?;
        let mut staged = 0usize;
        for (receipt, conflict) in &refused {
            if crate::receipt_admission::stage(&tx, self, seq, receipt, conflict)?.is_some() {
                staged += 1;
            }
        }
        if staged > 0 {
            self.record_commit(&tx, seq, staged)?;
        }
        tx.commit()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        Ok(())
    }

    fn try_commit(&self, batch: &mut WriteBatch) -> Result<CommitReceipt, ShenkoError> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| ShenkoError::storage(e.to_string()))?;
        let bound: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM commit_authority_policy)", [], |row| row.get(0))
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        if bound {
            // Generic ingestion preserves immutable replay and independent
            // terminal facts. It cannot bypass the dedicated quorum-bound
            // claim/T1/T2 transitions, even when a local source is attached.
            let identical = |table: &str, column: &str, id: EntityId, value: serde_json::Value| -> Result<Option<bool>, ShenkoError> {
                let raw: Option<String> = tx.query_row(&format!("SELECT data FROM {table} WHERE {column}=?1"),
                    [id.to_string()], |row| row.get(0)).optional().map_err(crate::store::storage_error)?;
                raw.map(|raw| Ok(serde_json::from_str::<serde_json::Value>(&raw)? == value)).transpose()
            };
            for task in &batch.task_upserts {
                let existing = identical("tasks", "task_id", task.task_id, serde_json::to_value(task)?)?;
                if existing != Some(true) && !(existing.is_none() && task.is_terminal()) {
                    return Err(shenko_types::authority::authority_fenced(
                        "task control requires its dedicated ordered Store boundary"));
                }
            }
            for operation in &batch.operation_upserts {
                if matches!(operation.state, shenko_types::operation::OperationState::Prepared | shenko_types::operation::OperationState::InFlight)
                    && identical("operations", "operation_id", operation.operation_id, serde_json::to_value(operation)?)? != Some(true)
                { return Err(shenko_types::authority::authority_fenced("new effects require the original operation admission boundary")); }
            }
            for attempt in &batch.attempt_upserts {
                if attempt.dispatch_stage == shenko_types::operation::AttemptDispatchStage::Armed
                    && identical("attempts", "attempt_id", attempt.attempt_id, serde_json::to_value(attempt)?)? != Some(true)
                { return Err(shenko_types::authority::authority_fenced("armed attempts require the original operation admission boundary")); }
            }
        }
        let seq = self.allocate_seq(&tx)?;
        let commit_ref = CommitRef::new(self.ledger_id, seq);
        let mut updated = vec![];

        // A record-revision row admits only the successor of the stored
        // revision: the caller builds its update on the revision it read, so
        // anything else is a stale or foreign writer — a typed conflict,
        // never a silent overwrite (generic write path).
        for task in &batch.task_upserts {
            let json =
                serde_json::to_string(task).map_err(|e| ShenkoError::storage(e.to_string()))?;
            let stored: Option<(i64, String)> = tx
                .query_row(
                    "SELECT record_revision, data FROM tasks WHERE task_id=?1",
                    params![task.task_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| ShenkoError::storage(e.to_string()))?;
            match stored {
                None => {
                    tx.execute(
                        "INSERT INTO tasks (task_id, record_revision, data, updated_commit_seq)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            task.task_id.to_string(),
                            task.record_revision.0 as i64,
                            json,
                            seq.0 as i64,
                        ],
                    )
                    .map_err(|e| ShenkoError::storage(e.to_string()))?;
                }
                Some((_, stored_json)) if stored_json == json => {
                    // An identical replay admits nothing new; the original
                    // fact stays the truth of record.
                }
                Some((stored_revision, stored_json)) => {
                    // A terminal task is never rewritten by any writer; a
                    // live one only by the revision its writer read.
                    let stored_terminal = serde_json::from_str::<
                        shenko_types::task::TaskRecord,
                    >(&stored_json)
                    .map(|stored| stored.is_terminal())
                    .unwrap_or(false);
                    if stored_terminal {
                        return Err(ShenkoError::conflict(format!(
                            "terminal task {} is never rewritten",
                            task.task_id
                        )));
                    }
                    if task.record_revision.0 as i64 != stored_revision + 1 {
                        return Err(ShenkoError::conflict(format!(
                            "task {} revision conflict: stored {stored_revision}, incoming {}",
                            task.task_id, task.record_revision.0
                        )));
                    }
                    tx.execute(
                        "UPDATE tasks SET record_revision=?2, data=?3, updated_commit_seq=?4
                         WHERE task_id=?1 AND record_revision=?5",
                        params![
                            task.task_id.to_string(),
                            task.record_revision.0 as i64,
                            json,
                            seq.0 as i64,
                            stored_revision,
                        ],
                    )
                    .map_err(|e| ShenkoError::storage(e.to_string()))?;
                }
            }
            updated.push(UpdatedEntity {
                entity_id: task.task_id,
                record_revision: task.record_revision,
            });
        }

        for op in &batch.operation_upserts {
            let json =
                serde_json::to_string(op).map_err(|e| ShenkoError::storage(e.to_string()))?;
            let stored: Option<(i64, String)> = tx
                .query_row(
                    "SELECT record_revision, data FROM operations WHERE operation_id=?1",
                    params![op.operation_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| ShenkoError::storage(e.to_string()))?;
            match stored {
                None => {
                    tx.execute(
                        "INSERT INTO operations (operation_id, record_revision, data, updated_commit_seq)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            op.operation_id.to_string(),
                            op.record_revision.0 as i64,
                            json,
                            seq.0 as i64,
                        ],
                    )
                    .map_err(|e| ShenkoError::storage(e.to_string()))?;
                }
                Some((_, stored_json)) if stored_json == json => {}
                Some((stored_revision, stored_json)) => {
                    // An operation whose effect outcome is already recorded
                    // is terminal: never rewritten, even by a successor
                    // revision (the recorded effect is final).
                    let stored_resolved = serde_json::from_str::<
                        shenko_types::operation::OperationRecord,
                    >(&stored_json)
                    .map(|stored| {
                        stored.outcome.is_some()
                            && stored.state == shenko_types::operation::OperationState::Resolved
                    })
                    .unwrap_or(false);
                    if stored_resolved {
                        return Err(ShenkoError::conflict(format!(
                            "resolved operation {} is never rewritten",
                            op.operation_id
                        )));
                    }
                    if op.record_revision.0 as i64 != stored_revision + 1 {
                        return Err(ShenkoError::conflict(format!(
                            "operation {} revision conflict: stored {stored_revision}, incoming {}",
                            op.operation_id, op.record_revision.0
                        )));
                    }
                    tx.execute(
                        "UPDATE operations SET record_revision=?2, data=?3, updated_commit_seq=?4
                         WHERE operation_id=?1 AND record_revision=?5",
                        params![
                            op.operation_id.to_string(),
                            op.record_revision.0 as i64,
                            json,
                            seq.0 as i64,
                            stored_revision,
                        ],
                    )
                    .map_err(|e| ShenkoError::storage(e.to_string()))?;
                }
            }
        }

        for att in &batch.attempt_upserts {
            let json =
                serde_json::to_string(att).map_err(|e| ShenkoError::storage(e.to_string()))?;
            let stored: Option<String> = tx
                .query_row(
                    "SELECT data FROM attempts WHERE attempt_id=?1",
                    params![att.attempt_id.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| ShenkoError::storage(e.to_string()))?;
            match stored {
                None => {
                    // Append-only: the armed/dispatch fact keeps its original
                    // identity; a later observation is new evidence, not an
                    // edit of the attempt fact.
                    tx.execute(
                        "INSERT INTO attempts (attempt_id, operation_id, data, updated_commit_seq)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            att.attempt_id.to_string(),
                            att.operation_id.to_string(),
                            json,
                            seq.0 as i64,
                        ],
                    )
                    .map_err(|e| ShenkoError::storage(e.to_string()))?;
                }
                Some(stored_json) if stored_json == json => {}
                Some(_) => {
                    return Err(ShenkoError::conflict(format!(
                        "attempt {} already holds a different fact",
                        att.attempt_id
                    )));
                }
            }
        }

        for ev in &mut batch.evidence_upserts {
            // Receipt admission (SPEC X04): one fact per receipt
            // identity and per producer-declared source event scope. An
            // identical replay binds to the first durable receipt, never a
            // competing fact; a different original under either identity is
            // RECEIPT_KEY_CONFLICT and is preserved by `commit`, never folded
            // into this operation's evidence.
            match crate::receipt_admission::decide(&tx, ev)
                .map_err(|e| ShenkoError::storage(e.to_string()))?
            {
                crate::receipt_admission::Admission::New => {}
                crate::receipt_admission::Admission::Replay { receipt_id } => {
                    updated.push(UpdatedEntity {
                        entity_id: receipt_id,
                        record_revision: shenko_types::ids::RecordRevision(0),
                    });
                    continue;
                }
                crate::receipt_admission::Admission::Conflict(conflict) => {
                    return Err(crate::receipt_admission::key_conflict_error(ev, &conflict));
                }
            }
            // Receipts are append-only; the admission above already refused a
            // different fact under an existing identity. The admission commit
            // belongs to this Store, never to the caller. Replay continued
            // above, preserving the first receipt's coordinate unchanged.
            ev.received_commit = commit_ref;
            let json =
                serde_json::to_string(ev).map_err(|e| ShenkoError::storage(e.to_string()))?;
            tx.execute(
                "INSERT INTO evidence_receipts (receipt_id, operation_id, data, updated_commit_seq)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    ev.receipt_id.to_string(),
                    ev.operation_id.map(|id| id.to_string()),
                    json,
                    seq.0 as i64,
                ],
            )
            .map_err(|e| ShenkoError::storage(e.to_string()))?;
            // E1 closure: the owning operation's evidence cursor advances with
            // the receipt in the same commit.
            if let Some(operation_id) = ev.operation_id {
                let op_row: Option<(i64, String)> = tx
                    .query_row(
                        "SELECT record_revision, data FROM operations WHERE operation_id=?1",
                        params![operation_id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()
                    .map_err(|e| ShenkoError::storage(e.to_string()))?;
                if let Some((op_revision, op_json)) = op_row {
                    let mut operation: shenko_types::operation::OperationRecord =
                        serde_json::from_str(&op_json)
                            .map_err(|e| ShenkoError::storage(e.to_string()))?;
                    // The cursor marks the commit through which this
                    // operation's evidence is folded; the newest admission
                    // advances it monotonically. A resolved operation already
                    // holds its final fact: its evidence cursor is never
                    // rewritten, the receipt itself stays durable.
                    let resolved = operation.outcome.is_some()
                        && operation.state == shenko_types::operation::OperationState::Resolved;
                    if !resolved && operation.evidence_through != Some(commit_ref) {
                        operation.evidence_through = Some(commit_ref);
                        operation.record_revision = operation.record_revision.next();
                        let operation_json = serde_json::to_string(&operation)
                            .map_err(|e| ShenkoError::storage(e.to_string()))?;
                        tx.execute(
                            "UPDATE operations SET record_revision=?2, data=?3, updated_commit_seq=?4
                             WHERE operation_id=?1 AND record_revision=?5",
                            params![
                                operation_id.to_string(),
                                operation.record_revision.0 as i64,
                                operation_json,
                                seq.0 as i64,
                                op_revision,
                            ],
                        )
                        .map_err(|e| ShenkoError::storage(e.to_string()))?;
                    }
                }
            }
        }

        for ie in &batch.inbox_event_upserts {
            // Raw deliveries are append-only facts: an identical replay is a
            // no-op, a different fact under an existing event identity is a
            // typed conflict — never a silently ignored row.
            let insert = tx.execute(
                "INSERT INTO inbox_events (event_id, agent_id, kind, source, consumed, data)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5)",
                params![
                    ie.event_id.to_string(),
                    ie.agent_id.to_string(),
                    ie.kind,
                    ie.source,
                    ie.payload_json,
                ],
            );
            match insert {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: rusqlite::ErrorCode::ConstraintViolation,
                        ..
                    },
                    _,
                )) => {
                    let stored: Option<(String, String, String, String)> = tx
                        .query_row(
                            "SELECT agent_id, kind, source, data FROM inbox_events WHERE event_id=?1",
                            params![ie.event_id.to_string()],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                        )
                        .optional()
                        .map_err(|e| ShenkoError::storage(e.to_string()))?;
                    if stored
                        .as_ref()
                        .is_none_or(|(agent, kind, source, data)| {
                            agent != &ie.agent_id.to_string()
                                || kind != &ie.kind
                                || source != &ie.source
                                || data != &ie.payload_json
                        })
                    {
                        return Err(ShenkoError::conflict(format!(
                            "inbox event {} already holds a different fact",
                            ie.event_id
                        )));
                    }
                }
                Err(error) => return Err(ShenkoError::storage(error.to_string())),
            }
            updated.push(UpdatedEntity {
                entity_id: ie.event_id,
                record_revision: shenko_types::ids::RecordRevision(0),
            });
        }

        // Raw session truth is append-only: a replay of the same fact is a
        // no-op, a different fact under an existing id or seq is a conflict
        // that rolls the whole batch back, never a silently ignored row.
        for rs in &batch.raw_session_upserts {
            crate::raw_session::write_raw_session(&tx, rs)?;
            updated.push(UpdatedEntity {
                entity_id: rs.session_id,
                record_revision: shenko_types::ids::RecordRevision(0),
            });
        }

        for se in &batch.session_event_upserts {
            crate::raw_session::write_session_event(&tx, se)?;
            updated.push(UpdatedEntity {
                entity_id: se.event_id,
                record_revision: shenko_types::ids::RecordRevision(0),
            });
        }

        // record commit in ledger log
        self.record_commit(&tx, seq, updated.len())?;

        tx.commit()
            .map_err(|e| ShenkoError::storage(e.to_string()))?;

        Ok(CommitReceipt {
            commit_ref,
            durability_profile: DurabilityProfile::PowerLossLocal,
            store_incarnation: self.incarnation,
            updated,
            sealed_objects: Vec::new(),
        })
    }

    /// One admission line for ordinary commits, subject/memory writes and T1/E1.
    /// Failure is published before queued writers can enter. Pessimistic closure
    /// also leaves the handle failed if a writer unwinds. Never hold over I/O.
    pub(crate) fn with_mutation<T, E: From<ShenkoError>>(
        &self,
        write: impl FnOnce() -> Result<T, E>,
        storage_failure: impl FnOnce(&E) -> bool,
    ) -> Result<T, E> {
        self.with_mutation_under(None, write, storage_failure)
    }

    pub(crate) fn acquire_cas_publication(&self) -> Result<crate::roots::CasWriteLease, ShenkoError> {
        crate::roots::CasWriteLease::acquire(&self.cas_base)
            .map_err(|error| ShenkoError::storage(format!("CAS write lease: {error}")))
    }

    pub(crate) fn acquire_installation(&self, selected: bool) -> Result<Option<InstallationPermit>, ShenkoError> {
        let Some((domain, _, _)) = &self.installation else { return Ok(None); };
        let permit = domain.acquire().map_err(crate::store::storage_error)?;
        if selected { self.check_selected_installation(&permit)?; }
        Ok(Some(permit))
    }

    pub(crate) fn check_selected_installation(&self, permit: &InstallationPermit) -> Result<(), ShenkoError> {
        self.check_installation_selection(permit, true)
    }

    fn check_installation_selection(&self, permit: &InstallationPermit, runtime: bool) -> Result<(), ShenkoError> {
        let Some((domain, db, cas)) = &self.installation else { return Ok(()); };
        permit.validate(domain).map_err(crate::store::storage_error)?;
        let validate = || -> anyhow::Result<()> {
            let physical_db = db.canonicalize()?;
            let slot = physical_db.parent().ok_or_else(|| anyhow::anyhow!("database slot is absent"))?;
            let root = domain.root();
            if slot != root && slot.parent() != Some(root.join("versions").as_path()) {
                anyhow::ensure!(runtime, "availability fork requires a physical runtime slot");
                // A data-only store can share a CAS before any software slot
                // exists. It still holds the SAME physical permit; location
                // alone must never demote a copied/retired runtime to data-only.
                for name in ["shenko.db", "versions", "current-slot"] {
                    match std::fs::symlink_metadata(root.join(name)) {
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                        Ok(_) => anyhow::bail!("detached database belongs to a managed installation"),
                    }
                }
                let conn = self.conn.lock();
                let bound: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM activation_selectors)
                         OR EXISTS(SELECT 1 FROM activation_records)
                         OR EXISTS(SELECT 1 FROM subject_branch_heads)
                         OR EXISTS(SELECT 1 FROM subject_genesis)
                         OR EXISTS(SELECT 1 FROM bootstrap_records)
                         OR EXISTS(SELECT 1 FROM local_installation_provisioning)",
                    [], |row| row.get(0),
                )?;
                anyhow::ensure!(!bound, "detached database retains runtime installation bindings");
                return Ok(());
            }
            let selected = match std::fs::read(root.join("current-slot")) {
                Ok(bytes) => {
                    let pointer: serde_json::Value = serde_json::from_slice(&bytes)?;
                    anyhow::ensure!(pointer["schema_version"] == 1, "unsupported installed route pointer");
                    let path = pointer["slot_dir"].as_str().ok_or_else(|| anyhow::anyhow!("invalid installed route pointer"))?;
                    std::path::Path::new(path).canonicalize()?
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => root.to_path_buf(),
                Err(error) => return Err(error.into()),
            };
            anyhow::ensure!(selected == root || selected.parent() == Some(root.join("versions").as_path()),
                "selected writer is outside this installation");
            anyhow::ensure!(slot == selected, "writer belongs to a retired or unselected installation slot");
            if !runtime { return Ok(()); }
            let conn = self.conn.lock();
            let agents = {
                let mut statement = conn.prepare("SELECT agent_id FROM activation_selectors")?;
                statement.query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for agent in agents {
                let agent = agent.parse()?;
                crate::context_windows::verify_selected_context_slot(&conn, agent, db, root, cas)?;
                let (head, _) = crate::store::read_subject(&conn, agent)?;
                anyhow::ensure!(head.mode != shenko_types::subject::SubjectMode::Handoff,
                    "retired handoff writer cannot admit fresh work");
            }
            Ok(())
        };
        validate().map_err(|error| shenko_types::authority::authority_fenced(format!("installation writer is not current: {error:#}")))
    }

    /// Only the exact first imported-subject fork may replace its foreign
    /// bootstrap route. The caller must verify that proof in its transaction;
    /// physical selection is still required under the original source permit.
    pub(crate) fn with_imported_fork_mutation_under<T, E: From<ShenkoError>>(
        &self, permit: &InstallationPermit, write: impl FnOnce() -> Result<T, E>,
        storage_failure: impl FnOnce(&E) -> bool,
    ) -> Result<T, E> {
        if self.installation.is_none() {
            return Err(shenko_types::authority::authority_fenced(
                "imported fork requires a real installed Store").into());
        }
        self.check_installation_selection(permit, false)?;
        self.with_installation_mutation(Some(permit), false, write, storage_failure)
    }

    /// Borrow the source reservation's original physical permit. Acquiring a
    /// second descriptor here would deadlock behind our own Node transaction.
    pub(crate) fn with_mutation_under<T, E: From<ShenkoError>>(
        &self, permit: Option<&InstallationPermit>, write: impl FnOnce() -> Result<T, E>,
        storage_failure: impl FnOnce(&E) -> bool,
    ) -> Result<T, E> {
        self.with_installation_mutation(permit, true, write, storage_failure)
    }

    /// Historical receipts and same-installation migration still serialize,
    /// but selection is not a prerequisite for preserving original facts.
    pub(crate) fn with_factual_mutation<T, E: From<ShenkoError>>(
        &self, write: impl FnOnce() -> Result<T, E>, storage_failure: impl FnOnce(&E) -> bool,
    ) -> Result<T, E> {
        self.with_installation_mutation(None, false, write, storage_failure)
    }

    /// The caller retains this exact, physically bound CAS lease before its
    /// physical/import/SQL exclusions. Never acquire another lease here.
    pub(crate) fn with_maintenance_mutation_owned_under<T, E: From<ShenkoError>>(
        &self, publication: &crate::roots::CasWriteLease,
        permit: Option<&InstallationPermit>, write: impl FnOnce() -> Result<T, E>,
        storage_failure: impl FnOnce(&E) -> bool,
    ) -> Result<T, E> {
        self.with_owned_installation_mutation(publication, permit, false, write, storage_failure)
    }

    fn with_installation_mutation<T, E: From<ShenkoError>>(
        &self, borrowed: Option<&InstallationPermit>, selected: bool,
        write: impl FnOnce() -> Result<T, E>, storage_failure: impl FnOnce(&E) -> bool,
    ) -> Result<T, E> {
        let write_lease = crate::roots::CasWriteLease::acquire(&self.cas_base)
            .map_err(|error| ShenkoError::storage(format!("CAS write lease: {error}")))?;
        self.with_owned_installation_mutation(&write_lease, borrowed, selected, write, storage_failure)
    }

    pub(crate) fn with_owned_installation_mutation<T, E: From<ShenkoError>>(
        &self, publication: &crate::roots::CasWriteLease,
        borrowed: Option<&InstallationPermit>, selected: bool,
        write: impl FnOnce() -> Result<T, E>, storage_failure: impl FnOnce(&E) -> bool,
    ) -> Result<T, E> {
        publication.validate_cas(&self.cas_base)
            .map_err(|error| ShenkoError::storage(format!("CAS publication custody: {error:#}")))?;
        let owned = if borrowed.is_none() { self.acquire_installation(selected)? } else { None };
        let permit = borrowed.or(owned.as_ref());
        if let Some((domain, _, _)) = &self.installation {
            let permit = permit.ok_or_else(|| ShenkoError::storage("installed mutation requires its physical permit"))?;
            permit.validate(domain).map_err(crate::store::storage_error)?;
            if selected { self.check_selected_installation(permit)?; }
        }
        let mut open = self.mutation_open.lock();
        if !*open {
            return Err(
                ShenkoError::storage("writer closed; reopen and query original operation").into(),
            );
        }
        *open = false;
        let result = write();
        if result.as_ref().err().is_none_or(|e| !storage_failure(e)) {
            *open = true;
        }
        result
    }

    pub(crate) fn close_writer(&self) {
        *self.mutation_open.lock() = false;
    }

    pub fn writer_is_open(&self) -> bool { *self.mutation_open.lock() }

    pub fn ledger_id(&self) -> EntityId {
        self.ledger_id
    }
}
