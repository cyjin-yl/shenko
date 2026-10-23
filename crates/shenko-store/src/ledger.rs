//! Ledger — single mutation line per local store. SPEC §41.2.1
use std::sync::Arc;
use parking_lot::Mutex;
use rusqlite::{Connection, params};
use anyhow::Result;
use tracing::instrument;

use shenko_types::commit::{CommitReceipt, CommitRef, DurabilityProfile, UpdatedEntity};
use shenko_types::ids::{EntityId, LedgerSeq, RecordRevision};
use shenko_types::store::WriteBatch;
use shenko_types::error::ShenkoError;

pub struct Ledger {
    conn: Arc<Mutex<Connection>>,
    incarnation: EntityId,
    ledger_id: EntityId,
    next_seq: Mutex<LedgerSeq>,
}

impl Ledger {
    pub fn new(conn: Arc<Mutex<Connection>>, incarnation: EntityId) -> Self {
        let ledger_id = EntityId::new();
        Self {
            conn,
            incarnation,
            ledger_id,
            next_seq: Mutex::new(LedgerSeq(1)),
        }
    }

    #[instrument(skip(self, batch))]
    pub fn commit(&self, batch: WriteBatch) -> Result<CommitReceipt, ShenkoError> {
        if batch.is_empty() {
            let seq = {
                let mut seq = self.next_seq.lock();
                let cur = *seq;
                *seq = seq.next();
                cur
            };
            return Ok(CommitReceipt {
                commit_ref: CommitRef::new(self.ledger_id, seq),
                durability_profile: DurabilityProfile::PowerLossLocal,
                store_incarnation: self.incarnation,
                updated: vec![],
            });
        }

        let seq = {
            let mut seq = self.next_seq.lock();
            let cur = *seq;
            *seq = seq.next();
            cur
        };

        let commit_ref = CommitRef::new(self.ledger_id, seq);
        let mut updated = vec![];

        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()
            .map_err(|e| ShenkoError::storage(e.to_string()))?;

        for task in &batch.task_upserts {
            let json = serde_json::to_string(task)
                .map_err(|e| ShenkoError::storage(e.to_string()))?;
            tx.execute(
                "INSERT OR REPLACE INTO tasks (task_id, record_revision, data, updated_commit_seq)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    task.task_id.to_string(),
                    task.record_revision.0 as i64,
                    json,
                    seq.0 as i64,
                ],
            ).map_err(|e| ShenkoError::storage(e.to_string()))?;
            updated.push(UpdatedEntity {
                entity_id: task.task_id,
                record_revision: task.record_revision,
            });
        }

        for op in &batch.operation_upserts {
            let json = serde_json::to_string(op)
                .map_err(|e| ShenkoError::storage(e.to_string()))?;
            tx.execute(
                "INSERT OR REPLACE INTO operations (operation_id, record_revision, data, updated_commit_seq)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    op.operation_id.to_string(),
                    op.record_revision.0 as i64,
                    json,
                    seq.0 as i64,
                ],
            ).map_err(|e| ShenkoError::storage(e.to_string()))?;
        }

        for att in &batch.attempt_upserts {
            let json = serde_json::to_string(att)
                .map_err(|e| ShenkoError::storage(e.to_string()))?;
            tx.execute(
                "INSERT OR REPLACE INTO attempts (attempt_id, operation_id, data, updated_commit_seq)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    att.attempt_id.to_string(),
                    att.operation_id.to_string(),
                    json,
                    seq.0 as i64,
                ],
            ).map_err(|e| ShenkoError::storage(e.to_string()))?;
        }

        for ev in &batch.evidence_upserts {
            let json = serde_json::to_string(ev)
                .map_err(|e| ShenkoError::storage(e.to_string()))?;
            tx.execute(
                "INSERT OR REPLACE INTO evidence_receipts (receipt_id, operation_id, data, updated_commit_seq)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    ev.receipt_id.to_string(),
                    ev.operation_id.map(|id| id.to_string()),
                    json,
                    seq.0 as i64,
                ],
            ).map_err(|e| ShenkoError::storage(e.to_string()))?;
        }

        // record commit in ledger log
        tx.execute(
            "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                self.ledger_id.to_string(),
                seq.0 as i64,
                self.incarnation.to_string(),
                updated.len() as i64,
            ],
        ).map_err(|e| ShenkoError::storage(e.to_string()))?;

        tx.commit().map_err(|e| ShenkoError::storage(e.to_string()))?;

        Ok(CommitReceipt {
            commit_ref,
            durability_profile: DurabilityProfile::PowerLossLocal,
            store_incarnation: self.incarnation,
            updated,
        })
    }

    pub fn ledger_id(&self) -> EntityId {
        self.ledger_id
    }
}
