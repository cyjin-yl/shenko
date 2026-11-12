//! Full-stack Playground — docs/specs/configuration-playground.md#spec-43-2.
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
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_store::Store;
use shenko_store::artifacts::{ArtifactManifest, ArtifactRecord, BackendManifest, FrontendBinding};
use shenko_subject::{NativeChildService, InboxEvent, InboxEventKind};
use shenko_types::{ids::EntityId, object::{Digest, ObjectRef}};
use tokio::sync::mpsc::Sender;
use uuid::Uuid;
use crate::playground_backend::{ActionError, ArtifactActor, BackendActionRequest, BackendBinding, BackendReceipt};

pub(crate) const ARTIFACT_SCHEMA: u32 = 2;

/// One immutable content version of an artifact. Content and version records
/// are immutable; only `meta.json` selects a head. A version is written and
/// synced before the head changes. This type is the file-backed content
/// record; the durable business record is `shenko_store::ArtifactRecord`.
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
        };
        state.load()?;
        state.recover_notifications()?;
        state.recover_durable()?;
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

    pub fn put(&self, id: EntityId, title: String, mime: String, bytes: &[u8]) -> io::Result<ArtifactVersion> {
        let mut records = self.records.write();
        let previous = records.get(&id);
        let revision = self.next_revision(id)?;
        let record = ArtifactVersion {
            schema_version: ARTIFACT_SCHEMA, artifact_id: id, title, mime: mime.clone(),
            bytes_path: self.version_path(id, revision), content_ref: self.seal_content(bytes, &mime)?,
            revision, parent_revision: previous.map(|r| r.revision), restored_from: None,
            updated_at: Utc::now(), backend: previous.and_then(|r| r.backend.clone()),
        };
        self.commit_version(&record, bytes)?;
        // The durable record and the fact announcing this revision commit
        // together; a failure here leaves the version chain committed and
        // reopen reconciles it, but the caller is never told the notification
        // succeeded when it did not.
        self.sync_durable(&record).map_err(io::Error::other)?;
        records.insert(id, record.clone());
        Ok(record)
    }

    pub fn get(&self, id: &EntityId) -> Option<ArtifactVersion> {
        self.records.read().get(id).cloned()
    }

    pub fn list(&self) -> Vec<ArtifactVersion> {
        let mut records: Vec<_> = self.records.read().values().cloned().collect();
        records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        records
    }

    /// The durable business record of one artifact, when it was synced.
    pub fn durable_record(&self, id: EntityId) -> anyhow::Result<Option<ArtifactRecord>> {
        self.store.artifact_record(id)
    }

    /// The enumerable roots this installation selected for its artifacts —
    /// docs/specs/configuration-playground.md#spec-43-2. Roots come from the durable records, not from listing the
    /// playground directory, so a stray directory is never mistaken for an
    /// artifact.
    pub fn roots(&self) -> Vec<(EntityId, PathBuf, Digest)> {
        self.store.artifact_roots().unwrap_or_default()
    }

    pub fn read_content(&self, record: &ArtifactVersion) -> io::Result<Vec<u8>> {
        let bytes = fs::read(self.version_path(record.artifact_id, record.revision))?;
        if bytes.len() as u64 != record.content_ref.bytes || Digest::blake3(&bytes) != record.content_ref.digest {
            return Err(io::Error::other("artifact version content digest mismatch"));
        }
        Ok(bytes)
    }

    pub(crate) fn read_version(&self, id: EntityId, revision: u64) -> io::Result<ArtifactVersion> {
        let mut record: ArtifactVersion = serde_json::from_slice(
            &fs::read(self.version_record_path(id, revision))?).map_err(io::Error::other)?;
        if record.schema_version != ARTIFACT_SCHEMA || record.artifact_id != id || record.revision != revision {
            return Err(io::Error::other("artifact version identity/schema mismatch"));
        }
        record.bytes_path = self.version_path(id, revision);
        Ok(record)
    }

    /// Only the committed ancestor chain is history; unrelated orphan writes
    /// remain on disk but are not silently published as earlier versions.
    pub fn revisions(&self, id: EntityId) -> io::Result<Vec<u64>> {
        let Some(head) = self.get(&id) else { return Ok(Vec::new()) };
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
    pub fn rollback(&self, id: EntityId, source_revision: u64) -> io::Result<ArtifactVersion> {
        if !self.revisions(id)?.contains(&source_revision) {
            return Err(io::Error::new(io::ErrorKind::NotFound, "committed artifact revision not found"));
        }
        let mut records = self.records.write();
        let head = records.get(&id).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "artifact not found"))?;
        let source = self.read_version(id, source_revision)?;
        let bytes = self.read_content(&source)?;
        let revision = self.next_revision(id)?;
        let record = ArtifactVersion {
            revision, parent_revision: Some(head.revision), restored_from: Some(source_revision),
            updated_at: Utc::now(), bytes_path: self.version_path(id, revision), ..source
        };
        self.commit_version(&record, &bytes)?;
        self.sync_durable(&record).map_err(io::Error::other)?;
        records.insert(id, record.clone());
        Ok(record)
    }

    /// Every binding is a new artifact version, including its invocation
    /// contract and CAS module identity. A code rollback restores this binding
    /// while keeping all state written by newer code.
    pub fn bind_backend(&self, id: EntityId, module: &[u8]) -> io::Result<ArtifactVersion> {
        let binding = BackendBinding::seal(&self.store, module).map_err(io::Error::other)?;
        let mut records = self.records.write();
        let previous = records.get(&id).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "artifact not found"))?;
        let bytes = self.read_content(previous)?;
        let revision = self.next_revision(id)?;
        let record = ArtifactVersion {
            revision, parent_revision: Some(previous.revision), restored_from: None,
            updated_at: Utc::now(), bytes_path: self.version_path(id, revision),
            backend: Some(binding), ..previous.clone()
        };
        self.commit_version(&record, &bytes)?;
        self.sync_durable(&record).map_err(io::Error::other)?;
        records.insert(id, record.clone());
        Ok(record)
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

    /// Store the manifest and the durable artifact record for one committed
    /// content version. Called on every content write so the record is never
    /// behind the committed head.
    fn sync_durable(&self, version: &ArtifactVersion) -> anyhow::Result<ArtifactRecord> {
        let (manifest, manifest_ref, version_record_ref) = self.build_manifest(version)?;
        self.store.put_artifact_manifest(&manifest)?;
        let root = self.artifact_dir(version.artifact_id);
        let root_identity = directory_identity(&root);
        let record = ArtifactRecord {
            schema_version: 1,
            artifact_id: version.artifact_id,
            subject_id: self.agent_id,
            creator_task_id: None,
            title: version.title.clone(),
            record_revision: 0,
            active_manifest_ref: Some(manifest_ref.clone()),
            candidate_manifest_refs: Vec::new(),
            state_revision: format!("{}:{}", version.revision, &manifest.version_digest.0[..16.min(manifest.version_digest.0.len())]),
            state_ref: version_record_ref.clone(),
            lifecycle: "draft".into(),
            runtime_instance_refs: Vec::new(),
            desired_lifecycle_ref: manifest_ref.clone(),
            provenance_and_causation_ref: version_record_ref,
            artifact_root: root,
            root_identity,
            updated_commit: shenko_types::commit::CommitRef::new(EntityId::nil(), shenko_types::ids::LedgerSeq::ZERO),
        };
        // The content revision and the fact announcing it commit together, so
        // a committed version is never missing from the subject's perception.
        let event = self.change_event(version);
        self.commit_record(&record, Some(&event))
    }

    /// Stage the current head as a candidate manifest without changing the
    /// active one. Staging is durable and replicated; nothing here executes and
    /// nothing is adopted: the candidate is a proposal, not the closure other
    /// nodes must run (docs/specs/cognition-units.md#spec-51-1 "explicit adoption before content joins").
    /// Whether the work is still at the revision an action names.
    ///
    /// A work that has moved on refuses the stale action: the action is bound
    /// to a revision, not to "whatever is there now".
    pub fn check_revision(&self, id: EntityId, expected: u64) -> anyhow::Result<()> {
        let current = self.get(&id).ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        anyhow::ensure!(
            current.revision == expected,
            "this action names revision {expected}, but the work is at revision {}",
            current.revision
        );
        Ok(())
    }

    pub fn stage_artifact(
        &self,
        id: EntityId,
        request_id: Option<&str>,
    ) -> anyhow::Result<ArtifactRecord> {
        let version = self.get(&id).ok_or_else(|| anyhow::anyhow!("artifact not found"))?;
        let (manifest, manifest_ref, _) = self.build_manifest(&version)?;
        self.store.put_artifact_manifest(&manifest)?;
        let mut record = self.store.artifact_record(id)?
            .ok_or_else(|| anyhow::anyhow!("artifact has no durable record"))?;
        if record.candidate_manifest_refs.contains(&manifest_ref)
            || record.active_manifest_ref.as_ref() == Some(&manifest_ref)
        {
            // Already staged (or already active): this is a replay of the
            // same proposal, not a new transition. No record write, no new
            // fact, no risk of re-admitting a different payload under one id.
            return Ok(record);
        }
        record.candidate_manifest_refs.push(manifest_ref);
        // Lifecycle is the artifact's declared stage, never a manifest-selection
        // word: staging changes the candidate set only.
        let event = self.state_event(&record, "stage", request_id);
        self.commit_record(&record, Some(&event))
    }

    /// Publish one staged candidate as the active manifest — the explicit
    /// adoption attitude for this content (docs/specs/cognition-units.md#spec-51-1, docs/specs/configuration-playground.md#spec-43-2).
    ///
    /// The candidate refs are NOT erased: a published manifest stays a recorded
    /// candidate and `active_manifest_ref` says which one is in force, so the
    /// previous proposal facts remain readable. The published manifest must be
    /// CAS-resolvable right now (digest and length), because it becomes the
    /// closure other nodes verify against.
    pub fn publish_artifact(
        &self,
        id: EntityId,
        manifest_digest: &Digest,
        request_id: Option<&str>,
    ) -> anyhow::Result<ArtifactRecord> {
        let mut record = self.store.artifact_record(id)?
            .ok_or_else(|| anyhow::anyhow!("artifact has no durable record"))?;
        if record.active_manifest_ref.as_ref().map(|reference| &reference.digest) == Some(manifest_digest) {
            // Exact replay of an already-published adoption: answer from the
            // durable record instead of writing a second transition.
            return Ok(record);
        }
        let published = record.candidate_manifest_refs.iter()
            .find(|reference| &reference.digest == manifest_digest).cloned()
            .ok_or_else(|| anyhow::anyhow!("publish requires a staged candidate manifest"))?;
        let manifest = self.store.artifact_manifest(id, manifest_digest)?
            .ok_or_else(|| anyhow::anyhow!("published manifest is not in the durable manifest set"))?;
        let bytes = self.store.cas.read_object(&published.digest)?;
        anyhow::ensure!(
            bytes.len() as u64 == published.bytes && Digest::blake3(&bytes) == published.digest,
            "published manifest reference does not resolve to its exact bytes"
        );
        anyhow::ensure!(
            serde_json::to_vec(&manifest)?.as_slice() == bytes.as_slice(),
            "durable manifest row and its CAS object describe different closures"
        );
        record.active_manifest_ref = Some(published);
        let event = self.state_event(&record, "publish", request_id);
        self.commit_record(&record, Some(&event))
    }

    /// A lifecycle/manifest transition of an artifact is a business fact the
    /// subject must be able to perceive — docs/specs/configuration-playground.md#spec-43-2 (`artifact.state_changed`
    /// enters the same subject inbox as actions and content revisions).
    ///
    /// The announcing fact is built BEFORE the record is written and committed
    /// with it in one transaction, so there is no "recorded but unnotified"
    /// window to retry: the transition and the subject's perception of it are
    /// one durable boundary (docs/specs/configuration-playground.md#spec-43-2, docs/specs/cognition-units.md#spec-51-1).
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
        // The identity names the transition, not a timestamp: the same
        // resulting state reached by the same cause is the same fact, and a
        // different cause or a different manifest set is a different one.
        InboxEvent {
            event_id: stable_id(&format!(
                "artifact-state/v2:{}:{cause}:{}:{}:{}",
                record.artifact_id,
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
                // The request that named this transition rides the durable
                // fact. It does NOT change the event's identity: the same
                // resulting state reached by the same cause is the same fact.
                "request_id": request_id,
                "lifecycle": &record.lifecycle,
                "active_manifest_digest": active,
                "candidate_manifest_digests": candidates,
                "state_revision": &record.state_revision,
                // Reason pointers: the subject can cite this exact record
                // state and commit, never a paraphrase of it.
                "updated_commit": record.updated_commit,
                "desired_lifecycle_ref": record.desired_lifecycle_ref,
                "explanation": format!(
                    "Artifact '{}' is in lifecycle '{}' after {cause}; the durable record, its manifest refs and its commit above are the exact transition.",
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

    /// Commit a durable record together with the fact announcing it. The
    /// volatile wake happens only after the fact is durable.
    pub(crate) fn commit_record(
        &self,
        record: &ArtifactRecord,
        event: Option<&InboxEvent>,
    ) -> anyhow::Result<ArtifactRecord> {
        let notification = match event {
            Some(event) => Some(self.notification(event)?),
            None => None,
        };
        let (record, _) = self
            .store
            .put_artifact_record_with_event(record, notification.as_ref())?;
        if let Some(event) = event {
            self.wake(event);
        }
        Ok(record)
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
                // pretend to be judged attitudes (docs/specs/cognition-units.md#spec-51-1).
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
        for artifact in self.list() {
            for revision in self.revisions(artifact.artifact_id)? {
                self.notify_changed(&self.read_version(artifact.artifact_id, revision)?)?;
            }
        }
        Ok(())
    }

    /// Reconcile the durable records with the committed version chain after a
    /// restart: every head that has no durable record yet gets one. The file
    /// chain is the content truth; the durable record is the business truth.
    fn recover_durable(&self) -> anyhow::Result<()> {
        for version in self.list() {
            if self.store.artifact_record(version.artifact_id)?.is_none() {
                self.sync_durable(&version)?;
            }
        }
        Ok(())
    }

    fn load(&self) -> anyhow::Result<()> {
        for entry in fs::read_dir(self.root.as_path())? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() { continue; }
            let Some(id) = entry.file_name().to_str().and_then(|name| name.parse::<EntityId>().ok()) else { continue };
            let meta = entry.path().join("meta.json");
            let raw = match fs::read(&meta) {
                Ok(bytes) => bytes,
                // A content write before first head selection is an orphan,
                // not an artifact to invent during recovery.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            let value: Value = serde_json::from_slice(&raw)?;
            let record = if value.get("schema_version").and_then(Value::as_u64) == Some(ARTIFACT_SCHEMA as u64) {
                let mut head: ArtifactVersion = serde_json::from_value(value)?;
                head.bytes_path = self.version_path(id, head.revision);
                let immutable = self.read_version(id, head.revision)?;
                anyhow::ensure!(head.artifact_id == id && serde_json::to_value(&head)? == serde_json::to_value(&immutable)?,
                    "artifact head differs from immutable version");
                self.read_content(&immutable)?;
                immutable
            } else {
                // Explicit one-way migration of the original flat content /
                // meta.json layout. Preserve the exact old metadata and content
                // files; do not execute its old reserved backend_script field.
                #[derive(Deserialize)]
                struct Legacy { artifact_id: EntityId, title: String, mime: String, revision: u64, updated_at: DateTime<Utc> }
                let legacy: Legacy = serde_json::from_slice(&raw)?;
                anyhow::ensure!(legacy.artifact_id == id, "legacy artifact identity mismatch");
                let bytes = fs::read(entry.path().join("content"))?;
                let record = ArtifactVersion {
                    schema_version: ARTIFACT_SCHEMA, artifact_id: id,
                    title: legacy.title, mime: legacy.mime.clone(), revision: legacy.revision,
                    content_ref: self.seal_content(&bytes, &legacy.mime)?, bytes_path: self.version_path(id, legacy.revision),
                    parent_revision: None, restored_from: None, updated_at: legacy.updated_at, backend: None,
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
        // it, not a second hand-written page (docs/specs/web-interface.md#spec-21, docs/specs/configuration-playground.md#spec-43-2).
        .route("/playground", get(list_handler))
        .route("/playground/{id}", get(get_handler).put(put_handler))
        .route("/playground/{id}/versions", get(versions_handler))
        .route("/playground/{id}/rollback", post(rollback_handler))
        .route("/playground/{id}/backend", get(backend_info_handler).put(bind_backend_handler))
        .route("/playground/{id}/backend/action", post(backend_action_handler).get(action_status_handler))
        .route("/playground/{id}/manifest", get(manifest_handler))
        .route("/playground/{id}/stage", post(stage_handler))
        .route("/playground/{id}/publish", post(publish_handler))
        .with_state(state)
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
    if let Err(error) = pg.check_revision(id, request.revision) {
        return error_response(StatusCode::CONFLICT, &format!("artifact stage refused: {error:#}"));
    }
    match pg.stage_artifact(id, Some(&request.request_id)) {
        Ok(record) => Json(json!({"ok": true, "replayed": false, "request_id": request.request_id, "record": record})).into_response(),
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
    if let Err(error) = pg.check_revision(id, request.revision) {
        return error_response(StatusCode::CONFLICT, &format!("artifact publish refused: {error:#}"));
    }
    match pg.publish_artifact(id, &request.manifest_digest, Some(&request.request_id)) {
        Ok(record) => Json(json!({"ok": true, "replayed": false, "request_id": request.request_id, "record": record})).into_response(),
        Err(e) => error_response(StatusCode::CONFLICT, &format!("artifact publish failed: {e:#}")),
    }
}

async fn list_handler() -> Redirect {
    Redirect::temporary("/?view=playground")
}

/// A work's content is the subject's (or a plugin's) own code, never the
/// shell's. Served same-origin without this, an HTML work opened directly
/// would run with the admin UI's origin and could drive `/ws`, config and
/// owner routes. The CSP `sandbox` directive gives the document an opaque
/// origin whether it is opened directly or framed by a lens, so its scripts
/// run but hold none of the shell's authority (docs/specs/web-interface.md#spec-21-3).
pub(crate) fn confine_artifact_content(headers: &mut axum::http::HeaderMap) {
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox allow-scripts allow-forms allow-popups allow-modals"),
    );
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
}

async fn get_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>) -> Response {
    let Some(record) = pg.get(&id) else { return error_response(StatusCode::NOT_FOUND, "artifact not found") };
    match pg.read_content(&record) {
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_str(&record.mime)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")));
            if let Ok(value) = HeaderValue::from_str(&record.revision.to_string()) { response.headers_mut().insert("x-artifact-revision", value); }
            confine_artifact_content(response.headers_mut());
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
    // A rollback APPENDS a new version: the older bytes stay, and the work's
    // own revision moves on. The named revision is the source, not the
    // target — that is why a repeated submit cannot produce a second
    // rollback by accident: the source has already been left behind.
    match pg.rollback(id, request.revision) {
        Ok(version) => Json(json!({"ok": true, "replayed": false, "request_id": request.request_id, "version": version})).into_response(),
        Err(e) => error_response(StatusCode::CONFLICT, &format!("artifact rollback failed: {e:#}")),
    }
}

async fn backend_info_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>) -> Response {
    match pg.get(&id) {
        Some(record) => Json(json!({"ok": true, "revision": record.revision, "backend": record.backend})).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "artifact not found"),
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
