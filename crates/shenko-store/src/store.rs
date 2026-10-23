//! Main store — SPEC §41.2.1
use std::path::PathBuf;
use std::sync::Arc;
use parking_lot::Mutex;
use rusqlite::{Connection, params};
use tracing::{info, error};
use anyhow::Result;
use serde_json;

use shenko_types::commit::{CommitReceipt, CommitRef, DurabilityProfile};
use shenko_types::ids::{EntityId, LedgerSeq, RecordRevision};
use shenko_types::store::{StoreCapabilities, WriteBatch, WatchEvent, ReadWatermark};
use shenko_types::error::ShenkoError;
use shenko_types::task::TaskRecord;

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
