//! Full-stack Playground.
//!
//! An artifact is a long-lived work with source, build lock, optional
//! frontend bundle, an executable backend bound to a registered runtime,
//! persistent business state and a recovery closure. The immutable version
//! chain below is the artifact's *content* store; the durable facts — the
//! artifact record, its versioned manifests and the runtime instances — live
//! in the Store ledger so a peer receives them with the same closure and a
//! lost workspace can be rebuilt from facts instead of from disk. Backend
//! actions use the existing native task/operation ledger, not these files.
//!
//! The subject sees every committed version as a stable inbox fact, so a
//! process stop before the SQL import still leaves a recoverable record.

use std::{collections::HashMap, fs, io::{self, Write}, path::{Path, PathBuf}, sync::Arc};
use axum::{
    body::{Body, Bytes}, extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Json, Redirect, Response}, routing::{get, post}, Router,
};
use chrono::{DateTime, Utc};
use parking_lot::{Mutex, MutexGuard, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_store::Store;
use shenko_store::artifacts::{ArtifactCommit, ArtifactManifest, ArtifactOperation, ArtifactRecord,
    ArtifactTransition, ArtifactReaderIdentity, ArtifactReaderRequest, ArtifactReaderResult, BackendManifest, FrontendBinding};
use shenko_subject::NativeChildService;
use shenko_types::inbox::{InboxEvent, InboxEventKind};
use shenko_types::{ids::EntityId, object::{Digest, ObjectRef}};
use tokio::sync::mpsc::Sender;
use uuid::Uuid;
use crate::playground_backend::{ActionError, ArtifactActor, BackendActionRequest, BackendBinding, BackendReceipt};

pub(crate) const ARTIFACT_SCHEMA: u32 = 2;

/// One immutable content version of an artifact. Content and version records
/// are immutable. The ledger's artifact record selects the head; files are
/// recoverable projections of the same CAS closure. Older file-selected heads
/// are imported on open without discarding their retained version chains.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactVersion {
    pub schema_version: u32,
    pub artifact_id: EntityId,
    pub title: String,
    pub mime: String,
    pub bytes_path: PathBuf,
    pub content_ref: ObjectRef,
    pub revision: u64,
    pub parent_revision: Option<u64>,
    pub restored_from: Option<u64>,
    pub updated_at: DateTime<Utc>,
    pub backend: Option<BackendBinding>,
}

#[derive(Clone)]
pub struct PlaygroundState {
    pub(crate) root: Arc<PathBuf>,
    pub(crate) store: Arc<Store>,
    pub(crate) agent_id: EntityId,
    /// The live native dispatcher. It is attached after the state is built
    /// (the server and the subject's own `playground.manage` handler share one
    /// Playground), so it is a shared cell rather than a per-clone `Option`: a
    /// clone that already exists must see the attachment instead of failing
    /// closed against a stale `None`.
    pub(crate) native: Arc<RwLock<Option<Arc<NativeChildService>>>>,
    /// The volatile wake channel. Only a wake: the durable fact is admitted by
    /// the store, and the subject's pending-inbox drain owns delivery.
    inbox_tx: Arc<RwLock<Option<Sender<InboxEvent>>>>,
    records: Arc<RwLock<HashMap<EntityId, ArtifactVersion>>>,
    mutation_lock: Arc<Mutex<()>>,
}

/// A CAS-sealed proposal, with no business effect until its Store transaction
/// commits. Native operations attach their exact observation to that boundary.
pub(crate) struct PreparedArtifact {
    pub record: ArtifactRecord,
    pub version: Option<ArtifactVersion>,
    expected_revision: Option<u64>,
    manifest: Option<ArtifactManifest>,
    event: Option<InboxEvent>,
    changed: bool,
}

impl PlaygroundState {
    /// Open the existing artifact tree and recover unimported facts from its
    /// committed version chain. No Store/subject means no writable Playground.
    pub fn open(root: PathBuf, store: Arc<Store>) -> anyhow::Result<Self> {
        let agent_id = store.get_existing_agent_id()?
            .ok_or_else(|| anyhow::anyhow!("Playground requires an existing subject"))?;
        store.load_subject(agent_id)?;
        fs::create_dir_all(&root)?;
        let state = Self {
            root: Arc::new(root.canonicalize()?), store, agent_id,
            native: Arc::new(RwLock::new(None)),
            inbox_tx: Arc::new(RwLock::new(None)),
            records: Arc::new(RwLock::new(HashMap::new())),
            mutation_lock: Arc::new(Mutex::new(())),
        };
        state.load()?;
        state.recover_durable()?;
        state.recover_notifications()?;
        Ok(state)
    }

    pub fn with_inbox(&self, tx: Sender<InboxEvent>) -> Self {
        *self.inbox_tx.write() = Some(tx);
        self.clone()
    }

    pub fn with_native(&self, native: Arc<NativeChildService>) -> Self {
        *self.native.write() = Some(native);
        self.clone()
    }

    pub(crate) fn native(&self) -> Option<Arc<NativeChildService>> {
        self.native.read().clone()
    }

    pub(crate) fn mutation(&self) -> MutexGuard<'_, ()> { self.mutation_lock.lock() }

    /// Wake the subject about a fact that is already durably admitted.
    fn wake(&self, event: &InboxEvent) {
        if let Some(tx) = self.inbox_tx.read().as_ref() {
            let _ = tx.try_send(event.clone());
        }
    }

    pub fn root(&self) -> &Path { self.root.as_path() }

    pub fn store(&self) -> &Arc<Store> { &self.store }

    pub(crate) fn artifact_dir(&self, id: EntityId) -> PathBuf {
        self.root.join(id.to_string())
    }

    fn version_path(&self, id: EntityId, revision: u64) -> PathBuf {
        self.artifact_dir(id).join("versions").join(revision.to_string())
    }

    fn version_record_path(&self, id: EntityId, revision: u64) -> PathBuf {
        self.artifact_dir(id).join("versions").join(format!("{revision}.json"))
    }

    fn seal_content(&self, bytes: &[u8], mime: &str) -> io::Result<ObjectRef> {
        match self.store.cas.seal_object(bytes, mime, "playground", "artifact-content", 1) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => { self.store.close_external_writer(); Err(io::Error::other(error)) }
        }
    }

    fn seal_playground_object(&self, value: &impl Serialize, codec: &str) -> io::Result<ObjectRef> {
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        match self.store.cas.seal_object(&bytes, "application/json", "playground", codec, 1) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => { self.store.close_external_writer(); Err(io::Error::other(error)) }
        }
    }

    /// Allocate beyond orphaned writes too: a crash never authorizes erasing
    /// an old version's bytes, even when it never became the selected head.
    fn next_revision(&self, id: EntityId) -> io::Result<u64> {
        let dir = self.artifact_dir(id).join("versions");
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(1),
            Err(e) => return Err(e),
        };
        let mut highest = 0u64;
        for entry in entries {
            let name = entry?.file_name();
            if let Some(number) = name.to_str().and_then(|s| s.trim_end_matches(".json").parse::<u64>().ok()) {
                highest = highest.max(number);
            }
        }
        highest.checked_add(1).ok_or_else(|| io::Error::other("artifact revision overflow"))
    }

    /// Commit immutable bytes + record before selecting the new head. The
    /// committed record is also the artifact's stable notification outbox.
    fn commit_version(&self, record: &ArtifactVersion, bytes: &[u8]) -> io::Result<()> {
        write_immutable(&self.version_path(record.artifact_id, record.revision), bytes)?;
        write_immutable(&self.version_record_path(record.artifact_id, record.revision),
            &serde_json::to_vec(record).map_err(io::Error::other)?)?;
        atomic_write(&self.artifact_dir(record.artifact_id).join("meta.json"),
            &serde_json::to_vec(record).map_err(io::Error::other)?)
    }

    pub(crate) fn prepare_put(
        &self,
        id: EntityId,
        title: String,
        mime: String,
        bytes: &[u8],
        create_only: bool,
    ) -> anyhow::Result<PreparedArtifact> {
        let previous_record = self.store.artifact_record(id)?;
        anyhow::ensure!(
            !create_only || previous_record.is_none(),
            "artifact already exists"
        );
        self.prepare_content(id, title, mime, bytes, previous_record)
    }

    pub(crate) fn prepare_edit(
        &self,
        id: EntityId,
        title: Option<String>,
        mime: Option<String>,
        bytes: &[u8],
    ) -> anyhow::Result<PreparedArtifact> {
        let previous_record = self
            .store
            .artifact_record(id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        let previous = self.version_from_ref(&previous_record.state_ref)?;
        self.prepare_content(
            id,
            title.unwrap_or(previous.title),
            mime.unwrap_or(previous.mime),
            bytes,
            Some(previous_record),
        )
    }

    fn prepare_content(
        &self,
        id: EntityId,
        title: String,
        mime: String,
        bytes: &[u8],
        previous_record: Option<ArtifactRecord>,
    ) -> anyhow::Result<PreparedArtifact> {
        let previous = previous_record
            .as_ref()
            .map(|record| self.version_from_ref(&record.state_ref))
            .transpose()?;
        let revision = self.next_revision(id)?.max(
            previous
                .as_ref()
                .map(|v| {
                    v.revision
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("artifact revision overflow"))
                })
                .transpose()?
                .unwrap_or(1),
        );
        let version = ArtifactVersion {
            schema_version: ARTIFACT_SCHEMA,
            artifact_id: id,
            title,
            mime: mime.clone(),
            bytes_path: self.version_path(id, revision),
            content_ref: self.seal_content(bytes, &mime)?,
            revision,
            parent_revision: previous.as_ref().map(|r| r.revision),
            restored_from: None,
            updated_at: Utc::now(),
            backend: previous.and_then(|r| r.backend),
        };
        self.prepare_version(version, previous_record)
    }

    pub fn put(
        &self,
        id: EntityId,
        title: String,
        mime: String,
        bytes: &[u8],
    ) -> io::Result<ArtifactVersion> {
        let _mutation = self.mutation();
        let prepared = self
            .prepare_put(id, title, mime, bytes, false)
            .map_err(io::Error::other)?;
        let version = prepared
            .version
            .clone()
            .ok_or_else(|| io::Error::other("content proposal has no version"))?;
        self.commit_prepared(Some(&prepared), None)
            .map_err(io::Error::other)?;
        Ok(version)
    }

    fn read_object(&self, reference: &ObjectRef) -> anyhow::Result<Vec<u8>> {
        let bytes = self.store.cas.read_object(&reference.digest)?;
        anyhow::ensure!(
            bytes.len() as u64 == reference.bytes && Digest::blake3(&bytes) == reference.digest,
            "artifact CAS object length/digest mismatch"
        );
        Ok(bytes)
    }

    fn version_from_ref(&self, reference: &ObjectRef) -> anyhow::Result<ArtifactVersion> {
        let mut version: ArtifactVersion = serde_json::from_slice(&self.read_object(reference)?)?;
        anyhow::ensure!(
            version.schema_version == ARTIFACT_SCHEMA,
            "artifact version schema mismatch"
        );
        version.bytes_path = self.version_path(version.artifact_id, version.revision);
        Ok(version)
    }

    pub(crate) fn current_version(&self, id: EntityId) -> anyhow::Result<Option<ArtifactVersion>> {
        match self.store.artifact_record(id)? {
            Some(record) => {
                let version = self.version_from_ref(&record.state_ref)?;
                anyhow::ensure!(
                    version.artifact_id == id,
                    "artifact version identity mismatch"
                );
                Ok(Some(version))
            }
            None => Ok(self.records.read().get(&id).cloned()),
        }
    }

    pub fn get(&self, id: &EntityId) -> anyhow::Result<Option<ArtifactVersion>> {
        self.current_version(*id)
    }

    pub(crate) fn list_current(&self) -> anyhow::Result<Vec<ArtifactVersion>> {
        let mut records = self
            .store
            .list_artifact_records()?
            .into_iter()
            .filter(|record| record.subject_id == self.agent_id)
            .map(|record| self.version_from_ref(&record.state_ref))
            .collect::<anyhow::Result<Vec<_>>>()?;
        records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(records)
    }

    pub fn list(&self) -> anyhow::Result<Vec<ArtifactVersion>> {
        self.list_current()
    }

    /// The durable business record of one artifact, when it was synced.
    pub fn durable_record(&self, id: EntityId) -> anyhow::Result<Option<ArtifactRecord>> {
        self.store.artifact_record(id)
    }

    /// The enumerable roots this installation selected for its artifacts —
    /// Roots come from the durable records, not from listing the
    /// playground directory, so a stray directory is never mistaken for an
    /// artifact.
    pub fn roots(&self) -> anyhow::Result<Vec<(EntityId, PathBuf, Digest)>> {
        self.store.artifact_roots()
    }

    pub fn read_content(&self, record: &ArtifactVersion) -> io::Result<Vec<u8>> {
        self.read_object(&record.content_ref)
            .map_err(io::Error::other)
    }

    pub(crate) fn read_version(&self, id: EntityId, revision: u64) -> io::Result<ArtifactVersion> {
        if let Some(current) = self.current_version(id).map_err(io::Error::other)?
            && current.revision == revision
        {
            return Ok(current);
        }
        // Every committed manifest retains its exact version record in CAS.
        // A missing/stale local projection must never turn a committed effect
        // into an unknown result or authorize a second mutation.
        let mut found: Option<ArtifactVersion> = None;
        for manifest in self
            .store
            .list_artifact_manifests(id)
            .map_err(io::Error::other)?
        {
            let closure: Value = serde_json::from_slice(
                &self
                    .read_object(&manifest.recovery_closure_ref)
                    .map_err(io::Error::other)?,
            )
            .map_err(io::Error::other)?;
            let reference: ObjectRef =
                serde_json::from_value(closure.get("version_record_ref").cloned().ok_or_else(
                    || io::Error::other("artifact recovery closure has no version record"),
                )?)
                .map_err(io::Error::other)?;
            let record = self
                .version_from_ref(&reference)
                .map_err(io::Error::other)?;
            if record.artifact_id == id && record.revision == revision {
                if let Some(previous) = &found
                    && serde_json::to_vec(previous).map_err(io::Error::other)?
                        != serde_json::to_vec(&record).map_err(io::Error::other)?
                {
                    return Err(io::Error::other("artifact revision is ambiguous across retained histories"));
                }
                found = Some(record);
            }
        }
        if let Some(record) = found { return Ok(record); }
        // Legacy committed chains may predate their durable manifest import.
        let mut record: ArtifactVersion =
            serde_json::from_slice(&fs::read(self.version_record_path(id, revision))?)
                .map_err(io::Error::other)?;
        if record.schema_version != ARTIFACT_SCHEMA
            || record.artifact_id != id
            || record.revision != revision
        {
            return Err(io::Error::other(
                "artifact version identity/schema mismatch",
            ));
        }
        record.bytes_path = self.version_path(id, revision);
        Ok(record)
    }

    /// Only the committed ancestor chain is history; unrelated orphan writes
    /// remain on disk but are not silently published as earlier versions.
    pub fn revisions(&self, id: EntityId) -> io::Result<Vec<u64>> {
        let Some(head) = self.get(&id).map_err(io::Error::other)? else { return Ok(Vec::new()) };
        let mut next = Some(head.revision);
        let mut revisions = Vec::new();
        while let Some(revision) = next {
            let record = self.read_version(id, revision)?;
            if record.parent_revision.is_some_and(|parent| parent >= revision) {
                return Err(io::Error::other("artifact history is not monotonic"));
            }
            revisions.push(revision);
            next = record.parent_revision;
        }
        revisions.reverse();
        Ok(revisions)
    }

    /// Restore the exact earlier content, MIME, title AND executable binding
    /// as a new version. Business state lives separately and is never erased.
    pub(crate) fn prepare_rollback(
        &self,
        id: EntityId,
        source_revision: u64,
    ) -> anyhow::Result<PreparedArtifact> {
        let previous_record = self
            .store
            .artifact_record(id)?
            .ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        let head = self.version_from_ref(&previous_record.state_ref)?;
        let mut ancestor = Some(head.revision);
        let mut source = None;
        while let Some(revision) = ancestor {
            let version = self.read_version(id, revision)?;
            anyhow::ensure!(
                version
                    .parent_revision
                    .is_none_or(|parent| parent < revision),
                "artifact history is not monotonic"
            );
            if revision == source_revision {
                source = Some(version);
                break;
            }
            ancestor = version.parent_revision;
        }
        let source =
            source.ok_or_else(|| anyhow::anyhow!("committed artifact revision not found"))?;
        self.read_content(&source)?;
        let revision = self.next_revision(id)?.max(
            head.revision
                .checked_add(1)
                .ok_or_else(|| io::Error::other("artifact revision overflow"))?,
        );
        let version = ArtifactVersion {
            revision,
            parent_revision: Some(head.revision),
            restored_from: Some(source_revision),
            updated_at: Utc::now(),
            bytes_path: self.version_path(id, revision),
            ..source
        };
        self.prepare_version(version, Some(previous_record))
    }

    pub fn rollback(&self, id: EntityId, source_revision: u64) -> io::Result<ArtifactVersion> {
        let _mutation = self.mutation();
        let prepared = self
            .prepare_rollback(id, source_revision)
            .map_err(io::Error::other)?;
        let version = prepared
            .version
            .clone()
            .ok_or_else(|| io::Error::other("rollback proposal has no version"))?;
        self.commit_prepared(Some(&prepared), None)
            .map_err(io::Error::other)?;
        Ok(version)
    }

    /// Every binding is a new artifact version, including its invocation
    /// contract and CAS module identity. A code rollback restores this binding
    /// while keeping all state written by newer code.
    pub fn bind_backend(&self, id: EntityId, module: &[u8]) -> io::Result<ArtifactVersion> {
        let _mutation = self.mutation();
        let binding = BackendBinding::seal(&self.store, module).map_err(io::Error::other)?;
        let previous_record = self
            .store
            .artifact_record(id)
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "artifact not found"))?;
        let previous = self
            .version_from_ref(&previous_record.state_ref)
            .map_err(io::Error::other)?;
        self.read_content(&previous)?;
        let revision = self.next_revision(id)?.max(
            previous
                .revision
                .checked_add(1)
                .ok_or_else(|| io::Error::other("artifact revision overflow"))?,
        );
        let version = ArtifactVersion {
            revision,
            parent_revision: Some(previous.revision),
            restored_from: None,
            updated_at: Utc::now(),
            bytes_path: self.version_path(id, revision),
            backend: Some(binding),
            ..previous
        };
        let prepared = self
            .prepare_version(version.clone(), Some(previous_record))
            .map_err(io::Error::other)?;
        self.commit_prepared(Some(&prepared), None)
            .map_err(io::Error::other)?;
        Ok(version)
    }

    /// Build the versioned manifest for one committed content version: the
    /// real closure is source refs, build lock, runtime binding, state
    /// bindings, action schemas, operation contracts and the recovery
    /// closure. The manifest digest binds to its canonical encoding.
    fn build_manifest(&self, version: &ArtifactVersion) -> anyhow::Result<(ArtifactManifest, ObjectRef, ObjectRef)> {
        let state_dir = self.artifact_dir(version.artifact_id).join("state");
        let build_lock = self.seal_playground_object(&json!({
            "artifact_schema": ARTIFACT_SCHEMA, "mime": version.mime,
            "invocation_contract": version.backend.as_ref().map(|binding| &binding.invocation_contract),
        }), "artifact-build-lock")?;
        let state_bindings = self.seal_playground_object(&json!({
            "state_root": state_dir, "recovery_domain": "artifact",
            "code_revision": version.revision, "data_revision_note": "code version and data revision are separate",
        }), "artifact-state-bindings")?;
        let action_schemas = self.seal_playground_object(
            &version.backend.as_ref().map(|binding| json!({"actions": binding.action_schemas()}))
                .unwrap_or_else(|| json!({"actions": []})),
            "artifact-action-schemas")?;
        let operation_contracts = self.seal_playground_object(
            &json!({"contract": crate::playground_backend::CONTRACT_TEXT}), "artifact-operation-contracts")?;
        let health_contract = self.seal_playground_object(
            &json!({"profile": "wasi-p1-isolated-small-state-or-host-process",
                "guest_exit_zero": true, "bounded_stdout_stderr": true, "state_snapshot": true}),
            "artifact-health-contract")?;
        let version_record_ref = self.seal_playground_object(version, "artifact-version-record")?;
        let recovery_closure = self.seal_playground_object(&json!({
            "artifact_root": self.artifact_dir(version.artifact_id),
            "state_root": state_dir,
            "version_record_ref": version_record_ref,
            "module_ref": version.backend.as_ref().map(|binding| &binding.module_ref),
        }), "artifact-recovery-closure")?;
        let backend = match version.backend.as_ref() {
            Some(binding) => {
                // Bindings sealed before the runtime distinction carry no
                // sealed runtime/contract identity; seal the contract from
                // the binding itself (content-addressed, so deterministic).
                let service_contract_ref = match binding.service_contract_ref.clone() {
                    Some(reference) => reference,
                    None => self.seal_playground_object(&json!({"contract": binding.invocation_contract}), "artifact-service-contract")?,
                };
                Some(BackendManifest {
                    entry_ref: binding.module_ref.clone(),
                    runtime_ref: binding.runtime_ref.clone().unwrap_or_else(|| binding.module_ref.clone()),
                    service_contract_ref,
                })
            }
            None => None,
        };
        let frontend = (version.mime == "text/html" || version.mime == "application/javascript").then(|| FrontendBinding {
            bundle_ref: version.content_ref.clone(),
            entry: "index.html".into(),
            surface_contract_ref: operation_contracts.clone(),
        });
        let manifest = ArtifactManifest {
            schema_version: 1,
            artifact_id: version.artifact_id,
            version_digest: Digest::blake3(b""), // replaced below after canonical encoding
            source_refs: vec![version.content_ref.clone()],
            build_and_dependency_lock_ref: build_lock,
            frontend,
            backend,
            state_bindings_ref: state_bindings,
            action_schemas_ref: action_schemas,
            operation_contracts_ref: operation_contracts,
            secret_handles_ref: None,
            health_contract_ref: health_contract,
            recovery_closure_ref: recovery_closure,
        };
        let canonical = serde_json::to_vec(&manifest)?;
        let manifest = ArtifactManifest { version_digest: Digest::blake3(&canonical), ..manifest };
        let canonical = serde_json::to_vec(&manifest)?;
        let manifest_ref = match self.store.cas.seal_object(&canonical, "application/json", "playground", "artifact-manifest", 1) {
            Ok(sealed) => sealed.object,
            Err(error) => { self.store.close_external_writer(); return Err(error.into()); }
        };
        Ok((manifest, manifest_ref, version_record_ref))
    }

    /// Seal the complete version closure before any artifact row changes.
    fn prepare_version(
        &self,
        version: ArtifactVersion,
        previous: Option<ArtifactRecord>,
    ) -> anyhow::Result<PreparedArtifact> {
        let (manifest, manifest_ref, version_record_ref) = self.build_manifest(&version)?;
        let expected_revision = previous.as_ref().map(|record| record.record_revision);
        let (
            creator_task_id,
            active_manifest_ref,
            candidate_manifest_refs,
            lifecycle,
            runtime_instance_refs,
        ) = match previous {
            Some(record) => (
                record.creator_task_id,
                record.active_manifest_ref,
                record.candidate_manifest_refs,
                record.lifecycle,
                record.runtime_instance_refs,
            ),
            None => (None, None, Vec::new(), "draft".into(), Vec::new()),
        };
        let root = self.artifact_dir(version.artifact_id);
        // The directory is a locator, never the commit point. Creating it
        // before the transaction gives the committed root a real identity.
        fs::create_dir_all(&root)?;
        let record = ArtifactRecord {
            schema_version: 1,
            artifact_id: version.artifact_id,
            subject_id: self.agent_id,
            creator_task_id,
            title: version.title.clone(),
            record_revision: expected_revision.unwrap_or(0),
            active_manifest_ref,
            candidate_manifest_refs,
            state_revision: format!(
                "{}:{}",
                version.revision,
                &manifest.version_digest.0[..16.min(manifest.version_digest.0.len())]
            ),
            state_ref: version_record_ref.clone(),
            lifecycle,
            runtime_instance_refs,
            desired_lifecycle_ref: manifest_ref,
            provenance_and_causation_ref: version_record_ref,
            root_identity: directory_identity(&root),
            artifact_root: root,
            updated_commit: shenko_types::commit::CommitRef::new(
                EntityId::nil(),
                shenko_types::ids::LedgerSeq::ZERO,
            ),
        };
        let event = self.change_event(&version);
        Ok(PreparedArtifact {
            record,
            version: Some(version),
            expected_revision,
            manifest: Some(manifest),
            event: Some(event),
            changed: true,
        })
    }

    /// SQLite is the effect boundary. Projection failures after that commit
    /// are recoverable cache failures; reporting the mutation as failed here
    /// would invite a second effect after an already committed first one.
    pub(crate) fn commit_prepared(
        &self,
        prepared: Option<&PreparedArtifact>,
        binding: Option<ArtifactOperation<'_>>,
    ) -> anyhow::Result<ArtifactCommit> {
        self.commit_prepared_request(prepared, binding, None)
    }

    fn commit_prepared_request(
        &self,
        prepared: Option<&PreparedArtifact>,
        binding: Option<ArtifactOperation<'_>>,
        reader: Option<ArtifactReaderRequest<'_>>,
    ) -> anyhow::Result<ArtifactCommit> {
        let changed = prepared.filter(|prepared| prepared.changed);
        let notification = changed
            .and_then(|prepared| prepared.event.as_ref())
            .map(|event| self.notification(event))
            .transpose()?;
        if changed.is_none() && binding.is_none() && reader.is_none() {
            return Ok(ArtifactCommit {
                record: prepared.map(|prepared| prepared.record.clone()),
                checkpoint_ref: None,
                reader_receipt: None,
                replayed: true,
            });
        }
        let committed = self.store.commit_artifact_transition(
            prepared
                .filter(|prepared| prepared.changed || reader.is_some())
                .map(|prepared| ArtifactTransition {
                    record: &prepared.record,
                    expected_revision: prepared.expected_revision,
                    manifest: prepared.manifest.as_ref(),
                    notification: notification.as_ref(),
                    write_record: prepared.changed,
                }),
            binding,
            reader,
        )?;
        if !committed.replayed {
            if let Some(reader) = &committed.reader_receipt {
                self.wake(&reader.event);
            }
            if let Some(prepared) = changed {
                if let Some(version) = &prepared.version {
                    self.records
                        .write()
                        .insert(version.artifact_id, version.clone());
                    if let Err(error) = self.project_version(version) {
                        tracing::warn!(artifact_id = %version.artifact_id, revision = version.revision,
                            %error, "artifact committed; local projection needs recovery");
                    }
                }
                if let Some(event) = &prepared.event {
                    self.wake(event);
                }
            }
        }
        Ok(committed)
    }

    /// The inbox request receipt and its source artifact transition commit
    /// together. Replays read that original receipt before today's revision,
    /// including after a later adoption or content edit.
    fn reader_action(
        &self,
        id: EntityId,
        actor: &ArtifactActor,
        request_id: &str,
        action: ReaderAction,
    ) -> anyhow::Result<Value> {
        anyhow::ensure!(
            !request_id.is_empty() && request_id.len() <= 256,
            "request_id must contain 1..256 bytes"
        );
        let _mutation = self.mutation();
        let identity = ArtifactReaderIdentity {
            event_id: stable_id(&format!(
                "artifact-reader/v1:{}",
                serde_json::to_string(&(id, actor.identity(), request_id))?
            )),
            artifact_id: id,
            subject_id: self.agent_id,
            actor: actor.identity(),
            request_id: request_id.into(),
            input_digest: Digest::blake3(&serde_json::to_vec(&action)?),
        };
        if let Some(original) = self.store.artifact_reader_receipt(&identity)? {
            return self.reader_response(original, &action, true);
        }
        let prepared = (|| -> anyhow::Result<_> {
            let input_ref = self.seal_playground_object(&action, "artifact-reader-input")?;
            let prepared = match &action {
                ReaderAction::Stage { revision } => {
                    self.prepare_stage(id, Some(*revision), Some(request_id))?
                }
                ReaderAction::Publish {
                    revision,
                    manifest_digest,
                } => self.prepare_publish(
                    id,
                    Some(manifest_digest),
                    Some(*revision),
                    Some(request_id),
                )?,
                ReaderAction::Rollback { source_revision } => {
                    self.prepare_rollback(id, *source_revision)?
                }
            };
            let result_ref = prepared
                .version
                .as_ref()
                .map(|version| self.seal_playground_object(version, "artifact-version-record"))
                .transpose()?;
            let committed = self.commit_prepared_request(
                Some(&prepared),
                None,
                Some(ArtifactReaderRequest {
                    identity: &identity,
                    input_ref: &input_ref,
                    result_ref: result_ref.as_ref(),
                }),
            )?;
            Ok((
                committed
                    .reader_receipt
                    .ok_or_else(|| anyhow::anyhow!("artifact commit has no reader receipt"))?,
                committed.replayed,
            ))
        })();
        match prepared {
            Ok((receipt, replayed)) => self.reader_response(receipt, &action, replayed),
            Err(error) => {
                // A competing same-key transaction may have won while this
                // caller prepared or saw a stale row. That original result
                // wins without preparing or executing another mutation.
                match self.store.artifact_reader_receipt(&identity)? {
                    Some(original) => self.reader_response(original, &action, true),
                    None => Err(error),
                }
            }
        }
    }

    fn reader_response(
        &self,
        original: ArtifactReaderResult,
        action: &ReaderAction,
        replayed: bool,
    ) -> anyhow::Result<Value> {
        let mut response = json!({"ok": true, "replayed": replayed,
            "request_id": original.receipt.identity.request_id,
            "receipt_ref": original.receipt_ref, "committed_at": original.receipt.committed_at});
        if matches!(action, ReaderAction::Rollback { .. }) {
            let reference = original
                .receipt
                .result_ref
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("rollback receipt has no version result"))?;
            response["version"] = serde_json::from_slice(&self.read_object(reference)?)?;
        } else {
            response["record"] = serde_json::to_value(original.receipt.record)?;
        }
        Ok(response)
    }

    fn project_version(&self, version: &ArtifactVersion) -> anyhow::Result<()> {
        let bytes = self.read_content(version)?;
        write_immutable(
            &self.version_path(version.artifact_id, version.revision),
            &bytes,
        )?;
        write_immutable(
            &self.version_record_path(version.artifact_id, version.revision),
            &serde_json::to_vec(version)?,
        )?;
        if self
            .current_version(version.artifact_id)?
            .is_some_and(|current| current.revision == version.revision)
        {
            let selected = serde_json::to_vec(version)?;
            self.retain_file_head(version.artifact_id, &selected)?;
            atomic_write(
                &self.artifact_dir(version.artifact_id).join("meta.json"),
                &selected,
            )?;
        }
        Ok(())
    }

    /// Preserve a superseded local selection as unselected evidence. Numeric
    /// file revisions never overrule the ledger, and recovery must not erase
    /// a legacy/foreign head just to make its projection look current.
    fn retain_file_head(&self, id: EntityId, selected: &[u8]) -> anyhow::Result<()> {
        let path = self.artifact_dir(id).join("meta.json");
        let previous = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if previous == selected {
            return Ok(());
        }
        let digest = blake3::hash(&previous).to_hex().to_string();
        let retained = self.artifact_dir(id).join("retained-heads");
        write_immutable(&retained.join(format!("{digest}.raw")), &previous)?;
        write_immutable(
            &retained.join(format!("{digest}.provenance.json")),
            &serde_json::to_vec(&json!({
                "schema_version": 1, "kind": "unselected_artifact_file_head", "artifact_id": id,
                "source_path": "meta.json", "raw_digest": Digest::blake3(&previous), "raw_bytes": previous.len(),
                "selection": "retained_only", "head_authority": "artifact_records.state_ref",
            }))?,
        )?;
        Ok(())
    }

    /// Whether the work is still at the revision an action names.
    /// Called only while the record write guard protects the entire action.
    fn check_revision(current: &ArtifactVersion, expected: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            current.revision == expected,
            "this action names revision {expected}, but the work is at revision {}",
            current.revision
        );
        Ok(())
    }

    /// The version the active (adopted) manifest's page names, if any: the
    /// authored head is a draft until a publication adopts it, so a published
    /// work lens shows exactly this version.
    pub fn adopted_version(&self, id: EntityId) -> anyhow::Result<Option<ArtifactVersion>> {
        let Some(record) = self.store.artifact_record(id)? else { return Ok(None) };
        let Some(active) = record.active_manifest_ref else { return Ok(None) };
        let manifest: ArtifactManifest = serde_json::from_slice(&self.read_object(&active)?)?;
        let Some(frontend) = manifest.frontend else { return Ok(None) };
        let closure: Value = serde_json::from_slice(&self.read_object(&manifest.recovery_closure_ref)?)?;
        let version_ref: ObjectRef = serde_json::from_value(closure.get("version_record_ref").cloned()
            .ok_or_else(|| anyhow::anyhow!("adopted manifest has no version record"))?)?;
        let version = self.version_from_ref(&version_ref)?;
        anyhow::ensure!(version.artifact_id == id && version.content_ref == frontend.bundle_ref,
            "adopted manifest and its version record disagree");
        Ok(Some(version))
    }

    /// Stage the current head without adopting it. Native actions may choose
    /// the current head; a reader's expected revision is checked under the
    /// same guard as the proposal's durable transition.
    pub(crate) fn prepare_stage(
        &self,
        id: EntityId,
        expected_revision: Option<u64>,
        request_id: Option<&str>,
    ) -> anyhow::Result<PreparedArtifact> {
        let mut record = self
            .store
            .artifact_record(id)?
            .ok_or_else(|| anyhow::anyhow!("artifact has no durable record"))?;
        let expected_record_revision = record.record_revision;
        let version = self.version_from_ref(&record.state_ref)?;
        if let Some(expected) = expected_revision {
            Self::check_revision(&version, expected)?;
        }
        let (manifest, manifest_ref, _) = self.build_manifest(&version)?;
        let changed = !record.candidate_manifest_refs.contains(&manifest_ref)
            && record.active_manifest_ref.as_ref() != Some(&manifest_ref);
        if changed {
            record.candidate_manifest_refs.push(manifest_ref);
        }
        let event = changed.then(|| self.state_event(&record, "stage", request_id));
        Ok(PreparedArtifact {
            record,
            version: None,
            expected_revision: Some(expected_record_revision),
            manifest: Some(manifest),
            event,
            changed,
        })
    }

    pub fn stage_artifact(
        &self,
        id: EntityId,
        expected_revision: Option<u64>,
        request_id: Option<&str>,
    ) -> anyhow::Result<ArtifactRecord> {
        let _mutation = self.mutation();
        let prepared = self.prepare_stage(id, expected_revision, request_id)?;
        Ok(self
            .commit_prepared(Some(&prepared), None)?
            .record
            .unwrap_or(prepared.record))
    }

    /// Publish one staged candidate as the active manifest — the explicit
    /// adoption attitude for this content.
    ///
    /// The candidate refs are NOT erased: a published manifest stays a recorded
    /// candidate and `active_manifest_ref` says which one is in force, so the
    /// previous proposal facts remain readable. The published manifest must be
    /// CAS-resolvable right now (digest and length), because it becomes the
    /// closure other nodes verify against.
    pub(crate) fn prepare_publish(
        &self,
        id: EntityId,
        manifest_digest: Option<&Digest>,
        expected_revision: Option<u64>,
        request_id: Option<&str>,
    ) -> anyhow::Result<PreparedArtifact> {
        let mut record = self
            .store
            .artifact_record(id)?
            .ok_or_else(|| anyhow::anyhow!("artifact has no durable record"))?;
        let expected_record_revision = record.record_revision;
        if let Some(expected) = expected_revision {
            Self::check_revision(&self.version_from_ref(&record.state_ref)?, expected)?;
        }
        let published = match manifest_digest {
            Some(digest) => record
                .candidate_manifest_refs
                .iter()
                .find(|reference| &reference.digest == digest),
            None => record.candidate_manifest_refs.last(),
        }
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("publish requires a staged candidate manifest"))?;
        let changed = record.active_manifest_ref.as_ref() != Some(&published);
        if changed {
            let bytes = self.read_object(&published)?;
            let staged: ArtifactManifest = serde_json::from_slice(&bytes)?;
            let manifest = self
                .store
                .artifact_manifest(id, &staged.version_digest)?
                .ok_or_else(|| {
                    anyhow::anyhow!("published manifest is not in the durable manifest set")
                })?;
            anyhow::ensure!(
                serde_json::to_vec(&manifest)? == bytes,
                "durable manifest row and its CAS object describe different closures"
            );
            record.active_manifest_ref = Some(published);
        }
        let event = changed.then(|| self.state_event(&record, "publish", request_id));
        Ok(PreparedArtifact {
            record,
            version: None,
            expected_revision: Some(expected_record_revision),
            manifest: None,
            event,
            changed,
        })
    }

    pub fn publish_artifact(
        &self,
        id: EntityId,
        manifest_digest: &Digest,
        expected_revision: Option<u64>,
        request_id: Option<&str>,
    ) -> anyhow::Result<ArtifactRecord> {
        let _mutation = self.mutation();
        let prepared =
            self.prepare_publish(id, Some(manifest_digest), expected_revision, request_id)?;
        Ok(self
            .commit_prepared(Some(&prepared), None)?
            .record
            .unwrap_or(prepared.record))
    }

    /// A lifecycle/manifest transition of an artifact is a business fact the
    /// subject must be able to perceive — (`artifact.state_changed`
    /// enters the same subject inbox as actions and content revisions).
    ///
    /// The announcing fact is built BEFORE the record is written and committed
    /// with it in one transaction, so there is no "recorded but unnotified"
    /// window to retry: the transition and the subject's perception of it are
    /// one durable boundary.
    fn state_event(
        &self,
        record: &ArtifactRecord,
        cause: &str,
        request_id: Option<&str>,
    ) -> InboxEvent {
        let active = record
            .active_manifest_ref
            .as_ref()
            .map(|reference| reference.digest.0.clone());
        let candidates = record
            .candidate_manifest_refs
            .iter()
            .map(|reference| reference.digest.0.clone())
            .collect::<Vec<_>>();
        // Include the pre-transition row revision: returning to an older
        // adoption after another edit is a new fact. An exact retry returns
        // before this method and does not create a second notification.
        InboxEvent {
            event_id: stable_id(&format!(
                "artifact-state/v3:{}:{}:{cause}:{}:{}:{}",
                record.artifact_id,
                record.record_revision,
                record.lifecycle,
                active.clone().unwrap_or_else(|| "-".into()),
                candidates.join(","),
            )),
            kind: InboxEventKind::ArtifactChanged,
            created_at: chrono::Utc::now(),
            source: "playground".into(),
            payload: json!({
                "kind": "artifact.state_changed",
                "artifact_id": record.artifact_id,
                "cause": cause,
                // The request names this transition; the row revision binds
                // its identity even when a later adoption returns here.
                "request_id": request_id,
                "lifecycle": &record.lifecycle,
                "active_manifest_digest": active,
                "candidate_manifest_digests": candidates,
                "state_revision": &record.state_revision,
                // Reason pointers: the subject can cite this exact record
                // state and commit, never a paraphrase of it.
                "previous_commit": record.updated_commit,
                "previous_record_revision": record.record_revision,
                "desired_lifecycle_ref": record.desired_lifecycle_ref,
                "explanation": format!(
                    "Artifact '{}' is in lifecycle '{}' after {cause}; its manifest refs describe the transition from the previous record revision above.",
                    record.artifact_id, record.lifecycle
                ),
            }),
        }
    }

    fn notification(&self, event: &InboxEvent) -> anyhow::Result<shenko_store::artifacts::ArtifactNotification> {
        Ok(shenko_store::artifacts::ArtifactNotification {
            event_id: event.event_id,
            agent_id: self.agent_id,
            kind: "artifact_changed".into(),
            source: "playground".into(),
            payload: serde_json::to_value(event)?,
        })
    }

    fn change_event(&self, record: &ArtifactVersion) -> InboxEvent {
        let backend = record.backend.as_ref().map(|binding| &binding.module_ref);
        InboxEvent {
            event_id: stable_id(&format!("artifact-change/v2:{}:{}", record.artifact_id, record.revision)),
            kind: InboxEventKind::ArtifactChanged, created_at: record.updated_at, source: "playground".into(),
            payload: json!({
                "artifact_id": record.artifact_id, "revision": record.revision,
                "title": record.title, "mime": record.mime,
                "parent_revision": record.parent_revision, "restored_from": record.restored_from,
                "content_digest": record.content_ref.digest, "content_ref": record.content_ref,
                "backend_module_ref": backend,
                // Reason pointers: this fact is provable from the immutable
                // version record and its content digest, and the subject can
                // cite either instead of a paraphrase. No confidence value and
                // no knowledge kind are attached — mechanism facts do not
                // pretend to be judged attitudes.
                "explanation": format!("Artifact '{}' committed revision {} ({} bytes, {}). Exact content and executable references are attached; binary content is not treated as text.",
                    record.title, record.revision, record.content_ref.bytes, record.mime),
            }),
        }
    }

    pub fn notify_changed(&self, record: &ArtifactVersion) -> anyhow::Result<()> {
        let event = self.change_event(record);
        self.store.admit_inbox_event(event.event_id, self.agent_id, "artifact_changed", "playground",
            &serde_json::to_value(&event)?)?;
        // This is only a wake. C11's pending_inbox_events drains the already
        // committed full InboxEvent if the queue is full/closed or absent.
        self.wake(&event);
        Ok(())
    }

    fn recover_notifications(&self) -> anyhow::Result<()> {
        for artifact in self.list()? {
            for revision in self.revisions(artifact.artifact_id)? {
                self.notify_changed(&self.read_version(artifact.artifact_id, revision)?)?;
            }
        }
        Ok(())
    }

    /// Import older file-selected heads once. New writes commit their full CAS
    /// closure to the ledger first; projection recovery cannot author a newer
    /// version or replace a record that advanced concurrently.
    fn recover_durable(&self) -> anyhow::Result<()> {
        let heads = self.records.read().values().cloned().collect::<Vec<_>>();
        for version in heads {
            let previous = self.store.artifact_record(version.artifact_id)?;
            if previous.is_none() {
                let prepared = self.prepare_version(version, previous)?;
                self.commit_prepared(Some(&prepared), None)?;
            }
        }
        // Historical manifests retain every committed revision independently
        // of local files. Rebuild missing projections without changing facts.
        for version in self.list_current()? {
            for revision in self.revisions(version.artifact_id)? {
                let retained = self.read_version(version.artifact_id, revision)?;
                if let Err(error) = self.project_version(&retained) {
                    tracing::warn!(artifact_id = %retained.artifact_id, %error,
                        "artifact remains committed; local projection needs recovery");
                }
            }
        }
        Ok(())
    }

    fn load(&self) -> anyhow::Result<()> {
        for record in self.store.list_artifact_records()? {
            if record.subject_id != self.agent_id {
                continue;
            }
            let version = self.version_from_ref(&record.state_ref)?;
            anyhow::ensure!(
                version.artifact_id == record.artifact_id,
                "artifact version identity mismatch"
            );
            self.records.write().insert(version.artifact_id, version);
        }
        for entry in fs::read_dir(self.root.as_path())? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<EntityId>().ok())
            else {
                continue;
            };
            let meta = entry.path().join("meta.json");
            // A file revision is not a commit ordering proof. A stale or
            // foreign projection, even with a larger counter, cannot select
            // over a durable head. Recovery preserves it before replacement.
            if self.records.read().contains_key(&id) {
                continue;
            }
            let raw = match fs::read(&meta) {
                Ok(bytes) => bytes,
                // A content write before first head selection is an orphan,
                // not an artifact to invent during recovery.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            let value: Value = serde_json::from_slice(&raw)?;
            let record = if value.get("schema_version").and_then(Value::as_u64)
                == Some(ARTIFACT_SCHEMA as u64)
            {
                let mut head: ArtifactVersion = serde_json::from_value(value)?;
                head.bytes_path = self.version_path(id, head.revision);
                let immutable = self.read_version(id, head.revision)?;
                anyhow::ensure!(
                    head.artifact_id == id
                        && serde_json::to_value(&head)? == serde_json::to_value(&immutable)?,
                    "artifact head differs from immutable version"
                );
                self.read_content(&immutable)?;
                immutable
            } else {
                // Explicit one-way migration of the original flat content /
                // meta.json layout. Preserve the exact old metadata and content
                // files; do not execute its old reserved backend_script field.
                #[derive(Deserialize)]
                struct Legacy {
                    artifact_id: EntityId,
                    title: String,
                    mime: String,
                    revision: u64,
                    updated_at: DateTime<Utc>,
                }
                let legacy: Legacy = serde_json::from_slice(&raw)?;
                anyhow::ensure!(
                    legacy.artifact_id == id,
                    "legacy artifact identity mismatch"
                );
                let bytes = fs::read(entry.path().join("content"))?;
                let record = ArtifactVersion {
                    schema_version: ARTIFACT_SCHEMA,
                    artifact_id: id,
                    title: legacy.title,
                    mime: legacy.mime.clone(),
                    revision: legacy.revision,
                    content_ref: self.seal_content(&bytes, &legacy.mime)?,
                    bytes_path: self.version_path(id, legacy.revision),
                    parent_revision: None,
                    restored_from: None,
                    updated_at: legacy.updated_at,
                    backend: None,
                };
                write_immutable(&entry.path().join("legacy-meta.json"), &raw)?;
                self.commit_version(&record, &bytes)?;
                record
            };
            self.records.write().insert(id, record);
        }
        Ok(())
    }


}

/// The dev/ino identity of a directory, or a zero digest when it cannot be
/// read. A zero identity is never equal to a live directory's identity, so a
/// missing root is always re-verified instead of trusted.
fn directory_identity(path: &Path) -> Digest {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(metadata) = fs::metadata(path) {
            if metadata.is_dir() {
                return Digest::blake3(format!("dir:{}:{}", metadata.dev(), metadata.ino()).as_bytes());
            }
        }
    }
    Digest::blake3(b"missing-directory")
}

pub(crate) fn stable_id(identity: &str) -> EntityId {
    let hash = blake3::hash(identity.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    EntityId(Uuid::from_bytes(bytes))
}

pub(crate) fn write_immutable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| io::Error::other("version parent missing"))?;
    fs::create_dir_all(parent)?;
    match fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => { file.write_all(bytes)?; file.sync_all()?; }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            if fs::read(path)? != bytes { return Err(io::Error::other("immutable artifact version conflict")); }
        }
        Err(e) => return Err(e),
    }
    fs::File::open(parent)?.sync_all()?;
    if let Some(grandparent) = parent.parent() { fs::File::open(grandparent)?.sync_all()?; }
    Ok(())
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| io::Error::other("artifact parent missing"))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".artifact-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() { let _ = fs::remove_file(&tmp); }
    result
}

pub fn router(state: PlaygroundState) -> Router {
    Router::new()
        // The one served UI is the Proto-UI app; the Playground is a view of
        // it, not a second hand-written page.
        .route("/playground", get(list_handler))
        .route("/playground/{id}", get(get_handler).put(put_handler))
        .route("/playground/{id}/versions", get(versions_handler))
        .route("/playground/{id}/adopted", get(adopted_handler))
        .route("/playground/{id}/rollback", post(rollback_handler))
        .route("/playground/{id}/backend", get(backend_info_handler).put(bind_backend_handler))
        .route("/playground/{id}/backend/action", post(backend_action_handler).get(action_status_handler))
        .route("/playground/{id}/manifest", get(manifest_handler))
        .route("/playground/{id}/stage", post(stage_handler))
        .route("/playground/{id}/publish", post(publish_handler))
        .with_state(state)
}

/// Frozen reader action arguments; request identity and actor are separate.
#[derive(Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ReaderAction {
    Stage { revision: u64 },
    Publish { revision: u64, manifest_digest: Digest },
    Rollback { source_revision: u64 },
}

/// The action a reader performs on a work.
///
/// It names the revision it acts on and the request that named it. The
/// revision is the durable guard: a work that has moved on refuses the stale
/// action instead of applying it to whatever is there now. The request id is
/// the idempotency key — it rides the durable transition, so a repeated
/// submit is recognisable as the same request rather than a new one.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkAction {
    revision: u64,
    request_id: String,
}

async fn stage_handler(
    State(pg): State<PlaygroundState>,
    AxumPath(id): AxumPath<EntityId>,
    Json(request): Json<WorkAction>,
) -> Response {
    match pg.reader_action(id, &ArtifactActor::Participant, &request.request_id, ReaderAction::Stage { revision: request.revision }) {
        Ok(response) => Json(response).into_response(),
        Err(e) => error_response(StatusCode::CONFLICT, &format!("artifact stage failed: {e:#}")),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishRequest {
    manifest_digest: Digest,
    revision: u64,
    request_id: String,
}

async fn publish_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>,
    Json(request): Json<PublishRequest>) -> Response {
    match pg.reader_action(id, &ArtifactActor::Participant, &request.request_id,
        ReaderAction::Publish { revision: request.revision, manifest_digest: request.manifest_digest }) {
        Ok(response) => Json(response).into_response(),
        Err(e) => error_response(StatusCode::CONFLICT, &format!("artifact publish failed: {e:#}")),
    }
}

async fn list_handler() -> Redirect {
    Redirect::temporary("/?view=playground")
}

/// A work's page is served as the trusted extension it is
/// the subject writes, self-tests and can
/// roll back its works, so the default is
/// trust — same origin, no sandbox — and the safety rope is the version
/// history plus the shell's safe mode, not a static deny. Only the bytes'
/// declared type is pinned, so a page is never sniffed into something else.
pub(crate) fn describe_artifact_content(headers: &mut axum::http::HeaderMap) {
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
}

/// `GET /playground/{id}/adopted` — the adopted page, never the draft head.
async fn adopted_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>) -> Response {
    match pg.adopted_version(id) {
        Ok(Some(version)) => match pg.read_content(&version) {
            Ok(bytes) => {
                let mut response = Response::new(Body::from(bytes));
                response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_str(&version.mime)
                    .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")));
                if let Ok(value) = HeaderValue::from_str(&version.revision.to_string()) { response.headers_mut().insert("x-artifact-revision", value); }
                describe_artifact_content(response.headers_mut());
                response
            }
            Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        },
        Ok(None) => error_response(StatusCode::NOT_FOUND, "no adopted page"),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn get_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>) -> Response {
    let record = match pg.get(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "artifact not found"),
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    match pg.read_content(&record) {
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_str(&record.mime)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")));
            if let Ok(value) = HeaderValue::from_str(&record.revision.to_string()) { response.headers_mut().insert("x-artifact-revision", value); }
            describe_artifact_content(response.headers_mut());
            response
        }
    }
}

async fn put_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>,
    Query(query): Query<HashMap<String, String>>, headers: HeaderMap, bytes: Bytes) -> Response {
    let title = query.get("title").cloned().or_else(|| header_str(&headers, "x-artifact-title"))
        .unwrap_or_else(|| format!("Artifact {id}"));
    let mime = query.get("mime").cloned().or_else(|| header_str(&headers, header::CONTENT_TYPE.as_str()))
        .unwrap_or_else(|| "application/octet-stream".into());
    artifact_response(pg.put(id, title, mime, &bytes))
}

async fn versions_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>) -> Response {
    match pg.revisions(id) {
        Ok(revisions) if revisions.is_empty() => error_response(StatusCode::NOT_FOUND, "artifact not found"),
        Ok(revisions) => Json(json!({"ok": true, "revisions": revisions})).into_response(),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackRequest {
    /// The revision the reader returns to.
    revision: u64,
    /// The request that named this action.
    request_id: String,
}
async fn rollback_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>, Json(request): Json<RollbackRequest>) -> Response {
    // This route has the same real Participant namespace as backend actions;
    // request JSON cannot impersonate an owner or a delegated subject task.
    match pg.reader_action(id, &ArtifactActor::Participant, &request.request_id,
        ReaderAction::Rollback { source_revision: request.revision }) {
        Ok(response) => Json(response).into_response(),
        Err(e) => error_response(StatusCode::CONFLICT, &format!("artifact rollback failed: {e:#}")),
    }
}

async fn backend_info_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>) -> Response {
    match pg.get(&id) {
        Ok(Some(record)) => Json(json!({"ok": true, "revision": record.revision, "backend": record.backend})).into_response(),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "artifact not found"),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn bind_backend_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>, bytes: Bytes) -> Response {
    artifact_response(pg.bind_backend(id, &bytes))
}

async fn manifest_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>) -> Response {
    match pg.durable_record(id) {
        Ok(Some(record)) => Json(json!({"ok": true, "record": record})).into_response(),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "artifact has no durable record"),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn backend_action_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>,
    Json(request): Json<BackendActionRequest>) -> Response {
    // Browser actions from the playground panel are Participant: an ordinary
    // session of this installation without an admin credential.
    let actor = ArtifactActor::Participant;
    receipt_response(pg.invoke_action(id, request, &actor))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusQuery { request_id: String }
async fn action_status_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>,
    Query(query): Query<StatusQuery>) -> Response {
    receipt_response(pg.action_status(id, &query.request_id))
}

fn receipt_response(receipt: Result<BackendReceipt, ActionError>) -> Response {
    match receipt {
        Ok(receipt) => {
            let status = if receipt.result.is_some() || receipt.evidence.is_some() { StatusCode::OK } else { StatusCode::ACCEPTED };
            (status, Json(json!({"accepted": true, "receipt": receipt}))).into_response()
        }
        Err(error) => error.into_response(),
    }
}

fn artifact_response(result: io::Result<ArtifactVersion>) -> Response {
    match result {
        Ok(record) => Json(json!({"ok": true, "notified": "durably_admitted", "artifact": record})).into_response(),
        Err(e) => error_response(if e.kind() == io::ErrorKind::NotFound { StatusCode::NOT_FOUND } else { StatusCode::INTERNAL_SERVER_ERROR },
            &format!("artifact write/import failed; any committed version is retained for recovery: {e}")),
    }
}

fn error_response(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({"ok": false, "error": error}))).into_response()
}
fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_string)
}

#[cfg(test)]
#[path = "playground_tests.rs"]
mod tests;
