//! Artifact backend admission and WASI execution on the existing native
//! Task/Operation/T1/E1/T2 path. Files are business state, never dispatch truth.

use std::{collections::BTreeMap, fs, io::Read, path::{Path, PathBuf}, sync::Arc};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use axum::{http::StatusCode, response::{IntoResponse, Response, Json}};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_store::{Store, child::{NATIVE_INPUT, NATIVE_OPERATION_ID, memo_uuid}};
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef, evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel},
    ids::*, object::*, operation::*, task::*,
};
use shenko_wasm::{BoundedOutputPipe, MemoryInputPipe, MountMode, MountSpec, WasiCommand, WasmRuntime, HOME_GUEST_PATH};
use crate::playground::{ArtifactRecord, PlaygroundState, stable_id};

pub const PLAYGROUND_BACKEND_KIND: &str = "playground.backend";
const PLUGIN: &str = "4c0f6d5b-5a1f-4d2e-9f76-7b5e3f6a7b8c";
const CONTRACT: &[u8] = b"shenko.playground.backend/1: WASI preview1 _start; argv program/action; JSON stdin/stdout; /home isolated persistent rw; /artifact ro; no replay after T1";
const MAX_MODULE: usize = 2 * 1024 * 1024;
const MAX_INPUT: usize = 64 * 1024;
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_STATE_ENTRIES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendBinding {
    pub schema_version: u32,
    pub module_digest: Digest,
    pub module_ref: ObjectRef,
    pub invocation_contract_digest: Digest,
    pub invocation_contract: InvocationContract,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationContract {
    pub version: u32,
    pub runtime: String,
    pub entry: String,
    pub argv: Vec<String>,
    pub stdin: String,
    pub stdout: String,
    pub mounts: BTreeMap<String, String>,
    pub max_stream_bytes: usize,
    pub max_state_bytes: u64,
    pub max_state_entries: usize,
}
impl InvocationContract {
    fn current() -> Self {
        Self { version:1, runtime:"shenko-wasm/wasi-preview1-command".into(), entry:"_start".into(),
            argv:vec!["playground-backend".into(),"<action>".into()],
            stdin:"exact frozen action input JSON".into(), stdout:"one bounded JSON value".into(),
            mounts:BTreeMap::from([("/home".into(),"read_write: isolated artifact state".into()),
                ("/artifact".into(),"read_only: artifact versions".into())]),
            max_stream_bytes:MAX_OUTPUT, max_state_bytes:MAX_STATE_BYTES, max_state_entries:MAX_STATE_ENTRIES }
    }
}

impl BackendBinding {
    pub(crate) fn seal(store: &Store, module: &[u8]) -> Result<Self> {
        ensure!(!module.is_empty() && module.len() <= MAX_MODULE, "backend module must be 1 byte..2MiB");
        let module_ref = seal_bytes(store, module, "application/wasm", "wasi-command")?;
        let invocation_contract = InvocationContract::current();
        Ok(Self { schema_version: 1, module_digest: module_ref.digest.clone(), module_ref,
            invocation_contract_digest: Digest::blake3(&serde_json::to_vec(&invocation_contract)?), invocation_contract })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendActionRequest {
    pub request_id: String,
    /// Required from GET .../backend, so old pages cannot call a new contract
    /// merely because it retained an action with the same name.
    pub backend_digest: Digest,
    pub action: String,
    pub input: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenAction {
    schema_version: u32,
    artifact_id: EntityId,
    artifact_revision: u64,
    /// CAS-sealed JSON of the immutable ArtifactRecord this action ran
    /// against. Required closure: the handler verifies the record from CAS,
    /// not from mutable workspace files, before the binding is trusted.
    /// `None` only for inputs admitted before the closure requirement; those
    /// tasks stay resolvable as history but must never execute.
    #[serde(default)]
    version_record_ref: Option<ObjectRef>,
    binding: BackendBinding,
    request: BackendActionRequest,
}

/// A projection of real rows; this structure is not a second persisted ledger.
#[derive(Debug, Serialize)]
pub struct BackendReceipt {
    pub task_id: EntityId,
    pub accepted_commit: CommitRef,
    pub input_ref: ObjectRef,
    pub state: String,
    pub replayed: bool,
    pub operation: Option<OperationRecord>,
    pub attempt: Option<AttemptRecord>,
    pub evidence: Option<EvidenceReceipt>,
    pub result: Option<TaskResult>,
}

#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("artifact or request not found")] NotFound,
    #[error("artifact has no executable backend binding")] NoBackend,
    #[error("ARTIFACT_VERSION_STALE: supplied module is not the selected binding")] Stale,
    #[error("ACTION_CONFLICT: request_id already belongs to different input")] Conflict,
    #[error("BACKEND_UNAVAILABLE: {0}")] Unavailable(String),
    #[error("ACTION_INVALID: {0}")] Invalid(String),
    #[error(transparent)] Internal(#[from] anyhow::Error),
}
impl IntoResponse for ActionError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Self::NotFound => (StatusCode::NOT_FOUND, "NOT_FOUND"),
            Self::NoBackend => (StatusCode::CONFLICT, "NO_BACKEND"),
            Self::Stale => (StatusCode::CONFLICT, "ARTIFACT_VERSION_STALE"),
            Self::Conflict => (StatusCode::CONFLICT, "ACTION_CONFLICT"),
            Self::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "BACKEND_UNAVAILABLE"),
            Self::Invalid(_) => (StatusCode::UNPROCESSABLE_ENTITY, "ACTION_INVALID"),
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL"),
        };
        (status, Json(json!({"accepted": false, "code": code, "error": self.to_string()}))).into_response()
    }
}

fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= 64 && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn task_id(artifact_id: EntityId, request_id: &str) -> EntityId {
    stable_id(&format!("artifact-action/v1:{artifact_id}:{request_id}"))
}
fn read_frozen(store: &Store, task: &TaskRecord) -> Result<FrozenAction> {
    ensure!(task.kind == PLAYGROUND_BACKEND_KIND && task.definition_version == 1, "action task kind/version mismatch");
    let bytes = store.cas.read_object(&task.input_ref.digest)?;
    ensure!(bytes.len() as u64 == task.input_ref.bytes && Digest::blake3(&bytes) == task.input_ref.digest,
        "frozen artifact input digest mismatch");
    let frozen: FrozenAction = serde_json::from_slice(&bytes)?;
    ensure!(frozen.schema_version == 1 || frozen.schema_version == 2, "unsupported artifact input schema");
    Ok(frozen)
}
fn same_request(store: &Store, task: &TaskRecord, artifact_id: EntityId, request: &BackendActionRequest) -> Result<bool> {
    let frozen = read_frozen(store, task)?;
    Ok(frozen.artifact_id == artifact_id && serde_json::to_value(&frozen.request)? == serde_json::to_value(request)?)
}

impl PlaygroundState {
    pub fn invoke_action(&self, id: EntityId, request: BackendActionRequest) -> Result<BackendReceipt, ActionError> {
        if !valid_token(&request.request_id) || !valid_token(&request.action) {
            return Err(ActionError::Invalid("request_id/action must be 1..64 ASCII letters, digits, '_' or '-'".into()));
        }
        if serde_json::to_vec(&request.input).map_err(anyhow::Error::from)?.len() > MAX_INPUT {
            return Err(ActionError::Invalid("input exceeds 64KiB".into()));
        }
        let idempotency_id = task_id(id, &request.request_id);
        // Original receipt wins BEFORE selected version checks. A response lost
        // before an upgrade still resolves its original task/attempt afterward.
        if let Some(existing) = self.store.get_task(idempotency_id)? {
            if !same_request(&self.store, &existing, id, &request)? { return Err(ActionError::Conflict); }
            let mut receipt = self.action_status(id, &request.request_id)?;
            receipt.replayed = true;
            return Ok(receipt);
        }
        let native = self.native.as_ref().ok_or_else(|| ActionError::Unavailable("native dispatcher not attached".into()))?;
        if !native.has_handler(PLAYGROUND_BACKEND_KIND, 1) {
            return Err(ActionError::Unavailable("WASI artifact handler is not installed".into()));
        }
        let record = self.get(&id).ok_or(ActionError::NotFound)?;
        let binding = record.backend.as_ref().ok_or(ActionError::NoBackend)?;
        if request.backend_digest != binding.module_digest { return Err(ActionError::Stale); }
        // The immutable version record must be CAS-sealed at admission: a
        // verified receipt must survive workspace loss, so its binding,
        // content ref and revision are required closure, not optional metadata.
        let version_record_ref = seal_json(&self.store, &record, "artifact-version-record")
            .map_err(|e| ActionError::Unavailable(format!("artifact version closure unavailable: {e:#}")))?;
        // The bound module bytes are also required closure: validate they are
        // CAS-resolvable NOW rather than after a false accepted ACK.
        {
            let module = self.store.cas.read_object(&binding.module_digest)
                .map_err(|e| ActionError::Unavailable(format!("bound module unavailable: {e:#}")))?;
            if module.len() as u64 != binding.module_ref.bytes || Digest::blake3(&module) != binding.module_digest {
                return Err(ActionError::Unavailable("bound module digest/length mismatch in CAS".into()));
            }
        }
        let frozen = FrozenAction { schema_version: 2, artifact_id: id, artifact_revision: record.revision,
            version_record_ref: Some(version_record_ref), binding: binding.clone(), request: request.clone() };
        let input_ref = seal_json(&self.store, &frozen, "artifact-action-input")?;
        let (head, _) = self.store.load_subject(self.agent_id).map_err(anyhow::Error::from)?;
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let task = TaskRecord {
            record_schema_version: 1, task_id: idempotency_id,
            task_revision: TaskRevision(0), record_revision: RecordRevision(0),
            agent_id: self.agent_id, conversation_id: self.agent_id,
            owner: TaskOwner::Task { id: head.subject_root_task_id }, background: false,
            kind: PLAYGROUND_BACKEND_KIND.into(), definition_version: 1,
            implementation_ref: implementation(), input_ref,
            state: TaskState::Pending { checkpoint_ref: None }, abort_request: None,
            memos: serde_json::Map::from_iter([
                ("intent_id".into(), json!(idempotency_id)), (NATIVE_INPUT.into(), json!("object")),
            ]),
            residual_case_ids: vec![], created_by_step_id: None, dispatch_eligibility: DispatchEligibility::Ready,
            no_progress_crashes: 0, first_run_at: None, terminal_at: None, created_commit: zero, updated_commit: zero,
        };
        let accepted_event = shenko_subject::InboxEvent {
            event_id: stable_id(&format!("artifact-action-accepted/v1:{idempotency_id}")),
            kind: shenko_subject::InboxEventKind::ArtifactChanged,
            created_at: chrono::Utc::now(), source: "playground".into(),
            payload: json!({"kind":"artifact.action", "artifact_id":id,"artifact_revision":record.revision,
                "task_id":idempotency_id,"request_id":request.request_id,"action":request.action,
                "backend_digest":request.backend_digest,"input_ref":task.input_ref,
                "explanation":"A browser action was durably accepted as a native task. Execution/result is a separate receipt; admission is not evidence that the guest ran."}),
        };
        let input_event = shenko_types::store::RawInboxEvent {
            event_id: accepted_event.event_id, agent_id: self.agent_id, source:"playground".into(), kind:"artifact_changed".into(),
            payload_json: serde_json::to_string(&accepted_event).map_err(anyhow::Error::from)?,
            received_at_ms: accepted_event.created_at.timestamp_millis(),
        };
        let replayed = match self.store.admit_native_task(&task, &input_event) {
            Ok(Some(existing)) => {
                if !same_request(&self.store, &existing, id, &request)? { return Err(ActionError::Conflict); }
                true
            }
            Ok(None) => false,
            Err(error) => {
                // A concurrent admission can commit between the initial lookup
                // and the writer transaction. The committed id always wins:
                // re-read it instead of surfacing a stale/availability error
                // to an actually-accepted action.
                if let Some(existing) = self.store.get_task(idempotency_id)? {
                    if !same_request(&self.store, &existing, id, &request)? { return Err(ActionError::Conflict); }
                    true
                } else if error.to_string().contains("already committed with different input") {
                    return Err(ActionError::Conflict);
                } else {
                    return Err(error.into());
                }
            }
        };
        // Do NOT run/cancel a second dispatch future inside HTTP. The existing
        // committed dispatcher owns execution, even if this response is lost.
        let mut receipt = self.action_status(id, &request.request_id)?;
        receipt.replayed = replayed;
        Ok(receipt)
    }

    pub fn action_status(&self, id: EntityId, request_id: &str) -> Result<BackendReceipt, ActionError> {
        if !valid_token(request_id) { return Err(ActionError::Invalid("invalid request_id".into())); }
        let task = self.store.get_task(task_id(id, request_id))?.ok_or(ActionError::NotFound)?;
        let frozen = read_frozen(&self.store, &task)?;
        if frozen.artifact_id != id || frozen.request.request_id != request_id || task.agent_id != self.agent_id {
            return Err(ActionError::Internal(anyhow::anyhow!("artifact action identity mismatch")));
        }
        let operation = memo_uuid(&task, NATIVE_OPERATION_ID).map(|id| self.store.get_operation(id)).transpose()
            .map_err(anyhow::Error::from)?.flatten();
        let attempt = operation.as_ref().map(|operation| self.store.external_operation_attempt(operation.operation_id)).transpose()?;
        let evidence = attempt.as_ref().and_then(|a| a.final_receipt_id)
            .map(|id| self.store.external_evidence(id)).transpose()?;
        let result = self.store.native_task_result(task.task_id)?;
        let state = if let Some(result) = &result {
            match &result.outcome { TaskOutcome::Succeeded => "executed", TaskOutcome::Failed { .. } => "failed",
                TaskOutcome::Cancelled => "cancelled", TaskOutcome::Interrupted => "interrupted" }
        } else if operation.as_ref().is_some_and(|o| o.state == OperationState::Reconciling
            || (o.state != OperationState::Resolved && !task.is_running())) {
            "unknown"
        } else if task.is_running() { "executing" } else { "admitted" };
        Ok(BackendReceipt { task_id: task.task_id, accepted_commit: task.created_commit, input_ref: task.input_ref,
            state: state.into(), replayed: false, operation, attempt, evidence, result })
    }
}

fn implementation() -> ImplementationRef {
    ImplementationRef { plugin_id: PLUGIN.parse().expect("fixed built-in plugin ID"),
        artifact_digest: Digest::blake3(CONTRACT), registry_revision: 0 }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StateEntry { kind: String, object: Option<ObjectRef> }
#[derive(Serialize)]
struct StateManifest<'a> { schema_version: u32, entries: &'a BTreeMap<String, StateEntry> }
struct Snapshot { object: ObjectRef, entries: BTreeMap<String, StateEntry> }

fn seal_json(store: &Store, value: &impl Serialize, codec: &str) -> Result<ObjectRef> {
    seal_bytes(store, &serde_json::to_vec(value)?, "application/json", codec)
}
fn seal_bytes(store: &Store, bytes: &[u8], media: &str, codec: &str) -> Result<ObjectRef> {
    match store.cas.seal_object(bytes, media, "playground", codec, 1) {
        Ok(sealed) => Ok(sealed.object),
        Err(error) => { store.close_external_writer(); Err(error.into()) }
    }
}

/// Snapshot exact file bytes, directory identities and literal symlinks, never
/// a symlink target. The explicit small-state profile caps total bytes/entries;
/// exceeding it is an observed failure/unknown, not silent omission.
fn snapshot(store: &Store, root: &Path) -> Result<Snapshot> {
    fn visit(store: &Store, root: &Path, dir: &Path, entries: &mut BTreeMap<String, StateEntry>, budget: &mut u64) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            ensure!(entries.len() < MAX_STATE_ENTRIES, "artifact state exceeds 256 entries");
            let path = entry.path();
            let relative = path.strip_prefix(root)?.to_str().context("artifact state path is not UTF-8")?.to_string();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                entries.insert(relative, StateEntry { kind: "directory".into(), object: None });
                visit(store, root, &path, entries, budget)?;
            } else if kind.is_file() {
                let mut file = fs::File::open(&path)?;
                let mut bytes = Vec::new();
                file.by_ref().take(budget.saturating_add(1)).read_to_end(&mut bytes)?;
                ensure!(bytes.len() as u64 <= *budget, "artifact state exceeds 16MiB");
                *budget -= bytes.len() as u64;
                file.sync_all()?;
                let object = seal_bytes(store, &bytes, "application/octet-stream", "artifact-state-file")?;
                entries.insert(relative, StateEntry { kind: "file".into(), object: Some(object) });
            } else if kind.is_symlink() {
                let target = fs::read_link(&path)?;
                let target = target.to_str().context("artifact symlink target is not UTF-8")?;
                ensure!(target.len() as u64 <= *budget, "artifact state exceeds 16MiB");
                *budget -= target.len() as u64;
                let object = seal_bytes(store, target.as_bytes(), "text/plain", "artifact-state-symlink")?;
                entries.insert(relative, StateEntry { kind: "symlink".into(), object: Some(object) });
            } else { anyhow::bail!("unsupported artifact state filesystem entry"); }
        }
        fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
    let mut entries = BTreeMap::new();
    let mut budget = MAX_STATE_BYTES;
    visit(store, root, root, &mut entries, &mut budget)?;
    let object = seal_json(store, &StateManifest { schema_version: 1, entries: &entries }, "artifact-state-manifest")?;
    Ok(Snapshot { object, entries })
}

fn changes(before: &Snapshot, after: &Snapshot) -> (usize, Vec<Value>) {
    let keys: std::collections::BTreeSet<_> = before.entries.keys().chain(after.entries.keys()).collect();
    let mut count = 0;
    let mut preview = Vec::new();
    for key in keys {
        if before.entries.get(key) == after.entries.get(key) { continue; }
        count += 1;
        if preview.len() < 32 { preview.push(json!({ "path": key, "before": before.entries.get(key), "after": after.entries.get(key) })); }
    }
    (count, preview)
}

pub struct PlaygroundBackendHandler { store: Arc<Store>, root: PathBuf }
impl PlaygroundBackendHandler {
    pub fn new(store: Arc<Store>, root: PathBuf) -> Self { Self { store, root } }

    fn artifact_dir(&self, id: EntityId) -> PathBuf { self.root.join(id.to_string()) }

    fn validate_input(&self, task: &TaskRecord, input: &FrozenAction) -> Result<Vec<u8>> {
        ensure!(task.task_id == task_id(input.artifact_id, &input.request.request_id), "backend request/task identity mismatch");
        let version_record_ref = input.version_record_ref.as_ref()
            .ok_or_else(|| anyhow::anyhow!("frozen action missing required sealed version closure"))?;
        ensure!(input.schema_version == 2 && input.binding.schema_version == 1
            && input.binding.invocation_contract == InvocationContract::current()
            && input.binding.invocation_contract_digest == Digest::blake3(&serde_json::to_vec(&input.binding.invocation_contract)?)
            && input.request.backend_digest == input.binding.module_digest
            && input.binding.module_digest == input.binding.module_ref.digest, "unsupported backend binding");
        ensure!(valid_token(&input.request.action) && valid_token(&input.request.request_id), "invalid frozen action token");
        // A source/delegation cannot turn an arbitrary CAS object into an
        // executable capability: it must match the artifact's immutable version.
        // The required version record comes from the sealed CAS closure.
        let version_bytes = self.store.cas.read_object(&version_record_ref.digest)?;
        ensure!(version_bytes.len() as u64 == version_record_ref.bytes
            && Digest::blake3(&version_bytes) == version_record_ref.digest,
            "sealed artifact version record mismatch");
        let version: ArtifactRecord = serde_json::from_slice(&version_bytes)?;
        ensure!(version.artifact_id == input.artifact_id && version.revision == input.artifact_revision
            && version.backend.as_ref() == Some(&input.binding), "backend not bound to artifact version");
        // The local immutable record must still be byte-identical to the
        // sealed one: a rewritten workspace file is tampering, not a fresh
        // equivalent record.
        let local_bytes = fs::read(self.artifact_dir(input.artifact_id)
            .join("versions").join(format!("{}.json", input.artifact_revision)))?;
        ensure!(local_bytes == version_bytes, "workspace artifact record diverged from sealed closure");
        let module = self.store.cas.read_object(&input.binding.module_digest)?;
        ensure!(module.len() as u64 == input.binding.module_ref.bytes && Digest::blake3(&module) == input.binding.module_digest,
            "bound module CAS mismatch");
        Ok(module)
    }

    async fn run(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: FrozenAction = serde_json::from_value(invocation.input)?;
        let module = self.validate_input(&invocation.task, &input)?;
        let dir = self.artifact_dir(input.artifact_id);
        let state_dir = dir.join("state");
        fs::create_dir_all(&state_dir)?;
        let before = snapshot(&self.store, &state_dir)?;
        let contract_digest = Digest::blake3(CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef { digest: contract_digest.clone(), locator: "builtin:playground-wasi-command/1".into() },
            symbol: "artifact.action".into(),
            schema: SchemaRef { name: "artifact-action-observation".into(), major: 1, digest: contract_digest.clone() },
        };
        let bindings = ResourceBindings {
            provider: "playground.wasi/1".into(), account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(), mount_generation: input.artifact_revision,
            capability_level: "wasi-p1-isolated-small-state".into(), extra: json!({
                "module_ref": input.binding.module_ref, "artifact_id": input.artifact_id,
                "artifact_revision": input.artifact_revision, "state_before": before.object,
                "mounts": [{"guest":"/home","mode":"read_write"},{"guest":"/artifact","mode":"read_only"}],
                "max_stdout_stderr_bytes": MAX_OUTPUT, "max_state_bytes": MAX_STATE_BYTES, "max_state_entries": MAX_STATE_ENTRIES,
            }),
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let mut operation = OperationRecord {
            record_schema_version: 1, operation_id: invocation.operation_id, record_revision: RecordRevision(0),
            task_id: invocation.task.task_id, logical_key: "artifact-action".into(), operation_name: "playground.wasi.action/1".into(),
            input_schema_version: 1, output_schema_version: 1,
            final_input_ref: invocation.task.input_ref.clone(), input_digest: invocation.task.input_ref.digest.clone(),
            contract_ref: OperationContractRef { plugin_id: implementation().plugin_id, artifact_digest: contract_digest.clone(),
                contract_digest, receipt_reader_ref: version.clone() },
            resource_bindings_ref: seal_json(&self.store, &bindings, "artifact-runtime-binding")?, authority_ref: invocation.authority,
            acceptance_predicate_ref: seal_json(&self.store, &"guest exited 0, stdout is bounded JSON, isolated state was synced and snapshotted", "artifact-acceptance")?,
            retry_contract_ref: seal_json(&self.store, &"no retry after armed T1; reconcile original attempt and preserve unknown", "artifact-retry")?,
            checkpoint_scope_ref: Some(before.object.clone()), state: OperationState::InFlight, outcome: None, outcome_conflict: false,
            effect_status: EffectStatus::Unknown, retry_of: None, compensates: None, possible_duplicate_of: vec![], prepared_commit: zero, evidence_through: None,
        };
        let mut attempt = AttemptRecord {
            record_schema_version: 1, attempt_id: EntityId::new(), operation_id: invocation.operation_id, attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id, worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id, epoch: invocation.task_invocation.epoch, implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed, armed_commit: zero, provider_handle: None, provider_idempotency: None,
            transport_ref: None, started_at: None, last_observed_at: None, final_receipt_id: None,
        };
        if let Some(existing) = self.store.arm_external_operation(&invocation.task_invocation, &mut operation, &mut attempt)? {
            return Ok(existing);
        }
        // Only the native dispatcher owns this future. HTTP disconnection does
        // not cancel it, and recovery never spawns another guest for this task.
        let action = input.request.action.clone();
        let request_id = input.request.request_id.clone();
        let artifact_id = input.artifact_id.to_string();
        let input_bytes = serde_json::to_vec(&input.request.input)?;
        let run_dir = dir.clone();
        let run = tokio::task::spawn_blocking(move || {
            let stdout = BoundedOutputPipe::new(MAX_OUTPUT);
            let stderr = BoundedOutputPipe::new(MAX_OUTPUT);
            let execution = (|| -> Result<i32> {
                let mut runtime = WasmRuntime::with_mounts(vec![
                    MountSpec::new(run_dir.join("state").canonicalize()?, HOME_GUEST_PATH, MountMode::ReadWrite, true, false),
                    MountSpec::new(run_dir.canonicalize()?, "/artifact", MountMode::ReadOnly, true, true),
                ])?;
                let mut command = WasiCommand::new(MemoryInputPipe::new(input_bytes), stdout.clone(), stderr.clone());
                command.arguments = vec!["playground-backend".into(), action];
                command.environment = vec![("SHENKO_ARTIFACT_ID".into(), artifact_id), ("SHENKO_REQUEST_ID".into(), request_id)];
                runtime.run_wasi_module(&module, command)
            })();
            (execution.map_err(|e| format!("{e:#}")), stdout.contents(), stderr.contents(),
                stdout.did_overflow(), stderr.did_overflow(), stdout.did_fail() || stderr.did_fail())
        }).await;
        let (execution, stdout, stderr, stdout_overflowed, stderr_overflowed, stream_failed) = match run {
            Ok(result) => result,
            Err(error) => (Err(format!("guest worker failed: {error}")), bytes::Bytes::new(), bytes::Bytes::new(), false, false, false),
        };
        // These seals are required evidence, not optional logging. Failure
        // propagates with the original T1 retained for unknown reconciliation.
        let stdout_ref = seal_bytes(&self.store, &stdout, "application/json", "artifact-stdout")?;
        let stderr_ref = seal_bytes(&self.store, &stderr, "application/octet-stream", "artifact-stderr")?;
        let after = snapshot(&self.store, &state_dir);
        let state_error = after.as_ref().err().map(|e| format!("{e:#}"));
        let output = serde_json::from_slice::<Value>(&stdout);
        let output_error = output.as_ref().err().map(|e| e.to_string());
        let (change_count, delta) = after.as_ref().map(|after| changes(&before, after)).unwrap_or_default();
        let exit_code = execution.as_ref().ok().copied();
        let runtime_error = execution.err();
        // Stream rejection/failure is independently recorded: a guest may
        // ignore the write error, emit a small valid JSON and exit 0. Retained
        // bytes are then a truncated prefix, so the attempt is INCONCLUSIVE —
        // never a verified success from the trap/exit code alone.
        let conclusive = runtime_error.is_none() && state_error.is_none()
            && !stdout_overflowed && !stderr_overflowed && !stream_failed;
        let succeeded = conclusive && exit_code == Some(0) && output_error.is_none();
        let outcome = conclusive.then_some(if succeeded { OperationOutcome::Succeeded } else { OperationOutcome::Failed });
        let effect = if !conclusive { EffectStatus::Unknown } else if change_count == 0 { EffectStatus::None }
            else if succeeded { EffectStatus::Complete } else { EffectStatus::Partial };
        let normalized = json!({
            "artifact_id": input.artifact_id, "artifact_revision": input.artifact_revision,
            "request_id": input.request.request_id, "action": input.request.action, "backend_digest": input.binding.module_digest,
            "exit_code": exit_code, "output": output.ok(), "output_error": output_error,
            "stdout_ref": stdout_ref, "stderr_ref": stderr_ref,
            "state_before": before.object, "state_after": after.as_ref().ok().map(|s| &s.object),
            "change_count": change_count, "changes": delta, "changes_preview_truncated": change_count > 32,
            "stdout_truncated": stdout_overflowed, "stderr_truncated": stderr_overflowed, "stream_failed": stream_failed,
            "runtime_error": runtime_error, "state_error": state_error,
            "explanation": "The WASI guest's isolated business-state files changed only as recorded in the before/after manifests. File ObjectRefs retain exact binary bytes; stdout is the guest's bounded structured output. An unknown outcome is not permission to replay.",
        });
        let normalized_ref = seal_json(&self.store, &normalized, "artifact-action-result")?;
        let raw = seal_json(&self.store, &json!({"input_ref":invocation.task.input_ref, "observation":normalized}), "artifact-action-observation")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1, receipt_id: EntityId::new(), operation_id: Some(operation.operation_id), attempt_id: Some(attempt.attempt_id),
            source_id: "playground.wasi/1".into(), source_instance_id: invocation.task_invocation.node_id.to_string(),
            source_event_key: Some(input.request.request_id.clone()), producer_epoch: Some(invocation.task_invocation.epoch.0),
            producer_schema_version: Some("1".into()), raw_ref: raw, decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: conclusive.then_some(if succeeded { ObservationLevel::PostconditionVerified } else { ObservationLevel::RemoteCompleted }),
            normalized_ref: Some(normalized_ref), observed_at: Some(chrono::Utc::now()), received_commit: operation.prepared_commit,
        };
        let unknown_evidence = (!conclusive).then(|| evidence.clone());
        let operation = self.store.record_external_evidence(operation.operation_id, attempt.attempt_id, evidence, outcome, effect)?;
        // Successful/failed terminal results are imported by native T2. Unknown
        // E1 has no T2, so admit its actual uncertainty as an ordinary durable
        // Playground input; reconciliation repairs any lost import below.
        if let Some(evidence) = unknown_evidence { self.publish_unknown(&invocation.task, &evidence)?; }
        Ok(operation)
    }

    fn publish_unknown(&self, task: &TaskRecord, evidence: &EvidenceReceipt) -> Result<()> {
        let frozen = read_frozen(&self.store, task)?;
        let event = shenko_subject::InboxEvent {
            event_id: evidence.receipt_id, kind: shenko_subject::InboxEventKind::ArtifactChanged,
            created_at: evidence.observed_at.context("backend evidence timestamp missing")?, source: "playground".into(),
            payload: json!({"artifact_id": frozen.artifact_id, "request_id": frozen.request.request_id,
                "task_id": task.task_id, "operation_id": evidence.operation_id, "attempt_id": evidence.attempt_id,
                "outcome":"unknown", "source_ref":evidence.raw_ref, "result_ref":evidence.normalized_ref,
                "explanation":"An original WASI backend attempt has inconclusive effects. Inspect its exact evidence/state references; it was not replayed."}),
        };
        self.store.admit_inbox_event(event.event_id, task.agent_id, "artifact_changed", "playground", &serde_json::to_value(event)?)?;
        Ok(())
    }
}

#[async_trait]
impl NativeTaskHandler for PlaygroundBackendHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor { kind: PLAYGROUND_BACKEND_KIND.into(), definition_version: 1, implementation_ref: implementation(),
            role_revision: "native-child/1".into(), authority_refs: vec!["wasi.artifact.isolated-state/1".into()], prompt_section: None }
    }
    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> { self.run(invocation).await }
    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let mut operation = self.store.external_operation(operation_id)?;
        ensure!(operation.task_id == task.task_id && operation.operation_name == "playground.wasi.action/1", "original backend operation mismatch");
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state == OperationState::InFlight && attempt.dispatch_stage != AttemptDispatchStage::Ended {
            let frozen = read_frozen(&self.store, task)?;
            let state = snapshot(&self.store, &self.artifact_dir(frozen.artifact_id).join("state"));
            let (state_ref, state_error) = match state {
                Ok(snapshot) => (Some(snapshot.object), None),
                Err(error) => (None, Some(error.to_string())),
            };
            let raw = seal_json(&self.store, &json!({"operation_id":operation_id,"attempt_id":attempt.attempt_id,
                "observation":"controller recovered without a conclusive guest receipt; current state is an observation, not proof of causation or permission to retry",
                "current_state_ref":state_ref,"state_error":state_error}), "artifact-recovery-observation")?;
            let evidence = EvidenceReceipt {
                record_schema_version: 1, receipt_id: EntityId::new(), operation_id: Some(operation_id), attempt_id: Some(attempt.attempt_id),
                source_id: "playground.wasi.recovery/1".into(), source_instance_id: self.store.capabilities.incarnation.to_string(),
                source_event_key: Some(format!("controller-recovery:{}",attempt.attempt_id)), producer_epoch: None, producer_schema_version: Some("1".into()),
                raw_ref: raw, decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()), decode_status: DecodeStatus::Decoded,
                observation_level: None, normalized_ref: None, observed_at: Some(chrono::Utc::now()), received_commit: operation.prepared_commit,
            };
            operation = self.store.record_external_evidence(operation_id, attempt.attempt_id, evidence, None, EffectStatus::Unknown)?;
        }
        if operation.state == OperationState::Reconciling {
            let attempt = self.store.external_operation_attempt(operation_id)?;
            if let Some(receipt_id) = attempt.final_receipt_id { self.publish_unknown(task, &self.store.external_evidence(receipt_id)?)?; }
        }
        Ok(operation)
    }
}
