//! Receipt admission: one fact per identity and per stable source event.
//! SPEC X04, §41.7.3.
//!
//! A receipt is admitted at most once. Its identity is the receipt id the
//! producer assigned, and — when the producer declares one — the stable source
//! event scope `(source_id, source_instance_id, source_event_key)`.
//!
//! * An identical fact under the same identity is a **replay**: it binds to
//!   the first durable receipt, writes nothing, and must not leave the single
//!   local writer closed. A replay is not evidence of a second external event.
//! * A *different* original under the same identity is
//!   `RECEIPT_KEY_CONFLICT`: the first receipt and the operation projection
//!   stay exactly as they were, and the refused original is preserved as its
//!   own durable fact. It is never a last-write-wins replacement, and it never
//!   authorizes sending the operation again.
//!
//! Only the original bytes decide whether two reports are one external event:
//! equal content hashes of two producer-generated payloads do not prove that
//! two events were one.
//!
//! Everything here runs inside the caller's IMMEDIATE write transaction, so
//! the lookup, the refusal and the conflict fact cannot interleave with
//! another writer on this ledger.

use anyhow::Result as AnyResult;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use shenko_types::commit::CommitRef;
use shenko_types::error::{ErrorScope, ProtocolError, RetryStrategy, ShenkoError};
use shenko_types::evidence::EvidenceReceipt;
use shenko_types::ids::{EntityId, LedgerSeq};
use tracing::info;

use crate::ledger::Ledger;
use crate::store::Store;

/// Additive schema: the preserved conflict facts, plus the lookup index the
/// source-scope admission rule reads.
///
/// The index is deliberately NOT unique: stores written before this rule can
/// already hold several receipts for one scope/key, and a migration must not
/// delete or re-key a durable receipt to make an index build. Those rows stay
/// readable; every new admission is decided by [`decide`] inside the write
/// transaction.
pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS evidence_receipt_conflicts (
    conflict_id TEXT PRIMARY KEY,
    receipt_id TEXT NOT NULL,
    original_receipt_id TEXT NOT NULL,
    operation_id TEXT,
    source_id TEXT NOT NULL,
    source_instance_id TEXT NOT NULL,
    source_event_key TEXT NOT NULL,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_evidence_conflicts_pair
    ON evidence_receipt_conflicts(receipt_id, original_receipt_id);
CREATE INDEX IF NOT EXISTS idx_evidence_conflicts_operation
    ON evidence_receipt_conflicts(operation_id);
CREATE INDEX IF NOT EXISTS idx_evidence_source_scope ON evidence_receipts(
    json_extract(data,'$.source_id'),
    json_extract(data,'$.source_instance_id'),
    json_extract(data,'$.source_event_key'));
"#;

/// The protocol code of a refused receipt (SPEC §41.3, §41.7.3).
pub(crate) const RECEIPT_KEY_CONFLICT: &str = "RECEIPT_KEY_CONFLICT";

/// Which identity an incoming receipt collided with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictIdentity {
    /// The same receipt id already holds a different fact. Receipts are
    /// append-only, so that identity can never be reused.
    ReceiptId,
    /// The same source scope and stable event key already bound a different
    /// original.
    SourceEventKey,
}

/// One refused receipt, preserved as its own durable fact.
///
/// It is not the operation's evidence: it never appears in
/// [`Store::external_operation_evidence`], never rewrites the first receipt
/// and never changes the operation projection. It exists so that a divergent
/// original is readable instead of silently suppressed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceReceiptConflict {
    pub schema_version: u32,
    pub conflict_id: EntityId,
    pub receipt_id: EntityId,
    pub original_receipt_id: EntityId,
    pub operation_id: Option<EntityId>,
    pub attempt_id: Option<EntityId>,
    pub source_id: String,
    pub source_instance_id: String,
    pub source_event_key: String,
    pub matched: ConflictIdentity,
    /// The names of the fields that differ from the durable original. Values
    /// are not duplicated here; the receipt itself carries them.
    pub differing_fields: Vec<String>,
    /// The refused receipt exactly as its producer supplied it, with the
    /// commit that made this conflict fact durable. Its `raw_ref` is the
    /// original bytes the producer sealed, so the divergent original stays
    /// readable through CAS.
    pub receipt: EvidenceReceipt,
}

/// The durable fact an incoming receipt contradicted.
#[derive(Debug, Clone)]
pub(crate) struct KeyConflict {
    pub matched: ConflictIdentity,
    pub original: EvidenceReceipt,
    pub differing_fields: Vec<String>,
}

/// What an incoming receipt means for this ledger.
#[derive(Debug, Clone)]
pub(crate) enum Admission {
    /// No receipt holds this identity or this source event scope.
    New,
    /// The identical fact is already durable under `receipt_id`.
    Replay { receipt_id: EntityId },
    /// The identity is taken by a different fact.
    Conflict(KeyConflict),
}

/// Decide one receipt against the durable facts of this ledger.
pub(crate) fn decide(tx: &Transaction<'_>, incoming: &EvidenceReceipt) -> AnyResult<Admission> {
    if let Some(data) = stored_receipt_by_id(tx, incoming.receipt_id)? {
        let original: EvidenceReceipt = serde_json::from_str(&data)?;
        if same_fact(&original, incoming)? {
            return Ok(Admission::Replay {
                receipt_id: original.receipt_id,
            });
        }
        let differing_fields = differing_fields(&original, incoming)?;
        return Ok(Admission::Conflict(KeyConflict {
            matched: ConflictIdentity::ReceiptId,
            original,
            differing_fields,
        }));
    }
    // Without a producer-declared stable event key there is no scope to
    // deduplicate: the receipt id is the only identity it has.
    let Some(key) = incoming
        .source_event_key
        .as_deref()
        .filter(|key| !key.is_empty())
    else {
        return Ok(Admission::New);
    };
    let Some(data) = stored_receipt_by_source_scope(tx, incoming, key)? else {
        return Ok(Admission::New);
    };
    let original: EvidenceReceipt = serde_json::from_str(&data)?;
    if same_original(&original, incoming) {
        // The same external event, reported again. It binds to the first
        // durable receipt; observation metadata of the later report is not a
        // second fact and never becomes one.
        info!(
            receipt = %incoming.receipt_id,
            bound_receipt = %original.receipt_id,
            source = incoming.source_id.as_str(),
            source_instance = incoming.source_instance_id.as_str(),
            source_event_key = key,
            "receipt replay bound to its first durable original"
        );
        return Ok(Admission::Replay {
            receipt_id: original.receipt_id,
        });
    }
    let differing_fields = differing_fields(&original, incoming)?;
    Ok(Admission::Conflict(KeyConflict {
        matched: ConflictIdentity::SourceEventKey,
        original,
        differing_fields,
    }))
}

/// The typed refusal a caller receives for a divergent original.
pub(crate) fn key_conflict_error(incoming: &EvidenceReceipt, conflict: &KeyConflict) -> ShenkoError {
    let mut error = ProtocolError::new(
        RECEIPT_KEY_CONFLICT,
        ErrorScope::Call,
        RetryStrategy::NeverAutomatically,
    )
    .with_details(serde_json::json!({
        "receipt_id": incoming.receipt_id.to_string(),
        "original_receipt_id": conflict.original.receipt_id.to_string(),
        "matched": conflict.matched,
        "operation_id": incoming.operation_id.map(|id| id.to_string()),
        "attempt_id": incoming.attempt_id.map(|id| id.to_string()),
        "source_id": incoming.source_id,
        "source_instance_id": incoming.source_instance_id,
        "source_event_key": incoming.source_event_key,
        "differing_fields": conflict.differing_fields,
    }));
    if let Some(operation_id) = incoming.operation_id {
        error = error.with_operation(operation_id);
    }
    ShenkoError::Protocol(error)
}

/// Whether `error` is the caller-side receipt identity conflict rather than
/// storage damage. A caller conflict never closes the single writer.
pub(crate) fn is_key_conflict(error: &ShenkoError) -> bool {
    matches!(error, ShenkoError::Protocol(protocol) if protocol.code == RECEIPT_KEY_CONFLICT)
}

/// Write one refused receipt into the caller's transaction.
///
/// Returns `None` when this exact (incoming, original) pair is already
/// preserved: a repeated conflict attempt adds no second fact and never
/// rewrites the first one.
pub(crate) fn stage(
    tx: &Transaction<'_>,
    ledger: &Ledger,
    seq: LedgerSeq,
    incoming: &EvidenceReceipt,
    conflict: &KeyConflict,
) -> Result<Option<EntityId>, ShenkoError> {
    let conflict_id = EntityId::derived(
        "shenko.evidence.receipt.key_conflict",
        format!(
            "{}:{}",
            incoming.receipt_id, conflict.original.receipt_id
        )
        .as_bytes(),
    );
    let preserved: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM evidence_receipt_conflicts
             WHERE receipt_id=?1 AND original_receipt_id=?2",
            params![
                incoming.receipt_id.to_string(),
                conflict.original.receipt_id.to_string()
            ],
            |row| row.get(0),
        )
        .map_err(|error| ShenkoError::storage(error.to_string()))?;
    if preserved > 0 {
        return Ok(None);
    }
    let mut receipt = incoming.clone();
    receipt.received_commit = CommitRef::new(ledger.ledger_id(), seq);
    let record = EvidenceReceiptConflict {
        schema_version: 1,
        conflict_id,
        receipt_id: incoming.receipt_id,
        original_receipt_id: conflict.original.receipt_id,
        operation_id: incoming.operation_id,
        attempt_id: incoming.attempt_id,
        source_id: incoming.source_id.clone(),
        source_instance_id: incoming.source_instance_id.clone(),
        source_event_key: incoming.source_event_key.clone().unwrap_or_default(),
        matched: conflict.matched,
        differing_fields: conflict.differing_fields.clone(),
        receipt,
    };
    let data =
        serde_json::to_string(&record).map_err(|error| ShenkoError::storage(error.to_string()))?;
    tx.execute(
        "INSERT INTO evidence_receipt_conflicts
         (conflict_id,receipt_id,original_receipt_id,operation_id,source_id,
          source_instance_id,source_event_key,data,updated_commit_seq)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            record.conflict_id.to_string(),
            record.receipt_id.to_string(),
            record.original_receipt_id.to_string(),
            record.operation_id.map(|id| id.to_string()),
            record.source_id,
            record.source_instance_id,
            record.source_event_key,
            data,
            seq.0 as i64,
        ],
    )
    .map_err(|error| ShenkoError::storage(error.to_string()))?;
    Ok(Some(record.conflict_id))
}

/// Preserve the refused receipt, then refuse it.
///
/// The conflict fact commits on its own line *before* the typed error is
/// returned: the rollback of the write that was refused must not erase the
/// evidence that it was refused. The caller's writer stays usable, because a
/// contradiction in the caller's own facts is not storage damage.
pub(crate) fn refuse(
    ledger: &Ledger,
    tx: Transaction<'_>,
    incoming: &EvidenceReceipt,
    conflict: KeyConflict,
) -> AnyResult<ShenkoError> {
    let seq = ledger.allocate_seq(&tx)?;
    if stage(&tx, ledger, seq, incoming, &conflict)?.is_some() {
        ledger.record_commit(&tx, seq, 1)?;
        tx.commit()?;
    }
    Ok(key_conflict_error(incoming, &conflict))
}

impl Store {
    /// Refused receipts of one operation, in the order they were refused.
    ///
    /// These are its own durable facts and are deliberately separate from
    /// [`Store::external_operation_evidence`]: a receipt that contradicts an
    /// admitted original never becomes that operation's evidence.
    pub fn external_receipt_conflicts(
        &self,
        operation_id: EntityId,
    ) -> AnyResult<Vec<EvidenceReceiptConflict>> {
        let conn = self.ledger.conn.lock();
        let mut query = conn.prepare(
            "SELECT data FROM evidence_receipt_conflicts WHERE operation_id=?1 ORDER BY rowid",
        )?;
        let rows = query
            .query_map([operation_id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|data| serde_json::from_str(&data).map_err(Into::into))
            .collect()
    }
}

fn stored_receipt_by_id(
    tx: &Transaction<'_>,
    receipt_id: EntityId,
) -> AnyResult<Option<String>> {
    Ok(tx
        .query_row(
            "SELECT data FROM evidence_receipts WHERE receipt_id=?1",
            params![receipt_id.to_string()],
            |row| row.get(0),
        )
        .optional()?)
}

/// The first durable receipt of one source event scope. Its commit is the
/// binding fact, not the observation time a later report happens to carry.
fn stored_receipt_by_source_scope(
    tx: &Transaction<'_>,
    incoming: &EvidenceReceipt,
    key: &str,
) -> AnyResult<Option<String>> {
    Ok(tx
        .query_row(
            "SELECT data FROM evidence_receipts
             WHERE json_extract(data,'$.source_id')=?1
               AND json_extract(data,'$.source_instance_id')=?2
               AND json_extract(data,'$.source_event_key')=?3
             ORDER BY updated_commit_seq, rowid LIMIT 1",
            params![incoming.source_id, incoming.source_instance_id, key],
            |row| row.get(0),
        )
        .optional()?)
}

/// The receipt's own fact, without the commit that delivered it: the same
/// fact delivered twice is a replay even at a different commit.
fn fact(receipt: &EvidenceReceipt) -> AnyResult<serde_json::Value> {
    let mut neutral = receipt.clone();
    neutral.received_commit = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
    Ok(serde_json::to_value(&neutral)?)
}

fn same_fact(a: &EvidenceReceipt, b: &EvidenceReceipt) -> AnyResult<bool> {
    Ok(fact(a)? == fact(b)?)
}

/// Content identity of the original bytes, which is what decides whether two
/// reports are the same external event.
fn same_original(a: &EvidenceReceipt, b: &EvidenceReceipt) -> bool {
    a.raw_ref.digest == b.raw_ref.digest && a.raw_ref.bytes == b.raw_ref.bytes
}

fn differing_fields(a: &EvidenceReceipt, b: &EvidenceReceipt) -> AnyResult<Vec<String>> {
    let (left, right) = (fact(a)?, fact(b)?);
    let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
        return Ok(vec![]);
    };
    let mut fields: Vec<String> = Vec::new();
    for (name, value) in left {
        if right.get(name) != Some(value) {
            fields.push(name.clone());
        }
    }
    for name in right.keys() {
        if !left.contains_key(name) {
            fields.push(name.clone());
        }
    }
    fields.sort();
    fields.dedup();
    Ok(fields)
}
