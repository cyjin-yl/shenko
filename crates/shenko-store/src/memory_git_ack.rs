//! Installation-local acknowledgements of actual Git projections.
//! No row here is replicated or treated as a memory/content revision.
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use shenko_types::{error::ShenkoError, ids::EntityId};
use crate::{MemoryEntry, Store};

pub(crate) fn ensure_memory_git_ack_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS memory_git_ack (
        entry_id TEXT NOT NULL,
        projected_revision INTEGER NOT NULL,
        state TEXT NOT NULL CHECK(state IN ('projected','failed')),
        git_commit TEXT,
        git_path TEXT,
        attempts INTEGER NOT NULL DEFAULT 0,
        last_attempt_ms INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(entry_id,projected_revision)
    );")?;
    Ok(())
}
fn db(error: rusqlite::Error) -> ShenkoError { ShenkoError::storage(error.to_string()) }

impl Store {
    /// Keyset pages over eligible work. A scan attempts each entry at most once;
    /// changed revisions remain pending for the next scan. Foreign Git markers
    /// never acknowledge a projection in this installation.
    pub fn pending_memory_git_page(
        &self, agent: EntityId, after: Option<EntityId>, limit: usize,
    ) -> Result<Vec<MemoryEntry>, ShenkoError> {
        let ids = {
            let conn = self.ledger.conn.lock();
            let mut stmt = conn.prepare(
                "SELECT m.entry_id FROM memory_entries m LEFT JOIN memory_git_ack p
                 ON p.entry_id=m.entry_id AND p.projected_revision=m.record_revision
                 WHERE m.agent_id=?1 AND (?2 IS NULL OR m.entry_id>?2)
                   AND (p.entry_id IS NULL OR (p.state='failed'
                        AND p.last_attempt_ms + MIN(p.attempts,60)*1000<=?3))
                 ORDER BY m.entry_id LIMIT ?4") .map_err(db)?;
            let rows=stmt.query_map(params![agent.to_string(),after.map(|id|id.to_string()),
                Utc::now().timestamp_millis(),limit.clamp(1,4096) as i64],
                |row| row.get::<_,String>(0)).map_err(db)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db)?
        };
        ids.into_iter().map(|id| {
            let id = id.parse().map_err(|_| ShenkoError::storage("invalid memory entry id"))?;
            self.memory_entry(id)?.ok_or_else(|| ShenkoError::not_found("memory entry"))
        }).collect()
    }

    /// Only acknowledge the revision whose bytes were actually committed.
    /// A concurrent edit returns false and leaves the current revision pending.
    pub fn mark_memory_git(&self, entry: EntityId, expected_revision: u64,
        commit: &str, path: &str) -> Result<bool, ShenkoError> {
        if commit.is_empty() || path.is_empty() {
            return Err(ShenkoError::conflict("Git projection requires commit and path"));
        }
        let revision = i64::try_from(expected_revision)
            .map_err(|_| ShenkoError::conflict("memory revision exceeds SQLite range"))?;
        self.ledger.with_mutation(|| {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(db)?;
            let current: Option<i64> = tx.query_row(
                "SELECT record_revision FROM memory_entries WHERE entry_id=?1",
                [entry.to_string()], |row|row.get(0)).optional().map_err(db)?;
            let current = current.ok_or_else(||ShenkoError::not_found("memory entry"))?;
            if current != revision { return Ok(false); }
            // Existing descriptive Git fields remain readable, but only this
            // installation-local table authorizes completion. No new entry or
            // archive field and no semantic memory revision is introduced.
            tx.execute("UPDATE memory_entries SET git_commit=?2,git_path=?3
                WHERE entry_id=?1 AND record_revision=?4",
                params![entry.to_string(),commit,path,revision]).map_err(db)?;
            tx.execute("INSERT INTO memory_git_ack
                (entry_id,projected_revision,state,git_commit,git_path,attempts,last_attempt_ms)
                VALUES(?1,?2,'projected',?3,?4,0,?5)
                ON CONFLICT(entry_id,projected_revision) DO UPDATE SET state='projected',
                git_commit=excluded.git_commit,git_path=excluded.git_path,attempts=0,
                last_attempt_ms=excluded.last_attempt_ms",
                params![entry.to_string(),revision,commit,path,Utc::now().timestamp_millis()]).map_err(db)?;
            tx.commit().map_err(db)?;
            Ok(true)
        }, |error: &ShenkoError|matches!(error,ShenkoError::Storage(_)|ShenkoError::Io(_)|ShenkoError::Serde(_)))
    }

    /// Failure is local bookkeeping, never a successful acknowledgement.
    pub fn record_memory_git_failure(&self, entry: EntityId, revision: u64) -> Result<(), ShenkoError> {
        let revision=i64::try_from(revision).map_err(|_|ShenkoError::conflict("memory revision exceeds SQLite range"))?;
        self.ledger.with_mutation(|| {
            self.ledger.conn.lock().execute("INSERT INTO memory_git_ack
                (entry_id,projected_revision,state,attempts,last_attempt_ms)
                VALUES(?1,?2,'failed',1,?3)
                ON CONFLICT(entry_id,projected_revision) DO UPDATE SET
                attempts=MIN(memory_git_ack.attempts+1,60),last_attempt_ms=excluded.last_attempt_ms
                WHERE memory_git_ack.state='failed'",
                params![entry.to_string(),revision,Utc::now().timestamp_millis()]).map_err(db)?;
            Ok(())
        }, |error: &ShenkoError|matches!(error,ShenkoError::Storage(_)|ShenkoError::Io(_)|ShenkoError::Serde(_)))
    }

}
