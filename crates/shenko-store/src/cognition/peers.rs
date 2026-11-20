//! Peer exchange bookkeeping, conflicts and imported-subject provenance.
//!
//! Export/ACK records describe transfers; they do not enter the replicated fact
//! catalog or change the snapshot fingerprint whose debt they describe.

use anyhow::{Context, Result, anyhow};
use rusqlite::{OptionalExtension, Transaction, params};

use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::Digest;

use crate::cognition_archive::{CognitionManifest, CognitionRow};
use crate::store::Store;

use super::rows::TableSpec;
use super::{
    CognitionAck, CognitionConflict, CognitionReceipt, CognitionSnapshotOptions,
    CognitionSubjectOrigin, PeerCognitionStatus, db,
};

impl Store {
    /// Record that a peer received the given snapshot.
    pub fn note_cognition_export(
        &self,
        peer_id: EntityId,
        receipt: &CognitionReceipt,
    ) -> Result<()> {
        let _permit = self.ledger.acquire_installation(false)?;
        let conn = self.ledger.conn.lock();
        conn.execute(
            "INSERT INTO cognition_peers(peer_id,exported_fingerprint,exported_high_water,exported_at,created_at)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(peer_id) DO UPDATE SET
                exported_fingerprint=excluded.exported_fingerprint,
                exported_high_water=excluded.exported_high_water,
                exported_at=excluded.exported_at",
            params![
                peer_id.to_string(),
                receipt.fingerprint.to_string(),
                receipt.high_water.0 as i64,
                receipt.exported_at.to_rfc3339(),
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Record the peer's answer about a snapshot.
    ///
    /// Only the source records the acknowledgement, and only when the peer
    /// answers exactly the snapshot that was sent to it. A partial transfer
    /// never advances the acknowledgement: the peer stays unverified until it
    /// confirms the complete closure.
    pub fn record_cognition_ack(&self, ack: &CognitionAck) -> Result<()> {
        let _permit = self.ledger.acquire_installation(false)?;
        let conn = self.ledger.conn.lock();
        let sent: Option<(Option<String>, Option<i64>)> = conn
            .query_row(
                "SELECT exported_fingerprint,exported_high_water FROM cognition_peers WHERE peer_id=?1",
                [ack.peer_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let matches_sent = match &sent {
            Some((Some(fingerprint), Some(high_water))) => {
                fingerprint == &ack.fingerprint.0 && *high_water == ack.high_water.0 as i64
            }
            _ => false,
        };
        if ack.verified && ack.missing.is_empty() && matches_sent {
            conn.execute(
                "INSERT INTO cognition_peers(peer_id,acked_fingerprint,acked_high_water,acked_at,last_failure,last_failure_at,created_at)
                 VALUES(?1,?2,?3,?4,NULL,NULL,?5)
                 ON CONFLICT(peer_id) DO UPDATE SET
                    acked_fingerprint=excluded.acked_fingerprint,
                    acked_high_water=excluded.acked_high_water,
                    acked_at=excluded.acked_at,
                    last_failure=NULL,
                    last_failure_at=NULL",
                params![
                    ack.peer_id.to_string(),
                    ack.fingerprint.to_string(),
                    ack.high_water.0 as i64,
                    chrono::Utc::now().to_rfc3339(),
                    chrono::Utc::now().to_rfc3339(),
                ],
            )?;
        } else {
            let reason = if !matches_sent {
                format!(
                    "the peer answered a different snapshot than the one that was sent to it (sent fingerprint {}, answered {})",
                    sent.as_ref()
                        .and_then(|(fingerprint, _)| fingerprint.clone())
                        .unwrap_or_else(|| "none".into()),
                    ack.fingerprint
                )
            } else {
                format!(
                    "peer verified={} missing={} note={}",
                    ack.verified,
                    ack.missing.len(),
                    ack.note.clone().unwrap_or_default()
                )
            };
            conn.execute(
                "INSERT INTO cognition_peers(peer_id,last_failure,last_failure_at,created_at)
                 VALUES(?1,?2,?3,?4)
                 ON CONFLICT(peer_id) DO UPDATE SET
                    last_failure=excluded.last_failure,
                    last_failure_at=excluded.last_failure_at",
                params![
                    ack.peer_id.to_string(),
                    reason,
                    chrono::Utc::now().to_rfc3339(),
                    chrono::Utc::now().to_rfc3339(),
                ],
            )?;
        }
        Ok(())
    }

    /// How far one peer is from holding a verified recoverable copy.
    pub fn cognition_peer_status(
        &self,
        peer_id: EntityId,
        options: &CognitionSnapshotOptions,
    ) -> Result<PeerCognitionStatus> {
        let (fingerprint, high_water) = self.cognition_fingerprint(options)?;
        let conn = self.ledger.conn.lock();
        let row: Option<(
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<String>,
        )> = conn
            .query_row(
                "SELECT exported_fingerprint,exported_high_water,exported_at,
                        acked_fingerprint,acked_high_water,acked_at,last_failure,last_failure_at
                 FROM cognition_peers WHERE peer_id=?1",
                [peer_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()?;
        let (
            exported_fingerprint,
            exported_high_water,
            exported_at,
            acked_fingerprint,
            acked_high_water,
            acked_at,
            last_failure,
            last_failure_at,
        ) = row.unwrap_or((None, None, None, None, None, None, None, None));
        let exported = exported_fingerprint.map(|value| Digest(value));
        let acked = acked_fingerprint.map(|value| Digest(value));
        let verified_recoverable = acked
            .as_ref()
            .map(|value| *value == fingerprint)
            .unwrap_or(false);
        let unsent_changes = match exported.as_ref() {
            Some(value) => *value != fingerprint,
            None => true,
        };
        let unverified_transfer = match (exported.as_ref(), acked.as_ref()) {
            (Some(sent), Some(verified)) => *sent != *verified,
            (Some(_), None) => true,
            _ => false,
        };
        Ok(PeerCognitionStatus {
            peer_id,
            local_fingerprint: fingerprint,
            local_high_water: high_water,
            exported_fingerprint: exported,
            exported_high_water: exported_high_water
                .map(|value| u64::try_from(value).map(LedgerSeq))
                .transpose()
                .context("invalid exported high water")?,
            exported_at: exported_at.and_then(|value| parse_time(&value)),
            acked_fingerprint: acked,
            acked_high_water: acked_high_water
                .map(|value| u64::try_from(value).map(LedgerSeq))
                .transpose()
                .context("invalid acknowledged high water")?,
            acked_at: acked_at.and_then(|value| parse_time(&value)),
            verified_recoverable,
            unsent_changes,
            unverified_transfer,
            last_failure,
            last_failure_at: last_failure_at.and_then(|value| parse_time(&value)),
        })
    }

    /// The status of every peer this store exchanged snapshots with.
    pub fn cognition_peer_statuses(
        &self,
        options: &CognitionSnapshotOptions,
    ) -> Result<Vec<PeerCognitionStatus>> {
        let peers: Vec<String> = {
            let conn = self.ledger.conn.lock();
            let mut stmt = conn.prepare("SELECT peer_id FROM cognition_peers ORDER BY peer_id")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut out = Vec::new();
        for peer in peers {
            let peer_id: EntityId = peer
                .parse()
                .map_err(|_| anyhow!("invalid peer id {peer}"))?;
            out.push(self.cognition_peer_status(peer_id, options)?);
        }
        Ok(out)
    }

    /// The facts that exist on both sides with different content.
    pub fn cognition_conflicts(&self, limit: usize) -> Result<Vec<CognitionConflict>> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT table_name,entity_key,detail FROM cognition_conflicts
             ORDER BY detected_at DESC, conflict_id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(CognitionConflict {
                table: row.get(0)?,
                key: row.get(1)?,
                detail: row.get(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Where an imported subject came from.
    ///
    /// `None` means this installation created the subject itself: there is no
    /// foreign home behind it.
    pub fn cognition_subject_origin(
        &self,
        agent_id: EntityId,
    ) -> Result<Option<CognitionSubjectOrigin>> {
        let conn = self.ledger.conn.lock();
        let row: Option<(String, Option<String>, String, String, String)> = conn
            .query_row(
                "SELECT source_ledger_id,delivered_by,fingerprint,genesis_id,imported_at
                 FROM cognition_origins WHERE agent_id=?1",
                [agent_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((source_ledger_id, delivered_by, fingerprint, genesis_id, imported_at)) = row
        else {
            return Ok(None);
        };
        Ok(Some(CognitionSubjectOrigin {
            agent_id,
            source_ledger_id: source_ledger_id
                .parse()
                .map_err(|_| anyhow!("invalid cognition origin ledger {source_ledger_id}"))?,
            delivered_by: match delivered_by {
                Some(value) => Some(
                    value
                        .parse()
                        .map_err(|_| anyhow!("invalid cognition origin peer {value}"))?,
                ),
                None => None,
            },
            fingerprint: Digest(fingerprint),
            genesis_id: genesis_id
                .parse()
                .map_err(|_| anyhow!("invalid cognition origin genesis {genesis_id}"))?,
            imported_at: parse_time(&imported_at)
                .ok_or_else(|| anyhow!("invalid cognition origin time {imported_at}"))?,
        }))
    }

    pub(super) fn record_receipt(
        &self,
        peer_id: EntityId,
        manifest: &CognitionManifest,
        verified_objects: usize,
        verified_files: usize,
        inserted: usize,
        updated: usize,
        unchanged: usize,
        conflicts: usize,
        publication: &crate::roots::CasWriteLease,
        permit: Option<&shenko_types::installation::InstallationPermit>,
    ) -> Result<()> {
        self.ledger.with_maintenance_mutation_owned_under(publication, permit, || {
        let conn = self.ledger.conn.lock();
        conn.execute(
            "INSERT INTO cognition_receipts(peer_id,fingerprint,high_water,verified_objects,verified_files,inserted,updated,unchanged,conflicts,received_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(peer_id,fingerprint) DO UPDATE SET
                high_water=excluded.high_water,
                verified_objects=excluded.verified_objects,
                verified_files=excluded.verified_files,
                inserted=excluded.inserted,
                updated=excluded.updated,
                unchanged=excluded.unchanged,
                conflicts=excluded.conflicts,
                received_at=excluded.received_at",
            params![
                peer_id.to_string(),
                manifest.fingerprint.to_string(),
                manifest.high_water.0 as i64,
                verified_objects as i64,
                verified_files as i64,
                inserted as i64,
                updated as i64,
                unchanged as i64,
                conflicts as i64,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
        }, super::storage_failure)
    }

    pub(super) fn store_local_fingerprint(&self, fingerprint: &Digest, high_water: LedgerSeq) -> Result<()> {
        let _permit = self.ledger.acquire_installation(false)?;
        let conn = self.ledger.conn.lock();
        conn.execute(
            "INSERT INTO cognition_state(singleton,fingerprint,high_water,computed_at)
             VALUES(1,?1,?2,?3)
             ON CONFLICT(singleton) DO UPDATE SET
                fingerprint=excluded.fingerprint,
                high_water=excluded.high_water,
                computed_at=excluded.computed_at",
            params![
                fingerprint.to_string(),
                high_water.0 as i64,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }
}

/// Record where an imported subject came from.
///
/// The genesis stays untouched: it keeps saying where the subject was born.
/// The provenance is a separate fact about this store, so the runtime can
/// tell an imported subject from a local one.
pub(super) fn record_origin(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    row: &CognitionRow,
    peer_id: EntityId,
    fingerprint: &Digest,
    source_ledger_id: EntityId,
) -> Result<()> {
    if spec.name != "subject_genesis" {
        return Ok(());
    }
    let subject_id = row.value("subject_id").unwrap_or_default();
    let genesis_id = row.value("genesis_id").unwrap_or_default();
    if subject_id.is_empty() || genesis_id.is_empty() {
        return Ok(());
    }
    let born_in = row
        .value("data")
        .and_then(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .and_then(|value| {
            value
                .get("created_commit")
                .and_then(|commit| commit.get("ledger_id"))
                .and_then(|ledger| ledger.as_str())
                .map(|ledger| ledger.to_string())
        })
        .and_then(|ledger| ledger.parse::<EntityId>().ok())
        .unwrap_or(source_ledger_id);
    tx.execute(
        "INSERT INTO cognition_origins(agent_id,source_ledger_id,delivered_by,fingerprint,genesis_id,imported_at)
         VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(agent_id) DO UPDATE SET
            source_ledger_id=excluded.source_ledger_id,
            delivered_by=excluded.delivered_by,
            fingerprint=excluded.fingerprint,
            genesis_id=excluded.genesis_id,
            imported_at=excluded.imported_at",
        params![
            subject_id,
            born_in.to_string(),
            peer_id.to_string(),
            fingerprint.to_string(),
            genesis_id,
            chrono::Utc::now().to_rfc3339(),
        ],
    )
    .map_err(db)?;
    Ok(())
}

pub(super) fn record_conflict(
    tx: &Transaction<'_>,
    peer_id: EntityId,
    table: &str,
    key: &str,
    detail: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO cognition_conflicts(conflict_id,peer_id,table_name,entity_key,detail,detected_at)
         VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            EntityId::new().to_string(),
            peer_id.to_string(),
            table,
            key,
            detail,
            chrono::Utc::now().to_rfc3339(),
        ],
    )
    .map_err(db)?;
    Ok(())
}

fn parse_time(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&chrono::Utc))
}
