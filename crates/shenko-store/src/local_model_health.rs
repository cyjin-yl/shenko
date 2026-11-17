//! Installation-local model health observations. A peer's usable model does
//! not grant this installation credentials, and restarting cannot erase a
//! retained generation refusal while reusing the same exact binding.
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::{error::ShenkoError, object::{Digest, ObjectRef}};

use crate::{Store, store::storage_error};

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
        raw.map(|raw| {
            let head: LocalModelHealthHead = serde_json::from_str(&raw)?;
            if head.schema_version != 1 || &head.binding_digest != binding {
                return Err(ShenkoError::conflict("local model health binding differs"));
            }
            self.check_local_model_health_snapshot(binding, &head.snapshot_ref)?;
            Ok(head.snapshot_ref)
        }).transpose()
    }

    fn check_local_model_health_snapshot(&self, binding: &Digest, object: &ObjectRef)
        -> Result<serde_json::Value, ShenkoError> {
        let bytes = self.cas.read_object(&object.digest)?;
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

    /// CAS the local observation chain. This records health evidence only;
    /// it neither selects a writer nor admits any inference or task effect.
    pub fn record_local_model_health(&self, binding: &Digest, expected: Option<&ObjectRef>,
        snapshot: &ObjectRef) -> Result<(), ShenkoError> {
        let body = self.check_local_model_health_snapshot(binding, snapshot)?;
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
