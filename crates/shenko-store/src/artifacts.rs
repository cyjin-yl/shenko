//! Durable Playground business state — docs/specs/configuration-playground.md#spec-43-2.
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
use serde_json::Value;

use shenko_types::commit::CommitRef;
use shenko_types::ids::EntityId;
use shenko_types::inbox::{InboxEvent, InboxEventKind};
use shenko_types::object::{Digest, ObjectRef};
use shenko_types::operation::{AttemptDispatchStage, AttemptRecord, OperationRecord, OperationState};
use shenko_types::task::{DispatchEligibility, TaskInvocation};

use crate::external::{InboxAdmission, validate_original_event};
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

/// The frontend half of a manifest — docs/specs/configuration-playground.md#spec-43-2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendBinding {
    pub bundle_ref: ObjectRef,
    pub entry: String,
    pub surface_contract_ref: ObjectRef,
}

/// The backend half of a manifest — docs/specs/configuration-playground.md#spec-43-2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendManifest {
    pub entry_ref: ObjectRef,
    /// The registered runtime provider that can actually run this backend.
    pub runtime_ref: ObjectRef,
    pub service_contract_ref: ObjectRef,
}

/// A versioned artifact manifest — docs/specs/configuration-playground.md#spec-43-2.
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

/// The durable state of one artifact — docs/specs/configuration-playground.md#spec-43-2.
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

/// The subject-facing fact that announces one artifact record transition —
/// docs/specs/configuration-playground.md#spec-43-2 (`artifact.state_changed` enters the same subject inbox).
///
/// It is committed in the SAME ledger transaction as the record it describes,
/// so a transition can never be durable without its admitted inbox fact and an
/// admitted fact can never describe a transition that was not committed. That
/// atomic boundary is what makes "notification admission" non-lossy: there is
/// no window in which the caller could retry a transition whose notification
/// was lost, and no blind repeat after an ambiguous effect.
#[derive(Debug, Clone)]
pub struct ArtifactNotification {
    pub event_id: EntityId,
    /// The subject that must drain this fact.
    pub agent_id: EntityId,
    /// Inbox kind column (`artifact_changed`).
    pub kind: String,
    pub source: String,
    /// The full serialized `InboxEvent`; it is validated exactly as an
    /// externally produced original event.
    pub payload: Value,
}

/// A prepared artifact transition. CAS objects are sealed before this reaches
/// the ledger; the expected row revision prevents an older proposal from
/// overwriting a newer edit, adoption, or runtime fact.
pub struct ArtifactTransition<'a> {
    pub record: &'a ArtifactRecord,
    /// `None` requires an absent artifact; `Some` requires this exact revision.
    pub expected_revision: Option<u64>,
    pub manifest: Option<&'a ArtifactManifest>,
    pub notification: Option<&'a ArtifactNotification>,
    /// A no-change request still checks its snapshot and may record a receipt.
    pub write_record: bool,
}

/// Bind a local artifact effect to its already armed native operation. The
/// checkpoint contains the original observation and E1 receipt, so recovery
/// can settle that same attempt without executing the mutation again.
pub struct ArtifactOperation<'a> {
    pub invocation: &'a TaskInvocation,
    pub operation_id: EntityId,
    pub operation_name: &'a str,
    pub contract_digest: &'a Digest,
    pub attempt_id: EntityId,
    pub input_digest: &'a Digest,
    pub checkpoint_ref: &'a ObjectRef,
}

pub struct ArtifactCommit {
    pub record: Option<ArtifactRecord>,
    pub checkpoint_ref: Option<ObjectRef>,
    pub reader_receipt: Option<ArtifactReaderResult>,
    /// An earlier commit of this operation won. No proposed facts were written.
    pub replayed: bool,
}

/// A caller-supplied request identity in the artifact's actual actor namespace.
/// The input digest freezes the action as well as all its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReaderIdentity {
    pub event_id: EntityId,
    pub artifact_id: EntityId,
    pub subject_id: EntityId,
    pub actor: String,
    pub request_id: String,
    pub input_digest: Digest,
}

pub struct ArtifactReaderRequest<'a> {
    pub identity: &'a ArtifactReaderIdentity,
    pub input_ref: &'a ObjectRef,
    /// An immutable additional result, such as the exact appended version.
    pub result_ref: Option<&'a ObjectRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReaderReceipt {
    pub schema_version: u32,
    pub identity: ArtifactReaderIdentity,
    pub input_ref: ObjectRef,
    pub record: ArtifactRecord,
    pub result_ref: Option<ObjectRef>,
    pub committed_at: CommitRef,
}

pub struct ArtifactReaderResult {
    pub receipt_ref: ObjectRef,
    pub receipt: ArtifactReaderReceipt,
    pub event: InboxEvent,
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
    /// Commit the manifest, record, announcing inbox fact and operation-bound
    /// observation in one transaction. A repeated operation returns its
    /// original checkpoint, including after its owner/epoch has become stale;
    /// a new effect must still own the live invocation at this commit boundary.
    pub fn commit_artifact_transition(
        &self,
        transition: Option<ArtifactTransition<'_>>,
        binding: Option<ArtifactOperation<'_>>,
        reader: Option<ArtifactReaderRequest<'_>>,
    ) -> Result<ArtifactCommit> {
        ensure!(
            transition.is_some() || binding.is_some(),
            "empty artifact commit"
        );
        let serialized_event = transition
            .as_ref()
            .and_then(|t| t.notification)
            .map(|event| {
                validate_original_event(
                    event.event_id,
                    &event.kind,
                    &event.source,
                    &event.payload,
                )?;
                Ok::<_, anyhow::Error>(serde_json::to_string(&event.payload)?)
            })
            .transpose()?;
        if let Some(binding) = &binding {
            let bytes = self.cas.read_object(&binding.checkpoint_ref.digest)?;
            ensure!(
                bytes.len() as u64 == binding.checkpoint_ref.bytes
                    && Digest::blake3(&bytes) == binding.checkpoint_ref.digest,
                "artifact operation checkpoint mismatch"
            );
        }
        if let Some(reader) = &reader {
            ensure!(
                reader.input_ref.digest == reader.identity.input_digest,
                "artifact reader input digest mismatch"
            );
            for reference in std::iter::once(reader.input_ref).chain(reader.result_ref) {
                let bytes = self.cas.read_object(&reference.digest)?;
                ensure!(
                    bytes.len() as u64 == reference.bytes
                        && Digest::blake3(&bytes) == reference.digest,
                    "artifact reader input/result closure mismatch"
                );
            }
        }
        self.ledger.with_mutation(|| {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(reader) = &reader {
                if let Some(original) = self.read_artifact_reader_receipt(&tx, reader.identity)? {
                    return Ok(ArtifactCommit { record: Some(original.receipt.record.clone()),
                        checkpoint_ref: None, reader_receipt: Some(original), replayed: true });
                }
                let head_exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM subject_branch_heads WHERE agent_id=?1)",
                    [reader.identity.subject_id.to_string()], |row| row.get(0))?;
                ensure!(head_exists, "artifact reader subject has no head");
                let transition = transition.as_ref().context("reader receipt requires an artifact snapshot")?;
                ensure!(transition.record.artifact_id == reader.identity.artifact_id
                    && transition.record.subject_id == reader.identity.subject_id,
                    "artifact reader identity mismatch");
            }
            let mut operation = if let Some(binding) = &binding {
                let raw: String = tx.query_row(
                    "SELECT data FROM operations WHERE operation_id=?1",
                    [binding.operation_id.to_string()], |row| row.get(0))?;
                let operation: OperationRecord = serde_json::from_str(&raw)?;
                let raw: String = tx.query_row(
                    "SELECT data FROM attempts WHERE attempt_id=?1",
                    [binding.attempt_id.to_string()], |row| row.get(0))?;
                let attempt: AttemptRecord = serde_json::from_str(&raw)?;
                ensure!(operation.operation_id == binding.operation_id
                    && operation.task_id == binding.invocation.task_id
                    && operation.operation_name == binding.operation_name
                    && operation.contract_ref.contract_digest == *binding.contract_digest
                    && operation.input_digest == *binding.input_digest
                    && attempt.operation_id == operation.operation_id
                    && attempt.attempt_id == binding.attempt_id
                    && attempt.attempt_no == 1,
                    "artifact operation/attempt identity mismatch");
                // A stale owner is allowed to read the already committed
                // result. It is never allowed to append a replacement result.
                if let Some(checkpoint_ref) = operation.checkpoint_scope_ref {
                    return Ok(ArtifactCommit { record: None, checkpoint_ref: Some(checkpoint_ref), reader_receipt: None, replayed: true });
                }
                let task = crate::child::read_task(&tx, operation.task_id)?
                    .context("artifact operation task missing")?;
                ensure!(self.native_invocation_current_tx(&tx, &task, binding.invocation)?
                    && matches!(task.dispatch_eligibility, DispatchEligibility::Ready)
                    && !task.memos.contains_key(crate::child::NATIVE_SETTLEMENT_ONLY)
                    && operation.state == OperationState::InFlight
                    && attempt.dispatch_stage == AttemptDispatchStage::Armed
                    && attempt.final_receipt_id.is_none()
                    && attempt.invocation_id == binding.invocation.invocation_id
                    && attempt.worker_id == binding.invocation.worker_id
                    && attempt.node_id == binding.invocation.node_id
                    && attempt.epoch == binding.invocation.epoch,
                    "stale artifact mutation invocation");
                if let Some(transition) = &transition {
                    ensure!(transition.record.subject_id == task.agent_id,
                        "artifact operation belongs to another subject");
                }
                Some(operation)
            } else { None };

            let mut unchanged_record = None;
            if let Some(transition) = &transition {
                let stored: Option<String> = tx.query_row(
                    "SELECT data FROM artifact_records WHERE artifact_id=?1",
                    [transition.record.artifact_id.to_string()], |row| row.get(0)).optional()?;
                let previous: Option<ArtifactRecord> = stored
                    .as_deref().map(serde_json::from_str).transpose()?;
                ensure!(previous.as_ref().map(|record| record.record_revision) == transition.expected_revision,
                    "artifact record revision changed before commit");
                ensure!(previous.as_ref().is_none_or(|record| record.subject_id == transition.record.subject_id),
                    "artifact subject cannot change");
                if !transition.write_record {
                    unchanged_record = Some(previous.context("unchanged artifact record missing")?);
                }
                if let Some(event) = transition.notification {
                    ensure!(event.agent_id == transition.record.subject_id,
                        "artifact notification belongs to another subject");
                    let head_exists: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM subject_branch_heads WHERE agent_id=?1)",
                        [event.agent_id.to_string()], |row| row.get(0))?;
                    ensure!(head_exists, "artifact notification agent has no subject head");
                }
                if let Some(manifest) = transition.manifest {
                    ensure!(manifest.artifact_id == transition.record.artifact_id,
                        "artifact manifest identity mismatch");
                    let stored: Option<String> = tx.query_row(
                        "SELECT data FROM artifact_manifests WHERE artifact_id=?1 AND version_digest=?2",
                        params![manifest.artifact_id.to_string(), manifest.version_digest.0],
                        |row| row.get(0)).optional()?;
                    if let Some(raw) = stored {
                        ensure!(serde_json::from_str::<ArtifactManifest>(&raw)? == *manifest,
                            "artifact manifest digest already describes a different manifest");
                    }
                }
            }
            let seq = self.ledger.allocate_seq(&tx)?;
            let commit = CommitRef::new(self.ledger.ledger_id(), seq);
            let mut entities = 0;
            let record = if let Some(transition) = &transition {
                let mut record = if transition.write_record { transition.record.clone() }
                    else { unchanged_record.context("unchanged artifact snapshot missing")? };
                if transition.write_record {
                record.record_revision = transition.expected_revision
                    .map(|revision| revision.checked_add(1).context("artifact revision overflow"))
                    .transpose()?.unwrap_or(0);
                record.updated_commit = commit;
                if let Some(manifest) = transition.manifest {
                    entities += tx.execute(
                        "INSERT OR IGNORE INTO artifact_manifests(artifact_id,version_digest,record_revision,data,updated_commit_seq)
                         VALUES(?1,?2,0,?3,?4)",
                        params![manifest.artifact_id.to_string(), manifest.version_digest.0,
                            serde_json::to_string(manifest)?, seq.0 as i64])?;
                }
                tx.execute(
                    "INSERT INTO artifact_records(artifact_id,subject_id,lifecycle,state_revision,record_revision,data,updated_commit_seq)
                     VALUES(?1,?2,?3,?4,?5,?6,?7)
                     ON CONFLICT(artifact_id) DO UPDATE SET lifecycle=excluded.lifecycle,
                       state_revision=excluded.state_revision,record_revision=excluded.record_revision,
                       data=excluded.data,updated_commit_seq=excluded.updated_commit_seq",
                    params![record.artifact_id.to_string(), record.subject_id.to_string(), record.lifecycle,
                        record.state_revision, record.record_revision as i64, serde_json::to_string(&record)?, seq.0 as i64])?;
                entities += 1;
                if let (Some(event), Some(serialized)) = (transition.notification, serialized_event.as_ref()) {
                    let existing: Option<(String, String, String, String)> = tx.query_row(
                        "SELECT agent_id,kind,source,data FROM inbox_events WHERE event_id=?1",
                        [event.event_id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).optional()?;
                    if let Some((agent, kind, source, data)) = existing {
                        ensure!(agent == event.agent_id.to_string() && kind == event.kind
                            && source == event.source && data == *serialized,
                            "inbox_event_conflict: same event_id with different facts");
                    } else {
                        tx.execute("INSERT INTO inbox_events(event_id,agent_id,kind,source,consumed,data)
                            VALUES(?1,?2,?3,?4,0,?5)",
                            params![event.event_id.to_string(), event.agent_id.to_string(), event.kind, event.source, serialized])?;
                        entities += 1;
                    }
                }
                }
                Some(record)
            } else { None };
            let reader_receipt = if let Some(reader) = &reader {
                let receipt = ArtifactReaderReceipt {
                    schema_version: 1, identity: reader.identity.clone(), input_ref: reader.input_ref.clone(),
                    record: record.clone().context("reader receipt has no artifact record")?,
                    result_ref: reader.result_ref.cloned(), committed_at: commit,
                };
                // Seal before the SQLite commit. A failed transaction may
                // leave unreferenced CAS bytes, never a durable dangling ref.
                let receipt_ref = self.cas.seal_object(&serde_json::to_vec(&receipt)?,
                    "application/json", "artifacts", "artifact-reader-receipt", 1)?.object;
                let event = InboxEvent {
                    event_id: reader.identity.event_id, kind: InboxEventKind::SystemEvent,
                    source: "playground".into(), created_at: chrono::Utc::now(),
                    payload: serde_json::json!({"kind": "artifact.request_recorded",
                        "artifact_id": reader.identity.artifact_id, "request": reader.identity, "receipt_ref": receipt_ref,
                        "cause_event_id": transition.as_ref().and_then(|transition| transition.notification).map(|event| event.event_id),
                        "changes_state": transition.as_ref().is_some_and(|transition| transition.write_record),
                    }),
                };
                let serialized = serde_json::to_string(&event)?;
                tx.execute("INSERT INTO inbox_events(event_id,agent_id,kind,source,consumed,data)
                    VALUES(?1,?2,'system_event','playground',0,?3)",
                    params![reader.identity.event_id.to_string(), reader.identity.subject_id.to_string(), serialized])?;
                entities += 1;
                Some(ArtifactReaderResult { receipt_ref, receipt, event })
            } else { None };
            if let (Some(operation), Some(binding)) = (operation.as_mut(), binding.as_ref()) {
                operation.checkpoint_scope_ref = Some(binding.checkpoint_ref.clone());
                operation.record_revision = operation.record_revision.next();
                tx.execute("UPDATE operations SET record_revision=?2,data=?3,updated_commit_seq=?4 WHERE operation_id=?1",
                    params![operation.operation_id.to_string(), operation.record_revision.0 as i64,
                        serde_json::to_string(operation)?, seq.0 as i64])?;
                entities += 1;
            }
            self.ledger.record_commit(&tx, seq, entities)?;
            tx.commit()?;
            Ok(ArtifactCommit { record, checkpoint_ref: operation.and_then(|op| op.checkpoint_scope_ref), reader_receipt, replayed: false })
        }, |error: &anyhow::Error| error.is::<rusqlite::Error>() || error.is::<serde_json::Error>()
            || error.downcast_ref::<shenko_types::error::ShenkoError>()
                .is_some_and(|error| matches!(error, shenko_types::error::ShenkoError::Storage(_))))
    }

    /// Resolve the original request before checking today's artifact head.
    pub fn artifact_reader_receipt(
        &self,
        identity: &ArtifactReaderIdentity,
    ) -> Result<Option<ArtifactReaderResult>> {
        let conn = self.ledger.conn.lock();
        self.read_artifact_reader_receipt(&conn, identity)
    }

    fn read_artifact_reader_receipt(
        &self,
        conn: &Connection,
        identity: &ArtifactReaderIdentity,
    ) -> Result<Option<ArtifactReaderResult>> {
        let stored: Option<(String, String, String, String)> = conn
            .query_row(
                "SELECT agent_id,kind,source,data FROM inbox_events WHERE event_id=?1",
                [identity.event_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((subject, kind, source, data)) = stored else {
            return Ok(None);
        };
        ensure!(
            subject == identity.subject_id.to_string()
                && kind == "system_event"
                && source == "playground",
            "ACTION_CONFLICT: request identity already describes another fact"
        );
        let event: InboxEvent = serde_json::from_str(&data)?;
        let request: ArtifactReaderIdentity = serde_json::from_value(
            event
                .payload
                .get("request")
                .cloned()
                .context("artifact reader receipt request missing")?,
        )?;
        ensure!(
            event.event_id == identity.event_id
                && event.kind == InboxEventKind::SystemEvent
                && event.source == "playground"
                && event.payload.get("kind").and_then(Value::as_str)
                    == Some("artifact.request_recorded")
                && request == *identity,
            "ACTION_CONFLICT: request_id already belongs to different input"
        );
        let receipt_ref: ObjectRef = serde_json::from_value(
            event
                .payload
                .get("receipt_ref")
                .cloned()
                .context("artifact reader receipt reference missing")?,
        )?;
        let bytes = self.cas.read_object(&receipt_ref.digest)?;
        ensure!(
            bytes.len() as u64 == receipt_ref.bytes && Digest::blake3(&bytes) == receipt_ref.digest,
            "artifact reader receipt closure mismatch"
        );
        let receipt: ArtifactReaderReceipt = serde_json::from_slice(&bytes)?;
        ensure!(
            receipt.schema_version == 1
                && receipt.identity == *identity
                && receipt.input_ref.digest == identity.input_digest
                && receipt.record.artifact_id == identity.artifact_id
                && receipt.record.subject_id == identity.subject_id,
            "artifact reader receipt identity mismatch"
        );
        Ok(Some(ArtifactReaderResult {
            receipt_ref,
            receipt,
            event,
        }))
    }

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

    /// The artifact roots this installation can enumerate — docs/specs/configuration-playground.md#spec-43-2.
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
        self.put_artifact_record_with_event(record, None)
            .map(|(record, _)| record)
    }

    /// Commit one artifact record and — when supplied — the subject-facing
    /// fact that announces it, in ONE ledger transaction.
    ///
    /// This is the one atomic boundary between "the transition happened" and
    /// "the subject was told": the record write and the admitted inbox event
    /// commit together, so a caller that lost the response finds the same
    /// transition and the same `Replay` admission afterwards. Repeating the
    /// transition re-admits the identical fact (same stable event id), so no
    /// retry can manufacture a second stimulus and no transition can be
    /// durable with its notification silently dropped.
    ///
    /// The event is validated before the writer is claimed: an invalid fact
    /// must fail before it poisons the mutation line.
    pub fn put_artifact_record_with_event(
        &self,
        record: &ArtifactRecord,
        notification: Option<&ArtifactNotification>,
    ) -> Result<(ArtifactRecord, Option<InboxAdmission>)> {
        let serialized_event = match notification {
            None => None,
            Some(notification) => {
                validate_original_event(
                    notification.event_id,
                    &notification.kind,
                    &notification.source,
                    &notification.payload,
                )?;
                // The fact needs a real drain owner before it is worth
                // committing; a missing head is a typed rejection here, not a
                // row failure inside the writer transaction.
                {
                    let conn = self.ledger.conn.lock();
                    let head_exists: Option<String> = conn
                        .query_row(
                            "SELECT data FROM subject_branch_heads WHERE agent_id=?1",
                            [notification.agent_id.to_string()],
                            |row| row.get(0),
                        )
                        .optional()?;
                    ensure!(
                        head_exists.is_some(),
                        "artifact notification agent has no subject head"
                    );
                }
                Some(serde_json::to_string(&notification.payload)?)
            }
        };
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
                let admission = match (notification, serialized_event.as_ref()) {
                    (Some(notification), Some(serialized)) => {
                        let existing: Option<(String, String, String, String)> = tx
                            .query_row(
                                "SELECT agent_id, kind, source, data FROM inbox_events WHERE event_id=?1",
                                [notification.event_id.to_string()],
                                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                            )
                            .optional()?;
                        match existing {
                            Some((agent, kind, source, data)) => {
                                if agent != notification.agent_id.to_string()
                                    || kind != notification.kind
                                    || source != notification.source
                                    || &data != serialized
                                {
                                    anyhow::bail!(
                                        "inbox_event_conflict: same event_id with different facts"
                                    );
                                }
                                Some(InboxAdmission::Replay)
                            }
                            None => {
                                tx.execute(
                                    "INSERT INTO inbox_events(event_id,agent_id,kind,source,consumed,data)
                                     VALUES(?1,?2,?3,?4,0,?5)",
                                    params![
                                        notification.event_id.to_string(),
                                        notification.agent_id.to_string(),
                                        notification.kind,
                                        notification.source,
                                        serialized,
                                    ],
                                )?;
                                Some(InboxAdmission::Inserted)
                            }
                        }
                    }
                    _ => None,
                };
                let entities = if admission.is_some() { 2 } else { 1 };
                self.ledger.record_commit(&tx, seq, entities)?;
                tx.commit()?;
                Ok((next, admission))
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
