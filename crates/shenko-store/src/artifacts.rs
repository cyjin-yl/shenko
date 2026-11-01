//! Durable Playground business state — SPEC §43.2.
//!
//! An artifact is a long-lived work: its source, build lock, frontend bundle,
//! backend entry, persistent state bindings, action schemas, operation
//! contracts and recovery closure are versioned facts, and its lifecycle is a
//! record rather than a directory listing. The records live in the same
//! ledger as tasks and operations so a peer receives them with the same
//! closure, and so a lost workspace can be rebuilt from durable facts instead
//! of from whatever happened to be on disk.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use shenko_types::commit::CommitRef;
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef};

use crate::store::Store;

/// Schema owned by this module. Additive; an existing installation keeps its
/// artifact rows.
pub const ARTIFACT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS artifact_records (
    artifact_id TEXT PRIMARY KEY,
    subject_id TEXT NOT NULL,
    lifecycle TEXT NOT NULL,
    state_revision TEXT NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_artifact_records_subject ON artifact_records(subject_id);

CREATE TABLE IF NOT EXISTS artifact_manifests (
    artifact_id TEXT NOT NULL,
    version_digest TEXT NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (artifact_id, version_digest)
);

CREATE TABLE IF NOT EXISTS artifact_runtime_instances (
    instance_id TEXT PRIMARY KEY,
    artifact_id TEXT NOT NULL,
    state TEXT NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_artifact_runtime_artifact
    ON artifact_runtime_instances(artifact_id, state);
"#;

/// Apply this module's schema. Idempotent.
pub fn ensure_artifact_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(ARTIFACT_SCHEMA).context("artifact schema")?;
    Ok(())
}

/// The frontend half of a manifest — SPEC §43.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendBinding {
    pub bundle_ref: ObjectRef,
    pub entry: String,
    pub surface_contract_ref: ObjectRef,
}

/// The backend half of a manifest — SPEC §43.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendManifest {
    pub entry_ref: ObjectRef,
    /// The registered runtime provider that can actually run this backend.
    pub runtime_ref: ObjectRef,
    pub service_contract_ref: ObjectRef,
}

/// A versioned artifact manifest — SPEC §43.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactManifest {
    pub schema_version: u32,
    pub artifact_id: EntityId,
    pub version_digest: Digest,
    pub source_refs: Vec<ObjectRef>,
    pub build_and_dependency_lock_ref: ObjectRef,
    pub frontend: Option<FrontendBinding>,
    pub backend: Option<BackendManifest>,
    /// Schema, durable location, recovery domain and migration relation of the
    /// artifact's business state. Code version and data revision are separate.
    pub state_bindings_ref: ObjectRef,
    pub action_schemas_ref: ObjectRef,
    pub operation_contracts_ref: ObjectRef,
    pub secret_handles_ref: Option<ObjectRef>,
    pub health_contract_ref: ObjectRef,
    pub recovery_closure_ref: ObjectRef,
}

/// The durable state of one artifact — SPEC §43.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRecord {
    pub schema_version: u32,
    pub artifact_id: EntityId,
    pub subject_id: EntityId,
    pub creator_task_id: Option<EntityId>,
    pub title: String,
    /// Durable row revision, allocated by the Store on every write. Rows
    /// serialized before this field existed default to 0.
    #[serde(default)]
    pub record_revision: u64,
    pub active_manifest_ref: Option<ObjectRef>,
    pub candidate_manifest_refs: Vec<ObjectRef>,
    pub state_revision: String,
    pub state_ref: ObjectRef,
    /// draft | running | stopped | archived | recovering
    pub lifecycle: String,
    pub runtime_instance_refs: Vec<ObjectRef>,
    pub desired_lifecycle_ref: ObjectRef,
    pub provenance_and_causation_ref: ObjectRef,
    /// The real artifact root this installation selected for this artifact.
    /// It is a locator; the identity below is what proves the root was not
    /// swapped under a running instance.
    pub artifact_root: PathBuf,
    pub root_identity: Digest,
    pub updated_commit: CommitRef,
}

/// The identity of a running host process — enough to tell "the same process
/// is still there" from "a new process reused the pid".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub process_start_token: String,
    pub executable_digest: Digest,
}

/// One selected runtime instance of an artifact's backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInstanceRecord {
    pub schema_version: u32,
    pub instance_id: EntityId,
    pub artifact_id: EntityId,
    pub artifact_revision: u64,
    /// Durable row revision, allocated by the Store on every write.
    #[serde(default)]
    pub record_revision: u64,
    pub manifest_digest: Digest,
    pub runtime_ref: ObjectRef,
    pub service_contract_ref: ObjectRef,
    /// The real root this instance owns. It is inside the installation's
    /// selected root, so an upgrade that swaps the root cannot leave an
    /// orphan service behind unnoticed.
    pub selected_root: PathBuf,
    pub root_identity: Digest,
    pub process_identity: Option<ProcessIdentity>,
    /// starting | running | stopped | unknown | failed
    pub state: String,
    pub started_at_commit: CommitRef,
    pub updated_commit: CommitRef,
}

impl Store {
    /// The durable record of one artifact.
    pub fn artifact_record(&self, artifact_id: EntityId) -> Result<Option<ArtifactRecord>> {
        let conn = self.ledger.conn.lock();
        let raw: Option<String> = conn
            .query_row(
                "SELECT data FROM artifact_records WHERE artifact_id=?1",
                [artifact_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        match raw {
            None => Ok(None),
            Some(raw) => {
                let record: ArtifactRecord = serde_json::from_str(&raw)?;
                ensure!(
                    record.artifact_id == artifact_id,
                    "artifact record identity mismatch"
                );
                Ok(Some(record))
            }
        }
    }

    /// Every artifact of this installation, newest first.
    pub fn list_artifact_records(&self) -> Result<Vec<ArtifactRecord>> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT data FROM artifact_records ORDER BY updated_commit_seq DESC, artifact_id",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str::<ArtifactRecord>(&row?)?);
        }
        Ok(out)
    }

    /// The artifact roots this installation can enumerate — SPEC §43.2.
    ///
    /// A root is only usable when the recorded identity still matches the
    /// directory on disk; a replaced root is reported, never silently used.
    pub fn artifact_roots(&self) -> Result<Vec<(EntityId, PathBuf, Digest)>> {
        Ok(self
            .list_artifact_records()?
            .into_iter()
            .map(|record| (record.artifact_id, record.artifact_root, record.root_identity))
            .collect())
    }

    /// Commit an artifact record. The record and its manifest move together
    /// with the record's revision in one ledger commit.
    pub fn put_artifact_record(&self, record: &ArtifactRecord) -> Result<ArtifactRecord> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let stored: Option<String> = tx
                    .query_row(
                        "SELECT data FROM artifact_records WHERE artifact_id=?1",
                        [record.artifact_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                let mut next = record.clone();
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let revision = match stored {
                    None => 0,
                    Some(raw) => {
                        let previous: ArtifactRecord = serde_json::from_str(&raw)?;
                        previous.record_revision + 1
                    }
                };
                next.record_revision = revision;
                next.updated_commit = commit;
                tx.execute(
                    "INSERT INTO artifact_records(artifact_id,subject_id,lifecycle,state_revision,record_revision,data,updated_commit_seq)
                     VALUES(?1,?2,?3,?4,?5,?6,?7)
                     ON CONFLICT(artifact_id) DO UPDATE SET
                       lifecycle=excluded.lifecycle,
                       state_revision=excluded.state_revision,
                       record_revision=excluded.record_revision,
                       data=excluded.data,
                       updated_commit_seq=excluded.updated_commit_seq",
                    params![
                        next.artifact_id.to_string(),
                        next.subject_id.to_string(),
                        next.lifecycle,
                        next.state_revision,
                        revision as i64,
                        serde_json::to_string(&next)?,
                        seq.0 as i64,
                    ],
                )?;
                self.ledger.record_commit(&tx, seq, 1)?;
                tx.commit()?;
                Ok(next)
            },
            |error: &anyhow::Error| error.is::<rusqlite::Error>() || error.is::<serde_json::Error>(),
        )
    }

    /// Commit an artifact manifest. Manifests are immutable: the same digest
    /// always describes the same manifest.
    pub fn put_artifact_manifest(&self, manifest: &ArtifactManifest) -> Result<ArtifactManifest> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let stored: Option<String> = tx
                    .query_row(
                        "SELECT data FROM artifact_manifests WHERE artifact_id=?1 AND version_digest=?2",
                        params![manifest.artifact_id.to_string(), manifest.version_digest.0],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(raw) = stored {
                    let existing: ArtifactManifest = serde_json::from_str(&raw)?;
                    ensure!(
                        existing == *manifest,
                        "artifact manifest digest already describes a different manifest"
                    );
                    tx.commit()?;
                    return Ok(existing);
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                tx.execute(
                    "INSERT INTO artifact_manifests(artifact_id,version_digest,record_revision,data,updated_commit_seq)
                     VALUES(?1,?2,0,?3,?4)",
                    params![
                        manifest.artifact_id.to_string(),
                        manifest.version_digest.0,
                        serde_json::to_string(manifest)?,
                        seq.0 as i64,
                    ],
                )?;
                self.ledger.record_commit(&tx, seq, 1)?;
                tx.commit()?;
                Ok(manifest.clone())
            },
            |error: &anyhow::Error| error.is::<rusqlite::Error>() || error.is::<serde_json::Error>(),
        )
    }

    /// One committed manifest of an artifact.
    pub fn artifact_manifest(
        &self,
        artifact_id: EntityId,
        version_digest: &Digest,
    ) -> Result<Option<ArtifactManifest>> {
        let conn = self.ledger.conn.lock();
        let raw: Option<String> = conn
            .query_row(
                "SELECT data FROM artifact_manifests WHERE artifact_id=?1 AND version_digest=?2",
                params![artifact_id.to_string(), version_digest.0],
                |row| row.get(0),
            )
            .optional()?;
        match raw {
            None => Ok(None),
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        }
    }

    /// Every committed manifest of one artifact.
    pub fn list_artifact_manifests(&self, artifact_id: EntityId) -> Result<Vec<ArtifactManifest>> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT data FROM artifact_manifests WHERE artifact_id=?1 ORDER BY version_digest",
        )?;
        let rows = stmt.query_map([artifact_id.to_string()], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str::<ArtifactManifest>(&row?)?);
        }
        Ok(out)
    }

    /// Commit a runtime instance record. The instance identity and its state
    /// are durable, so a restart can tell a still-running service from a
    /// stale row and must not start a second copy.
    pub fn put_runtime_instance(&self, instance: &RuntimeInstanceRecord) -> Result<RuntimeInstanceRecord> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let stored: Option<String> = tx
                    .query_row(
                        "SELECT data FROM artifact_runtime_instances WHERE instance_id=?1",
                        [instance.instance_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                let mut next = instance.clone();
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let revision = match stored {
                    None => 0,
                    Some(raw) => {
                        let previous: RuntimeInstanceRecord = serde_json::from_str(&raw)?;
                        previous.record_revision + 1
                    }
                };
                next.record_revision = revision;
                next.updated_commit = commit;
                tx.execute(
                    "INSERT INTO artifact_runtime_instances(instance_id,artifact_id,state,record_revision,data,updated_commit_seq)
                     VALUES(?1,?2,?3,?4,?5,?6)
                     ON CONFLICT(instance_id) DO UPDATE SET
                       state=excluded.state,
                       record_revision=excluded.record_revision,
                       data=excluded.data,
                       updated_commit_seq=excluded.updated_commit_seq",
                    params![
                        next.instance_id.to_string(),
                        next.artifact_id.to_string(),
                        next.state,
                        revision as i64,
                        serde_json::to_string(&next)?,
                        seq.0 as i64,
                    ],
                )?;
                self.ledger.record_commit(&tx, seq, 1)?;
                tx.commit()?;
                Ok(next)
            },
            |error: &anyhow::Error| error.is::<rusqlite::Error>() || error.is::<serde_json::Error>(),
        )
    }

    /// One runtime instance record.
    pub fn runtime_instance(&self, instance_id: EntityId) -> Result<Option<RuntimeInstanceRecord>> {
        let conn = self.ledger.conn.lock();
        let raw: Option<String> = conn
            .query_row(
                "SELECT data FROM artifact_runtime_instances WHERE instance_id=?1",
                [instance_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        match raw {
            None => Ok(None),
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        }
    }

    /// Every runtime instance record of one artifact.
    pub fn runtime_instances(&self, artifact_id: EntityId) -> Result<Vec<RuntimeInstanceRecord>> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT data FROM artifact_runtime_instances WHERE artifact_id=?1 ORDER BY instance_id",
        )?;
        let rows = stmt.query_map([artifact_id.to_string()], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str::<RuntimeInstanceRecord>(&row?)?);
        }
        Ok(out)
    }
}
