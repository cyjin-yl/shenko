//! Installation-local model health observations. A peer's usable model does
//! not grant this installation credentials, and restarting cannot erase a
//! retained generation refusal while reusing the same exact binding.
use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::{error::ShenkoError, object::{Digest, ObjectRef}};

use crate::{ContentStore, Store, store::storage_error};

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_model_health (
    binding_digest TEXT PRIMARY KEY,
    data TEXT NOT NULL
);
"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalModelHealthHead {
    pub schema_version: u32,
    pub binding_digest: Digest,
    pub snapshot_ref: ObjectRef,
}

impl Store {
    pub fn local_model_health(&self, binding: &Digest) -> Result<Option<ObjectRef>, ShenkoError> {
        let raw: Option<String> = self.ledger.conn.lock().query_row(
            "SELECT data FROM local_model_health WHERE binding_digest=?1", [&binding.0],
            |row| row.get(0)).optional().map_err(storage_error)?;
        raw.map(|raw| read_head(&self.cas, binding, &raw).map(|head| head.snapshot_ref)).transpose()
    }

    /// CAS the local observation chain. This records health evidence only;
    /// it neither selects a writer nor admits any inference or task effect.
    pub fn record_local_model_health(&self, binding: &Digest, expected: Option<&ObjectRef>,
        snapshot: &ObjectRef) -> Result<(), ShenkoError> {
        let body = check_local_model_health_snapshot(&self.cas, binding, snapshot)?;
        if body.get("previous_ref") != Some(&serde_json::to_value(expected)?) {
            return Err(ShenkoError::conflict("local model health predecessor differs"));
        }
        self.ledger.with_mutation(|| {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage_error)?;
            let current: Option<String> = tx.query_row(
                "SELECT data FROM local_model_health WHERE binding_digest=?1", [&binding.0],
                |row| row.get(0)).optional().map_err(storage_error)?;
            let current = current.map(|raw| serde_json::from_str::<LocalModelHealthHead>(&raw)).transpose()?;
            if current.as_ref().map(|head| &head.snapshot_ref) == Some(snapshot) { return Ok(()); }
            if current.as_ref().map(|head| &head.snapshot_ref) != expected {
                return Err(ShenkoError::conflict("local model health observation moved"));
            }
            let head = LocalModelHealthHead { schema_version: 1, binding_digest: binding.clone(),
                snapshot_ref: snapshot.clone() };
            tx.execute("INSERT INTO local_model_health(binding_digest,data) VALUES(?1,?2) ON CONFLICT(binding_digest) DO UPDATE SET data=excluded.data",
                params![binding.0, serde_json::to_string(&head)?]).map_err(storage_error)?;
            let seq = self.ledger.allocate_seq(&tx)?;
            self.ledger.record_commit(&tx, seq, 1)?;
            tx.commit().map_err(storage_error)
        }, |error| matches!(error, ShenkoError::Storage(_)))
    }
}

/// Read installation-local health owners through the collector's excluded
/// snapshot, using the same binding and manifest authentication as the getter.
/// Inventory limits refuse the whole snapshot; they never truncate history.
pub(crate) fn local_model_health_roots(
    conn: &Connection,
    cas: &ContentStore,
) -> Result<Vec<ObjectRef>, ShenkoError> {
    const MAX_STATES: usize = 65_536;
    const MAX_STATE_BYTES: u64 = 64 * 1024 * 1024;
    let mut statement = conn.prepare(
        "SELECT binding_digest, data FROM local_model_health ORDER BY binding_digest",
    ).map_err(storage_error)?;
    let mut rows = statement.query([]).map_err(storage_error)?;
    let mut roots = Vec::new();
    let mut states = 0usize;
    let mut state_bytes = 0u64;
    while let Some(row) = rows.next().map_err(storage_error)? {
        let binding = Digest(row.get::<_, String>(0).map_err(storage_error)?);
        let raw: String = row.get(1).map_err(storage_error)?;
        let head = decode_head(&binding, &raw)?;
        let mut pending = Some(head.snapshot_ref);
        let mut seen = BTreeSet::new();
        while let Some(snapshot) = pending {
            if !seen.insert(snapshot.digest.clone()) {
                return Err(ShenkoError::conflict("local model health predecessor cycle"));
            }
            if states >= MAX_STATES {
                return Err(ShenkoError::conflict("local model health root state budget exceeded"));
            }
            state_bytes = state_bytes.checked_add(snapshot.bytes)
                .filter(|bytes| *bytes <= MAX_STATE_BYTES)
                .ok_or_else(|| ShenkoError::conflict("local model health root byte budget exceeded"))?;
            states += 1;
            // Admit the declared length before reading/authenticating this
            // immutable CAS state; an opaque alias cannot bypass the budget.
            if cas.object_size(&snapshot.digest) != Some(snapshot.bytes) {
                return Err(ShenkoError::conflict("local model health snapshot length differs"));
            }
            let body = check_local_model_health_snapshot(cas, &binding, &snapshot)?;
            let previous = body.get("previous_ref")
                .ok_or_else(|| ShenkoError::conflict("local model health predecessor is missing"))?;
            pending = Option::<ObjectRef>::deserialize(previous)?;
            // Authenticate every state, not just the SQL head, before adding
            // JSON authority. Preserve its original alias as well as the
            // declaration that reveals its observations and older predecessor.
            if !crate::cognition::is_json_reference(&snapshot.media_type) {
                let mut declared = snapshot.clone();
                declared.media_type = "application/json".into();
                roots.push(declared);
            }
            roots.push(snapshot);
        }
    }
    Ok(roots)
}

fn read_head(
    cas: &ContentStore,
    binding: &Digest,
    raw: &str,
) -> Result<LocalModelHealthHead, ShenkoError> {
    let head = decode_head(binding, raw)?;
    check_local_model_health_snapshot(cas, binding, &head.snapshot_ref)?;
    Ok(head)
}

fn decode_head(binding: &Digest, raw: &str) -> Result<LocalModelHealthHead, ShenkoError> {
    let head: LocalModelHealthHead = serde_json::from_str(raw)?;
    if head.schema_version != 1 || &head.binding_digest != binding {
        return Err(ShenkoError::conflict("local model health binding differs"));
    }
    Ok(head)
}

fn check_local_model_health_snapshot(cas: &ContentStore, binding: &Digest, object: &ObjectRef)
    -> Result<serde_json::Value, ShenkoError> {
    let bytes = cas.read_object(&object.digest)?;
    if object.store_namespace != "model-health" || object.codec.name != "model-health-state"
        || object.codec.major != 1 || object.bytes != bytes.len() as u64
        || Digest::blake3(&bytes) != object.digest {
        return Err(ShenkoError::conflict("local model health snapshot identity differs"));
    }
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    if body["schema_version"] != 1 || body["binding_digest"] != serde_json::to_value(binding)? {
        return Err(ShenkoError::conflict("local model health snapshot binding differs"));
    }
    Ok(body)
}
