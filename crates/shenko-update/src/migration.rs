//! Same-reader state migration through Store's declared recovery archive.
//!
//! The Store owns mandatory-fact classes, reference decoding, tombstones and
//! atomic task/effect conflict units. SQLite table names are not a migration
//! contract; unknown formats require an actual versioned migrator/reader.
use std::{io::{Seek, SeekFrom}, path::Path};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use shenko_store::{Store, cognition::{CognitionImportOptions, CognitionSnapshotOptions}};
use shenko_types::{commit::CommitRef, ids::{EntityId, LedgerSeq}};

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("typed state migration unavailable: {0:#}")]
    Contract(#[from] anyhow::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableImport {
    pub table: String,
    pub rows_compared: u64,
    pub rows_written: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReport {
    pub tables: Vec<TableImport>,
    pub commits_imported: u64,
    pub watermark: Option<CommitRef>,
    pub blocked_units: Vec<String>,
}
impl ImportReport {
    pub fn rows_written(&self) -> u64 { self.tables.iter().map(|table| table.rows_written).sum() }
}

/// Import a verified Store archive, preserving its typed source identities.
/// The archive remains beside the destination as retained recovery evidence.
/// Local-only lease/owner rows stay local; the initial consistent A/B copy
/// retains them, and current owners explicitly hand resources over later.
/// Conflicts are retained by Store, never overwritten as a generic row merge.
pub fn import_delta(source_db: &Path, target_db: &Path, cas_dir: &Path) -> Result<ImportReport, MigrationError> {
    let source = Store::open(source_db.to_owned(), cas_dir.to_owned()).map_err(anyhow::Error::from)?;
    let target = Store::open(target_db.to_owned(), cas_dir.to_owned()).map_err(anyhow::Error::from)?;
    let source_root = source_db.parent().ok_or_else(|| anyhow::anyhow!("source directory missing"))?;
    let target_root = target_db.parent().ok_or_else(|| anyhow::anyhow!("target directory missing"))?;
    let source_memory = source_root.join("memory");
    let target_memory = target_root.join("memory");
    let export = source.export_cognition(&CognitionSnapshotOptions {
        memory_root: source_memory.is_dir().then_some(source_memory),
        workspace_roots: Vec::new(), include_git_history: true,
        // This is the same-installation handover, not a peer replication:
        // the final catch-up legitimately runs while the subject is
        // quiesced, and `copy_paused_activation_boundary` below installs
        // the complete paused boundary at the destination before the
        // archive is applied.
        local_boundary_preserved: true,
    })?;
    let archive_path = target_root.join(format!("activation-import-{}.shka", EntityId::new()));
    let mut archive = std::fs::OpenOptions::new().create_new(true).write(true).read(true).open(archive_path)?;
    let receipt = export.write_archive(&mut archive)?;
    archive.sync_all()?;
    if !receipt.complete {
        return Ok(ImportReport { tables: Vec::new(), commits_imported: 0,
            watermark: Some(CommitRef::new(source.ledger.ledger_id(), receipt.high_water)),
            blocked_units: receipt.gaps.into_iter().filter(|gap| gap.blocking)
                .map(|gap| format!("{}: {}", gap.kind, gap.detail)).collect() });
    }
    if let Some(agent) = source.get_existing_agent_id().map_err(anyhow::Error::from)? {
        target.copy_paused_activation_boundary(&source, agent).map_err(anyhow::Error::from)?;
    }
    archive.seek(SeekFrom::Start(0))?;
    let outcome = target.import_cognition_archive(archive, &CognitionImportOptions {
        memory_root: target_memory.is_dir().then_some(target_memory), workspace_root: None,
        // Stable source identity binds Store's baselines and exact repeated
        // imports; retries never create a fresh replication relationship.
        peer_id: Some(source.ledger.ledger_id()),
        // The declared same-installation handover is the only import that may
        // adopt local control facts (the scope's pending/answered rollover
        // request). A peer import cannot.
        local_control_handover: true,
    })?;
    let mut blocked_units = outcome.gaps.iter().filter(|gap| gap.blocking)
        .map(|gap| format!("{}: {}", gap.kind, gap.detail)).collect::<Vec<_>>();
    blocked_units.extend(outcome.conflicts.iter().map(|conflict| format!("typed recovery conflict: {conflict:?}")));
    Ok(ImportReport {
        tables: vec![TableImport { table: "store-declared-recovery-closure".into(),
            rows_compared: (outcome.inserted + outcome.updated + outcome.unchanged) as u64,
            rows_written: (outcome.inserted + outcome.updated) as u64 }],
        // Source commits remain source references; importing them as local
        // sequence numbers would alias independent A/B ledger commits.
        commits_imported: 0,
        watermark: Some(CommitRef::new(source.ledger.ledger_id(), outcome.high_water)),
        blocked_units,
    })
}

pub fn read_watermark(conn: &Connection) -> Result<Option<CommitRef>, MigrationError> {
    let ledger: Option<String> = conn.query_row("SELECT ledger_id FROM ledger_identity WHERE singleton=1", [], |row| row.get(0)).optional()?;
    let Some(ledger) = ledger else { return Ok(None); };
    let seq: i64 = conn.query_row("SELECT COALESCE(MAX(seq),0) FROM commit_log WHERE ledger_id=?1", [&ledger], |row| row.get(0))?;
    let ledger = ledger.parse::<EntityId>().map_err(|error| anyhow::anyhow!("invalid source ledger identity: {error}"))?;
    Ok(Some(CommitRef::new(ledger, LedgerSeq(seq as u64))))
}
pub fn watermark_of(db_path: &Path) -> Result<Option<CommitRef>, MigrationError> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    read_watermark(&conn)
}
