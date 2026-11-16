//! Receipt identity conflicts retain every refused fact through real SQLite
//! admission, CAS readback and Store reopen, including legacy pair-only rows.

use anyhow::Result;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use shenko_store::{ConflictIdentity, EvidenceReceiptConflict, Store};
use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::evidence::{DecodeStatus, EvidenceReceipt};
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::ObjectRef;
use shenko_types::store::WriteBatch;
use std::path::{Path, PathBuf};

fn fixture() -> Result<(PathBuf, Store)> {
    let dir = std::env::temp_dir().join(format!("shenko-receipt-conflicts-{}", EntityId::new()));
    std::fs::create_dir_all(&dir)?;
    let store = Store::open(dir.join("store.db"), dir.join("cas"))?;
    Ok((dir, store))
}

fn raw(store: &Store, bytes: &[u8]) -> Result<ObjectRef> {
    Ok(store.cas.seal_object(bytes, "text/plain", "test", "raw", 1)?.object)
}

fn original(store: &Store) -> Result<EvidenceReceipt> {
    Ok(EvidenceReceipt {
        record_schema_version: 1,
        receipt_id: EntityId::new(),
        operation_id: Some(EntityId::new()),
        attempt_id: Some(EntityId::new()),
        source_id: "receipt-conflict-test".into(),
        source_instance_id: "instance-1".into(),
        source_event_key: Some("event-1".into()),
        producer_epoch: Some(1),
        producer_schema_version: Some("1".into()),
        raw_ref: raw(store, b"original A")?,
        decoder_ref: None,
        decode_status: DecodeStatus::Uninterpreted,
        observation_level: None,
        normalized_ref: None,
        observed_at: None,
        received_commit: CommitRef::new(store.ledger.ledger_id(), LedgerSeq(1)),
    })
}

fn admit(store: &Store, receipt: EvidenceReceipt) -> Result<CommitRef> {
    let mut batch = WriteBatch::new();
    batch.evidence_upserts.push(receipt);
    Ok(store.commit(batch)?.commit_ref)
}

fn assert_refused(store: &Store, receipt: EvidenceReceipt) {
    let mut batch = WriteBatch::new();
    batch.evidence_upserts.push(receipt);
    match store.commit(batch).unwrap_err() {
        ShenkoError::Protocol(protocol) => assert_eq!(protocol.code, "RECEIPT_KEY_CONFLICT"),
        other => panic!("expected receipt identity conflict, got {other:?}"),
    }
    assert!(store.ledger.writer_is_open());
}

// Include every stored column and rowid so replay and migration cannot hide
// a rewrite behind equivalent decoded receipts or a replacement identity.
fn snapshot(db: &Path) -> Result<(Vec<String>, i64)> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut query = conn.prepare(
        "SELECT json_array(rowid,conflict_id,receipt_id,original_receipt_id,
            operation_id,source_id,source_instance_id,source_event_key,data,
            updated_commit_seq,created_at)
         FROM evidence_receipt_conflicts ORDER BY rowid",
    )?;
    let rows = query.query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    let commits = conn.query_row("SELECT COUNT(*) FROM commit_log", [], |row| row.get(0))?;
    Ok((rows, commits))
}

#[test]
fn same_receipt_id_retains_distinct_facts_and_deduplicates_delivery_replay() -> Result<()> {
    let (dir, store) = fixture()?;
    let db = dir.join("store.db");
    let a = original(&store)?;
    let operation_id = a.operation_id.unwrap();
    assert_eq!(admit(&store, a.clone())?, a.received_commit);
    let mut b = a.clone();
    b.raw_ref = raw(&store, b"refused B")?;
    let mut c = a.clone();
    c.raw_ref = raw(&store, b"refused C")?;
    assert_refused(&store, b.clone());
    assert_refused(&store, c.clone());
    let conflicts = store.external_receipt_conflicts(operation_id)?;
    assert_eq!(conflicts.len(), 2);
    assert_ne!(conflicts[0].conflict_id, conflicts[1].conflict_id);
    for (conflict, incoming) in conflicts.iter().zip([&b, &c]) {
        assert_eq!(conflict.receipt_id, a.receipt_id);
        assert_eq!(conflict.original_receipt_id, a.receipt_id);
        assert_eq!(conflict.matched, ConflictIdentity::ReceiptId);
        let mut delivered = incoming.clone();
        delivered.received_commit = conflict.receipt.received_commit;
        assert_eq!(conflict.receipt, delivered);
    }
    let before_reopen = snapshot(&db)?;
    drop(store);

    let store = Store::open(db.clone(), dir.join("cas"))?;
    assert_eq!(snapshot(&db)?, before_reopen);
    assert_eq!(store.external_operation_evidence(operation_id)?, vec![a.clone()]);
    for (conflict, bytes) in conflicts.iter().zip([b"refused B", b"refused C"]) {
        assert_eq!(store.cas.read_object(&conflict.receipt.raw_ref.digest)?, bytes);
    }
    assert_eq!(store.cas.read_object(&a.raw_ref.digest)?, b"original A");
    b.received_commit = CommitRef::new(EntityId::new(), LedgerSeq(900));
    assert_refused(&store, b.clone());
    assert_eq!(snapshot(&db)?, before_reopen, "replay must not add a commit or rewrite B");

    // Full fact identity includes metadata even when the raw bytes agree.
    let mut changed_metadata = b.clone();
    changed_metadata.producer_epoch = Some(2);
    assert_refused(&store, changed_metadata);
    let with_metadata = store.external_receipt_conflicts(operation_id)?;
    assert_eq!(with_metadata.len(), 3);
    assert_eq!(with_metadata[0].receipt.raw_ref, with_metadata[2].receipt.raw_ref);
    assert_ne!(with_metadata[0].conflict_id, with_metadata[2].conflict_id);
    assert_eq!(with_metadata[2].receipt.producer_epoch, Some(2));

    // Reversed delivery order and a different receiving ledger cannot change
    // either fact's derived identity. Both stores use the same real CAS.
    let second = Store::open(dir.join("second.db"), dir.join("cas"))?;
    admit(&second, a.clone())?;
    c.received_commit = CommitRef::new(EntityId::new(), LedgerSeq(901));
    assert_refused(&second, c);
    assert_refused(&second, b);
    let redelivered = second.external_receipt_conflicts(operation_id)?;
    assert_eq!(redelivered.len(), 2);
    assert_eq!(redelivered[0].conflict_id, conflicts[1].conflict_id);
    assert_eq!(redelivered[1].conflict_id, conflicts[0].conflict_id);
    assert_ne!(redelivered[1].receipt.received_commit, conflicts[0].receipt.received_commit);

    let final_snapshot = snapshot(&db)?;
    drop(store);
    let reopened = Store::open(db.clone(), dir.join("cas"))?;
    assert_eq!(snapshot(&db)?, final_snapshot);
    assert_eq!(reopened.external_receipt_conflicts(operation_id)?.len(), 3);
    assert_eq!(reopened.external_operation_evidence(operation_id)?, vec![a]);
    Ok(())
}

#[test]
fn legacy_pair_index_migration_keeps_rows_and_deduplicates_normalized_replay() -> Result<()> {
    let (dir, store) = fixture()?;
    let db = dir.join("store.db");
    let a = original(&store)?;
    let operation_id = a.operation_id.unwrap();
    let accepted = admit(&store, a.clone())?;
    let incarnation = store.capabilities.incarnation;
    let mut b = a.clone();
    b.receipt_id = EntityId::new();
    b.raw_ref = raw(&store, b"legacy refused B")?;
    let mut c = b.clone();
    c.raw_ref = raw(&store, b"new refused C")?;
    drop(store);

    // Seed exactly the old pair-only identity and unique index, with its
    // original delivery/commit log in the same SQLite transaction.
    let legacy_id = EntityId::derived(
        "shenko.evidence.receipt.key_conflict",
        format!("{}:{}", b.receipt_id, a.receipt_id).as_bytes(),
    );
    let mut conn = Connection::open(&db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let seq: u64 = tx.query_row("SELECT MAX(seq)+1 FROM commit_log", [], |row| row.get(0))?;
    b.received_commit = CommitRef::new(accepted.ledger_id, LedgerSeq(seq));
    let legacy = EvidenceReceiptConflict {
        schema_version: 1,
        conflict_id: legacy_id,
        receipt_id: b.receipt_id,
        original_receipt_id: a.receipt_id,
        operation_id: b.operation_id,
        attempt_id: b.attempt_id,
        source_id: b.source_id.clone(),
        source_instance_id: b.source_instance_id.clone(),
        source_event_key: b.source_event_key.clone().unwrap(),
        matched: ConflictIdentity::SourceEventKey,
        differing_fields: vec!["raw_ref".into(), "receipt_id".into()],
        receipt: b.clone(),
    };
    tx.execute_batch(
        "DROP INDEX idx_evidence_conflicts_pair;
         CREATE UNIQUE INDEX idx_evidence_conflicts_pair
            ON evidence_receipt_conflicts(receipt_id,original_receipt_id);",
    )?;
    tx.execute(
        "INSERT INTO evidence_receipt_conflicts
         (conflict_id,receipt_id,original_receipt_id,operation_id,source_id,
          source_instance_id,source_event_key,data,updated_commit_seq)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![legacy_id.to_string(), b.receipt_id.to_string(), a.receipt_id.to_string(),
            operation_id.to_string(), b.source_id, b.source_instance_id, b.source_event_key,
            serde_json::to_string(&legacy)?, seq],
    )?;
    tx.execute(
        "INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count) VALUES(?1,?2,?3,1)",
        params![accepted.ledger_id.to_string(), seq, incarnation.to_string()],
    )?;
    tx.commit()?;
    let unique: bool = conn.query_row(
        "SELECT \"unique\" FROM pragma_index_list('evidence_receipt_conflicts')
         WHERE name='idx_evidence_conflicts_pair'", [], |row| row.get(0),
    )?;
    assert!(unique);
    drop(conn);
    let legacy_snapshot = snapshot(&db)?;

    let store = Store::open(db.clone(), dir.join("cas"))?;
    assert_eq!(snapshot(&db)?, legacy_snapshot, "migration must preserve every row byte and identity");
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let unique: bool = conn.query_row(
        "SELECT \"unique\" FROM pragma_index_list('evidence_receipt_conflicts')
         WHERE name='idx_evidence_conflicts_pair'", [], |row| row.get(0),
    )?;
    assert!(!unique, "the pair must be a nonunique lookup index");
    drop(conn);
    b.received_commit = CommitRef::new(EntityId::new(), LedgerSeq(902));
    assert_refused(&store, b.clone());
    assert_eq!(snapshot(&db)?, legacy_snapshot, "legacy replay must not get a new identity");
    assert_refused(&store, c.clone());
    let conflicts = store.external_receipt_conflicts(operation_id)?;
    assert_eq!(conflicts.len(), 2);
    assert_eq!(conflicts[0].conflict_id, legacy_id);
    assert_ne!(conflicts[1].conflict_id, legacy_id);
    assert_eq!(conflicts[1].matched, ConflictIdentity::SourceEventKey);
    assert_eq!(snapshot(&db)?.0[0], legacy_snapshot.0[0]);
    assert_eq!(store.cas.read_object(&conflicts[0].receipt.raw_ref.digest)?, b"legacy refused B");
    assert_eq!(store.cas.read_object(&conflicts[1].receipt.raw_ref.digest)?, b"new refused C");
    let before_reopen = snapshot(&db)?;
    drop(store);

    // Reopening a migrated pair with multiple rows must remain idempotent.
    let reopened = Store::open(db.clone(), dir.join("cas"))?;
    assert_eq!(snapshot(&db)?, before_reopen);
    c.received_commit = CommitRef::new(EntityId::new(), LedgerSeq(903));
    assert_refused(&reopened, b);
    assert_refused(&reopened, c);
    assert_eq!(snapshot(&db)?, before_reopen);
    assert_eq!(reopened.external_operation_evidence(operation_id)?, vec![a]);
    Ok(())
}
