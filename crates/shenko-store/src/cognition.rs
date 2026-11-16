//! Cognition recovery replication, R113/R145/R147.
//!
//! A store can hand another node the facts a recovering self needs: identity
//! and soul, raw history, subject decisions, goals, intents, tasks,
//! checkpoints, effects, attempts, receipts, context windows and the managed
//! memory content — together with the verified closure of every referenced
//! CAS object and the memory repository history.
//!
//! Rules this module keeps:
//!
//! * one consistent SQLite read snapshot per export;
//! * nothing is published on the receiving side before the whole closure
//!   arrived and verified, so a partial transfer can never be reported as
//!   recoverable;
//! * per peer export/ack watermarks separate "durable here", "sent" and
//!   "verified recoverable at the peer", and the bookkeeping of that exchange
//!   never becomes a fact of the store, so it cannot create new debt;
//! * imported work is parked, never dispatched here: a replica is not a
//!   second live writer and an armed or unknown effect is never replayed;
//! * a fact that this store still holds exactly as the peer last delivered
//!   it takes the newer value, so a continuing source advances instead of
//!   looking like a divergence;
//! * a fact that this store changed on its own is kept on both sides and
//!   reported as an explicit conflict — never a last-write-wins;
//! * writer and dispatcher leases, local credentials, provider secrets and
//!   the working directory are never part of an export.
//! * a pending input is recovery data: the inbox travels with the state the
//!   source recorded, while the local consumption cursor stays local.
//!
//! What an import does and does not do:
//!
//! * it restores the facts and the memory content into the store and the
//!   given memory root; the receiving installation keeps its own home, its
//!   own installation identity and its own dispatcher;
//! * it does not adopt the replicated subject as this installation's own
//!   subject. That belongs to the election and handoff path, and the
//!   replicated genesis stays the fact it is: the record of where the
//!   subject came from;
//! * it never starts a dispatcher, never re-arms an attempt and never
//!   replays an unknown effect. Imported work stays parked until an
//!   explicit decision moves it;
//! * it does not adopt unknown data into a running home. A home prepares
//!   itself first — the seed creates the home manifest and the empty memory
//!   repository — and the import then fills that repository. A memory root
//!   that already holds other content keeps it: the divergence is reported
//!   instead of being overwritten.

//!
//! Public recovery values and local bookkeeping schema stay in this module.
//! Private owners separate snapshot/closure, structural admission, transactional
//! import, domain conflict units, file/Git restoration and peer exchange records.
//! Only the import owner spans reconciliation and filesystem publication.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use shenko_types::error::ShenkoError;
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::Digest;

use crate::cas::ContentStore;
use crate::cognition_archive::{CognitionFileDescriptor, CognitionGap, CognitionManifest};

mod admission;
mod closure;
mod files;
mod import;
mod memory;
mod peers;
mod reconcile;
mod rows;
mod snapshot;
mod tasks;

/// Local bookkeeping of the replication exchange, plus the declared recovery
/// source facts. Bookkeeping never enters the fingerprint: recording an
/// export or acknowledgement must not create new debt. `recovery_sources`
/// is replicated through `TABLES` and its typed references join the closure.
pub(crate) const COGNITION_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS recovery_sources (
    source_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    data TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS cognition_peers (
    peer_id TEXT PRIMARY KEY,
    exported_fingerprint TEXT,
    exported_high_water INTEGER,
    exported_at TEXT,
    acked_fingerprint TEXT,
    acked_high_water INTEGER,
    acked_at TEXT,
    last_failure TEXT,
    last_failure_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS cognition_conflicts (
    conflict_id TEXT PRIMARY KEY,
    peer_id TEXT NOT NULL,
    table_name TEXT NOT NULL,
    entity_key TEXT NOT NULL,
    detail TEXT NOT NULL,
    detected_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_cognition_conflicts_peer
    ON cognition_conflicts(peer_id, detected_at);
CREATE TABLE IF NOT EXISTS cognition_receipts (
    peer_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    high_water INTEGER NOT NULL,
    verified_objects INTEGER NOT NULL,
    verified_files INTEGER NOT NULL,
    inserted INTEGER NOT NULL,
    updated INTEGER NOT NULL DEFAULT 0,
    unchanged INTEGER NOT NULL,
    conflicts INTEGER NOT NULL,
    received_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (peer_id, fingerprint)
);
CREATE TABLE IF NOT EXISTS cognition_state (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    fingerprint TEXT,
    high_water INTEGER,
    computed_at TEXT
);
CREATE TABLE IF NOT EXISTS cognition_export_order (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    sequence INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS cognition_retirements (
    path TEXT PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS cognition_import_order (
    source_ledger_id TEXT PRIMARY KEY,
    peer_id TEXT NOT NULL,
    snapshot_seq INTEGER NOT NULL,
    high_water INTEGER NOT NULL,
    fingerprint TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS cognition_baselines (
    peer_id TEXT NOT NULL,
    table_name TEXT NOT NULL,
    entity_key TEXT NOT NULL,
    row_values TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (peer_id, table_name, entity_key)
);
CREATE TABLE IF NOT EXISTS cognition_file_baselines (
    peer_id TEXT NOT NULL,
    origin TEXT NOT NULL,
    path TEXT NOT NULL,
    digest TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'present',
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (peer_id, origin, path)
);
CREATE TABLE IF NOT EXISTS cognition_origins (
    agent_id TEXT PRIMARY KEY,
    source_ledger_id TEXT NOT NULL,
    delivered_by TEXT,
    fingerprint TEXT NOT NULL,
    genesis_id TEXT NOT NULL,
    imported_at TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

/// What a snapshot contains.
#[derive(Debug, Clone)]
pub struct CognitionSnapshotOptions {
    /// The managed memory root. It is recovery data by default.
    pub memory_root: Option<PathBuf>,
    /// Extra roots copied only when the operator asks for them.
    pub workspace_roots: Vec<PathBuf>,
    /// Capture the memory repository history. On by default.
    pub include_git_history: bool,
    /// The caller guarantees the paused local subject boundary is preserved
    /// at the destination before this archive is applied. Only the
    /// same-installation A/B handover sets this: its importer installs the
    /// complete paused head/successor/quiesce state through
    /// `copy_paused_activation_boundary` before applying the archive, so a
    /// paused head is a snapshot the receiver is equipped to carry, never a
    /// stranded one. Peer replication must leave this unset — a peer archive
    /// cannot deliver a local boundary safely.
    pub local_boundary_preserved: bool,
}

impl Default for CognitionSnapshotOptions {
    fn default() -> Self {
        Self {
            memory_root: None,
            workspace_roots: Vec::new(),
            include_git_history: true,
            local_boundary_preserved: false,
        }
    }
}

/// Where an imported snapshot is placed.
#[derive(Debug, Clone, Default)]
pub struct CognitionImportOptions {
    /// Root that receives the memory files and the memory history.
    pub memory_root: Option<PathBuf>,
    /// Root that receives the opt-in workspace files.
    pub workspace_root: Option<PathBuf>,
    /// The peer that sent the snapshot.
    pub peer_id: Option<EntityId>,
    /// The caller declares this archive the same-installation handover (the
    /// A/B catch-up), not a peer replication. Only then may the archive
    /// carry and install local control facts (the scope's rollover
    /// request). The claim is never read from the archive: a peer archive
    /// cannot make itself authoritative over this installation's context
    /// boundaries. Peer importers leave this false, so an archive that
    /// declares the control class is refused whole.
    pub local_control_handover: bool,
}

/// What an export produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CognitionReceipt {
    /// Content fingerprint of the snapshot.
    pub fingerprint: Digest,
    /// Local ledger sequence the snapshot was taken at.
    pub high_water: LedgerSeq,
    pub rows: usize,
    pub objects: usize,
    pub files: usize,
    /// Holes in the closure. A blocking gap forbids claiming this snapshot
    /// recoverable.
    pub gaps: Vec<CognitionGap>,
    /// True when no blocking gap was found.
    pub complete: bool,
    pub exported_at: chrono::DateTime<chrono::Utc>,
}

/// An exported snapshot: its manifest and the payloads the manifest declares.
pub struct CognitionExport {
    /// What the snapshot consists of: the facts, the closure and the content.
    pub manifest: CognitionManifest,
    /// The verified CAS objects are streamed from here when the archive is
    /// written. Content addressed objects are immutable, so the snapshot that
    /// is written is the snapshot that was computed.
    cas: Arc<ContentStore>,
    /// The memory and workspace files, with the path they are read from.
    files: Vec<(CognitionFileDescriptor, PathBuf)>,
    /// The memory history, as a Git bundle.
    git_bundle: Option<Vec<u8>>,
}


/// A fact that exists on both sides with different content. Both sides keep
/// their own value; nothing is overwritten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CognitionConflict {
    pub table: String,
    pub key: String,
    pub detail: String,
}

/// What one import did to the replicated facts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CognitionApplyOutcome {
    /// Facts that were new on this store.
    pub inserted: usize,
    /// Facts that took a newer value from the peer, because this store still
    /// held the value the peer last delivered.
    pub updated: usize,
    /// Facts that were already present with the same content.
    pub unchanged: usize,
    /// Facts that exist on both sides with different content.
    pub conflicts: Vec<CognitionConflict>,
}

/// What an import did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CognitionImportOutcome {
    pub fingerprint: Digest,
    pub high_water: LedgerSeq,
    /// Rows that were new on this store.
    pub inserted: usize,
    /// Rows that took a newer value from the peer, because this store still
    /// held the value the peer last delivered.
    pub updated: usize,
    /// Rows that were already present with the same content.
    pub unchanged: usize,
    pub conflicts: Vec<CognitionConflict>,
    pub verified_objects: usize,
    pub verified_files: usize,
    /// True when the memory repository history was restored.
    pub restored_git_history: bool,
    pub gaps: Vec<CognitionGap>,
}

impl CognitionImportOutcome {
    /// The answer this store gives about the snapshot it just received.
    ///
    /// A snapshot is verified only when it has no blocking gaps or conflicts
    /// with local history; conflicting facts remain available for reconciliation.
    pub fn ack(&self, peer_id: EntityId) -> CognitionAck {
        let blocking = self.gaps.iter().filter(|gap| gap.blocking).count();
        CognitionAck {
            peer_id,
            fingerprint: self.fingerprint.clone(),
            high_water: self.high_water,
            verified: blocking == 0 && self.conflicts.is_empty(),
            missing: Vec::new(),
            note: if self.conflicts.is_empty() {
                None
            } else {
                Some(format!(
                    "{} conflicting facts kept on both sides",
                    self.conflicts.len()
                ))
            },
        }
    }
}

/// The peer's answer about one snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CognitionAck {
    pub peer_id: EntityId,
    /// Fingerprint the peer actually verified.
    pub fingerprint: Digest,
    /// Watermark the peer verified.
    pub high_water: LedgerSeq,
    /// True only when the peer verified the complete closure.
    pub verified: bool,
    /// Objects the peer could not verify.
    #[serde(default)]
    pub missing: Vec<Digest>,
    pub note: Option<String>,
}

/// How far one peer is from holding a verified recoverable copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerCognitionStatus {
    pub peer_id: EntityId,
    /// Fingerprint of what this store holds right now.
    pub local_fingerprint: Digest,
    pub local_high_water: LedgerSeq,
    /// Fingerprint of the last snapshot sent to this peer.
    pub exported_fingerprint: Option<Digest>,
    pub exported_high_water: Option<LedgerSeq>,
    pub exported_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Fingerprint the peer confirmed as verified recoverable.
    pub acked_fingerprint: Option<Digest>,
    pub acked_high_water: Option<LedgerSeq>,
    pub acked_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The peer holds exactly the snapshot this store has now.
    pub verified_recoverable: bool,
    /// This store changed after the last export.
    pub unsent_changes: bool,
    /// The peer received an export that it has not verified.
    pub unverified_transfer: bool,
    pub last_failure: Option<String>,
    pub last_failure_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Where an imported subject came from.
///
/// A subject that arrived through replication keeps its own genesis: the
/// record still says where the subject was born. This provenance lets the
/// runtime tell an imported subject from one this installation created, so
/// the local home is never compared against a foreign home manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CognitionSubjectOrigin {
    pub agent_id: EntityId,
    /// The ledger the subject was born in.
    pub source_ledger_id: EntityId,
    /// The peer that delivered the subject.
    pub delivered_by: Option<EntityId>,
    /// The snapshot that carried the subject.
    pub fingerprint: Digest,
    pub genesis_id: EntityId,
    pub imported_at: chrono::DateTime<chrono::Utc>,
}

/// Local receipt coordinates follow the same table catalog as archive import.
pub(crate) fn control_local_sequence(table: &str) -> Option<&'static str> {
    rows::TABLES.iter().find(|spec| spec.name == table).and_then(|spec| spec.local_seq)
}

fn db(error: rusqlite::Error) -> anyhow::Error {
    anyhow::Error::from(error)
}

fn is_constraint_violation(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<rusqlite::Error>()
        .is_some_and(|inner| {
            matches!(
                inner,
                rusqlite::Error::SqliteFailure(failure, _)
                    if failure.code == rusqlite::ErrorCode::ConstraintViolation
            )
        })
}

fn storage_failure(error: &anyhow::Error) -> bool {
    error.is::<rusqlite::Error>()
        || error.is::<serde_json::Error>()
        || error
            .downcast_ref::<ShenkoError>()
            .is_some_and(|inner| matches!(inner, ShenkoError::Storage(_)))
}

#[cfg(test)]
mod archive_admission_tests;

#[cfg(test)]
mod obligation_merge_tests;
