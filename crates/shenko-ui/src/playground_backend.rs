//! Artifact backend admission and execution on the existing native
//! Task/Operation/T1/E1/T2 path — SPEC §43.2. Files are business state, never
//! dispatch truth.
//!
//! A backend binds to a registered runtime provider. Two providers exist:
//!
//! * `wasi-command` — the isolated WASI preview1 guest, one shot per action,
//!   with a bounded stream and a snapshotted persistent state directory;
//! * `host-process` — a long-lived host-managed process speaking one JSON
//!   request/response per line on stdin/stdout. The instance is a real
//!   selected root plus a durable runtime-instance record, so a restart
//!   adopts a live process instead of starting a second copy, and a
//!   generation hand-off stops it through the update seam instead of leaving
//!   an orphan against a stale root.
//!
//! Every action runs through the same native task/operation ledger as every
//! other effect; the receipts name the exact manifest and runtime that ran.

use std::{collections::BTreeMap, fs, io::{BufRead, BufReader, Read, Write}, path::{Path, PathBuf}, sync::Arc};
use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use axum::{http::StatusCode, response::{IntoResponse, Response, Json}};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_store::{Store, child::{NATIVE_INPUT, NATIVE_OPERATION_ID, memo_uuid}};
use shenko_store::artifacts::ProcessIdentity;
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef, evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel},
    ids::*, object::*, operation::*, task::*,
};
use shenko_wasm::{BoundedOutputPipe, MemoryInputPipe, MountMode, MountSpec, WasiCommand, WasmRuntime, WasiRunOutcome, HOME_GUEST_PATH};
use crate::playground::{ARTIFACT_SCHEMA, ArtifactVersion, PlaygroundState, stable_id, write_immutable};

pub const PLAYGROUND_BACKEND_KIND: &str = "playground.backend";
pub const CONTRACT_TEXT: &str = "shenko.playground.backend/1: WASI preview1 _start or host-process jsonl-1; argv program/action; JSON stdin/stdout; /home isolated persistent rw; /artifact ro; no replay after T1";
const PLUGIN: &str = "4c0f6d5b-5a1f-4d2e-9f76-7b5e3f6a7b8c";
const CONTRACT: &[u8] = CONTRACT_TEXT.as_bytes();
const MAX_MODULE: usize = 2 * 1024 * 1024;
const MAX_INPUT: usize = 64 * 1024;
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_STATE_ENTRIES: usize = 256;
const MAX_SERVICE_LINE: usize = 1024 * 1024;

/// The runtime provider a backend binds to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeBinding {
    /// One isolated WASI preview1 command per action.
    #[serde(rename = "wasi-command")]
    WasiCommand,
    /// A long-lived host process speaking JSON lines on stdin/stdout.
    #[serde(rename = "host-process")]
    HostProcess {
        program: String,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    },
}

impl Default for RuntimeBinding {
    fn default() -> Self {
        Self::WasiCommand
    }
}

/// The executable descriptor of a host-process backend, sealed into CAS. The
/// launcher re-reads it from CAS and never trusts a copy that arrived inside
/// a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostProcessDescriptor {
    pub schema_version: u32,
    pub program: String,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub protocol: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendBinding {
    pub schema_version: u32,
    pub module_digest: Digest,
    pub module_ref: ObjectRef,
    pub invocation_contract_digest: Digest,
    pub invocation_contract: InvocationContract,
    /// The runtime provider. Absent in bindings sealed before the runtime
    /// distinction existed; they resolve as `wasi-command`.
    #[serde(default)]
    pub runtime: RuntimeBinding,
    /// The action names this backend declares. Empty means the backend does
    /// not publish a schema and accepts any action name.
    #[serde(default)]
    pub action_names: Vec<String>,
    /// Sealed runtime identity used by the durable manifest.
    #[serde(default)]
    pub runtime_ref: Option<ObjectRef>,
    /// Sealed service contract used by the durable manifest.
    #[serde(default)]
    pub service_contract_ref: Option<ObjectRef>,
}

impl BackendBinding {
    /// The declared action names, or every action when none are declared.
    pub(crate) fn action_schemas(&self) -> Vec<String> {
        self.action_names.clone()
    }

    fn current_contract() -> InvocationContract {
        InvocationContract::current()
    }

    fn seal_runtime_refs(store: &Store, runtime: &RuntimeBinding) -> Result<(Option<ObjectRef>, Option<ObjectRef>)> {
        let runtime_ref = seal_json(store, &json!({"runtime": runtime}), "artifact-runtime")?;
        let service_contract_ref = seal_json(store, &json!({
            "protocol": match runtime {
                RuntimeBinding::WasiCommand => "wasi-command",
                RuntimeBinding::HostProcess { .. } => "jsonl-1",
            },
            "max_request_bytes": MAX_INPUT, "max_response_bytes": MAX_SERVICE_LINE,
        }), "artifact-service-contract")?;
        Ok((Some(runtime_ref), Some(service_contract_ref)))
    }

    pub(crate) fn seal(store: &Store, module: &[u8]) -> Result<Self> {
        ensure!(!module.is_empty() && module.len() <= MAX_MODULE, "backend module must be 1 byte..2MiB");
        let module_ref = seal_bytes(store, module, "application/wasm", "wasi-command")?;
        let invocation_contract = Self::current_contract();
        let (runtime_ref, service_contract_ref) = Self::seal_runtime_refs(store, &RuntimeBinding::WasiCommand)?;
        Ok(Self {
            schema_version: 1,
            module_digest: module_ref.digest.clone(),
            module_ref,
            invocation_contract_digest: Digest::blake3(&serde_json::to_vec(&invocation_contract)?),
            invocation_contract,
            runtime: RuntimeBinding::WasiCommand,
            action_names: Vec::new(),
            runtime_ref,
            service_contract_ref,
        })
    }

    /// Bind a long-lived host-process backend. The descriptor is sealed into
    /// CAS; the digest the browser must present is the descriptor's, so a
    /// page carrying an old descriptor cannot drive a new contract.
    pub(crate) fn seal_host_process(
        store: &Store,
        program: String,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> Result<Self> {
        let descriptor = HostProcessDescriptor {
            schema_version: 1,
            program: program.clone(),
            argv: argv.clone(),
            env: env.clone(),
            protocol: "jsonl-1".into(),
        };
        let descriptor_bytes = serde_json::to_vec(&descriptor)?;
        ensure!(descriptor_bytes.len() <= MAX_INPUT, "host-process descriptor exceeds 64KiB");
        let module_ref = seal_bytes(store, &descriptor_bytes, "application/json", "host-process-descriptor")?;
        let invocation_contract = Self::current_contract();
        let runtime = RuntimeBinding::HostProcess { program, argv, env };
        let (runtime_ref, service_contract_ref) = Self::seal_runtime_refs(store, &runtime)?;
        Ok(Self {
            schema_version: 1,
            module_digest: module_ref.digest.clone(),
            module_ref,
            invocation_contract_digest: Digest::blake3(&serde_json::to_vec(&invocation_contract)?),
            invocation_contract,
            runtime,
            action_names: Vec::new(),
            runtime_ref,
            service_contract_ref,
        })
    }
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
        Self { version:1, runtime:"shenko-runtime/1".into(), entry:"backend".into(),
            argv:vec!["playground-backend".into(),"<action>".into()],
            stdin:"exact frozen action input JSON".into(), stdout:"one bounded JSON value".into(),
            mounts:BTreeMap::from([("/home".into(),"read_write: isolated artifact state".into()),
                ("/artifact".into(),"read_only: artifact versions".into())]),
            max_stream_bytes:MAX_OUTPUT, max_state_bytes:MAX_STATE_BYTES, max_state_entries:MAX_STATE_ENTRIES }
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
    /// CAS-sealed JSON of the immutable ArtifactVersion this action ran
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
    #[error("ACTION_INVALID: action is not declared by this backend's schema")] UndeclaredAction,
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
            Self::UndeclaredAction => (StatusCode::CONFLICT, "ACTION_UNDECLARED"),
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
    ensure!(frozen.schema_version >= 1 && frozen.schema_version <= 3, "unsupported artifact input schema");
    Ok(frozen)
}
fn same_request(store: &Store, task: &TaskRecord, artifact_id: EntityId, request: &BackendActionRequest) -> Result<bool> {
    let frozen = read_frozen(store, task)?;
    Ok(frozen.artifact_id == artifact_id && serde_json::to_value(&frozen.request)? == serde_json::to_value(request)?)
}

impl PlaygroundState {
    fn committed_action(&self, id: EntityId, request: &BackendActionRequest) -> Result<Option<BackendReceipt>, ActionError> {
        let Some(existing) = self.store.get_task(task_id(id, &request.request_id))? else { return Ok(None) };
        if !same_request(&self.store, &existing, id, request)? { return Err(ActionError::Conflict); }
        let mut receipt = self.action_status(id, &request.request_id)?;
        receipt.replayed = true;
        Ok(Some(receipt))
    }

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
        if let Some(receipt) = self.committed_action(id, &request)? { return Ok(receipt); }
        // Every preparatory rejection (including handler withdrawal, missing
        // artifact/binding and stale revisions) resolves a concurrent committed
        // identity before returning accepted:false, not only SQL errors below.
        let candidate = (|| -> Result<_, ActionError> {
        let native = self.native.as_ref().ok_or_else(|| ActionError::Unavailable("native dispatcher not attached".into()))?;
        if !native.has_handler(PLAYGROUND_BACKEND_KIND, 1) {
            return Err(ActionError::Unavailable("WASI artifact handler is not installed".into()));
        }
        let record = self.get(&id).ok_or(ActionError::NotFound)?;
        let binding = record.backend.as_ref().ok_or(ActionError::NoBackend)?;
        if request.backend_digest != binding.module_digest { return Err(ActionError::Stale); }
        // An action must be declared by the backend's published schema. A
        // backend that declares no schema keeps its previous open contract;
        // one that declares a schema refuses everything it did not declare.
        if !binding.action_names.is_empty() && !binding.action_names.iter().any(|name| name == &request.action) {
            return Err(ActionError::UndeclaredAction);
        }
        // The immutable version record must be CAS-sealed at admission: a
        // verified receipt must survive workspace loss, so its binding,
        // content ref and revision are required closure, not optional metadata.
        let version_record_ref = seal_json(&self.store, &record, "artifact-version-record")
            .map_err(|e| ActionError::Unavailable(format!("artifact version closure unavailable: {e:#}")))?;
        // The bound module/descriptor bytes are also required closure: validate
        // they are CAS-resolvable NOW rather than after a false accepted ACK.
        {
            let module = self.store.cas.read_object(&binding.module_digest)
                .map_err(|e| ActionError::Unavailable(format!("bound backend descriptor unavailable: {e:#}")))?;
            if module.len() as u64 != binding.module_ref.bytes || Digest::blake3(&module) != binding.module_digest {
                return Err(ActionError::Unavailable("bound backend digest/length mismatch in CAS".into()));
            }
        }
        let content = self.store.cas.read_object(&record.content_ref.digest)
            .map_err(|e| ActionError::Unavailable(format!("artifact content unavailable: {e:#}")))?;
        if content.len() as u64 != record.content_ref.bytes || Digest::blake3(&content) != record.content_ref.digest {
            return Err(ActionError::Unavailable("artifact content digest/length mismatch in CAS".into()));
        }
        let frozen = FrozenAction { schema_version: 3, artifact_id: id, artifact_revision: record.revision,
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
                "explanation":"A browser action was durably accepted as a native task. Execution/result is a separate receipt; admission is not evidence that the backend ran."}),
        };
        let input_event = shenko_types::store::RawInboxEvent {
            event_id: accepted_event.event_id, agent_id: self.agent_id, source:"playground".into(), kind:"artifact_changed".into(),
            payload_json: serde_json::to_string(&accepted_event).map_err(anyhow::Error::from)?,
            received_at_ms: accepted_event.created_at.timestamp_millis(),
        };
        Ok((task, input_event))
        })();
        let (task, input_event) = match candidate {
            Ok(candidate) => candidate,
            Err(error) => return match self.committed_action(id, &request)? {
                Some(receipt) => Ok(receipt), None => Err(error),
            },
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
                if let Some(receipt) = self.committed_action(id, &request)? { return Ok(receipt); }
                if matches!(error.downcast_ref::<shenko_types::error::ShenkoError>(),
                    Some(shenko_types::error::ShenkoError::Conflict(_)))
                {
                    return Err(ActionError::Conflict);
                }
                return Err(error.into());
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

/// One live host-process service instance, owned by the controller.
struct RunningService {
    instance_id: EntityId,
    artifact_id: EntityId,
    artifact_revision: u64,
    manifest_digest: Digest,
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::sync::Mutex<BufReader<std::process::ChildStdout>>,
}

/// The launcher/owner of long-lived artifact backend services.
///
/// Every instance is a real selected root plus a durable runtime-instance
/// record. At boot, rows that claim `running` are re-verified against the
/// live process table before they are trusted; a stale row is reported, not
/// silently reused. A generation hand-off stops the services through the
/// update seam and can hand them back on an abandoned activation.
pub struct ArtifactServiceController {
    store: Arc<Store>,
    playground_root: PathBuf,
    services: RwLock<BTreeMap<EntityId, RunningService>>,
    quiesced: RwLock<Vec<EntityId>>,
    admission_closed: std::sync::atomic::AtomicBool,
}

impl ArtifactServiceController {
    pub fn new(store: Arc<Store>, playground_root: PathBuf) -> Self {
        Self {
            store,
            playground_root,
            services: RwLock::new(BTreeMap::new()),
            quiesced: RwLock::new(Vec::new()),
            admission_closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn descriptor_dir(&self, artifact_id: EntityId) -> PathBuf {
        self.playground_root.join(artifact_id.to_string())
    }

    /// Reconcile durable `running` rows with the live process table. A row
    /// whose pid/start-token/exe no longer checks out is marked `unknown`,
    /// never silently reused. Called once at boot before any launch.
    pub fn recover_on_boot(&self) -> Result<()> {
        let records = self.store.list_artifact_records()?;
        for record in records {
            for instance in self.store.runtime_instances(record.artifact_id)? {
                if instance.state != "running" { continue; }
                let alive = instance.process_identity.as_ref().is_some_and(|identity| process_alive(identity));
                if !alive {
                    let mut stopped = instance.clone();
                    stopped.state = "unknown".into();
                    self.store.put_runtime_instance(&stopped)?;
                }
            }
        }
        Ok(())
    }

    /// Launch or adopt the service instance for one frozen action, then run
    /// one request/response exchange against it. The descriptor is re-read
    /// from CAS and verified; the frozen input is identity, not parameters.
    fn invoke(&self, input: &FrozenAction, request_input: &Value) -> Result<Value> {
        let RuntimeBinding::HostProcess { program, argv, env } = &input.binding.runtime else {
            anyhow::bail!("host-process runtime expected");
        };
        // Re-read the descriptor from CAS: the frozen input names it, the
        // verified bytes drive the launch.
        let descriptor_bytes = self.store.cas.read_object(&input.binding.module_digest)?;
        ensure!(descriptor_bytes.len() as u64 == input.binding.module_ref.bytes
            && Digest::blake3(&descriptor_bytes) == input.binding.module_digest,
            "host-process descriptor CAS mismatch");
        let descriptor: HostProcessDescriptor = serde_json::from_slice(&descriptor_bytes)?;
        ensure!(descriptor.program == *program && descriptor.argv == *argv && descriptor.env == *env,
            "frozen host-process descriptor diverged from CAS");
        let instance_id = stable_id(&format!("artifact-service/v1:{}:{}", input.artifact_id, input.binding.module_digest));
        let mut services = self.services.write();
        ensure!(!self.admission_closed.load(std::sync::atomic::Ordering::Acquire),
            "artifact service admission closed for handoff");
        if !services.contains_key(&instance_id) {
            let launched = launch_service(&self.store, &self.descriptor_dir(input.artifact_id),
                instance_id, input.artifact_id, input.artifact_revision, &input.binding.module_digest, &descriptor)?;
            services.insert(instance_id, launched);
        }
        let service = services.get_mut(&instance_id).expect("service just ensured");
        let request_line = serde_json::to_vec(&json!({
            "action": input.request.action, "input": request_input,
            "request_id": input.request.request_id, "artifact_id": input.artifact_id,
        }))?;
        ensure!(request_line.len() <= MAX_INPUT, "service request exceeds 64KiB");
        service.stdin.write_all(&request_line)?;
        service.stdin.write_all(b"\n")?;
        service.stdin.flush()?;
        let mut line = Vec::new();
        let read = match service.stdout.lock() {
            Ok(mut reader) => reader.read_until(b'\n', &mut line),
            Err(poisoned) => poisoned.into_inner().read_until(b'\n', &mut line),
        };
        match read {
            Ok(0) => anyhow::bail!("artifact service closed its stdout without a response"),
            Ok(_) => {
                let line = line.trim_ascii();
                ensure!(!line.is_empty(), "artifact service returned an empty response");
                ensure!(line.len() <= MAX_SERVICE_LINE, "artifact service response exceeds 1MiB");
                Ok(serde_json::from_slice(line)?)
            }
            Err(error) => anyhow::bail!("artifact service response read failed: {error}"),
        }
    }
}

fn launch_service(
    store: &Store,
    artifact_dir: &Path,
    instance_id: EntityId,
    artifact_id: EntityId,
    artifact_revision: u64,
    manifest_digest: &Digest,
    descriptor: &HostProcessDescriptor,
) -> Result<RunningService> {
    let state_dir = artifact_dir.join("state");
    fs::create_dir_all(&state_dir)?;
    let root = artifact_dir.canonicalize()?;
    let mut command = std::process::Command::new(&descriptor.program);
    command.args(&descriptor.argv)
        .envs(&descriptor.env)
        .current_dir(&root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("host-process service requires Linux process-group observation for owned teardown");
    let mut child = command.spawn()
        .with_context(|| format!("launch artifact service {}", descriptor.program))?;
    let pid = child.id();
    let identity = process_identity(pid);
    ensure!(identity.is_some(), "artifact service exited before its identity could be read");
    let identity = identity.expect("checked above");
    let stdin = child.stdin.take().context("artifact service stdin missing")?;
    let stdout = child.stdout.take().context("artifact service stdout missing")?;
    let record = shenko_store::artifacts::RuntimeInstanceRecord {
        schema_version: 1,
        instance_id,
        artifact_id,
        artifact_revision,
        record_revision: 0,
        manifest_digest: manifest_digest.clone(),
        runtime_ref: seal_json(store, &json!({"runtime": "host-process", "program": descriptor.program}), "artifact-runtime")?,
        service_contract_ref: seal_json(store, &json!({"protocol": descriptor.protocol}), "artifact-service-contract")?,
        selected_root: root.clone(),
        root_identity: directory_identity(&root),
        process_identity: Some(identity),
        state: "running".into(),
        started_at_commit: CommitRef::new(EntityId::nil(), LedgerSeq::ZERO),
        updated_commit: CommitRef::new(EntityId::nil(), LedgerSeq::ZERO),
    };
    store.put_runtime_instance(&record)?;
    Ok(RunningService {
        instance_id, artifact_id, artifact_revision,
        manifest_digest: manifest_digest.clone(),
        child, stdin,
        stdout: std::sync::Mutex::new(BufReader::new(stdout)),
    })
}

/// The identity of a live process: pid, its kernel start token and the digest
/// of its executable image. All three together tell "the same process" from
/// "a new process that reused the pid".
fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit(") ").next()?;
    let start_token = after_comm.split_whitespace().nth(19)?.to_string();
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let bytes = std::fs::read(exe).ok()?;
    Some(ProcessIdentity {
        pid,
        process_start_token: start_token,
        executable_digest: Digest::blake3(&bytes),
    })
}

fn process_alive(identity: &ProcessIdentity) -> bool {
    match process_identity(identity.pid) {
        None => false,
        Some(current) => current.process_start_token == identity.process_start_token
            && current.executable_digest == identity.executable_digest,
    }
}

/// Linux host-service profile owns a dedicated process group. A retained
/// direct Child prevents its PID/group ID from being reused until reaped.
/// Keep that Child and its durable claim on every unproved stop.
#[cfg(target_os = "linux")]
fn stop_owned_service_group(service: &mut RunningService, identity: &ProcessIdentity) -> Result<Vec<ProcessIdentity>> {
    fn group_members(group: u32) -> Result<Vec<(u32, char)>> {
        let mut members = Vec::new();
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else { continue };
            let raw = match fs::read_to_string(entry.path().join("stat")) {
                Ok(raw) => raw,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let fields: Vec<_> = raw.rsplit(") ").next().context("invalid process stat")?.split_whitespace().collect();
            ensure!(fields.len() >= 3, "truncated process stat");
            if fields[2].parse::<u32>()? == group {
                members.push((pid, fields[0].chars().next().context("process state missing")?));
            }
        }
        Ok(members)
    }
    let group = identity.pid;
    let members = group_members(group)?;
    // Old profiles without a dedicated group cannot be relabelled as
    // isolated: retain them as an actionable recovery failure.
    ensure!(members.iter().any(|(pid, _)| *pid == group),
        "owned service lacks its dedicated process group; stop scope unproved");
    let observed = members.iter().filter(|(_, state)| !matches!(*state, 'Z' | 'X'))
        .map(|(pid, _)| process_identity(*pid).context("owned group member identity unavailable before stop"))
        .collect::<Result<Vec<_>>>()?;
    unsafe extern "C" { fn kill(pid: i32, signal: i32) -> i32; }
    let group = i32::try_from(group)?;
    let result = unsafe { kill(-group, 9) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("terminate owned service group");
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let live = group_members(identity.pid)?.into_iter()
            .filter(|(_, state)| !matches!(*state, 'Z' | 'X')).collect::<Vec<_>>();
        if live.is_empty() { break; }
        ensure!(std::time::Instant::now() < deadline,
            "owned service group termination unconfirmed: {live:?}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    service.child.wait().context("reap owned service after group termination")?;
    Ok(observed)
}

#[cfg(not(target_os = "linux"))]
fn stop_owned_service_group(_service: &mut RunningService, _identity: &ProcessIdentity) -> Result<Vec<ProcessIdentity>> {
    anyhow::bail!("owned process-group stop observation unavailable on this platform")
}

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

impl ArtifactServiceController {
    /// Stop admitting new work and release the services bound to the old
    /// generation. Returns the process identities that were actually live
    /// and stopped; the caller's handoff receipt must name exactly these.
    pub fn quiesce_for_handoff(&self) -> Result<Vec<ProcessIdentity>> {
        self.admission_closed.store(true, std::sync::atomic::Ordering::Release);
        let mut stopped: Vec<ProcessIdentity> = Vec::new();
        {
            let mut services = self.services.try_write_for(std::time::Duration::from_secs(5))
                .context("artifact service exchange still active; handoff quiesce unconfirmed")?;
            let keys: Vec<EntityId> = services.keys().cloned().collect();
            for instance_id in keys {
                let service = services.get_mut(&instance_id).context("owned service disappeared")?;
                let mut row = self.store.runtime_instance(instance_id)?
                    .context("owned service runtime record missing")?;
                let identity = row.process_identity.clone().context("owned process identity missing")?;
                ensure!(identity.pid == service.child.id(), "owned service PID disagrees with runtime record");
                // Retain the exact pre-stop identity, never re-query a reaped
                // process and manufacture an empty successful receipt.
                let live_identity = process_identity(identity.pid);
                if let Some(current) = &live_identity {
                    ensure!(current.process_start_token == identity.process_start_token
                        && current.executable_digest == identity.executable_digest,
                        "owned process identity changed; refusing to signal reused PID");
                }
                let stopped_group = stop_owned_service_group(service, &identity)?;
                // Preserve recovery ownership even if the following ledger
                // write fails: abort-resume must know what we stopped.
                self.quiesced.write().push(instance_id);
                row.state = "stopped".into();
                self.store.put_runtime_instance(&row)?;
                services.remove(&instance_id);
                stopped.extend(stopped_group);
            }
        }
        Ok(stopped)
    }

    /// The activation was abandoned before the selector CAS: restart exactly
    /// the instances this quiesce stopped, under the current selected root.
    pub fn resume_after_abandoned_activation(&self) -> Result<()> {
        let stopped_ids = self.quiesced.read().clone();
        for instance_id in stopped_ids {
            let row = self.store.runtime_instance(instance_id)?.context("quiesced runtime record missing")?;
            let descriptor_bytes = self.store.cas.read_object(&row.manifest_digest)?;
            ensure!(Digest::blake3(&descriptor_bytes) == row.manifest_digest, "quiesced descriptor digest mismatch");
            let descriptor: HostProcessDescriptor = serde_json::from_slice(&descriptor_bytes)?;
            // Re-verify the root identity now: a copied slot has a different
            // inode, and the live directory is the authority.
            let artifact_dir = self.descriptor_dir(row.artifact_id);
            let launched = launch_service(&self.store, &artifact_dir, instance_id, row.artifact_id,
                row.artifact_revision, &row.manifest_digest, &descriptor)?;
            self.services.write().insert(instance_id, launched);
            self.quiesced.write().retain(|id| *id != instance_id);
        }
        self.admission_closed.store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

pub struct PlaygroundBackendHandler {
    store: Arc<Store>,
    root: PathBuf,
    services: Option<Arc<ArtifactServiceController>>,
}

impl PlaygroundBackendHandler {
    pub fn new(store: Arc<Store>, root: PathBuf) -> Self {
        Self { store, root, services: None }
    }

    /// Attach the long-lived service controller. Without it, host-process
    /// backends report BACKEND_UNAVAILABLE instead of failing open.
    pub fn with_services(mut self, services: Arc<ArtifactServiceController>) -> Self {
        self.services = Some(services);
        self
    }

    fn artifact_dir(&self, id: EntityId) -> PathBuf { self.root.join(id.to_string()) }

    fn validate_input(&self, task: &TaskRecord, input: &FrozenAction) -> Result<Vec<u8>> {
        ensure!(task.task_id == task_id(input.artifact_id, &input.request.request_id), "backend request/task identity mismatch");
        let version_record_ref = input.version_record_ref.as_ref()
            .ok_or_else(|| anyhow::anyhow!("frozen action missing required sealed version closure"))?;
        ensure!(input.schema_version >= 2 && input.binding.schema_version == 1
            && input.binding.invocation_contract == InvocationContract::current()
            && input.request.backend_digest == input.binding.module_digest
            && input.binding.module_digest == input.binding.module_ref.digest, "unsupported backend binding");
        ensure!(valid_token(&input.request.action) && valid_token(&input.request.request_id), "invalid frozen action token");
        // An action not declared by the bound schema is rejected again at
        // execution: the frozen input was validated at admission, but the
        // schema binding is part of the closure that must survive recovery.
        if !input.binding.action_names.is_empty()
            && !input.binding.action_names.iter().any(|name| name == &input.request.action)
        {
            anyhow::bail!("ACTION_UNDECLARED: action is not declared by this backend's schema");
        }
        // A source/delegation cannot turn an arbitrary CAS object into an
        // executable capability: it must match the artifact's immutable version.
        // The required version record comes from the sealed CAS closure.
        let version_bytes = self.store.cas.read_object(&version_record_ref.digest)?;
        ensure!(version_bytes.len() as u64 == version_record_ref.bytes
            && Digest::blake3(&version_bytes) == version_record_ref.digest,
            "sealed artifact version record mismatch");
        let mut version: ArtifactVersion = serde_json::from_slice(&version_bytes)?;
        ensure!(version.schema_version == ARTIFACT_SCHEMA && version.artifact_id == input.artifact_id && version.revision == input.artifact_revision
            && version.backend.as_ref() == Some(&input.binding), "backend not bound to artifact version");
        // CAS is the authority; workspace paths are relocatable locators.
        // Materialize missing immutable inputs after CAS-only recovery without
        // inventing/selecting a new head or replaying any action.
        let versions = self.artifact_dir(input.artifact_id).join("versions");
        let local_record_path = versions.join(format!("{}.json", input.artifact_revision));
        version.bytes_path = versions.join(input.artifact_revision.to_string());
        match fs::read(&local_record_path) {
            Ok(bytes) => {
                let mut local: ArtifactVersion = serde_json::from_slice(&bytes)?;
                local.bytes_path = version.bytes_path.clone();
                ensure!(serde_json::to_value(&local)? == serde_json::to_value(&version)?,
                    "workspace artifact record diverged from sealed closure");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_immutable(&local_record_path, &version_bytes)?;
            }
            Err(error) => return Err(error.into()),
        }
        let content = self.store.cas.read_object(&version.content_ref.digest)?;
        ensure!(content.len() as u64 == version.content_ref.bytes && Digest::blake3(&content) == version.content_ref.digest,
            "sealed artifact content mismatch");
        write_immutable(&version.bytes_path, &content)?;
        let module = self.store.cas.read_object(&input.binding.module_digest)?;
        ensure!(module.len() as u64 == input.binding.module_ref.bytes && Digest::blake3(&module) == input.binding.module_digest,
            "bound backend descriptor CAS mismatch");
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
            artifact: ArtifactRef { digest: contract_digest.clone(), locator: "builtin:playground-backend/1".into() },
            symbol: "artifact.action".into(),
            schema: SchemaRef { name: "artifact-action-observation".into(), major: 1, digest: contract_digest.clone() },
        };
        let bindings = ResourceBindings {
            provider: "playground.runtime/1".into(), account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(), mount_generation: input.artifact_revision,
            capability_level: match &input.binding.runtime {
                RuntimeBinding::WasiCommand => "wasi-p1-isolated-small-state",
                RuntimeBinding::HostProcess { .. } => "host-process-jsonl-1",
            }.into(), extra: json!({
                "module_ref": input.binding.module_ref, "artifact_id": input.artifact_id,
                "artifact_revision": input.artifact_revision, "state_before": before.object,
                "runtime": input.binding.runtime,
                "mounts": [{"guest":"/home","mode":"read_write"},{"guest":"/artifact","mode":"read_only"}],
                "max_stdout_stderr_bytes": MAX_OUTPUT, "max_state_bytes": MAX_STATE_BYTES, "max_state_entries": MAX_STATE_ENTRIES,
            }),
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let mut operation = OperationRecord {
            record_schema_version: 1, operation_id: invocation.operation_id, record_revision: RecordRevision(0),
            task_id: invocation.task.task_id, logical_key: "artifact-action".into(), operation_name: "playground.backend.action/1".into(),
            input_schema_version: 1, output_schema_version: 1,
            final_input_ref: invocation.task.input_ref.clone(), input_digest: invocation.task.input_ref.digest.clone(),
            contract_ref: OperationContractRef { plugin_id: implementation().plugin_id, artifact_digest: contract_digest.clone(),
                contract_digest, receipt_reader_ref: version.clone() },
            resource_bindings_ref: seal_json(&self.store, &bindings, "artifact-runtime-binding")?, authority_ref: invocation.authority,
            acceptance_predicate_ref: seal_json(&self.store, &"guest/service answered with bounded JSON, isolated state was synced and snapshotted", "artifact-acceptance")?,
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
        // not cancel it, and recovery never spawns another backend for this task.
        // `input` itself moves into the worker; the receipt below binds these
        // facts from copies taken before the move.
        let action = input.request.action.clone();
        let request_id = input.request.request_id.clone();
        let artifact_id = input.artifact_id.to_string();
        let frozen_artifact_id = input.artifact_id;
        let frozen_artifact_revision = input.artifact_revision;
        let frozen_backend_digest = input.binding.module_digest.clone();
        let request_input = input.request.input.clone();
        let run_dir = dir.clone();
        let runtime = input.binding.runtime.clone();
        let services = self.services.clone();
        let run = tokio::task::spawn_blocking(move || {
            match &runtime {
                RuntimeBinding::WasiCommand => {
                    let stdout = BoundedOutputPipe::new(MAX_OUTPUT);
                    let stderr = BoundedOutputPipe::new(MAX_OUTPUT);
                    let execution = (|| -> Result<i32> {
                        let mut runtime = WasmRuntime::with_mounts(vec![
                            MountSpec::new(run_dir.join("state").canonicalize()?, HOME_GUEST_PATH, MountMode::ReadWrite, true, false),
                            MountSpec::new(run_dir.canonicalize()?, "/artifact", MountMode::ReadOnly, true, true),
                        ])?;
                        let mut command = WasiCommand::new(MemoryInputPipe::new(serde_json::to_vec(&request_input)?), stdout.clone(), stderr.clone());
                        command.arguments = vec!["playground-backend".into(), action];
                        command.environment = vec![("SHENKO_ARTIFACT_ID".into(), artifact_id), ("SHENKO_REQUEST_ID".into(), request_id)];
                        match runtime.run_wasi_module(&module, command)? {
                            WasiRunOutcome::Exited(code) => Ok(code),
                            WasiRunOutcome::Cancelled => Err(anyhow!("playground guest run cancelled")),
                            WasiRunOutcome::DeadlineExceeded => Err(anyhow!("playground guest run deadline exceeded")),
                        }
                    })();
                    let conclusive = execution.is_ok();
                    let runtime_error = execution.err().map(|error| format!("{error:#}"));
                    BackendRun {
                        output: None,
                        runtime_error,
                        stdout: stdout.contents(),
                        stderr: stderr.contents(),
                        stdout_overflowed: stdout.did_overflow(),
                        stderr_overflowed: stderr.did_overflow(),
                        stream_failed: stdout.did_fail() || stderr.did_fail(),
                        conclusive,
                        service_response: None,
                    }
                }
                RuntimeBinding::HostProcess { .. } => {
                    let controller = match services {
                        None => BackendRun::unavailable("host-process services are not attached"),
                        Some(controller) => match controller.invoke(&input, &request_input) {
                            Ok(response) => BackendRun {
                                output: None, runtime_error: None,
                                stdout: bytes::Bytes::from(serde_json::to_vec(&response).unwrap_or_default()),
                                stderr: bytes::Bytes::new(),
                                stdout_overflowed: false, stderr_overflowed: false, stream_failed: false,
                                conclusive: true, service_response: Some(response),
                            },
                            Err(error) => BackendRun::unavailable(format!("{error:#}")),
                        },
                    };
                    controller
                }
            }
        }).await;
        let run = match run {
            Ok(run) => run,
            Err(error) => BackendRun::unavailable(format!("backend worker failed: {error}")),
        };
        // These seals are required evidence, not optional logging. Failure
        // propagates with the original T1 retained for unknown reconciliation.
        let stdout_ref = seal_bytes(&self.store, &run.stdout, "application/json", "artifact-stdout")?;
        let stderr_ref = seal_bytes(&self.store, &run.stderr, "application/octet-stream", "artifact-stderr")?;
        let after = snapshot(&self.store, &state_dir);
        let state_error = after.as_ref().err().map(|e| format!("{e:#}"));
        let output: Result<Value, _> = match &run.service_response {
            Some(response) => Ok(response.clone()),
            None => serde_json::from_slice::<Value>(&run.stdout),
        };
        let output_error = output.as_ref().err().map(|e| e.to_string());
        let (change_count, delta) = after.as_ref().map(|after| changes(&before, after)).unwrap_or_default();
        let exit_code = if run.service_response.is_some() { Some(0) } else { None };
        let runtime_error = run.runtime_error;
        // Stream rejection/failure is independently recorded: a guest may
        // ignore the write error, emit a small valid JSON and exit 0. Retained
        // bytes are then a truncated prefix, so the attempt is INCONCLUSIVE —
        // never a verified success from the trap/exit code alone.
        let conclusive = run.conclusive && state_error.is_none()
            && !run.stdout_overflowed && !run.stderr_overflowed && !run.stream_failed;
        let succeeded = conclusive && output_error.is_none()
            && (exit_code == Some(0) || run.service_response.is_some());
        let outcome = conclusive.then_some(if succeeded { OperationOutcome::Succeeded } else { OperationOutcome::Failed });
        let effect = if !conclusive { EffectStatus::Unknown } else if change_count == 0 { EffectStatus::None }
            else if succeeded { EffectStatus::Complete } else { EffectStatus::Partial };
        let normalized = json!({
            "artifact_id": frozen_artifact_id, "artifact_revision": frozen_artifact_revision,
            "request_id": request_id, "action": action, "backend_digest": frozen_backend_digest,
            "exit_code": exit_code, "output": output.ok(), "output_error": output_error,
            "stdout_ref": stdout_ref, "stderr_ref": stderr_ref,
            "state_before": before.object, "state_after": after.as_ref().ok().map(|s| &s.object),
            "change_count": change_count, "changes": delta, "changes_preview_truncated": change_count > 32,
            "stdout_truncated": run.stdout_overflowed, "stderr_truncated": run.stderr_overflowed, "stream_failed": run.stream_failed,
            "runtime_error": runtime_error, "state_error": state_error,
            "explanation": "The backend's isolated business-state files changed only as recorded in the before/after manifests. File ObjectRefs retain exact binary bytes; stdout is the backend's bounded structured output. An unknown outcome is not permission to replay.",
        });
        let normalized_ref = seal_json(&self.store, &normalized, "artifact-action-result")?;
        let raw = seal_json(&self.store, &json!({"input_ref":invocation.task.input_ref, "observation":normalized}), "artifact-action-observation")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1, receipt_id: EntityId::new(), operation_id: Some(operation.operation_id), attempt_id: Some(attempt.attempt_id),
            source_id: "playground.runtime/1".into(), source_instance_id: invocation.task_invocation.node_id.to_string(),
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
                "explanation":"An original backend attempt has inconclusive effects. Inspect its exact evidence/state references; it was not replayed."}),
        };
        self.store.admit_inbox_event(event.event_id, task.agent_id, "artifact_changed", "playground", &serde_json::to_value(event)?)?;
        Ok(())
    }
}

/// The bounded result of one backend execution attempt, shared by both runtimes.
struct BackendRun {
    output: Option<String>,
    runtime_error: Option<String>,
    stdout: bytes::Bytes,
    stderr: bytes::Bytes,
    stdout_overflowed: bool,
    stderr_overflowed: bool,
    stream_failed: bool,
    conclusive: bool,
    service_response: Option<Value>,
}

impl BackendRun {
    fn unavailable(detail: String) -> Self {
        Self {
            output: None,
            runtime_error: Some(detail),
            stdout: bytes::Bytes::new(),
            stderr: bytes::Bytes::new(),
            stdout_overflowed: false,
            stderr_overflowed: false,
            stream_failed: false,
            conclusive: false,
            service_response: None,
        }
    }
}

#[async_trait]
impl NativeTaskHandler for PlaygroundBackendHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor { kind: PLAYGROUND_BACKEND_KIND.into(), definition_version: 1, implementation_ref: implementation(),
            role_revision: "native-child/1".into(), authority_refs: vec!["backend.artifact.isolated-state/1".into()], prompt_section: None }
    }
    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> { self.run(invocation).await }
    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let mut operation = self.store.external_operation(operation_id)?;
        ensure!(operation.task_id == task.task_id && operation.operation_name == "playground.backend.action/1", "original backend operation mismatch");
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state == OperationState::InFlight && attempt.dispatch_stage != AttemptDispatchStage::Ended {
            let frozen = read_frozen(&self.store, task)?;
            let state = snapshot(&self.store, &self.artifact_dir(frozen.artifact_id).join("state"));
            let (state_ref, state_error) = match state {
                Ok(snapshot) => (Some(snapshot.object), None),
                Err(error) => (None, Some(error.to_string())),
            };
            let raw = seal_json(&self.store, &json!({"operation_id":operation_id,"attempt_id":attempt.attempt_id,
                "observation":"controller recovered without a conclusive backend receipt; current state is an observation, not proof of causation or permission to retry",
                "current_state_ref":state_ref,"state_error":state_error}), "artifact-recovery-observation")?;
            let evidence = EvidenceReceipt {
                record_schema_version: 1, receipt_id: EntityId::new(), operation_id: Some(operation_id), attempt_id: Some(attempt.attempt_id),
                source_id: "playground.runtime.recovery/1".into(), source_instance_id: self.store.capabilities.incarnation.to_string(),
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

trait TrimAscii {
    fn trim_ascii(&self) -> &[u8];
}

impl TrimAscii for [u8] {
    fn trim_ascii(&self) -> &[u8] {
        let mut start = 0;
        let mut end = self.len();
        while start < end && self[start].is_ascii_whitespace() { start += 1; }
        while end > start && self[end - 1].is_ascii_whitespace() { end -= 1; }
        &self[start..end]
    }
}
