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

use std::collections::{BTreeSet, HashMap, HashSet};
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
use shenko_types::subject::{SubjectBranchHead, SubjectMode, SubjectStepState};
use shenko_types::task::{DispatchEligibility, TaskRecord, TaskState};

use crate::cas::ContentStore;
use crate::child::COGNITION_IMPORTED;
use crate::cognition_archive::{
    CognitionDerivative, CognitionFileDescriptor, CognitionFileOrigin, CognitionGap,
    CognitionGitDescriptor, CognitionManifest, CognitionObjectDescriptor, CognitionRow,
    read_archive, write_manifest, write_payload,
};
use crate::store::Store;

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
    /// An inbox event. The pending input is recovery data; the local
    /// consumption cursor is not.
    InboxEvent,
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
        columns: &[
            "genesis_id",
            "subject_id",
            "bootstrap_id",
            "data",
            "created_at",
        ],
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
        name: "subject_identity_receipts",
        key: &["step_id"],
        columns: &["step_id", "data"],
        // The receipt's local commit position follows the receiving store's
        // import commit. Its identity outcome, step binding and source
        // `data.committed_at` coordinate stay verbatim.
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    // Wait facts and receipts travel with the branch and their referenced
    // sealed inputs; their local commit positions belong to the receiver.
    TableSpec {
        name: "semantic_waits",
        key: &["wait_id"],
        columns: &["wait_id", "agent_id", "status", "data"],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "semantic_wait_receipts",
        key: &["step_id"],
        columns: &["step_id", "data"],
        local_seq: Some("commit_seq"),
        kind: RowKind::Verbatim,
    },
    TableSpec {
        name: "subject_event_dispositions",
        key: &["branch_id", "event_id"],
        columns: &[
            "branch_id",
            "event_id",
            "step_id",
            "disposition",
            "commit_seq",
        ],
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
        name: "memory_intent_bindings",
        key: &["intent_id"],
        columns: &["intent_id", "data"],
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
        name: "recovery_sources",
        key: &["source_id"],
        columns: &["source_id", "kind", "data"],
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
        name: "inbox_events",
        key: &["event_id"],
        columns: &[
            "event_id",
            "agent_id",
            "kind",
            "source",
            "consumed",
            "data",
            "created_at",
        ],
        local_seq: None,
        kind: RowKind::InboxEvent,
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
    file_origins: Vec<CognitionFileOrigin>,
    git_history: Option<CognitionGitDescriptor>,
    git_bundle: Option<Vec<u8>>,
    gaps: Vec<CognitionGap>,
    agent_id: Option<EntityId>,
    high_water: LedgerSeq,
    snapshot_seq: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileBaselineState {
    Present,
    LocalDeleted,
    Deleted,
}

impl FileBaselineState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::LocalDeleted => "local_deleted",
            Self::Deleted => "deleted",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "present" => Ok(Self::Present),
            "local_deleted" => Ok(Self::LocalDeleted),
            "deleted" => Ok(Self::Deleted),
            _ => bail!("invalid cognition file baseline state {value}"),
        }
    }
}

struct FileBaseline {
    path: String,
    digest: Digest,
    state: FileBaselineState,
}

struct GitBaseline {
    head: String,
    // Older local baselines did not record the symbolic ref. Their commit is
    // still useful; the next matching import binds the exact source ref.
    reference: Option<String>,
}

impl Store {
    /// Export the recovery snapshot of this store.
    ///
    /// The facts come from one consistent read snapshot. The CAS closure is
    /// verified object by object, recursively: every referenced object must
    /// exist and must hash to its digest, otherwise the snapshot is
    /// incomplete and cannot be claimed recoverable.
    pub fn export_cognition(&self, options: &CognitionSnapshotOptions) -> Result<CognitionExport> {
        let collected = self.collect_snapshot(options, true)?;
        let fingerprint = fingerprint_of(&collected);
        let source_snapshot_seq = collected
            .snapshot_seq
            .ok_or_else(|| anyhow!("cognition export did not allocate its durable order"))?;
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
            source_snapshot_seq,
            agent_id: collected.agent_id,
            created_at: chrono::Utc::now(),
            high_water: collected.high_water,
            rows: collected.rows,
            derivatives: collected.derivatives,
            objects: collected.objects,
            file_origins: collected.file_origins,
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
        let mut import_gate = self.cognition_import_gate.lock();
        let _gate_tx = import_gate.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
        let fingerprint = fingerprint_of_manifest(&manifest);
        ensure!(
            fingerprint == manifest.fingerprint,
            "cognition manifest fingerprint does not match its received rows and verified payload descriptors"
        );
        validate_cognition_rows(&manifest)?;

        let mut gaps = manifest.gaps.clone();
        gaps.extend(memory_intent_binding_gaps(&manifest.rows));
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
        self.check_import_order(&manifest)?;
        // Reject unresumable imported heads or a conflicting paused successor
        // before publishing files or rows. Recheck under the mutation
        // transaction as the local subject may reach a boundary meanwhile.
        {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction().map_err(db)?;
            validate_subject_boundaries(&tx, &manifest)?;
            tx.commit().map_err(db)?;
        }

        for descriptor in &manifest.files {
            ensure!(
                manifest.file_origins.contains(&descriptor.origin),
                "cognition file {} has an origin outside the declared snapshot coverage",
                descriptor.path
            );
        }
        ensure!(
            !manifest.file_origins.contains(&CognitionFileOrigin::Memory)
                || options.memory_root.is_some(),
            "cognition snapshot covers memory files but no memory root was given"
        );
        ensure!(
            !manifest
                .file_origins
                .contains(&CognitionFileOrigin::Workspace)
                || options.workspace_root.is_some(),
            "cognition snapshot covers workspace files but no workspace root was given"
        );
        ensure!(
            manifest.git_history.is_none() || options.memory_root.is_some(),
            "cognition snapshot carries memory history but no memory root was given"
        );

        let mut conflicts: Vec<CognitionConflict> = Vec::new();
        let mut restored_git_history = false;
        let (verified_files, file_conflicts) = self.restore_files(
            &files,
            &options.memory_root,
            &options.workspace_root,
            &manifest.file_origins,
            peer_id,
        )?;
        conflicts.extend(file_conflicts);
        if let Some(root) = options.memory_root.clone() {
            if git_history.is_some() {
                let (restored, conflict) =
                    self.restore_git_history(&git_history, &root, peer_id)?;
                restored_git_history = restored;
                if let Some(conflict) = conflict {
                    conflicts.push(conflict);
                }
            }
        }

        let applied = self.apply_rows(&manifest, peer_id)?;
        conflicts.extend(applied.conflicts.clone());

        self.record_receipt(
            peer_id,
            &manifest,
            verified_objects,
            verified_files,
            applied.inserted,
            applied.updated,
            applied.unchanged,
            conflicts.len(),
        )?;

        Ok(CognitionImportOutcome {
            fingerprint,
            high_water: manifest.high_water,
            inserted: applied.inserted,
            updated: applied.updated,
            unchanged: applied.unchanged,
            conflicts,
            verified_objects,
            verified_files,
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
        let collected = self.collect_snapshot(options, false)?;
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

    fn collect_snapshot(
        &self,
        options: &CognitionSnapshotOptions,
        allocate_order: bool,
    ) -> Result<Collected> {
        let (rows, derivatives, agent_id, high_water, snapshot_seq) =
            self.snapshot_rows(allocate_order)?;
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
        let retired_roots = {
            let conn = self.ledger.conn.lock();
            let mut stmt = conn.prepare("SELECT path FROM cognition_retirements")?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows.into_iter().map(PathBuf::from).collect::<BTreeSet<_>>()
        };
        let mut file_origins = Vec::new();
        let mut gaps = memory_intent_binding_gaps(&rows);
        if let Some(root) = &options.memory_root {
            file_origins.push(CognitionFileOrigin::Memory);
            collect_files(
                root,
                CognitionFileOrigin::Memory,
                &retired_roots,
                &mut files,
                &mut gaps,
            )?;
        }
        if !options.workspace_roots.is_empty() {
            file_origins.push(CognitionFileOrigin::Workspace);
            for root in &options.workspace_roots {
                collect_files(
                    root,
                    CognitionFileOrigin::Workspace,
                    &retired_roots,
                    &mut files,
                    &mut gaps,
                )?;
            }
        }
        file_origins.sort_unstable();
        let mut git_history: Option<CognitionGitDescriptor> = None;
        let mut git_bundle: Option<Vec<u8>> = None;
        if options.include_git_history {
            if let Some(root) = &options.memory_root {
                match capture_git_history(root) {
                    Ok(Some((descriptor, bytes))) => {
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
            file_origins,
            git_history,
            git_bundle,
            gaps,
            agent_id,
            high_water,
            snapshot_seq,
        })
    }

    /// One consistent read snapshot of the replicated facts.
    fn snapshot_rows(
        &self,
        allocate_order: bool,
    ) -> Result<(
        Vec<CognitionRow>,
        Vec<CognitionDerivative>,
        Option<EntityId>,
        LedgerSeq,
        Option<u64>,
    )> {
        let mut conn = self.ledger.conn.lock();
        let behavior = if allocate_order {
            TransactionBehavior::Immediate
        } else {
            TransactionBehavior::Deferred
        };
        let tx = conn.transaction_with_behavior(behavior).map_err(db)?;
        // The subject boundary request is local controller state, not part of
        // this archive schema. A paused head without its request would strand
        // the receiver and must never be acknowledged as a complete snapshot.
        let paused: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM subject_branch_heads WHERE mode='paused')",
            [],
            |row| row.get(0),
        ).map_err(db)?;
        ensure!(!paused, "cognition snapshot unavailable while a local subject boundary is paused; preserve the complete store or resume before exporting");
        let snapshot_seq = if allocate_order {
            let current: Option<i64> = tx
                .query_row(
                    "SELECT sequence FROM cognition_export_order WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db)?;
            let next = current.unwrap_or(0).checked_add(1).ok_or_else(|| {
                anyhow!("cognition export order exhausted its durable integer range")
            })?;
            tx.execute(
                "INSERT INTO cognition_export_order(singleton,sequence) VALUES(1,?1)
                 ON CONFLICT(singleton) DO UPDATE SET sequence=excluded.sequence",
                [next],
            )
            .map_err(db)?;
            Some(u64::try_from(next)?)
        } else {
            None
        };
        let mut rows: Vec<CognitionRow> = Vec::new();
        for spec in TABLES {
            rows.extend(read_table(&tx, spec)?);
        }
        let derivatives = read_derivatives(&tx)?;
        let agent_id: Option<String> = tx
            .query_row(
                "SELECT subject_id FROM subject_genesis LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(db)?;
        let high_water: i64 = tx
            .query_row("SELECT COALESCE(MAX(seq),0) FROM commit_log", [], |row| {
                row.get(0)
            })
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
        Ok((
            rows,
            derivatives,
            agent_id,
            LedgerSeq(high_water as u64),
            snapshot_seq,
        ))
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
            let bytes = match self.cas.read_object(&descriptor.digest) {
                Ok(bytes) => bytes,
                Err(error) => {
                    gaps.push(CognitionGap {
                        kind: "unreadable_retained_object".into(),
                        detail: format!("{}: {error}", descriptor.digest),
                        blocking: true,
                    });
                    continue;
                }
            };
            if bytes.len() as u64 != descriptor.bytes || Digest::blake3(&bytes) != descriptor.digest
            {
                gaps.push(CognitionGap {
                    kind: "corrupt_retained_object".into(),
                    detail: descriptor.digest.to_string(),
                    blocking: true,
                });
                continue;
            }
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                let mut nested: Vec<ObjectRef> = Vec::new();
                let mut _nested_external: Vec<Digest> = Vec::new();
                collect_references(&json, &mut nested, &mut _nested_external);
                roots.extend(nested);
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

    fn check_import_order(&self, manifest: &CognitionManifest) -> Result<()> {
        let conn = self.ledger.conn.lock();
        check_import_order_conn(&conn, manifest)
    }

    /// Apply the replicated facts to this store.
    ///
    /// A row that already exists with the same content is left alone, so a
    /// replay never duplicates a fact. A row that this store still holds
    /// exactly as the peer last delivered it takes the newer value, so a
    /// continuing source advances instead of looking like a divergence. A row
    /// that this store changed on its own keeps the local value and is
    /// reported as a conflict.
    fn apply_rows(
        &self,
        manifest: &CognitionManifest,
        peer_id: EntityId,
    ) -> Result<CognitionApplyOutcome> {
        let mut ordered: Vec<&CognitionRow> = manifest.rows.iter().collect();
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
                validate_cognition_rows(manifest)?;
                check_import_order_conn(&tx, manifest)?;
                validate_subject_boundaries(&tx, manifest)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let mut inserted = 0usize;
                let mut updated = 0usize;
                let mut unchanged = 0usize;
                let mut conflicts: Vec<CognitionConflict> = Vec::new();
                let referenced_memory_entries: HashSet<&str> = ordered.iter()
                    .filter(|row| row.table == "memory_intents")
                    .filter_map(|row| row.value("entry_id"))
                    .collect();
                let mut memory_applied = false;
                let mut task_closure_applied = false;
                let mut task_owned_rows = HashSet::new();
                for row in &ordered {
                    if !memory_applied && matches!(row.table.as_str(), "memory_entries" | "memory_intents" | "memory_intent_bindings") {
                        let outcome = apply_memory_pairs(&tx, &ordered, peer_id, seq)?;
                        inserted += outcome.inserted;
                        updated += outcome.updated;
                        unchanged += outcome.unchanged;
                        conflicts.extend(outcome.conflicts);
                        memory_applied = true;
                    }
                    // A task and every dependent execution/result fact is ONE
                    // conflict unit. Importing remote operations, evidence or
                    // results under a task_id whose local input differs would
                    // stitch unrelated execution history onto the local task.
                    if !task_closure_applied
                        && matches!(row.table.as_str(), "session_events" | "subject_raw_inputs" | "inbox_events"
                            | "tasks" | "operations" | "attempts" | "evidence_receipts" | "task_results")
                    {
                        // raw_sessions and subject_steps have already applied,
                        // but NO pending/raw/staged delivery has been published.
                        let (outcome, owned, rejected, rejected_ids) =
                            apply_task_closures(&tx, &ordered, peer_id, seq, manifest)?;
                        inserted += outcome.inserted;
                        updated += outcome.updated;
                        unchanged += outcome.unchanged;
                        conflicts.extend(outcome.conflicts);
                        task_owned_rows = owned;
                        // Whole-publication boundary: if any consumer
                        // committed earlier in this transaction (subject
                        // step's frozen S0, context window chain, manifest
                        // derivatives) depends on a rejected delivery and the
                        // dependency cannot be proven removable without
                        // mutating sealed identities, reject the whole
                        // import instead of publishing a partial falsehood.
                        verify_no_rejected_task_consumers(
                            self, &tx, &ordered, &manifest.derivatives,
                            &rejected, &rejected_ids,
                        )?;
                        task_closure_applied = true;
                    }
                    if row.table == "memory_intents" || row.table == "memory_intent_bindings"
                        || (row.table == "memory_entries" && referenced_memory_entries.contains(row.key.as_str()))
                    {
                        continue;
                    }
                    if task_owned_rows.contains(&(row.table.as_str(), row.key.as_str())) {
                        continue;
                    }
                    let Some(spec) = spec_for(&row.table) else {
                        continue;
                    };
                    let effective = effective_row(row, spec)?;
                    let local = read_existing(&tx, spec, &effective)?;
                    let baseline = read_baseline(&tx, peer_id, spec.name, &effective.key)?;
                    match local {
                        None => {
                            match insert_row(&tx, spec, &effective, seq) {
                                Ok(()) => {
                                    inserted += 1;
                                    write_baseline(
                                        &tx,
                                        peer_id,
                                        spec.name,
                                        &effective.key,
                                        &effective.values,
                                    )?;
                                    record_origin(
                                        &tx,
                                        spec,
                                        &effective,
                                        peer_id,
                                        &manifest.fingerprint,
                                        manifest.source_ledger_id,
                                    )?;
                                }
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
                        Some(local_values) => {
                            if rows_equivalent(spec, &local_values, &effective.values) {
                                unchanged += 1;
                                write_baseline(
                                    &tx,
                                    peer_id,
                                    spec.name,
                                    &effective.key,
                                    &effective.values,
                                )?;
                            } else if let Some(baseline_values) = baseline.as_ref() {
                                if rows_equivalent(spec, &local_values, baseline_values) {
                                    // This store still holds the value the
                                    // peer last delivered, so the newer
                                    // value is the peer's continuation, not
                                    // a divergence.
                                    update_row(&tx, spec, &effective, seq)?;
                                    updated += 1;
                                    write_baseline(
                                        &tx,
                                        peer_id,
                                        spec.name,
                                        &effective.key,
                                        &effective.values,
                                    )?;
                                    record_origin(
                                        &tx,
                                        spec,
                                        &effective,
                                        peer_id,
                                        &manifest.fingerprint,
                                        manifest.source_ledger_id,
                                    )?;
                                } else {
                                    let detail = describe_difference(
                                        &local_values,
                                        &effective.values,
                                    );
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
                            } else {
                                // The fact was not delivered by this peer, so
                                // the local value is not a copy of it.
                                let detail = describe_difference(
                                    &local_values,
                                    &effective.values,
                                );
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
                        }
                    }
                }
                for derivative in &manifest.derivatives {
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
                if inserted > 0 || updated > 0 {
                    tx.execute(
                        "INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count)
                         VALUES(?1,?2,?3,?4)",
                        params![
                            self.ledger.ledger_id().to_string(),
                            seq.0 as i64,
                            self.capabilities.incarnation.to_string(),
                            (inserted + updated) as i64,
                        ],
                    )
                    .map_err(db)?;
                }
                record_import_order(&tx, peer_id, manifest)?;
                tx.commit().map_err(db)?;
                Ok(CognitionApplyOutcome {
                    inserted,
                    updated,
                    unchanged,
                    conflicts,
                })
            },
            storage_failure,
        )
    }

    /// Put the memory and workspace files back in place.
    ///
    /// A file that this store still holds exactly as the peer last delivered
    /// it takes the newer content, so a continuing source advances. A file
    /// that this store changed on its own keeps the local content and is
    /// reported as a conflict.
    fn restore_files(
        &self,
        files: &[(CognitionFileDescriptor, Vec<u8>)],
        memory_root: &Option<PathBuf>,
        workspace_root: &Option<PathBuf>,
        origins: &[CognitionFileOrigin],
        peer_id: EntityId,
    ) -> Result<(usize, Vec<CognitionConflict>)> {
        let mut verified = 0usize;
        let mut conflicts: Vec<CognitionConflict> = Vec::new();
        for file_origin in origins {
            let origin = file_origin_name(*file_origin);
            let root = match file_origin {
                CognitionFileOrigin::Memory => memory_root.clone(),
                CognitionFileOrigin::Workspace => workspace_root.clone(),
            }
            .ok_or_else(|| anyhow!("no target root configured for {origin} files"))?;
            let incoming_paths: BTreeSet<&str> = files
                .iter()
                .filter(|(descriptor, _)| descriptor.origin == *file_origin)
                .map(|(descriptor, _)| descriptor.path.as_str())
                .collect();
            let baselines = self.read_file_baselines(peer_id, origin)?;
            for (descriptor, bytes) in files
                .iter()
                .filter(|(descriptor, _)| descriptor.origin == *file_origin)
            {
                let relative = safe_relative_path(&descriptor.path)?;
                let target = root.join(&relative);
                let baseline = self.read_file_baseline(peer_id, origin, &descriptor.path)?;
                if target.exists() {
                    if !fs::symlink_metadata(&target)?.file_type().is_file() {
                        conflicts.push(CognitionConflict {
                            table: "file".into(),
                            key: descriptor.path.clone(),
                            detail: "the local path is no longer a regular file; its independent replacement is kept".into(),
                        });
                        continue;
                    }
                    let existing = fs::read(&target)
                        .with_context(|| format!("read local file {}", target.display()))?;
                    if existing == *bytes {
                        self.write_file_baseline(
                            peer_id,
                            origin,
                            &descriptor.path,
                            &descriptor.digest,
                            FileBaselineState::Present,
                        )?;
                        verified += 1;
                        continue;
                    }
                    let local_digest = Digest::blake3(&existing);
                    if baseline.as_ref().is_some_and(|baseline| {
                        baseline.state == FileBaselineState::Present
                            && baseline.digest == local_digest
                    }) {
                        // Only an untouched peer baseline may advance. A local
                        // recreation after a deletion is independent content.
                        fs::write(&target, bytes)
                            .with_context(|| format!("write file {}", target.display()))?;
                        verify_restored_file(&target, descriptor)?;
                        self.write_file_baseline(
                            peer_id,
                            origin,
                            &descriptor.path,
                            &descriptor.digest,
                            FileBaselineState::Present,
                        )?;
                        verified += 1;
                        continue;
                    }
                    conflicts.push(CognitionConflict {
                        table: "file".into(),
                        key: descriptor.path.clone(),
                        detail: "local file differs; the local content is kept".into(),
                    });
                    continue;
                }
                if let Some(baseline) = baseline {
                    if baseline.state != FileBaselineState::Deleted {
                        self.write_file_baseline(
                            peer_id,
                            origin,
                            &descriptor.path,
                            &baseline.digest,
                            FileBaselineState::LocalDeleted,
                        )?;
                        conflicts.push(CognitionConflict {
                            table: "file".into(),
                            key: descriptor.path.clone(),
                            detail: "the peer file exists but was independently deleted here; the local deletion is kept"
                                .into(),
                        });
                        continue;
                    }
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create directory {}", parent.display()))?;
                }
                fs::write(&target, bytes)
                    .with_context(|| format!("write file {}", target.display()))?;
                verify_restored_file(&target, descriptor)?;
                self.write_file_baseline(
                    peer_id,
                    origin,
                    &descriptor.path,
                    &descriptor.digest,
                    FileBaselineState::Present,
                )?;
                verified += 1;
            }

            // An origin declared by the manifest is a complete current file
            // set. A baseline path absent from that set is a source deletion,
            // not a path that happened to be omitted from the export options.
            for baseline in baselines {
                if incoming_paths.contains(baseline.path.as_str()) {
                    continue;
                }
                let relative = safe_relative_path(&baseline.path)?;
                let target = root.join(&relative);
                if baseline.state == FileBaselineState::Deleted {
                    if target.exists() {
                        conflicts.push(CognitionConflict {
                            table: "file".into(),
                            key: baseline.path,
                            detail: "a local file was recreated where the peer deletion was already applied; the local content is kept"
                                .into(),
                        });
                    }
                    continue;
                }
                if !target.exists() {
                    // Both sides now independently agree on absence. Retain a
                    // tombstone so a later source recreation is distinguishable
                    // from an unchanged source file deleted only by the receiver.
                    self.write_file_baseline(
                        peer_id,
                        origin,
                        &baseline.path,
                        &baseline.digest,
                        FileBaselineState::Deleted,
                    )?;
                    continue;
                }
                if !fs::symlink_metadata(&target)?.file_type().is_file() {
                    conflicts.push(CognitionConflict {
                        table: "file".into(),
                        key: baseline.path,
                        detail: "the peer deleted a file but the local path was independently replaced; the local replacement is kept".into(),
                    });
                    continue;
                }
                let existing = fs::read(&target)
                    .with_context(|| format!("read local file {}", target.display()))?;
                if baseline.state == FileBaselineState::Present
                    && Digest::blake3(&existing) == baseline.digest
                {
                    self.retire_deleted_file(
                        peer_id,
                        origin,
                        &root,
                        &relative,
                        &target,
                        &baseline.digest,
                    )?;
                    self.write_file_baseline(
                        peer_id,
                        origin,
                        &baseline.path,
                        &baseline.digest,
                        FileBaselineState::Deleted,
                    )?;
                } else {
                    conflicts.push(CognitionConflict {
                        table: "file".into(),
                        key: baseline.path,
                        detail: "the peer deleted this path but the local file changed independently; the local content is kept"
                            .into(),
                    });
                }
            }
        }
        Ok((verified, conflicts))
    }

    fn retire_deleted_file(
        &self,
        peer_id: EntityId,
        origin: &str,
        root: &Path,
        relative: &Path,
        target: &Path,
        expected: &Digest,
    ) -> Result<()> {
        let retirement_root =
            fs::canonicalize(root)?.join(format!(".shenko-retired-{}", EntityId::new()));
        fs::create_dir(&retirement_root)?;
        self.ledger.conn.lock().execute(
            "INSERT INTO cognition_retirements(path) VALUES(?1)",
            [retirement_root.to_string_lossy().as_ref()],
        )?;
        let retired = retirement_root
            .join(peer_id.to_string())
            .join(origin)
            .join(EntityId::new().to_string())
            .join(relative);
        if let Some(parent) = retired.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create retirement directory {}", parent.display()))?;
        }
        fs::rename(target, &retired).with_context(|| {
            format!(
                "preserve deleted file {} by moving it to {}",
                target.display(),
                retired.display()
            )
        })?;
        let retained = fs::read(&retired)
            .with_context(|| format!("read retired file {}", retired.display()))?;
        ensure!(
            Digest::blake3(&retained) == *expected,
            "retired file {} does not preserve its original bytes",
            retired.display()
        );
        Ok(())
    }

    fn read_file_baselines(&self, peer_id: EntityId, origin: &str) -> Result<Vec<FileBaseline>> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT path,digest,state FROM cognition_file_baselines
             WHERE peer_id=?1 AND origin=?2 ORDER BY path",
        )?;
        let rows = stmt
            .query_map(params![peer_id.to_string(), origin], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(path, digest, state)| {
                Ok(FileBaseline {
                    path,
                    digest: Digest(digest),
                    state: FileBaselineState::parse(&state)?,
                })
            })
            .collect()
    }

    /// The content and reconciliation state the peer last delivered for one file.
    fn read_file_baseline(
        &self,
        peer_id: EntityId,
        origin: &str,
        path: &str,
    ) -> Result<Option<FileBaseline>> {
        let conn = self.ledger.conn.lock();
        let stored: Option<(String, String)> = conn
            .query_row(
                "SELECT digest,state FROM cognition_file_baselines
                 WHERE peer_id=?1 AND origin=?2 AND path=?3",
                params![peer_id.to_string(), origin, path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(db)?;
        stored
            .map(|(digest, state)| {
                Ok(FileBaseline {
                    path: path.to_string(),
                    digest: Digest(digest),
                    state: FileBaselineState::parse(&state)?,
                })
            })
            .transpose()
    }

    /// Remember the content and reconciliation state the peer delivered.
    fn write_file_baseline(
        &self,
        peer_id: EntityId,
        origin: &str,
        path: &str,
        digest: &Digest,
        state: FileBaselineState,
    ) -> Result<()> {
        let conn = self.ledger.conn.lock();
        conn.execute(
            "INSERT INTO cognition_file_baselines(peer_id,origin,path,digest,state,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(peer_id,origin,path) DO UPDATE SET
                digest=excluded.digest,
                state=excluded.state,
                updated_at=excluded.updated_at",
            params![
                peer_id.to_string(),
                origin,
                path,
                digest.to_string(),
                state.as_str(),
                chrono::Utc::now().to_rfc3339(),
            ],
        )
        .map_err(db)?;
        Ok(())
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
        let temp =
            std::env::temp_dir().join(format!("shenko-cognition-{}.bundle", EntityId::new()));
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
        let branch = descriptor
            .head
            .strip_prefix("refs/heads/")
            .filter(|branch| !branch.is_empty())
            .ok_or_else(|| anyhow!("cognition Git HEAD is not a branch: {}", descriptor.head))?;
        let replicated = descriptor
            .refs
            .iter()
            .find(|(name, _)| name == &descriptor.head)
            .map(|(_, sha)| sha.clone())
            .ok_or_else(|| {
                anyhow!(
                    "cognition Git descriptor does not bind HEAD {} to a replicated commit",
                    descriptor.head
                )
            })?;
        if !root.join(".git").exists() {
            fs::create_dir_all(root)?;
            let initial_branch = format!("--initial-branch={branch}");
            let (ok, _, error) = git(root, &["init", "--quiet", initial_branch.as_str()])?;
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
        let (has_destination, destination_out, _) =
            git(root, &["rev-parse", "--verify", &descriptor.head])?;
        let (_, current_ref, _) = git(root, &["symbolic-ref", "--quiet", "HEAD"])?;
        if has_destination
            && destination_out.trim() != replicated
            && current_ref.trim() != descriptor.head
        {
            return Ok((true, Some(CognitionConflict {
                table: "git".into(),
                key: descriptor.head.clone(),
                detail: "the source HEAD branch already has an independent local tip; both histories are kept".into(),
            })));
        }
        let (has_head, head_out, _) = git(root, &["rev-parse", "--verify", "HEAD"])?;
        if !has_head {
            self.adopt_git_head(root, peer_id, descriptor, &replicated)?;
            return Ok((true, None));
        }
        let local_head = head_out.trim().to_string();
        let (_, local_ref_out, _) = git(root, &["symbolic-ref", "--quiet", "HEAD"])?;
        let local_ref = local_ref_out
            .lines()
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let baseline = self.read_git_baseline(peer_id)?;
        let unchanged = baseline.as_ref().is_some_and(|baseline| {
            baseline.head == local_head
                && baseline
                    .reference
                    .as_deref()
                    .is_none_or(|reference| Some(reference) == local_ref.as_deref())
        });
        if unchanged
            && (replicated != local_head || local_ref.as_deref() != Some(descriptor.head.as_str()))
        {
            // The local history is still the one the peer delivered, so a
            // newer remote head is the peer's continuation, not a conflict.
            let fast_forward = if replicated == local_head {
                true
            } else {
                let (ok, _, _) = git(
                    root,
                    &["merge-base", "--is-ancestor", &local_head, &replicated],
                )?;
                ok
            };
            if fast_forward {
                self.adopt_git_head(root, peer_id, descriptor, &replicated)?;
                return Ok((true, None));
            }
        }
        let conflict = if replicated != local_head
            || local_ref.as_deref() != Some(descriptor.head.as_str())
        {
            Some(CognitionConflict {
                table: "git".into(),
                key: "memory".into(),
                detail: format!(
                    "local HEAD {} ({local_head}) differs from replicated HEAD {} ({replicated}); both histories are kept",
                    local_ref.as_deref().unwrap_or("detached"),
                    descriptor.head
                ),
            })
        } else {
            None
        };
        if conflict.is_none() {
            self.write_git_baseline(peer_id, &replicated, &descriptor.head)?;
        }
        Ok((true, conflict))
    }

    fn adopt_git_head(
        &self,
        root: &Path,
        peer_id: EntityId,
        descriptor: &CognitionGitDescriptor,
        head: &str,
    ) -> Result<()> {
        let (ok, _, error) = git(root, &["update-ref", &descriptor.head, head])?;
        ensure!(ok, "git update-ref failed: {error}");
        let (ok, _, error) = git(root, &["symbolic-ref", "HEAD", &descriptor.head])?;
        ensure!(ok, "git symbolic-ref failed: {error}");
        let (ok, _, error) = git(root, &["reset", "--mixed"])?;
        ensure!(ok, "git reset failed: {error}");
        self.write_git_baseline(peer_id, head, &descriptor.head)
    }

    /// The remote head this store last took from the peer.
    fn read_git_baseline(&self, peer_id: EntityId) -> Result<Option<GitBaseline>> {
        let conn = self.ledger.conn.lock();
        let stored: Option<String> = conn
            .query_row(
                "SELECT row_values FROM cognition_baselines WHERE peer_id=?1 AND table_name='git' AND entity_key='memory'",
                [peer_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(db)?;
        match stored {
            Some(json) => {
                let values: Vec<(String, Option<String>)> = serde_json::from_str(&json)
                    .map_err(|error| anyhow!("invalid cognition git baseline: {error}"))?;
                let head = values
                    .iter()
                    .find(|(name, _)| name == "head")
                    .and_then(|(_, value)| value.clone());
                let reference = values
                    .into_iter()
                    .find(|(name, _)| name == "ref")
                    .and_then(|(_, value)| value);
                Ok(head.map(|head| GitBaseline { head, reference }))
            }
            None => Ok(None),
        }
    }

    /// Remember the remote head this store took from the peer.
    fn write_git_baseline(&self, peer_id: EntityId, head: &str, reference: &str) -> Result<()> {
        let conn = self.ledger.conn.lock();
        let values = vec![
            ("head".to_string(), Some(head.to_string())),
            ("ref".to_string(), Some(reference.to_string())),
        ];
        let json = serde_json::to_string(&values)?;
        conn.execute(
            "INSERT INTO cognition_baselines(peer_id,table_name,entity_key,row_values,updated_at)
             VALUES(?1,'git','memory',?2,?3)
             ON CONFLICT(peer_id,table_name,entity_key) DO UPDATE SET
                row_values=excluded.row_values,
                updated_at=excluded.updated_at",
            params![peer_id.to_string(), json, chrono::Utc::now().to_rfc3339(),],
        )
        .map_err(db)?;
        Ok(())
    }

    fn record_receipt(
        &self,
        peer_id: EntityId,
        manifest: &CognitionManifest,
        verified_objects: usize,
        verified_files: usize,
        inserted: usize,
        updated: usize,
        unchanged: usize,
        conflicts: usize,
    ) -> Result<()> {
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

fn check_import_order_conn(
    conn: &rusqlite::Connection,
    manifest: &CognitionManifest,
) -> Result<()> {
    let previous: Option<(i64, i64, String)> = conn
        .query_row(
            "SELECT snapshot_seq,high_water,fingerprint FROM cognition_import_order
             WHERE source_ledger_id=?1",
            [manifest.source_ledger_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let snapshot_seq = i64::try_from(manifest.source_snapshot_seq)?;
    let high_water = i64::try_from(manifest.high_water.0)?;
    if let Some((previous_seq, previous_high_water, previous_fingerprint)) = previous {
        ensure!(
            snapshot_seq >= previous_seq,
            "stale cognition snapshot from {}: durable export order {snapshot_seq} precedes {previous_seq}",
            manifest.source_ledger_id
        );
        ensure!(
            high_water >= previous_high_water,
            "stale cognition snapshot from {}: source high-water {high_water} precedes {previous_high_water}",
            manifest.source_ledger_id
        );
        if snapshot_seq == previous_seq {
            ensure!(
                high_water == previous_high_water
                    && manifest.fingerprint.0.as_str() == previous_fingerprint.as_str(),
                "conflicting cognition snapshots share source ledger {} and durable export order {snapshot_seq}",
                manifest.source_ledger_id
            );
        }
    }
    Ok(())
}

/// The pause record is local controller state and is not in archive schema
/// 2/3. A peer cannot manufacture that authority merely by importing a head.
/// Likewise a divergent peer's step must not be applied independently of a
/// retained paused head, even if the step still matches the peer baseline.
fn validate_subject_boundaries(
    conn: &rusqlite::Connection,
    manifest: &CognitionManifest,
) -> Result<()> {
    let head_spec = spec_for("subject_branch_heads").expect("subject head table is registered");
    let step_spec = spec_for("subject_steps").expect("subject step table is registered");

    for incoming in manifest.rows.iter().filter(|row| row.table == head_spec.name) {
        let data = incoming.value("data").context("imported subject head has no data")?;
        let value: serde_json::Value = serde_json::from_str(data).context("invalid imported subject head")?;
        let data_mode = value.get("mode").and_then(serde_json::Value::as_str)
            .context("imported subject head has no serialized mode")?;
        let column_mode = incoming.value("mode").context("imported subject head has no mode column")?;
        if column_mode == "paused" || data_mode == "paused" {
            ensure!(column_mode == "paused" && data_mode == "paused",
                "imported paused subject head has inconsistent mode fields");
            let local = read_existing(conn, head_spec, incoming)?
                .context("imported paused subject head has no local boundary")?;
            ensure!(rows_equivalent(head_spec, &local, &incoming.values),
                "imported paused subject head differs from the locally retained boundary");
            let head: SubjectBranchHead = serde_json::from_value(value)?;
            validate_local_paused_successor(conn, &head)?;
        }
    }

    // Check the actual local head, not merely the peer's head row. The peer
    // can omit its head or submit an active one that conflicts with ours.
    let mut query = conn.prepare(
        "SELECT data,mode FROM subject_branch_heads WHERE mode='paused' OR json_extract(data,'$.mode')='paused'",
    ).map_err(db)?;
    let local_heads = query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(db)?.collect::<rusqlite::Result<Vec<_>>>().map_err(db)?;
    for (data, mode) in local_heads {
        let head: SubjectBranchHead = serde_json::from_str(&data)?;
        ensure!(mode == "paused" && head.mode == SubjectMode::Paused,
            "local paused subject head has inconsistent mode fields");
        let agent_id = head.agent_id.to_string();
        for incoming in manifest.rows.iter().filter(|row|
            row.table == head_spec.name && row.value("agent_id") == Some(agent_id.as_str()))
        {
            validate_local_paused_successor(conn, &head)?;
            let local = read_existing(conn, head_spec, incoming)?
                .context("local paused subject head is missing")?;
            ensure!(rows_equivalent(head_spec, &local, &incoming.values),
                "imported head conflicts with the locally retained paused subject boundary");
        }
        let next_id = head.next_step_id.to_string();
        for incoming in manifest.rows.iter().filter(|row|
            row.table == step_spec.name && row.value("step_id") == Some(next_id.as_str()))
        {
            validate_local_paused_successor(conn, &head)?;
            let local = read_existing(conn, step_spec, incoming)?
                .context("local paused subject successor is missing")?;
            ensure!(rows_equivalent(step_spec, &local, &incoming.values),
                "imported step conflicts with the locally retained paused subject successor");
        }
    }
    Ok(())
}

fn validate_local_paused_successor(conn: &rusqlite::Connection, head: &SubjectBranchHead) -> Result<()> {
    crate::runtime_boundary::validate_paused_head(conn, head)?;
    let step = crate::store::read_step(conn, &head.branch_id, head.next_step_id)?
        .context("local paused subject successor is missing")?;
    crate::store::validate_step_binding(head, &step)?;
    ensure!(step.state == SubjectStepState::Planned
        && step.active_request_id.is_none() && step.active_attempt_id.is_none(),
        "local paused subject successor is not a clean boundary");
    Ok(())
}

fn record_import_order(
    tx: &Transaction<'_>,
    peer_id: EntityId,
    manifest: &CognitionManifest,
) -> Result<()> {
    tx.execute(
        "INSERT INTO cognition_import_order(source_ledger_id,peer_id,snapshot_seq,high_water,fingerprint)
         VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(source_ledger_id) DO UPDATE SET
            peer_id=excluded.peer_id,
            snapshot_seq=excluded.snapshot_seq,
            high_water=excluded.high_water,
            fingerprint=excluded.fingerprint",
        params![
            manifest.source_ledger_id.to_string(),
            peer_id.to_string(),
            i64::try_from(manifest.source_snapshot_seq)?,
            i64::try_from(manifest.high_water.0)?,
            manifest.fingerprint.to_string(),
        ],
    )?;
    Ok(())
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
    fingerprint_parts(
        &collected.rows,
        &collected.objects,
        collected.files.iter().map(|(descriptor, _)| descriptor),
        &collected.file_origins,
        collected.git_history.as_ref(),
    )
}

fn fingerprint_of_manifest(manifest: &CognitionManifest) -> Digest {
    fingerprint_parts(
        &manifest.rows,
        &manifest.objects,
        manifest.files.iter(),
        &manifest.file_origins,
        manifest.git_history.as_ref(),
    )
}

fn fingerprint_parts<'a>(
    rows: &[CognitionRow],
    objects: &[CognitionObjectDescriptor],
    files: impl IntoIterator<Item = &'a CognitionFileDescriptor>,
    file_origins: &[CognitionFileOrigin],
    git_history: Option<&CognitionGitDescriptor>,
) -> Digest {
    let mut buffer: Vec<u8> = Vec::new();
    buffer.extend_from_slice(b"shenko-cognition-fingerprint/2\n");
    let mut rows: Vec<&CognitionRow> = rows.iter().collect();
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
    let mut objects: Vec<&CognitionObjectDescriptor> = objects.iter().collect();
    objects.sort_by(|a, b| a.digest.0.cmp(&b.digest.0));
    for descriptor in objects {
        buffer.extend_from_slice(b"object\t");
        buffer.extend_from_slice(descriptor.digest.0.as_bytes());
        buffer.push(b'\n');
    }
    let mut file_origins = file_origins.to_vec();
    file_origins.sort_unstable();
    for origin in file_origins {
        buffer.extend_from_slice(b"file-origin\t");
        buffer.extend_from_slice(file_origin_name(origin).as_bytes());
        buffer.push(b'\n');
    }
    let mut files: Vec<&CognitionFileDescriptor> = files.into_iter().collect();
    files.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(file_origin_name(a.origin).cmp(file_origin_name(b.origin)))
            .then(a.digest.0.cmp(&b.digest.0))
    });
    for descriptor in files {
        buffer.extend_from_slice(b"file\t");
        buffer.extend_from_slice(file_origin_name(descriptor.origin).as_bytes());
        buffer.push(b'\t');
        buffer.extend_from_slice(descriptor.path.as_bytes());
        buffer.push(b'\t');
        buffer.extend_from_slice(descriptor.digest.0.as_bytes());
        buffer.push(b'\n');
    }
    if let Some(descriptor) = git_history {
        buffer.extend_from_slice(b"git-head\t");
        buffer.extend_from_slice(descriptor.head.as_bytes());
        buffer.push(b'\n');
        let mut refs: Vec<&(String, String)> = descriptor.refs.iter().collect();
        refs.sort();
        for (name, sha) in refs {
            buffer.extend_from_slice(b"git\t");
            buffer.extend_from_slice(name.as_bytes());
            buffer.push(b'\t');
            buffer.extend_from_slice(sha.as_bytes());
            buffer.push(b'\n');
        }
    }
    Digest::blake3(&buffer)
}

fn file_origin_name(origin: CognitionFileOrigin) -> &'static str {
    match origin {
        CognitionFileOrigin::Memory => "memory",
        CognitionFileOrigin::Workspace => "workspace",
    }
}

fn spec_for(table: &str) -> Option<&'static TableSpec> {
    TABLES.iter().find(|spec| spec.name == table)
}

/// `insert_row` uses positional values while admission and recovery use
/// labels. Bind those two views, and the primary-key identity, before any
/// publication. Both supported archive schemas use these same TableSpecs.
fn validate_cognition_rows(manifest: &CognitionManifest) -> Result<()> {
    let mut identities = HashSet::with_capacity(manifest.rows.len());
    for row in &manifest.rows {
        let spec = spec_for(&row.table)
            .ok_or_else(|| anyhow!("unsupported cognition fact table {}", row.table))?;
        ensure!(row.values.len() == spec.columns.len()
            && row.values.iter().zip(spec.columns).all(|((name, _), expected)| name.as_str() == *expected),
            "invalid cognition column layout for {}", row.table);

        // Use the same encoding as read_table, without allocating another
        // joined key for every row. NULL has the exporter's empty encoding.
        let mut remainder = row.key.as_str();
        for (index, name) in spec.key.iter().enumerate() {
            if index != 0 {
                remainder = remainder.strip_prefix('\u{1f}')
                    .ok_or_else(|| anyhow!("cognition primary key mismatch for {}", row.table))?;
            }
            remainder = remainder.strip_prefix(row.value(name).unwrap_or_default())
                .ok_or_else(|| anyhow!("cognition primary key mismatch for {}", row.table))?;
        }
        ensure!(remainder.is_empty(), "cognition primary key mismatch for {}", row.table);
        ensure!(identities.insert((row.table.as_str(), row.key.as_str())),
            "duplicate cognition fact identity in {}", row.table);
    }
    Ok(())
}

fn memory_intent_binding_gaps(rows: &[CognitionRow]) -> Vec<CognitionGap> {
    let mut intents = HashMap::new();
    let mut bindings = HashMap::new();
    let mut entries = HashSet::new();
    let mut gaps = Vec::new();
    for row in rows.iter().filter(|row| row.table == "memory_entries") {
        if row.value("entry_id") != Some(row.key.as_str()) || !entries.insert(row.key.as_str()) {
            gaps.push(CognitionGap {
                kind: "memory_intent_identity".into(),
                detail: format!("invalid or duplicate memory entry {}", row.key),
                blocking: true,
            });
        }
    }
    for row in rows.iter().filter(|row| row.table == "memory_intents" || row.table == "memory_intent_bindings") {
        let id = row.value("intent_id");
        let map = if row.table == "memory_intents" { &mut intents } else { &mut bindings };
        if id != Some(row.key.as_str()) || map.insert(row.key.as_str(), row).is_some() {
            gaps.push(CognitionGap {
                kind: "memory_intent_identity".into(),
                detail: format!("invalid or duplicate {} row {}", row.table, row.key),
                blocking: true,
            });
        }
    }
    for (id, binding) in &bindings {
        let valid = intents.get(id).is_some_and(|intent| {
            binding.value("data").is_some_and(|data| {
                crate::memory::memory_binding_matches_locator(data, intent.value("content_ref"))
            })
        });
        if !valid {
            gaps.push(CognitionGap {
                kind: "memory_intent_identity".into(),
                detail: format!("unbound or inconsistent memory reference for {id}"),
                blocking: true,
            });
        }
    }
    let mut entry_owners = HashMap::new();
    for (id, intent) in &intents {
        let library_pending = intent.value("state") == Some("accepted")
            && matches!(intent.value("kind"), Some("save" | "correct" | "forget" | "recall"));
        let entry_present = intent.value("entry_id").is_some_and(|id| id.parse::<EntityId>().is_ok());
        let entry_id = intent.value("entry_id");
        let duplicate_entry = entry_id.is_some_and(|entry| entry_owners.insert(entry, *id).is_some());
        let missing_committed_entry = intent.value("state") == Some("committed")
            && entry_id.is_none_or(|entry| !entries.contains(entry));
        let missing_pending_identity = library_pending
            && (!entry_present || (intent.value("content_ref").is_some() && !bindings.contains_key(id)));
        if missing_pending_identity || duplicate_entry || missing_committed_entry {
            gaps.push(CognitionGap {
                kind: "memory_intent_identity".into(),
                detail: if missing_pending_identity {
                    format!("pending memory intent {id} lacks its complete identity")
                } else {
                    format!("memory intent {id} has a missing or shared committed entry")
                },
                blocking: true,
            });
        }
    }
    gaps
}

#[derive(Clone, Copy)]
enum RecoveryFactOutcome {
    Inserted,
    Updated,
    Unchanged,
    Conflict,
}

fn memory_row_value<'a>(values: &'a [(String, Option<String>)], name: &str) -> Option<&'a str> {
    values.iter().find(|(column, _)| column == name).and_then(|(_, value)| value.as_deref())
}

fn apply_recovery_fact(
    tx: &Transaction<'_>, spec: &TableSpec, row: &CognitionRow, peer_id: EntityId, seq: LedgerSeq,
) -> Result<RecoveryFactOutcome> {
    let local = read_existing(tx, spec, row)?;
    let baseline = read_baseline(tx, peer_id, spec.name, &row.key)?;
    let result = match local {
        None => match insert_row(tx, spec, row, seq) {
            Ok(()) => RecoveryFactOutcome::Inserted,
            Err(error) if is_constraint_violation(&error) => return Ok(RecoveryFactOutcome::Conflict),
            Err(error) => return Err(error),
        },
        Some(local) if rows_equivalent(spec, &local, &row.values) => RecoveryFactOutcome::Unchanged,
        Some(local) if baseline.as_ref().is_some_and(|baseline| rows_equivalent(spec, &local, baseline)) => {
            match update_row(tx, spec, row, seq) {
                Ok(()) => {}
                Err(error) if is_constraint_violation(&error) => return Ok(RecoveryFactOutcome::Conflict),
                Err(error) => return Err(error),
            }
            RecoveryFactOutcome::Updated
        }
        Some(_) => return Ok(RecoveryFactOutcome::Conflict),
    };
    write_baseline(tx, peer_id, spec.name, &row.key, &row.values)?;
    Ok(result)
}

/// A request, immutable reference and associated committed entry are reconciled
/// together. A locator-only legacy request cannot acquire another source's
/// digest merely because its intent ID or locator happens to match.
fn apply_memory_pairs(
    tx: &Transaction<'_>, rows: &[&CognitionRow], peer_id: EntityId, seq: LedgerSeq,
) -> Result<CognitionApplyOutcome> {
    let parents = spec_for("memory_intents").expect("memory intent table is registered");
    let sidecars = spec_for("memory_intent_bindings").expect("memory binding table is registered");
    let entry_spec = spec_for("memory_entries").expect("memory entry table is registered");
    let bindings: HashMap<&str, &CognitionRow> = rows.iter()
        .filter(|row| row.table == sidecars.name)
        .map(|row| (row.key.as_str(), *row)).collect();
    let entries: HashMap<&str, &CognitionRow> = rows.iter()
        .filter(|row| row.table == entry_spec.name)
        .map(|row| (row.key.as_str(), *row)).collect();
    let mut outcome = CognitionApplyOutcome::default();
    for row in rows.iter().filter(|row| row.table == parents.name) {
        let mut parent = effective_row(row, parents)?;
        let mut binding = bindings.get(row.key.as_str()).map(|row| effective_row(row, sidecars)).transpose()?;
        let entry = parent.value("entry_id").and_then(|id| entries.get(id))
            .map(|row| effective_row(row, entry_spec)).transpose()?;
        let local = read_existing(tx, parents, &parent)?;
        let local_binding: Option<String> = tx.query_row(
            "SELECT data FROM memory_intent_bindings WHERE intent_id=?1",
            [&row.key], |record| record.get(0),
        ).optional().map_err(db)?;
        let mut compatible = true;
        if let Some(local) = &local {
            for column in ["agent_id", "kind", "scope", "content", "entry_id", "created_at_ms"] {
                compatible &= memory_row_value(local, column) == parent.value(column);
            }
            let local_state = memory_row_value(local, "state");
            let incoming_state = parent.value("state");
            compatible &= match local_state {
                Some("accepted") => matches!(incoming_state, Some("accepted" | "committed" | "failed")),
                Some("committed" | "failed") => local_state == incoming_state
                    && memory_row_value(local, "reason") == parent.value("reason")
                    && memory_row_value(local, "updated_at_ms") == parent.value("updated_at_ms"),
                _ => false,
            };
            let local_locator = memory_row_value(local, "content_ref");
            match (local_binding.as_deref(), binding.as_mut()) {
                (None, None) => compatible &= local_locator == parent.value("content_ref"),
                (Some(local_data), Some(incoming)) => {
                    let incoming_data = incoming.value("data");
                    compatible &= crate::memory::memory_binding_matches_locator(local_data, local_locator)
                        && incoming_data.is_some_and(|data| crate::memory::same_memory_bindings(local_data, data));
                    if let Some(baseline) = read_baseline(tx, peer_id, sidecars.name, &incoming.key)? {
                        compatible &= memory_row_value(&baseline, "data")
                            .is_some_and(|data| crate::memory::same_memory_bindings(local_data, data));
                    }
                    if compatible {
                        set_value(&mut parent, "content_ref", local_locator.map(str::to_owned));
                        set_value(incoming, "data", Some(local_data.to_owned()));
                    }
                }
                _ => compatible = false,
            }
        } else if local_binding.is_some() {
            compatible = false;
        }

        let mut results = [None; 3];
        if compatible {
            tx.execute("SAVEPOINT memory_intent_pair", []).map_err(db)?;
            results[0] = Some(apply_recovery_fact(tx, parents, &parent, peer_id, seq)?);
            if !matches!(results[0], Some(RecoveryFactOutcome::Conflict)) {
                if let Some(binding) = &binding {
                    results[1] = Some(apply_recovery_fact(tx, sidecars, binding, peer_id, seq)?);
                }
            }
            if !results.iter().any(|result| matches!(result, Some(RecoveryFactOutcome::Conflict))) {
                if let Some(entry) = &entry {
                    results[2] = Some(apply_recovery_fact(tx, entry_spec, entry, peer_id, seq)?);
                }
            }
            if results.iter().any(|result| matches!(result, Some(RecoveryFactOutcome::Conflict))) {
                tx.execute("ROLLBACK TO memory_intent_pair", []).map_err(db)?;
                compatible = false;
            }
            tx.execute("RELEASE memory_intent_pair", []).map_err(db)?;
        }
        if compatible {
            for result in results.into_iter().flatten() {
                match result {
                    RecoveryFactOutcome::Inserted => outcome.inserted += 1,
                    RecoveryFactOutcome::Updated => outcome.updated += 1,
                    RecoveryFactOutcome::Unchanged => outcome.unchanged += 1,
                    RecoveryFactOutcome::Conflict => unreachable!(),
                }
            }
        } else {
            let detail = "memory intent and its reference conflict; the local identity is kept".to_string();
            record_conflict(tx, peer_id, parents.name, &row.key, &detail)?;
            outcome.conflicts.push(CognitionConflict { table: parents.name.into(), key: row.key.clone(), detail });
        }
    }
    Ok(outcome)
}

fn task_row_json(row: &CognitionRow) -> Option<serde_json::Value> {
    row.value("data").and_then(|data| serde_json::from_str(data).ok())
}

/// The exact task-owned event shapes produced by native T2 / Playground.
/// No recursive keyword search: an arbitrary message mentioning a task id
/// must remain an independent observation.
fn task_delivery_owner(event: &serde_json::Value) -> Option<String> {
    let source = event.get("source")?.as_str()?;
    let payload = event.get("payload")?;
    if (source == "native-child" && event.get("kind")?.as_str()? == "task_result")
        || source == "playground"
    {
        payload.get("task_id")?.as_str().map(str::to_owned)
    } else { None }
}

/// Complete task execution AND delivery facts form one conflict unit.
/// Parentless executions are rejected, never grafted onto a local task merely
/// because its deterministic id happens to match. Pending and already-staged
/// T2 text is applied inside the same savepoint as the original sealed input.
fn apply_task_closures<'a>(
    tx: &Transaction<'_>,
    rows: &[&'a CognitionRow],
    peer_id: EntityId,
    seq: LedgerSeq,
    manifest: &CognitionManifest,
) -> Result<(
    CognitionApplyOutcome,
    HashSet<(&'a str, &'a str)>,
    HashSet<(&'a str, &'a str)>,
    HashSet<String>,
)> {
    fn add<'a>(
        groups: &mut HashMap<String, Vec<&'a CognitionRow>>,
        owned: &mut HashSet<(&'a str, &'a str)>, row: &'a CognitionRow, task: Option<String>,
    ) -> String {
        let task = task.unwrap_or_else(|| format!("unbound/{}/{}", row.table, row.key));
        groups.entry(task.clone()).or_default().push(row);
        owned.insert((row.table.as_str(), row.key.as_str()));
        task
    }
    let task_spec = spec_for("tasks").expect("task table is registered");
    let tasks: HashMap<&str, &CognitionRow> = rows.iter().copied().filter(|r| r.table == "tasks")
        .map(|row| (row.key.as_str(), row)).collect();
    let mut groups = HashMap::new();
    let mut owned = HashSet::new();
    for row in tasks.values().copied() {
        add(&mut groups, &mut owned, row, Some(row.key.clone()));
    }
    let mut operation_owners = HashMap::new();
    for row in rows.iter().copied().filter(|row| row.table == "operations") {
        let owner = task_row_json(row).and_then(|data| data.get("task_id")?.as_str().map(str::to_owned));
        let task = add(&mut groups, &mut owned, row, owner);
        operation_owners.insert(row.key.as_str(), task);
    }
    for row in rows.iter().copied().filter(|row| matches!(row.table.as_str(), "attempts" | "evidence_receipts" | "task_results")) {
        let task = if row.table == "task_results" {
            row.value("task_id").map(str::to_owned)
        } else {
            row.value("operation_id").and_then(|id| operation_owners.get(id)).cloned()
        };
        add(&mut groups, &mut owned, row, task);
    }
    let mut event_owners = HashMap::new();
    for row in rows.iter().copied().filter(|row| row.table == "inbox_events") {
        let data = task_row_json(row);
        let native = row.value("kind") == Some("task_result") && row.value("source") == Some("native-child");
        let task = if native {
            data.as_ref().and_then(|value| value.get("task_id")?.as_str().map(str::to_owned))
        } else { data.as_ref().and_then(task_delivery_owner) };
        if native || task.is_some() {
            let task = add(&mut groups, &mut owned, row, task);
            event_owners.insert(row.key.as_str(), task);
        }
    }
    let mut raw_owners = HashMap::new();
    let mut invalid_groups = HashSet::new();
    for row in rows.iter().copied().filter(|row| row.table == "session_events") {
        let data = task_row_json(row);
        let original = data.as_ref().and_then(|data| data.get("inbox_event"));
        let task = original.and_then(task_delivery_owner);
        let native = original.is_some_and(|event| event.get("source").and_then(serde_json::Value::as_str) == Some("native-child")
            && event.get("kind").and_then(serde_json::Value::as_str) == Some("task_result"));
        if native || task.is_some() {
            let task = add(&mut groups, &mut owned, row, task);
            let source = original.and_then(|event| event.get("event_id")).and_then(serde_json::Value::as_str).map(str::to_owned);
            raw_owners.insert(row.key.as_str(), (task, source));
        }
    }
    for row in rows.iter().copied().filter(|row| row.table == "subject_raw_inputs") {
        let raw = row.value("raw_event_id").and_then(|id| raw_owners.get(id));
        let event = row.value("source_event_id").and_then(|id| event_owners.get(id));
        if raw.is_some() || event.is_some() {
            let task = raw.map(|(task, _)| task.clone()).or_else(|| event.cloned());
            let task = add(&mut groups, &mut owned, row, task);
            if raw.is_none_or(|(_, source)| source.as_deref() != row.value("source_event_id"))
                || event.is_some_and(|owner| owner != &task)
            {
                invalid_groups.insert(task);
                if let Some(owner) = event { invalid_groups.insert(owner.clone()); }
            }
        }
    }
    let mut outcome = CognitionApplyOutcome::default();
    let mut rejected = HashSet::new();
    let mut rejected_ids = HashSet::new();
    for (task_id, mut group) in groups {
        let parent = tasks.get(task_id.as_str()).copied();
        let mut conflict = parent.is_none() || invalid_groups.contains(&task_id);
        if let Some(parent) = parent {
            // An immutable action input cannot change, even if the nil-peer
            // baseline happens to match the local record from another source.
            if let Some(local) = read_existing(tx, task_spec, parent)? {
                let local = memory_row_value(&local, "data").and_then(|data| serde_json::from_str::<serde_json::Value>(data).ok());
                let incoming = task_row_json(parent);
                if local.as_ref().is_some_and(|data| data["kind"] == "playground.backend")
                    || incoming.as_ref().is_some_and(|data| data["kind"] == "playground.backend")
                {
                    conflict |= match (&local, &incoming) {
                        (Some(local), Some(incoming)) => ["task_id", "agent_id", "kind", "definition_version"].iter()
                            .any(|key| local.get(*key) != incoming.get(*key))
                            || ["digest", "bytes"].iter().any(|key|
                                local.get("input_ref").and_then(|input| input.get(*key))
                                    != incoming.get("input_ref").and_then(|input| input.get(*key))),
                        _ => true,
                    };
                }
            }
        }
        // The same fixed SQL identifier is reused only after RELEASE. UUIDs
        // are data, never interpolated into SQLite identifiers.
        tx.execute("SAVEPOINT task_closure", []).map_err(db)?;
        let mut part = CognitionApplyOutcome::default();
        group.sort_by_key(|row| TABLES.iter().position(|spec| spec.name == row.table));
        if !conflict {
            for row in &group {
                let spec = spec_for(&row.table).context("unregistered task recovery table")?;
                let effective = effective_row(row, spec)?;
                match apply_recovery_fact(tx, spec, &effective, peer_id, seq)? {
                    RecoveryFactOutcome::Conflict => { conflict = true; break; }
                    RecoveryFactOutcome::Inserted => part.inserted += 1,
                    RecoveryFactOutcome::Updated => part.updated += 1,
                    RecoveryFactOutcome::Unchanged => { part.unchanged += 1; continue; }
                }
                record_origin(tx, spec, &effective, peer_id, &manifest.fingerprint, manifest.source_ledger_id)?;
            }
        }
        if conflict { tx.execute("ROLLBACK TO task_closure", []).map_err(db)?; }
        tx.execute("RELEASE task_closure", []).map_err(db)?;
        if conflict {
            rejected_ids.insert(task_id.clone());
            for row in group {
                rejected.insert((row.table.as_str(), row.key.as_str()));
                rejected_ids.insert(row.key.clone());
                let detail = format!("task {task_id} input/execution/delivery closure conflicts or lacks its incoming parent; the complete local unit is kept");
                record_conflict(tx, peer_id, &row.table, &row.key, &detail)?;
                outcome.conflicts.push(CognitionConflict { table: row.table.clone(), key: row.key.clone(), detail });
            }
        } else {
            outcome.inserted += part.inserted;
            outcome.updated += part.updated;
            outcome.unchanged += part.unchanged;
        }
    }
    Ok((outcome, owned, rejected, rejected_ids))
}

/// Every string value and every object key, recursively: window call_states
/// hold task ids as MAP KEYS, and sealed S0 refs can appear nested anywhere.
fn json_holds_any_id(value: &serde_json::Value, ids: &HashSet<String>) -> bool {
    match value {
        serde_json::Value::Object(map) => map.iter().any(|(key, v)| {
            ids.contains(key.as_str()) || json_holds_any_id(v, ids)
        }),
        serde_json::Value::Array(items) => items.iter().any(|v| json_holds_any_id(v, ids)),
        serde_json::Value::String(text) => ids.contains(text),
        _ => false,
    }
}

/// A rejected task delivery must not survive through consumers committed in
/// an earlier table phase. Rather than mutating sealed step/window records
/// (which would falsify their identity), the ENTIRE import transaction is
/// rejected when a dependency cannot be established as safe: the peer gets an
/// honest "publication refused", never a half-applied snapshot or fake ACK.
fn verify_no_rejected_task_consumers(
    store: &Store,
    tx: &Transaction<'_>,
    rows: &[&CognitionRow],
    derivatives: &[CognitionDerivative],
    rejected: &HashSet<(&str, &str)>,
    rejected_ids: &HashSet<String>,
) -> Result<()> {
    if rejected.is_empty() {
        return Ok(());
    }
    let refuse = |what: String| -> Result<()> {
        bail!("cognition import refused: task closure rejection cannot publish dependent {what}; the complete import is rolled back, nothing was applied")
    };

    // The seq of every rejected staged raw event, for from/to range tests.
    let rejected_raw_seqs: Vec<(String, i64)> = {
        let mut out = Vec::new();
        for (table, key) in rejected.iter().filter(|(table, _)| *table == "session_events") {
            let row: Option<(String, i64)> = rows.iter().copied()
                .find(|r| r.table == "session_events" && r.key == *key)
                .and_then(|r| {
                    Some((r.value("session_id")?.to_owned(), r.value("seq")?.parse::<i64>().ok()?))
                });
            if let Some(pair) = row { out.push(pair); }
        }
        out
    };

    // Union of session_ids so a range can only match its own branch.
    let rejected_sessions: HashSet<&str> =
        rejected_raw_seqs.iter().map(|(session, _)| session.as_str()).collect();

    // Window bodies by window_id (manifest + existing chain head reads).
    let window_body = |id: &str| -> Result<Option<serde_json::Value>> {
        if let Some(body) = rows.iter().copied()
            .find(|r| r.table == "context_windows" && r.key == id)
            .and_then(|r| r.value("body"))
        {
            return Ok(serde_json::from_str(body).ok());
        }
        let body: Option<String> = tx.query_row(
            "SELECT body FROM context_windows WHERE window_id=?1", [id], |r| r.get(0),
        ).optional().map_err(db)?;
        Ok(body.and_then(|body| serde_json::from_str(&body).ok()))
    };

    // Transitive dirty-window closure: direct id hits, from/to ranges that
    // cover a rejected raw seq, and base.source_window predecessors.
    let mut dirty_windows: HashSet<String> = HashSet::new();
    let mut work: Vec<String> = rows.iter().copied()
        .filter(|r| r.table == "context_windows")
        .map(|r| r.key.clone())
        .collect();
    // Seed with windows any rejected step's frozen context points at.
    for row in rows.iter().copied().filter(|r| r.table == "subject_steps") {
        if let Some(data) = row.value("data").and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok()) {
            if let Some(id) = data.get("input_plan_ref")
                .and_then(|o| o.get("digest")).and_then(serde_json::Value::as_str)
                .and_then(|d| store.cas.read_object(&Digest(d.to_owned())).ok())
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|plan| plan.pointer("/context/source/window_id").and_then(serde_json::Value::as_str).map(str::to_owned))
            {
                if json_holds_any_id(&data, rejected_ids) { dirty_windows.insert(id); }
            }
        }
    }
    while let Some(id) = work.pop() {
        let Some(body) = window_body(&id)? else { continue };
        if dirty_windows.contains(&id) {
            // Propagate to predecessor and to windows naming this as source.
            if let Some(pred) = body.pointer("/base/source_window").and_then(serde_json::Value::as_str) {
                if dirty_windows.insert(pred.to_owned()) { work.push(pred.to_owned()); }
            }
            continue;
        }
        let hit_ids = json_holds_any_id(&body, rejected_ids);
        let hit_range = rejected_sessions.contains(
            body.get("scope_id").or_else(|| body.pointer("/base/scope_id")).and_then(serde_json::Value::as_str).unwrap_or(""),
        ) && {
            let from = body.pointer("/base/from").or_else(|| body.get("from")).and_then(serde_json::Value::as_i64);
            let to = body.pointer("/base/to").or_else(|| body.get("to")).and_then(serde_json::Value::as_i64);
            matches!((from, to), (Some(f), Some(t))
                if rejected_raw_seqs.iter().any(|(s, seq)| *seq >= f && *seq <= t
                    && (body.get("scope_id").or_else(|| body.pointer("/base/scope_id"))
                        .and_then(serde_json::Value::as_str) == Some(s.as_str()))))
        };
        if hit_ids || hit_range {
            dirty_windows.insert(id.clone());
            if let Some(pred) = body.pointer("/base/source_window").and_then(serde_json::Value::as_str) {
                work.push(pred.to_owned());
            }
            // Successor windows in the same manifest that name this window
            // as their source inherit the contamination.
            for candidate in rows.iter().copied().filter(|r| r.table == "context_windows") {
                if !dirty_windows.contains(candidate.key.as_str()) { work.push(candidate.key.clone()); }
            }
        }
    }
    // Steps whose frozen S0 selects rejected deliveries or a dirty window.
    for row in rows.iter().copied().filter(|r| r.table == "subject_steps") {
        let Some(data) = row.value("data").and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok()) else { continue };
        let plan: Option<serde_json::Value> = data.get("input_plan_ref")
            .and_then(|o| o.get("digest")).and_then(serde_json::Value::as_str)
            .and_then(|d| store.cas.read_object(&Digest(d.to_owned())).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let Some(plan) = plan else {
            // Cannot verify this step's dependency closure while its own
            // data mentions rejected facts: refuse rather than guess.
            if json_holds_any_id(&data, rejected_ids) {
                return refuse(format!("subject step {} with unresolved input plan", row.key));
            }
            continue;
        };
        let window = plan.pointer("/context/source/window_id").and_then(serde_json::Value::as_str);
        if json_holds_any_id(&plan, rejected_ids)
            || window.is_some_and(|id| dirty_windows.contains(id) || rejected_ids.contains(id))
        {
            return refuse(format!("subject step {} frozen input", row.key));
        }
    }
    // Dirty imported windows or any derivative of one fail publication.
    if !dirty_windows.is_empty() {
        let mut dirty: Vec<_> = dirty_windows.iter().cloned().collect();
        dirty.sort();
        return refuse(format!("context window generations {dirty:?}"));
    }
    for derivative in derivatives {
        if dirty_windows.contains(derivative.window_id.to_string().as_str()) {
            return refuse(format!("derivative {}:{}", derivative.window_id, derivative.idx));
        }
    }
    // An imported context_window_scopes row naming a rejected window's scope
    // is safe only when no window became dirty — already covered above.
    Ok(())
}

/// The row as it will be stored here.
///
/// Imported work is parked: a receiving store is not a second live writer.
/// The original state is kept in a memo, so no fact is lost.
fn effective_row(row: &CognitionRow, spec: &TableSpec) -> Result<CognitionRow> {
    let mut effective = row.clone();
    match spec.kind {
        RowKind::Verbatim | RowKind::InboxEvent => {}
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
            if task.memos.contains_key(COGNITION_IMPORTED)
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
            task.state = TaskState::Pending {
                checkpoint_ref: checkpoint,
            };
            task.dispatch_eligibility = DispatchEligibility::Parked {
                reason_ref: task.task_id,
                until_utc: None,
            };
            task.record_revision = task.record_revision.next();
            task.memos.insert(
                COGNITION_IMPORTED.into(),
                serde_json::json!({
                    "source_state": source_state,
                    "note": "imported work is parked on the receiving store",
                }),
            );
            Ok((serde_json::to_string(&task)?, Some(task.record_revision.0)))
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
            if let Some(object) = value.as_object_mut() {
                let memos = object
                    .entry("memos")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(memos) = memos.as_object_mut() {
                    memos.insert(
                        COGNITION_IMPORTED.into(),
                        serde_json::json!({
                            "note": "imported work is parked on the receiving store"
                        }),
                    );
                }
            }
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
    let sql = format!(
        "SELECT {columns} FROM {table} ORDER BY {order}",
        table = spec.name
    );
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
        ValueRef::Blob(_) => Err(anyhow!(
            "column {index} holds binary data and is not replicated"
        )),
    }
}

/// The stored row with the same primary key, if it exists.
fn read_existing(
    conn: &rusqlite::Connection,
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
    let mut stmt = conn.prepare(&sql).map_err(db)?;
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
    let mut values: Vec<Option<String>> =
        row.values.iter().map(|(_, value)| value.clone()).collect();
    if let Some(local_seq) = spec.local_seq {
        columns.push(local_seq);
        values.push(Some(seq.0.to_string()));
    }
    let placeholders: Vec<String> = (1..=columns.len())
        .map(|index| format!("?{index}"))
        .collect();
    let sql = format!(
        "INSERT INTO {table} ({columns}) VALUES ({placeholders})",
        table = spec.name,
        columns = columns.join(", "),
        placeholders = placeholders.join(", ")
    );
    tx.execute(&sql, rusqlite::params_from_iter(values))
        .map_err(db)?;
    Ok(())
}

/// Take a newer value from the peer for a fact this store still holds as the
/// peer last delivered it.
fn update_row(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    row: &CognitionRow,
    seq: LedgerSeq,
) -> Result<()> {
    let mut columns: Vec<&str> = spec.columns.to_vec();
    let mut values: Vec<Option<String>> =
        row.values.iter().map(|(_, value)| value.clone()).collect();
    if let Some(local_seq) = spec.local_seq {
        columns.push(local_seq);
        values.push(Some(seq.0.to_string()));
    }
    let assignments: Vec<String> = (1..=columns.len())
        .map(|index| format!("{}=?{index}", columns[index - 1]))
        .collect();
    let offset = columns.len();
    let conditions: Vec<String> = spec
        .key
        .iter()
        .enumerate()
        .map(|(index, name)| format!("{name}=?{}", offset + index + 1))
        .collect();
    let keys: Vec<String> = spec
        .key
        .iter()
        .map(|name| row.value(name).unwrap_or_default().to_string())
        .collect();
    let sql = format!(
        "UPDATE {table} SET {assignments} WHERE {conditions}",
        table = spec.name,
        assignments = assignments.join(", "),
        conditions = conditions.join(" AND ")
    );
    tx.execute(
        &sql,
        rusqlite::params_from_iter(values.into_iter().chain(keys.into_iter().map(Some))),
    )
    .map_err(db)?;
    Ok(())
}

/// The value the peer last delivered for one fact.
fn read_baseline(
    tx: &Transaction<'_>,
    peer_id: EntityId,
    table: &str,
    key: &str,
) -> Result<Option<Vec<(String, Option<String>)>>> {
    let stored: Option<String> = tx
        .query_row(
            "SELECT row_values FROM cognition_baselines WHERE peer_id=?1 AND table_name=?2 AND entity_key=?3",
            params![peer_id.to_string(), table, key],
            |row| row.get(0),
        )
        .optional()
        .map_err(db)?;
    match stored {
        Some(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|error| anyhow!("invalid cognition baseline: {error}")),
        None => Ok(None),
    }
}

/// Remember the value the peer delivered for one fact.
fn write_baseline(
    tx: &Transaction<'_>,
    peer_id: EntityId,
    table: &str,
    key: &str,
    values: &[(String, Option<String>)],
) -> Result<()> {
    let json = serde_json::to_string(values)?;
    tx.execute(
        "INSERT INTO cognition_baselines(peer_id,table_name,entity_key,row_values,updated_at)
         VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(peer_id,table_name,entity_key) DO UPDATE SET
            row_values=excluded.row_values,
            updated_at=excluded.updated_at",
        params![
            peer_id.to_string(),
            table,
            key,
            json,
            chrono::Utc::now().to_rfc3339(),
        ],
    )
    .map_err(db)?;
    Ok(())
}

/// Record where an imported subject came from.
///
/// The genesis stays untouched: it keeps saying where the subject was born.
/// The provenance is a separate fact about this store, so the runtime can
/// tell an imported subject from a local one.
fn record_origin(
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
    let cursor = cursor_columns(spec);
    if cursor.is_empty() {
        return local == incoming;
    }
    if local.len() != incoming.len() {
        return false;
    }
    for (index, (name, value)) in incoming.iter().enumerate() {
        if cursor.contains(&name.as_str()) {
            // A local cursor, not a fact of the other store.
            continue;
        }
        let other = local.get(index).and_then(|(_, other)| other.as_ref());
        if other != value.as_ref() {
            return false;
        }
    }
    true
}

/// The columns that are a local cursor rather than a replicated fact.
fn cursor_columns(spec: &TableSpec) -> &'static [&'static str] {
    match spec.kind {
        RowKind::InboxEvent => &["consumed"],
        _ => &[],
    }
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
    let store_namespace = map
        .get("store_namespace")
        .and_then(serde_json::Value::as_str)?;
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
    retired_roots: &BTreeSet<PathBuf>,
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
    let mut stack: Vec<(PathBuf, String)> = vec![(fs::canonicalize(root)?, String::new())];
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
            if retired_roots.contains(&path) {
                continue;
            }
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
fn capture_git_history(root: &Path) -> Result<Option<(CognitionGitDescriptor, Vec<u8>)>> {
    if !root.join(".git").exists() {
        return Ok(None);
    }
    let (has_head, head_id, _) = git(root, &["rev-parse", "--verify", "HEAD"])?;
    if !has_head {
        return Ok(None);
    }
    let (symbolic, head, _) = git(root, &["symbolic-ref", "--quiet", "HEAD"])?;
    ensure!(
        symbolic,
        "memory repository HEAD is detached; replication requires an actual source branch"
    );
    let head = head.trim().to_string();
    ensure!(
        head.starts_with("refs/heads/"),
        "memory repository HEAD is not a branch: {head}"
    );
    let temp = std::env::temp_dir().join(format!("shenko-cognition-{}.bundle", EntityId::new()));
    let temp_path = temp.to_string_lossy().into_owned();
    // Replicated peer refs are receiver-local bookkeeping, not the source's
    // memory history. Including them would make every re-export look changed.
    let (ok, _, error) = git(
        root,
        &[
            "bundle",
            "create",
            temp_path.as_str(),
            "--branches",
            "--tags",
        ],
    )?;
    if !ok {
        let _ = fs::remove_file(&temp);
        bail!("git bundle create failed: {error}");
    }
    let bytes = fs::read(&temp).with_context(|| format!("read {}", temp.display()))?;
    let _ = fs::remove_file(&temp);
    let mut refs: Vec<(String, String)> = Vec::new();
    let (ok, out, error) = git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/tags",
        ],
    )?;
    ensure!(ok, "git ref enumeration failed: {error}");
    for line in out.lines() {
        if let Some((name, sha)) = line.split_once(' ') {
            refs.push((name.to_string(), sha.to_string()));
        }
    }
    ensure!(
        refs.iter()
            .any(|(name, sha)| { name == &head && sha == head_id.trim() }),
        "memory repository HEAD {head} is not bound to its advertised commit"
    );
    let descriptor = CognitionGitDescriptor {
        bytes: bytes.len() as u64,
        digest: Digest::blake3(&bytes),
        head,
        refs,
    };
    Ok(Some((descriptor, bytes)))
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

fn verify_restored_file(target: &Path, descriptor: &CognitionFileDescriptor) -> Result<()> {
    let retained =
        fs::read(target).with_context(|| format!("read restored file {}", target.display()))?;
    ensure!(
        retained.len() as u64 == descriptor.bytes && Digest::blake3(&retained) == descriptor.digest,
        "restored file {} does not match its verified descriptor",
        target.display()
    );
    Ok(())
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

#[cfg(test)]
mod archive_admission_tests {
    use super::*;
    use crate::memory::MemoryEntry;

    #[test]
    fn reordered_labels_cannot_publish_a_paused_head_or_files() {
        use shenko_types::commit::CommitRef;
        use shenko_types::ids::{BranchId, ControllerEpoch, RecordRevision, RuntimeGeneration};
        use shenko_types::subject::{GenesisRef, SubjectGenesis, SubjectStep};

        let root = std::env::temp_dir().join(format!("shenko-row-order-{}", EntityId::new()));
        fs::create_dir(&root).unwrap();
        let source = Store::open(root.join("source.db"), root.join("source-cas")).unwrap();
        let object = source.cas.seal_object(b"seed", "text/plain", "test", "raw", 1).unwrap().object;
        let agent = EntityId::new();
        let genesis_id = EntityId::new();
        let branch = BranchId::new();
        let first = EntityId::new();
        let commit = CommitRef::new(source.ledger.ledger_id(), LedgerSeq::ZERO);
        let genesis = SubjectGenesis {
            genesis_id, subject_id: agent, bootstrap_id: EntityId::new(), launch_id: EntityId::new(),
            seed_manifest_ref: object.clone(), initial_profile_ref: object.clone(),
            home_manifest_ref: object.clone(), bootstrap_event_ref: object, created_commit: commit,
        };
        let head = SubjectBranchHead {
            agent_id: agent, branch_id: branch.clone(), subject_root_task_id: EntityId::new(),
            controller_epoch: ControllerEpoch(1), software_generation: RuntimeGeneration::default(),
            genesis_ref: GenesisRef { ledger_id: commit.ledger_id, genesis_id },
            committed_step_no: 0, committed_step_id: None, committed_at: commit,
            next_step_id: first, mode: SubjectMode::Active, mode_reason_ref: None,
            record_revision: RecordRevision(0),
        };
        let step = SubjectStep {
            step_id: first, branch_id: branch, step_no: 1, expected_parent_commit: commit,
            controller_epoch: head.controller_epoch, software_generation: head.software_generation.clone(),
            state: SubjectStepState::Planned, input_plan_ref: None, context_revision: None,
            active_request_id: None, active_attempt_id: None, model_result_ref: None,
            model_outcome_ref: None, decision_ref: None, commit_ref: None,
            retry_reason_ref: None, record_revision: RecordRevision(0), created_by_step_id: None,
        };
        source.initialize_genesis(&genesis, &head, &step).unwrap();
        let memory = root.join("source-memory");
        fs::create_dir(&memory).unwrap();
        fs::write(memory.join("remote.txt"), b"must not publish").unwrap();
        let mut export = source.export_cognition(&CognitionSnapshotOptions {
            memory_root: Some(memory), include_git_history: false, ..Default::default()
        }).unwrap();
        // Schema 2 takes the same admission path. The named fields look active
        // to a label-based guard, while positional SQL would insert a paused
        // column and paused serialized head without a local boundary record.
        export.manifest.schema = 2;
        let row = export.manifest.rows.iter_mut().find(|row| row.table == "subject_branch_heads").unwrap();
        let active = row.value("data").unwrap().to_string();
        let mut paused: serde_json::Value = serde_json::from_str(&active).unwrap();
        paused["mode"] = "paused".into();
        row.values[3] = ("mode".into(), Some("active".into()));
        row.values[5] = ("committed_step_id".into(), Some("paused".into()));
        row.values[7] = ("created_at".into(), Some(serde_json::to_string(&paused).unwrap()));
        row.values[8] = ("data".into(), Some(active));
        assert_eq!(row.value("mode"), Some("active"));
        assert_eq!(serde_json::from_str::<serde_json::Value>(row.value("data").unwrap()).unwrap()["mode"], "active");
        assert_eq!(row.values[5].1.as_deref(), Some("paused"));
        assert_eq!(serde_json::from_str::<serde_json::Value>(row.values[7].1.as_deref().unwrap()).unwrap()["mode"], "paused");
        export.manifest.fingerprint = fingerprint_of_manifest(&export.manifest);
        let mut archive = Vec::new();
        export.write_archive(&mut archive).unwrap();

        let receiver = Store::open(root.join("receiver.db"), root.join("receiver-cas")).unwrap();
        let receiver_memory = root.join("receiver-memory");
        fs::create_dir(&receiver_memory).unwrap();
        let before = receiver.current_commit_seq().unwrap();
        let error = receiver.import_cognition_archive(&archive[..], &CognitionImportOptions {
            memory_root: Some(receiver_memory.clone()), ..Default::default()
        }).unwrap_err();
        assert!(error.to_string().contains("invalid cognition column layout"));
        assert_eq!(receiver.current_commit_seq().unwrap(), before);
        assert!(receiver.get_existing_agent_id().unwrap().is_none());
        assert!(!receiver_memory.join("remote.txt").exists());
    }

    #[test]
    fn unknown_fact_does_not_produce_a_verified_partial_import() {
        let root = std::env::temp_dir().join(format!("shenko-cognition-{}", EntityId::new()));
        fs::create_dir(&root).unwrap();
        let source = Store::open(root.join("source.db"), root.join("source-cas")).unwrap();
        source.insert_memory_entry(&MemoryEntry::new(EntityId::new(), "known fact")).unwrap();
        let mut export = source.export_cognition(&Default::default()).unwrap();
        export.manifest.rows.push(CognitionRow {
            table: "future_required_fact".into(),
            key: "1".into(),
            values: vec![("id".into(), Some("1".into()))],
        });
        export.manifest.fingerprint = fingerprint_of_manifest(&export.manifest);
        let mut archive = Vec::new();
        export.write_archive(&mut archive).unwrap();
        let receiver = Store::open(root.join("receiver.db"), root.join("receiver-cas")).unwrap();
        assert!(receiver.import_cognition_archive(&archive[..], &Default::default()).is_err());
        assert!(receiver.list_memory(10).unwrap().is_empty());
    }
}
