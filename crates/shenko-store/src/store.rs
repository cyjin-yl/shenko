//! Main store — SPEC §41.2.1
use std::path::PathBuf;
use std::sync::Arc;
use parking_lot::Mutex;
use rusqlite::Connection;
use tracing::info;
use anyhow::Result;

use shenko_types::commit::{CommitReceipt, DurabilityProfile};
use shenko_types::ids::EntityId;
use shenko_types::store::{StoreCapabilities, WriteBatch};
use shenko_types::error::ShenkoError;

use crate::ledger::Ledger;
use crate::cas::ContentStore;

pub struct Store {
    pub ledger: Arc<Ledger>,
    pub cas: Arc<ContentStore>,
    pub capabilities: StoreCapabilities,
}

impl Store {
    pub fn open(db_path: PathBuf, cas_dir: PathBuf) -> Result<Self> {
        let conn = Connection::open(&db_path)?;
        configure_sqlite(&conn)?;

        // Run migrations
        crate::migrations::run_migrations(&conn)?;

        let sqlite_version: String = conn.query_row("SELECT sqlite_version()", [], |r| r.get(0))?;
        let journal_mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        let synchronous: i64 = conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?;

        // FTS5 detection
        let fts5: bool = conn.query_row(
            "SELECT count(*) FROM pragma_compile_options WHERE compile_options='ENABLE_FTS5'",
            [], |r| r.get::<_, i64>(0)
        ).unwrap_or(0) > 0;

        let incarnation = EntityId::new();
        let capabilities = StoreCapabilities {
            sqlite_version,
            wal_mode: journal_mode.to_lowercase() == "wal",
            fts5_available: fts5,
            synchronous: synchronous.to_string(),
            journal_mode,
            incarnation,
            durability_profile: DurabilityProfile::PowerLossLocal,
        };

        info!(
            fts5 = capabilities.fts5_available,
            wal = capabilities.wal_mode,
            sqlite = %capabilities.sqlite_version,
            "store opened"
        );

        let conn = Arc::new(Mutex::new(conn));
        let ledger = Arc::new(Ledger::new(Arc::clone(&conn), incarnation));
        let cas = Arc::new(ContentStore::new(cas_dir));

        Ok(Self { ledger, cas, capabilities })
    }

    pub fn commit(&self, batch: WriteBatch) -> Result<CommitReceipt, ShenkoError> {
        self.ledger.commit(batch)
    }

    pub fn capabilities(&self) -> &StoreCapabilities {
        &self.capabilities
    }

    /// Initialize genesis + branch head + first step atomically. SPEC §44.6
    pub fn initialize_genesis(
        &self,
        genesis: &shenko_types::subject::SubjectGenesis,
        head: &shenko_types::subject::SubjectBranchHead,
        first_step: &shenko_types::subject::SubjectStep,
    ) -> Result<CommitReceipt, ShenkoError> {
        use rusqlite::params;
        use shenko_types::commit::{CommitRef, UpdatedEntity};

        let genesis_json = serde_json::to_string(genesis).map_err(|e| ShenkoError::storage(e.to_string()))?;
        let head_json = serde_json::to_string(head).map_err(|e| ShenkoError::storage(e.to_string()))?;
        let step_json = serde_json::to_string(first_step).map_err(|e| ShenkoError::storage(e.to_string()))?;

        let seq = {
            let mut s = self.ledger.next_seq.lock();
            let cur = *s;
            *s = s.next();
            cur
        };
        let commit_ref = CommitRef::new(self.ledger.ledger_id(), seq);

        let conn = self.ledger.conn.lock();
        let tx = conn.unchecked_transaction().map_err(|e| ShenkoError::storage(e.to_string()))?;

        tx.execute(
            "INSERT OR IGNORE INTO subject_genesis (genesis_id, subject_id, bootstrap_id, data, created_commit_seq) VALUES (?1,?2,?3,?4,?5)",
            params![genesis.genesis_id.to_string(), genesis.subject_id.to_string(), genesis.bootstrap_id.to_string(), genesis_json, seq.0 as i64],
        ).map_err(|e| ShenkoError::storage(e.to_string()))?;

        tx.execute(
            "INSERT OR REPLACE INTO subject_branch_heads (agent_id, branch_id, committed_step_no, committed_step_id, next_step_id, mode, record_revision, data, updated_commit_seq) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![head.agent_id.to_string(), head.branch_id.0.to_string(), head.committed_step_no as i64, head.committed_step_id.map(|id| id.to_string()), head.next_step_id.to_string(), "active", head.record_revision.0 as i64, head_json, seq.0 as i64],
        ).map_err(|e| ShenkoError::storage(e.to_string()))?;

        tx.execute(
            "INSERT OR IGNORE INTO subject_steps (step_id, branch_id, step_no, state, record_revision, data, updated_commit_seq) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![first_step.step_id.to_string(), first_step.branch_id.0.to_string(), first_step.step_no as i64, "planned", first_step.record_revision.0 as i64, step_json, seq.0 as i64],
        ).map_err(|e| ShenkoError::storage(e.to_string()))?;

        tx.execute(
            "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count) VALUES (?1,?2,?3,3)",
            params![self.ledger.ledger_id().to_string(), seq.0 as i64, self.capabilities.incarnation.to_string()],
        ).map_err(|e| ShenkoError::storage(e.to_string()))?;

        tx.commit().map_err(|e| ShenkoError::storage(e.to_string()))?;

        Ok(CommitReceipt {
            commit_ref,
            durability_profile: DurabilityProfile::PowerLossLocal,
            store_incarnation: self.capabilities.incarnation,
            updated: vec![
                UpdatedEntity { entity_id: genesis.genesis_id, record_revision: shenko_types::ids::RecordRevision(0) },
                UpdatedEntity { entity_id: head.agent_id, record_revision: head.record_revision },
                UpdatedEntity { entity_id: first_step.step_id, record_revision: first_step.record_revision },
            ],
        })
    }

    /// Persist a subject step state transition (S0 → S1). SPEC §41.5
    pub fn upsert_step(
        &self,
        step: &shenko_types::subject::SubjectStep,
    ) -> Result<CommitReceipt, ShenkoError> {
        use rusqlite::params;
        use shenko_types::commit::{CommitRef, UpdatedEntity};

        let json = serde_json::to_string(step).map_err(|e| ShenkoError::storage(e.to_string()))?;
        let state_str = format!("{:?}", step.state).to_lowercase();

        let seq = {
            let mut s = self.ledger.next_seq.lock();
            let cur = *s;
            *s = s.next();
            cur
        };

        let conn = self.ledger.conn.lock();
        let tx = conn.unchecked_transaction().map_err(|e| ShenkoError::storage(e.to_string()))?;

        tx.execute(
            "INSERT OR REPLACE INTO subject_steps (step_id, branch_id, step_no, state, record_revision, data, updated_commit_seq) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![step.step_id.to_string(), step.branch_id.0.to_string(), step.step_no as i64, state_str, step.record_revision.0 as i64, json, seq.0 as i64],
        ).map_err(|e| ShenkoError::storage(e.to_string()))?;

        tx.execute(
            "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count) VALUES (?1,?2,?3,1)",
            params![self.ledger.ledger_id().to_string(), seq.0 as i64, self.capabilities.incarnation.to_string()],
        ).map_err(|e| ShenkoError::storage(e.to_string()))?;

        tx.commit().map_err(|e| ShenkoError::storage(e.to_string()))?;

        Ok(CommitReceipt {
            commit_ref: CommitRef::new(self.ledger.ledger_id(), seq),
            durability_profile: DurabilityProfile::PowerLossLocal,
            store_incarnation: self.capabilities.incarnation,
            updated: vec![UpdatedEntity { entity_id: step.step_id, record_revision: step.record_revision }],
        })
    }

    /// Atomically commit a step AND advance the branch head in one SQL transaction.
    /// SPEC §41.2.1 (single mutation line), §44.6 (head tracks committed step + next step).
    pub fn commit_step_and_advance_head(
        &self,
        step: &shenko_types::subject::SubjectStep,
        head_agent_id: shenko_types::ids::EntityId,
        head_branch_id: shenko_types::ids::BranchId,
        next_step_id: shenko_types::ids::EntityId,
        head_record_revision: shenko_types::ids::RecordRevision,
    ) -> Result<CommitReceipt, ShenkoError> {
        use rusqlite::params;
        use shenko_types::commit::{CommitRef, UpdatedEntity};

        let step_json = serde_json::to_string(step).map_err(|e| ShenkoError::storage(e.to_string()))?;
        let state_str = format!("{:?}", step.state).to_lowercase();

        let seq = {
            let mut s = self.ledger.next_seq.lock();
            let cur = *s;
            *s = s.next();
            cur
        };
        let commit_ref = CommitRef::new(self.ledger.ledger_id(), seq);

        let conn = self.ledger.conn.lock();
        let tx = conn.unchecked_transaction().map_err(|e| ShenkoError::storage(e.to_string()))?;

        // Upsert the step
        tx.execute(
            "INSERT OR REPLACE INTO subject_steps (step_id, branch_id, step_no, state, record_revision, data, updated_commit_seq) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![step.step_id.to_string(), step.branch_id.0.to_string(), step.step_no as i64, state_str, step.record_revision.0 as i64, step_json, seq.0 as i64],
        ).map_err(|e| ShenkoError::storage(e.to_string()))?;

        // Advance the branch head: committed_step_id/no → this step, next_step_id → successor
        let new_rev = shenko_types::ids::RecordRevision(head_record_revision.0 + 1);
        // Read the existing head to preserve immutable fields (genesis_ref, root_task_id, etc.)
        let existing_head_json: Option<String> = {
            let c = &*conn;
            c.query_row(
                "SELECT data FROM subject_branch_heads WHERE agent_id=?1 LIMIT 1",
                [head_agent_id.to_string()],
                |r| r.get(0),
            ).ok()
        };
        // Parse existing head or build a minimal placeholder preserving required fields
        let head_row: shenko_types::subject::SubjectBranchHead = if let Some(json) = existing_head_json {
            let mut h: shenko_types::subject::SubjectBranchHead =
                serde_json::from_str(&json).map_err(|e| ShenkoError::storage(e.to_string()))?;
            h.committed_step_no = step.step_no;
            h.committed_step_id = Some(step.step_id);
            h.committed_at = commit_ref;
            h.next_step_id = next_step_id;
            h.record_revision = new_rev;
            h
        } else {
            return Err(ShenkoError::storage("branch head not found; call initialize_genesis first"));
        };
        let head_json = serde_json::to_string(&head_row).map_err(|e| ShenkoError::storage(e.to_string()))?;
        tx.execute(
            "INSERT OR REPLACE INTO subject_branch_heads (agent_id, branch_id, committed_step_no, committed_step_id, next_step_id, mode, record_revision, data, updated_commit_seq) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                head_agent_id.to_string(),
                head_branch_id.0.to_string(),
                step.step_no as i64,
                step.step_id.to_string(),
                next_step_id.to_string(),
                "active",
                new_rev.0 as i64,
                head_json,
                seq.0 as i64
            ],
        ).map_err(|e| ShenkoError::storage(e.to_string()))?;

        tx.execute(
            "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count) VALUES (?1,?2,?3,2)",
            params![self.ledger.ledger_id().to_string(), seq.0 as i64, self.capabilities.incarnation.to_string()],
        ).map_err(|e| ShenkoError::storage(e.to_string()))?;

        tx.commit().map_err(|e| ShenkoError::storage(e.to_string()))?;

        Ok(CommitReceipt {
            commit_ref,
            durability_profile: DurabilityProfile::PowerLossLocal,
            store_incarnation: self.capabilities.incarnation,
            updated: vec![
                UpdatedEntity { entity_id: step.step_id, record_revision: step.record_revision },
                UpdatedEntity { entity_id: head_agent_id, record_revision: new_rev },
            ],
        })
    }

    /// Retrieve an existing agent_id from genesis, or None on first run. SPEC §44.6
    pub fn get_existing_agent_id(&self) -> Result<Option<shenko_types::ids::EntityId>, shenko_types::error::ShenkoError> {
        let conn = self.ledger.conn.lock();
        let result: Option<String> = conn.query_row(
            "SELECT subject_id FROM subject_genesis LIMIT 1", [], |r| r.get(0)
        ).ok();
        drop(conn);
        match result {
            Some(s) => s.parse().map(Some).map_err(|_| shenko_types::error::ShenkoError::storage("invalid stored agent_id")),
            None => Ok(None),
        }
    }
}

/// Apply required SQLite settings. SPEC §41.2.1
pub fn configure_sqlite(conn: &Connection) -> Result<()> {
    conn.execute_batch("
        PRAGMA journal_mode=WAL;
        PRAGMA synchronous=FULL;
        PRAGMA foreign_keys=ON;
        PRAGMA busy_timeout=5000;
    ")?;
    Ok(())
}
