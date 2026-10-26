//! Cognition recovery replication — SPEC §5.4, §36.5, §41.2, R113/R145/R147.
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
//! * a fact that already exists with different content is kept on both sides
//!   and reported as an explicit conflict — never a last-write-wins;
//! * writer and dispatcher leases, local credentials, provider secrets and
//!   the working directory are never part of an export.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail, ensure};
use rusqlite::types::ValueRef;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use shenko_types::error::ShenkoError;
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::{Digest, ObjectRef, SchemaRef};
use shenko_types::task::{DispatchEligibility, TaskRecord, TaskState};

use crate::cognition_archive::{
    CognitionDerivative, CognitionFileDescriptor, CognitionFileOrigin, CognitionGap,
    CognitionGitDescriptor, CognitionManifest, CognitionObjectDescriptor, CognitionRow,
    read_archive, write_manifest, write_payload,
};
use crate::cas::ContentStore;
use crate::store::Store;

/// Local bookkeeping of the replication exchange. These tables are never
/// replicated and never enter the fingerprint: recording an export or an
/// acknowledgement must not create a new fact and therefore must not create
/// new debt.
pub(crate) const COGNITION_SCHEMA: &str = r#"
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
}

impl Default for CognitionSnapshotOptions {
    fn default() -> Self {
        Self {
            memory_root: None,
            workspace_roots: Vec::new(),
            include_git_history: true,
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

impl CognitionExport {
    /// Stream this snapshot as an archive.
    ///
    /// The snapshot is not rebuilt: the manifest, the fingerprint and the
    /// closure were computed once, and every payload is verified against the
    /// manifest while it is written. A payload that changed in between stops
    /// the transfer instead of shipping bytes that do not match the
    /// fingerprint.
    pub fn write_archive<W: Write>(&self, mut writer: W) -> Result<CognitionReceipt> {
        write_manifest(&mut writer, &self.manifest)?;
        for (index, descriptor) in self.manifest.objects.iter().enumerate() {
            let bytes = self.cas.read_object(&descriptor.digest)?;
            ensure!(
                bytes.len() as u64 == descriptor.bytes
                    && Digest::blake3(&bytes) == descriptor.digest,
                "the exported object {} no longer matches the snapshot",
                descriptor.digest
            );
            write_payload(
                &mut writer,
                "object",
                &[index.to_string(), bytes.len().to_string()],
                &bytes,
            )?;
        }
        for (index, (descriptor, path)) in self.files.iter().enumerate() {
            let bytes = fs::read(path)
                .with_context(|| format!("read the exported file {}", path.display()))?;
            ensure!(
                Digest::blake3(&bytes) == descriptor.digest,
                "the exported file {} changed after the snapshot was taken",
                descriptor.path
            );
            write_payload(
                &mut writer,
                "file",
                &[index.to_string(), bytes.len().to_string()],
                &bytes,
            )?;
        }
        if let (Some(descriptor), Some(bytes)) =
            (self.manifest.git_history.as_ref(), self.git_bundle.as_ref())
        {
            ensure!(
                bytes.len() as u64 == descriptor.bytes
                    && Digest::blake3(&bytes) == descriptor.digest,
                "the exported memory history no longer matches the snapshot"
            );
            write_payload(&mut writer, "git", &[bytes.len().to_string()], bytes)?;
        }
        writer.flush()?;
        Ok(CognitionReceipt {
            fingerprint: self.manifest.fingerprint.clone(),
            high_water: self.manifest.high_water,
            rows: self.manifest.rows.len(),
            objects: self.manifest.objects.len(),
            files: self.manifest.files.len(),
            gaps: self.manifest.gaps.clone(),
            complete: self.manifest.blocking_gaps().is_empty(),
            exported_at: self.manifest.created_at,
        })
    }
}

/// A fact that exists on both sides with different content. Both sides keep
/// their own value; nothing is overwritten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CognitionConflict {
    pub table: String,
    pub key: String,
    pub detail: String,
}

/// What an import did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CognitionImportOutcome {
    pub fingerprint: Digest,
    pub high_water: LedgerSeq,
    /// Rows that were new on this store.
    pub inserted: usize,
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
    /// A snapshot that arrived without a blocking gap is verified; conflicts
    /// with the local history do not make the transfer unverified, they are
    /// reported so the operator can reconcile them.
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
                Some(format!("{} conflicting facts kept on both sides", self.conflicts.len()))
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

/// How a replicated row is treated on import.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowKind {
    /// The row is replicated as it is stored.
    Verbatim,
    /// A task. Imported work is parked on the receiving store.
    Task,
    /// A subject intent. The dispatcher lease is dropped and unfinished
    /// intents become explicitly unknown, so nothing is dispatched here.
    SubjectIntent,
}

/// One replicated table.
struct TableSpec {
    name: &'static str,
    /// Primary key columns.
    key: &'static [&'static str],
    /// Replicated columns, in a fixed order.
    columns: &'static [&'static str],
    /// Column the importing store owns; it receives the local commit seq.
    local_seq: Option<&'static str>,
    kind: RowKind,
}

/// The replicated tables, in the order the rows must be applied: a row may
/// only reference a row that already exists.
const TABLES: &[TableSpec] = &[
    TableSpec {
        name: "identity_profiles",
        key: &["agent_id", "revision"],
        columns: &["agent_id", "revision", "profile", "created_at"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_genesis",
        key: &["genesis_id"],
        columns: &["genesis_id", "subject_id", "bootstrap_id", "data", "created_at"],
        local_seq: Some("created_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_branch_heads",
        key: &["agent_id"],
        columns: &[
            "agent_id",
            "branch_id",
            "committed_step_no",
            "committed_step_id",
            "next_step_id",
            "mode",
            "record_revision",
            "data",
            "created_at",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_steps",
        key: &["step_id"],
        columns: &[
            "step_id",
            "branch_id",
            "step_no",
            "state",
            "record_revision",
            "data",
            "created_at",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_event_dispositions",
        key: &["branch_id", "event_id"],
        columns: &["branch_id", "event_id", "step_id", "disposition", "commit_seq"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_intent_steps",
        key: &["step_id"],
        columns: &["step_id", "migration"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_intents",
        key: &["intent_id"],
        columns: &[
            "intent_id",
            "step_id",
            "agent_id",
            "kind",
            "contract",
            "payload",
            "decision_ref",
            "state",
            "revision",
            "created_seq",
            "updated_seq",
            "result_ref",
        ],
        local_seq: None,
        kind: RowKind::SubjectIntent,
    },
    TableSpec {
        name: "bootstrap_records",
        key: &["bootstrap_id"],
        columns: &[
            "bootstrap_id",
            "installation_id",
            "subject_id",
            "state",
            "data",
            "created_at",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "agent_profiles",
        key: &["agent_id"],
        columns: &["agent_id", "data", "updated_at"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "memory_entries",
        key: &["entry_id"],
        columns: &[
            "entry_id",
            "agent_id",
            "kind",
            "content",
            "source",
            "applicability",
            "created_at",
            "updated_at",
            "deleted",
            "source_ref",
            "scope",
            "record_revision",
            "created_at_ms",
        ],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "memory_intents",
        key: &["intent_id"],
        columns: &[
            "intent_id",
            "agent_id",
            "kind",
            "scope",
            "content",
            "content_ref",
            "entry_id",
            "state",
            "reason",
            "created_at_ms",
            "updated_at_ms",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "raw_sessions",
        key: &["session_id"],
        columns: &["session_id", "agent_id", "started_at", "ended_at", "data"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "session_events",
        key: &["event_id"],
        columns: &[
            "event_id",
            "session_id",
            "seq",
            "event_type",
            "role",
            "data",
            "created_at",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_raw_inputs",
        key: &["step_id", "source_event_id"],
        columns: &[
            "step_id",
            "source_event_id",
            "raw_event_id",
            "session_id",
            "seq",
            "commit_seq",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "tasks",
        key: &["task_id"],
        columns: &["task_id", "record_revision", "data", "created_at"],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Task,
    },
    TableSpec {
        name: "operations",
        key: &["operation_id"],
        columns: &["operation_id", "record_revision", "data", "created_at"],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "attempts",
        key: &["attempt_id"],
        columns: &["attempt_id", "operation_id", "data", "created_at"],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "evidence_receipts",
        key: &["receipt_id"],
        columns: &["receipt_id", "operation_id", "data", "created_at"],
        local_seq: Some("updated_commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "task_results",
        key: &["result_id"],
        columns: &[
            "result_id",
            "task_id",
            "agent_id",
            "kind",
            "data",
            "commit_seq",
            "created_at",
        ],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "context_window_scopes",
        key: &["scope_id"],
        columns: &["scope_id", "model_generation", "profile_digest"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "context_windows",
        key: &["window_id"],
        columns: &["window_id", "scope_id", "generation", "body", "commit_seq"],
        local_seq: None,
        kind: RowKind::Verbatim,
    },
];

/// Everything one snapshot consists of.
struct Collected {
    rows: Vec<CognitionRow>,
    derivatives: Vec<CognitionDerivative>,
    objects: Vec<CognitionObjectDescriptor>,
    files: Vec<(CognitionFileDescriptor, PathBuf)>,
    git_history: Option<CognitionGitDescriptor>,
    git_bundle: Option<Vec<u8>>,
    git_refs: Vec<(String, String)>,
    gaps: Vec<CognitionGap>,
    agent_id: Option<EntityId>,
    high_water: LedgerSeq,
}

impl Store {
    /// Export the recovery snapshot of this store.
    ///
    /// The facts come from one consistent read snapshot. The CAS closure is
    /// verified object by object, recursively: every referenced object must
    /// exist and must hash to its digest, otherwise the snapshot is
    /// incomplete and cannot be claimed recoverable.
    pub fn export_cognition(
        &self,
        options: &CognitionSnapshotOptions,
    ) -> Result<CognitionExport> {
        let collected = self.collect_snapshot(options)?;
        let fingerprint = fingerprint_of(&collected);
        let files: Vec<CognitionFileDescriptor> = collected
            .files
            .iter()
            .map(|(descriptor, _)| descriptor.clone())
            .collect();
        let git_history = collected.git_history.clone();
        let manifest = CognitionManifest {
            schema: crate::cognition_archive::ARCHIVE_SCHEMA,
            fingerprint,
            source_ledger_id: self.ledger.ledger_id(),
            source_incarnation: self.capabilities.incarnation,
            agent_id: collected.agent_id,
            created_at: chrono::Utc::now(),
            high_water: collected.high_water,
            rows: collected.rows,
            derivatives: collected.derivatives,
            objects: collected.objects,
            files,
            git_history,
            gaps: collected.gaps,
        };
        Ok(CognitionExport {
            manifest,
            cas: Arc::clone(&self.cas),
            files: collected.files,
            git_bundle: collected.git_bundle,
        })
    }

    /// Write the recovery snapshot as a streamable archive.
    ///
    /// When a peer is given, the export is recorded for that peer: the peer
    /// now holds this snapshot, but it is not yet verified recoverable.
    pub fn write_cognition_archive<W: Write>(
        &self,
        options: &CognitionSnapshotOptions,
        peer_id: Option<EntityId>,
        mut writer: W,
    ) -> Result<CognitionReceipt> {
        let export = self.export_cognition(options)?;
        let receipt = export.write_archive(&mut writer)?;
        if let Some(peer_id) = peer_id {
            self.note_cognition_export(peer_id, &receipt)?;
        }
        Ok(receipt)
    }

    /// Import a recovery snapshot.
    ///
    /// Nothing is published before the whole closure arrived and verified: a
    /// missing object, a corrupt payload or an incomplete closure stops the
    /// import and leaves the receiving store untouched.
    pub fn import_cognition_archive<R: Read>(
        &self,
        reader: R,
        options: &CognitionImportOptions,
    ) -> Result<CognitionImportOutcome> {
        let peer_id = options.peer_id.unwrap_or(EntityId::nil());
        let mut files: Vec<(CognitionFileDescriptor, Vec<u8>)> = Vec::new();
        let mut git_history: Option<(CognitionGitDescriptor, Vec<u8>)> = None;
        let manifest = {
            let mut reader = reader;
            read_archive(
                &mut reader,
                |descriptor, bytes| {
                    self.cas.seal_object(
                        bytes,
                        &descriptor.media_type,
                        &descriptor.store_namespace,
                        &descriptor.codec.name,
                        descriptor.codec.major,
                    )?;
                    Ok(())
                },
                &mut files,
                &mut git_history,
            )?
        };
        let verified_objects = manifest.objects.len();

        let mut gaps = manifest.gaps.clone();
        gaps.extend(self.verify_manifest_closure(&manifest));
        let blocking: Vec<String> = gaps
            .iter()
            .filter(|gap| gap.blocking)
            .map(|gap| format!("{}: {}", gap.kind, gap.detail))
            .collect();
        if !blocking.is_empty() {
            bail!(
                "cognition snapshot is incomplete, nothing published: {}",
                blocking.join(", ")
            );
        }

        let mut conflicts: Vec<CognitionConflict> = Vec::new();
        let mut restored_git_history = false;
        if let Some(root) = options.memory_root.clone() {
            conflicts.extend(self.restore_files(&files, &root, &options.workspace_root)?);
            let (restored, conflict) = self.restore_git_history(&git_history, &root, peer_id)?;
            restored_git_history = restored;
            if let Some(conflict) = conflict {
                conflicts.push(conflict);
            }
        } else if files
            .iter()
            .any(|(descriptor, _)| descriptor.origin == CognitionFileOrigin::Memory)
        {
            bail!("cognition snapshot carries memory files but no memory root was given");
        } else if let Some((descriptor, bytes)) = git_history.as_ref() {
            // No memory root: the history cannot be placed, so the snapshot is
            // not complete.
            let _ = (descriptor, bytes);
            bail!("cognition snapshot carries memory history but no memory root was given");
        }
        if files
            .iter()
            .any(|(descriptor, _)| descriptor.origin == CognitionFileOrigin::Workspace)
            && options.workspace_root.is_none()
        {
            bail!("cognition snapshot carries workspace files but no workspace root was given");
        }

        let (inserted, unchanged, row_conflicts) = self.apply_rows(&manifest.rows, &manifest.derivatives, peer_id)?;
        conflicts.extend(row_conflicts);

        self.record_receipt(
            peer_id,
            &manifest,
            verified_objects,
            files.len(),
            inserted,
            unchanged,
            conflicts.len(),
        )?;

        Ok(CognitionImportOutcome {
            fingerprint: manifest.fingerprint.clone(),
            high_water: manifest.high_water,
            inserted,
            unchanged,
            conflicts,
            verified_objects,
            verified_files: files.len(),
            restored_git_history,
            gaps,
        })
    }

    /// Fingerprint of the snapshot this store currently holds.
    ///
    /// The fingerprint covers the replicated facts, the verified object
    /// closure and the memory files, so a memory or identity change is
    /// visible even when the ledger sequence did not move.
    pub fn cognition_fingerprint(
        &self,
        options: &CognitionSnapshotOptions,
    ) -> Result<(Digest, LedgerSeq)> {
        let collected = self.collect_snapshot(options)?;
        let fingerprint = fingerprint_of(&collected);
        let high_water = collected.high_water;
        self.store_local_fingerprint(&fingerprint, high_water)?;
        Ok((fingerprint, high_water))
    }

    /// Record that a peer received the given snapshot.
    pub fn note_cognition_export(
        &self,
        peer_id: EntityId,
        receipt: &CognitionReceipt,
    ) -> Result<()> {
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
        let conn = self.ledger.conn.lock();
        let sent: Option<(Option<String>, Option<i64>)> = conn
            .query_row(
                "SELECT exported_fingerprint,exported_high_water FROM cognition_peers WHERE peer_id=?1",
                [ack.peer_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let matches_sent = match sent {
            Some((Some(fingerprint), Some(high_water))) => {
                fingerprint == ack.fingerprint.to_string()
                    && high_water == ack.high_water.0 as i64
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
            exported_high_water: exported_high_water.map(LedgerSeq),
            exported_at: exported_at.and_then(|value| parse_time(&value)),
            acked_fingerprint: acked,
            acked_high_water: acked_high_water.map(LedgerSeq),
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
            let peer_id: EntityId = peer.parse().map_err(|_| anyhow!("invalid peer id {peer}"))?;
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
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    fn collect_snapshot(&self, options: &CognitionSnapshotOptions) -> Result<Collected> {
        let (rows, derivatives, agent_id, high_water) = self.snapshot_rows()?;
        let mut roots: Vec<ObjectRef> = Vec::new();
        let mut external: Vec<Digest> = Vec::new();
        for row in &rows {
            for (_, value) in &row.values {
                if let Some(value) = value.as_deref() {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(value) {
                        collect_references(&json, &mut roots, &mut external);
                    }
                }
            }
        }
        for digest in &external {
            if let Some(object) = self.bare_object(digest) {
                roots.push(object);
            }
        }
        let (objects, missing) = self.collect_closure(&roots)?;

        let mut files: Vec<(CognitionFileDescriptor, PathBuf)> = Vec::new();
        let mut gaps: Vec<CognitionGap> = Vec::new();
        if let Some(root) = &options.memory_root {
            collect_files(root, CognitionFileOrigin::Memory, &mut files, &mut gaps)?;
        }
        for root in &options.workspace_roots {
            collect_files(root, CognitionFileOrigin::Workspace, &mut files, &mut gaps)?;
        }
        let mut git_history: Option<CognitionGitDescriptor> = None;
        let mut git_bundle: Option<Vec<u8>> = None;
        let mut git_refs: Vec<(String, String)> = Vec::new();
        if options.include_git_history {
            if let Some(root) = &options.memory_root {
                match capture_git_history(root) {
                    Ok(Some((descriptor, bytes, refs))) => {
                        git_refs = refs;
                        git_bundle = Some(bytes);
                        git_history = Some(descriptor);
                    }
                    Ok(None) => {}
                    Err(error) => gaps.push(CognitionGap {
                        kind: "git_history".into(),
                        detail: error.to_string(),
                        blocking: true,
                    }),
                }
            }
        }
        for digest in missing {
            gaps.push(CognitionGap {
                kind: "missing_object".into(),
                detail: digest.to_string(),
                blocking: true,
            });
        }
        for digest in &external {
            if !self.cas.object_exists(digest) {
                gaps.push(CognitionGap {
                    kind: "external_artifact".into(),
                    detail: digest.to_string(),
                    blocking: false,
                });
            }
        }
        Ok(Collected {
            rows,
            derivatives,
            objects,
            files,
            git_history,
            git_bundle,
            git_refs,
            gaps,
            agent_id,
            high_water,
        })
    }

    /// One consistent read snapshot of the replicated facts.
    fn snapshot_rows(
        &self,
    ) -> Result<(Vec<CognitionRow>, Vec<CognitionDerivative>, Option<EntityId>, LedgerSeq)> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn.transaction().map_err(db)?;
        let mut rows: Vec<CognitionRow> = Vec::new();
        for spec in TABLES {
            rows.extend(read_table(&tx, spec)?);
        }
        let derivatives = read_derivatives(&tx)?;
        let agent_id: Option<String> = tx
            .query_row("SELECT subject_id FROM subject_genesis LIMIT 1", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(db)?;
        let high_water: i64 = tx
            .query_row("SELECT COALESCE(MAX(seq),0) FROM commit_log", [], |row| row.get(0))
            .map_err(db)?;
        tx.commit().map_err(db)?;
        let agent_id = match agent_id {
            Some(value) => Some(
                value
                    .parse::<EntityId>()
                    .map_err(|_| anyhow!("invalid stored agent id {value}"))?,
            ),
            None => None,
        };
        Ok((rows, derivatives, agent_id, LedgerSeq(high_water as u64)))
    }

    /// The verified CAS closure of the given roots, recursively.
    fn collect_closure(
        &self,
        roots: &[ObjectRef],
    ) -> Result<(Vec<CognitionObjectDescriptor>, Vec<Digest>)> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut missing: Vec<Digest> = Vec::new();
        let mut out: Vec<CognitionObjectDescriptor> = Vec::new();
        let mut queue: std::collections::VecDeque<ObjectRef> = roots.iter().cloned().collect();
        while let Some(object) = queue.pop_front() {
            if !seen.insert(object.digest.to_string()) {
                continue;
            }
            let bytes = match self.cas.read_object(&object.digest) {
                Ok(bytes) => bytes,
                Err(_) => {
                    missing.push(object.digest.clone());
                    continue;
                }
            };
            if bytes.len() as u64 != object.bytes || Digest::blake3(&bytes) != object.digest {
                missing.push(object.digest.clone());
                continue;
            }
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                let mut nested: Vec<ObjectRef> = Vec::new();
                let mut external: Vec<Digest> = Vec::new();
                collect_references(&json, &mut nested, &mut external);
                queue.extend(nested);
                for digest in external {
                    if let Some(bare) = self.bare_object(&digest) {
                        queue.push_back(bare);
                    }
                }
            }
            out.push(CognitionObjectDescriptor {
                digest: object.digest.clone(),
                bytes: object.bytes,
                media_type: object.media_type.clone(),
                store_namespace: object.store_namespace.clone(),
                codec: object.codec.clone(),
            });
        }
        Ok((out, missing))
    }

    /// A digest that exists in this store's CAS, seen as a plain object.
    fn bare_object(&self, digest: &Digest) -> Option<ObjectRef> {
        if !self.cas.object_exists(digest) {
            return None;
        }
        let bytes = self.cas.object_size(digest)?;
        Some(ObjectRef {
            digest: digest.clone(),
            bytes,
            media_type: "application/octet-stream".into(),
            store_namespace: "replicated".into(),
            locator: String::new(),
            codec: raw_codec(),
        })
    }

    /// Check on the receiving side that every referenced object really is in
    /// the archive, including the objects referenced by other objects.
    fn verify_manifest_closure(&self, manifest: &CognitionManifest) -> Vec<CognitionGap> {
        let listed: BTreeSet<String> = manifest
            .objects
            .iter()
            .map(|descriptor| descriptor.digest.to_string())
            .collect();
        let mut gaps: Vec<CognitionGap> = Vec::new();
        let mut roots: Vec<ObjectRef> = Vec::new();
        let mut _external: Vec<Digest> = Vec::new();
        for row in &manifest.rows {
            for (_, value) in &row.values {
                if let Some(value) = value.as_deref() {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(value) {
                        collect_references(&json, &mut roots, &mut _external);
                    }
                }
            }
        }
        for descriptor in &manifest.objects {
            if let Ok(bytes) = self.cas.read_object(&descriptor.digest) {
                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    let mut nested: Vec<ObjectRef> = Vec::new();
                    let mut _nested_external: Vec<Digest> = Vec::new();
                    collect_references(&json, &mut nested, &mut _nested_external);
                    roots.extend(nested);
                }
            }
        }
        for object in &roots {
            if !listed.contains(&object.digest.to_string()) {
                gaps.push(CognitionGap {
                    kind: "missing_object".into(),
                    detail: object.digest.to_string(),
                    blocking: true,
                });
            }
        }
        gaps
    }

    /// Apply the replicated facts to this store.
    ///
    /// A row that already exists with the same content is left alone, so a
    /// replay never duplicates a fact. A row that exists with different
    /// content keeps the local value and is reported as a conflict.
    fn apply_rows(
        &self,
        rows: &[CognitionRow],
        derivatives: &[CognitionDerivative],
        peer_id: EntityId,
    ) -> Result<(usize, usize, Vec<CognitionConflict>)> {
        let mut ordered: Vec<&CognitionRow> = rows.iter().collect();
        ordered.sort_by_key(|row| {
            TABLES
                .iter()
                .position(|spec| spec.name == row.table)
                .unwrap_or(usize::MAX)
        });
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(db)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let mut inserted = 0usize;
                let mut unchanged = 0usize;
                let mut conflicts: Vec<CognitionConflict> = Vec::new();
                for row in &ordered {
                    let Some(spec) = spec_for(&row.table) else {
                        continue;
                    };
                    let effective = effective_row(row, spec)?;
                    let local = read_existing(&tx, spec, &effective)?;
                    match local {
                        Some(local) => {
                            if rows_equivalent(spec, &local, &effective.values) {
                                unchanged += 1;
                            } else {
                                let detail = describe_difference(&local, &effective.values);
                                conflicts.push(CognitionConflict {
                                    table: spec.name.to_string(),
                                    key: effective.key.clone(),
                                    detail: detail.clone(),
                                });
                                record_conflict(&tx, peer_id, spec.name, &effective.key, &detail)?;
                            }
                        }
                        None => {
                            match insert_row(&tx, spec, &effective, seq) {
                                Ok(()) => inserted += 1,
                                Err(error) if is_constraint_violation(&error) => {
                                    let detail =
                                        "the imported fact breaks a local uniqueness rule; the local value is kept"
                                            .to_string();
                                    conflicts.push(CognitionConflict {
                                        table: spec.name.to_string(),
                                        key: effective.key.clone(),
                                        detail: detail.clone(),
                                    });
                                    record_conflict(
                                        &tx,
                                        peer_id,
                                        spec.name,
                                        &effective.key,
                                        &detail,
                                    )?;
                                }
                                Err(error) => return Err(error),
                            }
                        }
                    }
                }
                for derivative in derivatives {
                    ensure!(
                        derivative.blake3.as_str()
                            == blake3::hash(&derivative.bytes).to_hex().as_str(),
                        "context window derivative fails its blake3"
                    );
                    let exists: Option<i64> = tx
                        .query_row(
                            "SELECT 1 FROM context_window_derivatives WHERE window_id=?1 AND idx=?2",
                            params![derivative.window_id.to_string(), derivative.idx],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(db)?;
                    if exists.is_none() {
                        tx.execute(
                            "INSERT INTO context_window_derivatives(window_id,idx,media_type,blake3,bytes)
                             VALUES(?1,?2,?3,?4,?5)",
                            params![
                                derivative.window_id.to_string(),
                                derivative.idx,
                                derivative.media_type,
                                derivative.blake3,
                                derivative.bytes,
                            ],
                        )
                        .map_err(db)?;
                        inserted += 1;
                    } else {
                        unchanged += 1;
                    }
                }
                if inserted > 0 {
                    tx.execute(
                        "INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count)
                         VALUES(?1,?2,?3,?4)",
                        params![
                            self.ledger.ledger_id().to_string(),
                            seq.0 as i64,
                            self.capabilities.incarnation.to_string(),
                            inserted as i64,
                        ],
                    )
                    .map_err(db)?;
                }
                tx.commit().map_err(db)?;
                Ok((inserted, unchanged, conflicts))
            },
            storage_failure,
        )
    }

    /// Put the memory and workspace files back in place.
    ///
    /// A file that exists locally with different content keeps the local
    /// content and is reported as a conflict.
    fn restore_files(
        &self,
        files: &[(CognitionFileDescriptor, Vec<u8>)],
        memory_root: &Path,
        workspace_root: &Option<PathBuf>,
    ) -> Result<Vec<CognitionConflict>> {
        let mut conflicts: Vec<CognitionConflict> = Vec::new();
        for (descriptor, bytes) in files {
            let root = match descriptor.origin {
                CognitionFileOrigin::Memory => Some(memory_root.to_path_buf()),
                CognitionFileOrigin::Workspace => workspace_root.clone(),
            };
            let Some(root) = root else {
                conflicts.push(CognitionConflict {
                    table: "file".into(),
                    key: descriptor.path.clone(),
                    detail: "no target root configured for this file origin".into(),
                });
                continue;
            };
            let relative = safe_relative_path(&descriptor.path)?;
            let target = root.join(relative);
            if target.exists() {
                let existing = fs::read(&target)
                    .with_context(|| format!("read local file {}", target.display()))?;
                if existing == *bytes {
                    continue;
                }
                conflicts.push(CognitionConflict {
                    table: "file".into(),
                    key: descriptor.path.clone(),
                    detail: "local file differs; the local content is kept".into(),
                });
                continue;
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create directory {}", parent.display()))?;
            }
            fs::write(&target, bytes)
                .with_context(|| format!("write file {}", target.display()))?;
        }
        Ok(conflicts)
    }

    /// Put the memory repository history back in place.
    ///
    /// The history is fetched into a per peer namespace, so a local history is
    /// never overwritten. A store without local commits adopts the replicated
    /// head; a store with its own history keeps both and reports the
    /// divergence.
    fn restore_git_history(
        &self,
        git_history: &Option<(CognitionGitDescriptor, Vec<u8>)>,
        root: &Path,
        peer_id: EntityId,
    ) -> Result<(bool, Option<CognitionConflict>)> {
        let Some((descriptor, bytes)) = git_history.as_ref() else {
            return Ok((false, None));
        };
        let temp = std::env::temp_dir().join(format!("shenko-cognition-{}.bundle", EntityId::new()));
        fs::write(&temp, bytes).with_context(|| format!("write {}", temp.display()))?;
        let outcome = self.restore_git_history_inner(root, peer_id, descriptor, &temp);
        let _ = fs::remove_file(&temp);
        outcome
    }

    fn restore_git_history_inner(
        &self,
        root: &Path,
        peer_id: EntityId,
        descriptor: &CognitionGitDescriptor,
        bundle: &Path,
    ) -> Result<(bool, Option<CognitionConflict>)> {
        if !root.join(".git").exists() {
            fs::create_dir_all(root)?;
            let (ok, _, error) = git(root, &["init", "--quiet", "--initial-branch=main"])?;
            ensure!(ok, "git init failed: {error}");
        }
        let label = peer_id.to_string();
        let bundle_path = bundle.to_string_lossy().into_owned();
        let heads = format!("+refs/heads/*:refs/replicated/{label}/*");
        let tags = format!("+refs/tags/*:refs/replicated/{label}/tags/*");
        let (ok, _, error) = git(
            root,
            &[
                "fetch",
                "--quiet",
                bundle_path.as_str(),
                heads.as_str(),
                tags.as_str(),
            ],
        )?;
        ensure!(ok, "git fetch of the memory history failed: {error}");
        let (has_head, head_out, _) = git(root, &["rev-parse", "--verify", "HEAD"])?;
        if !has_head {
            if let Some((_, sha)) = descriptor
                .refs
                .iter()
                .find(|(name, _)| name == "refs/heads/main")
                .or_else(|| descriptor.refs.first())
            {
                let (ok, _, error) =
                    git(root, &["update-ref", "refs/heads/main", sha.as_str()])?;
                ensure!(ok, "git update-ref failed: {error}");
                let (ok, _, error) = git(root, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
                ensure!(ok, "git symbolic-ref failed: {error}");
                let (ok, _, error) = git(root, &["reset", "--mixed"])?;
                ensure!(ok, "git reset failed: {error}");
            }
            return Ok((true, None));
        }
        let local_head = head_out.trim().to_string();
        let replicated = descriptor
            .refs
            .iter()
            .find(|(name, _)| name == "refs/heads/main")
            .map(|(_, sha)| sha.clone());
        let conflict = match replicated {
            Some(remote) if remote != local_head => Some(CognitionConflict {
                table: "git".into(),
                key: "memory".into(),
                detail: format!(
                    "local head {local_head} differs from the replicated head {remote}; both histories are kept"
                ),
            }),
            _ => None,
        };
        Ok((true, conflict))
    }

    fn record_receipt(
        &self,
        peer_id: EntityId,
        manifest: &CognitionManifest,
        verified_objects: usize,
        verified_files: usize,
        inserted: usize,
        unchanged: usize,
        conflicts: usize,
    ) -> Result<()> {
        let conn = self.ledger.conn.lock();
        conn.execute(
            "INSERT INTO cognition_receipts(peer_id,fingerprint,high_water,verified_objects,verified_files,inserted,unchanged,conflicts,received_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(peer_id,fingerprint) DO UPDATE SET
                high_water=excluded.high_water,
                verified_objects=excluded.verified_objects,
                verified_files=excluded.verified_files,
                inserted=excluded.inserted,
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
                unchanged as i64,
                conflicts as i64,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn store_local_fingerprint(&self, fingerprint: &Digest, high_water: LedgerSeq) -> Result<()> {
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

/// Content fingerprint of a snapshot.
///
/// The fingerprint covers the replicated facts, the verified object closure
/// and the memory content. It deliberately does not cover the ledger
/// sequence or the export time: a snapshot that carries the same content is
/// the same snapshot, so recording an acknowledgement can never create new
/// debt and an identity or memory change is visible even when the ledger
/// sequence did not move.
fn fingerprint_of(collected: &Collected) -> Digest {
    let mut buffer: Vec<u8> = Vec::new();
    buffer.extend_from_slice(b"shenko-cognition-fingerprint/1\n");
    let mut rows: Vec<&CognitionRow> = collected.rows.iter().collect();
    rows.sort_by(|a, b| a.table.cmp(&b.table).then(a.key.cmp(&b.key)));
    for row in rows {
        buffer.extend_from_slice(b"row\t");
        buffer.extend_from_slice(row.table.as_bytes());
        buffer.push(b'\t');
        buffer.extend_from_slice(row.key.as_bytes());
        for (name, value) in &row.values {
            buffer.push(b'\t');
            buffer.extend_from_slice(name.as_bytes());
            buffer.push(b'=');
            if let Some(value) = value {
                buffer.extend_from_slice(value.as_bytes());
            }
        }
        buffer.push(b'\n');
    }
    let mut digests: Vec<String> = collected
        .objects
        .iter()
        .map(|descriptor| descriptor.digest.to_string())
        .collect();
    digests.sort();
    for digest in digests {
        buffer.extend_from_slice(b"object\t");
        buffer.extend_from_slice(digest.as_bytes());
        buffer.push(b'\n');
    }
    let mut files: Vec<(String, String)> = collected
        .files
        .iter()
        .map(|(descriptor, _)| (descriptor.path.clone(), descriptor.digest.to_string()))
        .collect();
    files.sort();
    for (path, digest) in files {
        buffer.extend_from_slice(b"file\t");
        buffer.extend_from_slice(path.as_bytes());
        buffer.push(b'\t');
        buffer.extend_from_slice(digest.as_bytes());
        buffer.push(b'\n');
    }
    let mut refs: Vec<&(String, String)> = collected.git_refs.iter().collect();
    refs.sort();
    for (name, sha) in refs {
        buffer.extend_from_slice(b"git\t");
        buffer.extend_from_slice(name.as_bytes());
        buffer.push(b'\t');
        buffer.extend_from_slice(sha.as_bytes());
        buffer.push(b'\n');
    }
    Digest::blake3(&buffer)
}

fn spec_for(table: &str) -> Option<&'static TableSpec> {
    TABLES.iter().find(|spec| spec.name == table)
}

/// The row as it will be stored here.
///
/// Imported work is parked: a receiving store is not a second live writer.
/// The original state is kept in a memo, so no fact is lost.
fn effective_row(row: &CognitionRow, spec: &TableSpec) -> Result<CognitionRow> {
    let mut effective = row.clone();
    match spec.kind {
        RowKind::Verbatim => {}
        RowKind::SubjectIntent => {
            let state = row.value("state").unwrap_or_default().to_string();
            if state == "ready" || state == "claimed" {
                set_value(&mut effective, "state", Some("unknown".into()));
                let result_ref = row.value("result_ref").unwrap_or_default();
                if result_ref.is_empty() {
                    set_value(
                        &mut effective,
                        "result_ref",
                        Some(format!("imported:source_state={state}")),
                    );
                }
            }
        }
        RowKind::Task => {
            let data = row.value("data").unwrap_or_default().to_string();
            let (transformed, revision) = transform_task(&data)?;
            set_value(&mut effective, "data", Some(transformed));
            if let Some(revision) = revision {
                set_value(
                    &mut effective,
                    "record_revision",
                    Some(revision.to_string()),
                );
            }
        }
    }
    Ok(effective)
}

/// Park an imported task and report the revision the local record carries.
fn transform_task(data: &str) -> Result<(String, Option<u64>)> {
    match serde_json::from_str::<TaskRecord>(data) {
        Ok(mut task) => {
            if task.is_terminal() {
                return Ok((data.to_string(), None));
            }
            if task.memos.contains_key("cognition_imported")
                && matches!(
                    task.dispatch_eligibility,
                    DispatchEligibility::Parked { .. }
                )
            {
                // Already parked by an earlier import. Parking it again would
                // invent a new revision, so the record stays as it is.
                return Ok((data.to_string(), None));
            }
            let checkpoint = checkpoint_of(&task.state);
            let source_state = serde_json::to_string(&task.state)?;
            task.state = TaskState::Pending { checkpoint_ref: checkpoint };
            task.dispatch_eligibility = DispatchEligibility::Parked {
                reason_ref: task.task_id,
                until_utc: None,
            };
            task.record_revision = task.record_revision.next();
            task.memos.insert(
                "cognition_imported".into(),
                serde_json::json!({
                    "source_state": source_state,
                    "note": "imported work is parked on the receiving store",
                }),
            );
            Ok((
                serde_json::to_string(&task)?,
                Some(task.record_revision.0),
            ))
        }
        Err(_) => {
            // The stored fact stays, but it must not become dispatchable here.
            let mut value: serde_json::Value = match serde_json::from_str(data) {
                Ok(value) => value,
                Err(_) => return Ok((data.to_string(), None)),
            };
            if !value.is_object() {
                return Ok((data.to_string(), None));
            }
            let task_id = value
                .get("task_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            value["state"] = serde_json::json!({"status": "pending", "checkpoint_ref": null});
            value["dispatch_eligibility"] = serde_json::json!({
                "status": "parked",
                "reason_ref": task_id,
                "until_utc": null,
            });
            Ok((serde_json::to_string(&value)?, None))
        }
    }
}

fn checkpoint_of(state: &TaskState) -> Option<ObjectRef> {
    match state {
        TaskState::Pending { checkpoint_ref } => checkpoint_ref.clone(),
        TaskState::Running { checkpoint_ref, .. } => checkpoint_ref.clone(),
        TaskState::Waiting { checkpoint_ref, .. } => checkpoint_ref.clone(),
        TaskState::Sleeping { checkpoint_ref, .. } => checkpoint_ref.clone(),
        TaskState::Completing { .. } | TaskState::Terminal { .. } => None,
    }
}

fn set_value(row: &mut CognitionRow, column: &str, value: Option<String>) {
    for entry in row.values.iter_mut() {
        if entry.0 == column {
            entry.1 = value;
            return;
        }
    }
}

/// Read one replicated table.
fn read_table(conn: &rusqlite::Connection, spec: &TableSpec) -> Result<Vec<CognitionRow>> {
    let columns = spec.columns.join(", ");
    let order = spec.key.join(", ");
    let sql = format!("SELECT {columns} FROM {table} ORDER BY {order}", table = spec.name);
    let mut stmt = conn.prepare(&sql).map_err(db)?;
    let mut rows: Vec<CognitionRow> = Vec::new();
    let mut cursor = stmt.query([]).map_err(db)?;
    while let Some(row) = cursor.next().map_err(db)? {
        let mut values: Vec<(String, Option<String>)> = Vec::with_capacity(spec.columns.len());
        for index in 0..spec.columns.len() {
            values.push((spec.columns[index].to_string(), column_text(row, index)?));
        }
        let key = spec
            .key
            .iter()
            .map(|name| {
                values
                    .iter()
                    .find(|(column, _)| column == name)
                    .and_then(|(_, value)| value.clone())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("\u{1f}");
        rows.push(CognitionRow {
            table: spec.name.to_string(),
            key,
            values,
        });
    }
    Ok(rows)
}

fn read_derivatives(conn: &rusqlite::Connection) -> Result<Vec<CognitionDerivative>> {
    let mut stmt = conn
        .prepare("SELECT window_id,idx,media_type,blake3,bytes FROM context_window_derivatives ORDER BY window_id,idx")
        .map_err(db)?;
    let mut out: Vec<CognitionDerivative> = Vec::new();
    let mut cursor = stmt.query([]).map_err(db)?;
    while let Some(row) = cursor.next().map_err(db)? {
        let window_id: String = row.get(0).map_err(db)?;
        out.push(CognitionDerivative {
            window_id: window_id
                .parse()
                .map_err(|_| anyhow!("invalid context window id {window_id}"))?,
            idx: row.get(1).map_err(db)?,
            media_type: row.get(2).map_err(db)?,
            blake3: row.get(3).map_err(db)?,
            bytes: row.get(4).map_err(db)?,
        });
    }
    Ok(out)
}

fn column_text(row: &rusqlite::Row<'_>, index: usize) -> Result<Option<String>> {
    match row.get_ref(index).map_err(db)? {
        ValueRef::Null => Ok(None),
        ValueRef::Integer(value) => Ok(Some(value.to_string())),
        ValueRef::Real(value) => Ok(Some(value.to_string())),
        ValueRef::Text(value) => Ok(Some(String::from_utf8_lossy(value).into_owned())),
        ValueRef::Blob(_) => Err(anyhow!("column {index} holds binary data and is not replicated")),
    }
}

/// The stored row with the same primary key, if it exists.
fn read_existing(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    row: &CognitionRow,
) -> Result<Option<Vec<(String, Option<String>)>>> {
    let mut conditions: Vec<String> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    for (index, name) in spec.key.iter().enumerate() {
        conditions.push(format!("{name}=?{}", index + 1));
        keys.push(row.value(name).unwrap_or_default().to_string());
    }
    let sql = format!(
        "SELECT {} FROM {table} WHERE {conditions}",
        spec.columns.join(", "),
        table = spec.name,
        conditions = conditions.join(" AND ")
    );
    let mut stmt = tx.prepare(&sql).map_err(db)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(keys)).map_err(db)?;
    let mut found: Option<Vec<(String, Option<String>)>> = None;
    while let Some(row) = rows.next().map_err(db)? {
        let mut values: Vec<(String, Option<String>)> = Vec::with_capacity(spec.columns.len());
        for index in 0..spec.columns.len() {
            values.push((spec.columns[index].to_string(), column_text(row, index)?));
        }
        found = Some(values);
    }
    Ok(found)
}

/// Insert one replicated row.
fn insert_row(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    row: &CognitionRow,
    seq: LedgerSeq,
) -> Result<()> {
    let mut columns: Vec<&str> = spec.columns.to_vec();
    let mut values: Vec<Option<String>> = row
        .values
        .iter()
        .map(|(_, value)| value.clone())
        .collect();
    if let Some(local_seq) = spec.local_seq {
        columns.push(local_seq);
        values.push(Some(seq.0.to_string()));
    }
    let placeholders: Vec<String> = (1..=columns.len()).map(|index| format!("?{index}")).collect();
    let sql = format!(
        "INSERT INTO {table} ({columns}) VALUES ({placeholders})",
        table = spec.name,
        columns = columns.join(", "),
        placeholders = placeholders.join(", ")
    );
    tx.execute(&sql, rusqlite::params_from_iter(values)).map_err(db)?;
    Ok(())
}

fn record_conflict(
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

fn describe_difference(
    local: &[(String, Option<String>)],
    incoming: &[(String, Option<String>)],
) -> String {
    for (index, (name, value)) in incoming.iter().enumerate() {
        let other = local.get(index).and_then(|(_, other)| other.as_ref());
        if other != value.as_ref() {
            return format!("column {name} differs; the local value is kept");
        }
    }
    "the rows differ; the local value is kept".into()
}

/// Are the stored row and the imported row the same fact?
///
/// A subject intent carries a dispatch state. The receiving store adapts that
/// state, because it must not dispatch an intent it did not create, and it
/// remembers the state the source recorded. Both sides therefore hold the
/// same fact even when their local dispatch state differs.
fn rows_equivalent(
    spec: &TableSpec,
    local: &[(String, Option<String>)],
    incoming: &[(String, Option<String>)],
) -> bool {
    if spec.kind == RowKind::SubjectIntent {
        return intent_equivalent(local, incoming);
    }
    local == incoming
}

fn intent_equivalent(
    local: &[(String, Option<String>)],
    incoming: &[(String, Option<String>)],
) -> bool {
    if intent_source_state(local) != intent_source_state(incoming) {
        return false;
    }
    if local.len() != incoming.len() {
        return false;
    }
    for (index, (name, value)) in incoming.iter().enumerate() {
        if name == "state" || name == "result_ref" {
            // The dispatch state is a local adaptation of the same fact.
            continue;
        }
        let other = local.get(index).and_then(|(_, other)| other.as_ref());
        if other != value.as_ref() {
            return false;
        }
    }
    true
}

/// The dispatch state as the source recorded it.
fn intent_source_state(values: &[(String, Option<String>)]) -> Option<String> {
    let state = values
        .iter()
        .find(|(name, _)| name == "state")
        .and_then(|(_, value)| value.clone())?;
    let marker = values
        .iter()
        .find(|(name, _)| name == "result_ref")
        .and_then(|(_, value)| value.clone())
        .unwrap_or_default();
    if state == "unknown" {
        if let Some(source) = marker.strip_prefix("imported:source_state=") {
            return Some(source.to_string());
        }
    }
    Some(state)
}

/// Collect every CAS reference of a JSON value.
fn collect_references(
    value: &serde_json::Value,
    roots: &mut Vec<ObjectRef>,
    external: &mut Vec<Digest>,
) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(object) = object_ref_from(map) {
                roots.push(object);
                return;
            }
            if let Some(digest) = digest_from(map) {
                external.push(digest);
                return;
            }
            for nested in map.values() {
                collect_references(nested, roots, external);
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items {
                collect_references(nested, roots, external);
            }
        }
        serde_json::Value::String(text) => {
            if let Some(digest) = parse_digest(text) {
                external.push(digest);
            }
        }
        _ => {}
    }
}

fn object_ref_from(map: &serde_json::Map<String, serde_json::Value>) -> Option<ObjectRef> {
    let digest = map.get("digest").and_then(serde_json::Value::as_str)?;
    let digest = parse_digest(digest)?;
    let bytes = map.get("bytes").and_then(serde_json::Value::as_u64)?;
    let media_type = map.get("media_type").and_then(serde_json::Value::as_str)?;
    let store_namespace = map.get("store_namespace").and_then(serde_json::Value::as_str)?;
    let codec: SchemaRef = serde_json::from_value(map.get("codec")?.clone()).ok()?;
    Some(ObjectRef {
        digest,
        bytes,
        media_type: media_type.to_string(),
        store_namespace: store_namespace.to_string(),
        locator: String::new(),
        codec,
    })
}

fn digest_from(map: &serde_json::Map<String, serde_json::Value>) -> Option<Digest> {
    let digest = map.get("digest").and_then(serde_json::Value::as_str)?;
    parse_digest(digest)
}

fn parse_digest(text: &str) -> Option<Digest> {
    let hex = text.strip_prefix("blake3:")?;
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(Digest(text.to_string()))
}

fn raw_codec() -> SchemaRef {
    SchemaRef {
        name: "raw".into(),
        major: 1,
        digest: Digest::blake3(b"raw"),
    }
}

/// Collect the files of one root. The Git directory of the memory root is
/// not copied as files; its history travels as a Git bundle.
fn collect_files(
    root: &Path,
    origin: CognitionFileOrigin,
    out: &mut Vec<(CognitionFileDescriptor, PathBuf)>,
    gaps: &mut Vec<CognitionGap>,
) -> Result<()> {
    if !root.exists() {
        gaps.push(CognitionGap {
            kind: "missing_root".into(),
            detail: root.display().to_string(),
            blocking: true,
        });
        return Ok(());
    }
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                gaps.push(CognitionGap {
                    kind: "unreadable_directory".into(),
                    detail: format!("{}: {error}", dir.display()),
                    blocking: true,
                });
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    gaps.push(CognitionGap {
                        kind: "unreadable_entry".into(),
                        detail: format!("{}: {error}", dir.display()),
                        blocking: true,
                    });
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if prefix.is_empty() && name == ".git" {
                continue;
            }
            let path = entry.path();
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    gaps.push(CognitionGap {
                        kind: "unreadable_entry".into(),
                        detail: format!("{}: {error}", path.display()),
                        blocking: true,
                    });
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                gaps.push(CognitionGap {
                    kind: "symlink".into(),
                    detail: relative,
                    blocking: true,
                });
                continue;
            }
            if metadata.is_dir() {
                stack.push((path, relative));
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    gaps.push(CognitionGap {
                        kind: "unreadable_file".into(),
                        detail: format!("{}: {error}", path.display()),
                        blocking: true,
                    });
                    continue;
                }
            };
            out.push((
                CognitionFileDescriptor {
                    path: relative,
                    bytes: bytes.len() as u64,
                    digest: Digest::blake3(&bytes),
                    origin,
                },
                path,
            ));
        }
    }
    out.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    Ok(())
}

/// Capture the history of a Git managed memory root.
///
/// The bundle carries the objects and the refs only. The repository's own
/// configuration, and with it any local credential, never leaves the node.
fn capture_git_history(
    root: &Path,
) -> Result<Option<(CognitionGitDescriptor, Vec<u8>, Vec<(String, String)>)>> {
    if !root.join(".git").exists() {
        return Ok(None);
    }
    let (has_head, _, _) = git(root, &["rev-parse", "--verify", "HEAD"])?;
    if !has_head {
        return Ok(None);
    }
    let temp = std::env::temp_dir().join(format!("shenko-cognition-{}.bundle", EntityId::new()));
    let temp_path = temp.to_string_lossy().into_owned();
    let (ok, _, error) = git(root, &["bundle", "create", temp_path.as_str(), "--all"])?;
    if !ok {
        let _ = fs::remove_file(&temp);
        bail!("git bundle create failed: {error}");
    }
    let bytes = fs::read(&temp).with_context(|| format!("read {}", temp.display()))?;
    let _ = fs::remove_file(&temp);
    let mut refs: Vec<(String, String)> = Vec::new();
    let (ok, out, _) = git(root, &["for-each-ref", "--format=%(refname) %(objectname)"])?;
    if ok {
        for line in out.lines() {
            if let Some((name, sha)) = line.split_once(' ') {
                refs.push((name.to_string(), sha.to_string()));
            }
        }
    }
    let descriptor = CognitionGitDescriptor {
        bytes: bytes.len() as u64,
        digest: Digest::blake3(&bytes),
        refs: refs.clone(),
    };
    Ok(Some((descriptor, bytes, refs)))
}

fn git(root: &Path, args: &[&str]) -> Result<(bool, String, String)> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

fn safe_relative_path(path: &str) -> Result<PathBuf> {
    let candidate = Path::new(path);
    ensure!(!candidate.is_absolute(), "file path {path} is not relative");
    let mut out = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => bail!("file path {path} escapes the target root"),
        }
    }
    ensure!(!out.as_os_str().is_empty(), "file path is empty");
    Ok(out)
}

fn parse_time(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&chrono::Utc))
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
