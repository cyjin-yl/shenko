//! Native task handlers exposing §19 REPL sessions to the subject runtime.
//!
//! Two handler kinds follow the exact `wasi.exec` ledger pattern:
//!
//! * `repl.session` v1 — session lifecycle: `list_kernels`, `create`,
//!   `inspect`, `checkpoint`, `restore`, `fork`, `reset`, `cancel`,
//!   `dispose`, `install_dependencies`.
//! * `repl.cell` v1 — cell execution: `execute` (one journaled cell on the
//!   session's real engine).
//!
//! Every op arms T1 (`arm_external_operation`), runs the blocking session
//! work under `spawn_blocking`, seals frozen input / observation / bounded
//! normalized result into CAS, and settles with `record_external_evidence`.
//! Resolution refusals (`REPL_SESSION_UNAVAILABLE`, `REPL_KERNEL_UNAVAILABLE`,
//! path-admission refusals) are recorded as the task's explicit result, never
//! a silent park. A recovered InFlight operation is settled as E1 unknown —
//! a controller restart is not proof a live interpreter process stopped.

use crate::contract::{ReplProvider, ReplSession};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef,
    evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel},
    ids::*,
    object::*,
    operation::*,
    task::*,
};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_SOURCE_BYTES: usize = 512 * 1024;
const MAX_PREVIEW_BYTES: usize = 16 * 1024;
const MAX_NAME_BYTES: usize = 256;

const SESSION_CONTRACT: &[u8] =
    b"shenko.repl.session/v1: manage a real long-lived REPL session (create/inspect/checkpoint/\
      restore/fork/reset/cancel/dispose/install_dependencies) with a durable R0 journal, an R1 \
      driver state file where the kernel declares one, candidate-environment dependency commits, \
      and no R2 claim anywhere";
const CELL_CONTRACT: &[u8] =
    b"shenko.repl.cell/v1: execute one journaled cell on the session's real engine (live owned \
      interpreter process or one-shot WASI guest), preserve the R0 work record with stream \
      digests and the driver report, and never replay declared-effect cells during recovery";

// ---------------------------------------------------------------------------
// Input schemas (model-facing, deny-unknown-fields).
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionInput {
    op: SessionOp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionOp {
    ListKernels,
    Create,
    Inspect,
    Checkpoint,
    Restore,
    Fork,
    Reset,
    Cancel,
    Dispose,
    InstallDependencies,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInput {
    /// Named adapter target (see `list_kernels`).
    kernel_id: String,
    /// Interpreter artifact path, canonical-contained under `<data>/repl/kernels`.
    module: PathBuf,
    /// Optional CPython stdlib directory (python-wasi only).
    #[serde(default)]
    stdlib: Option<PathBuf>,
    #[serde(default)]
    limits: LimitInput,
    #[serde(default)]
    preview_bytes: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitInput {
    #[serde(default)]
    max_wall_clock_ms: Option<u64>,
    #[serde(default)]
    max_fuel: Option<u64>,
    #[serde(default)]
    max_memory_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionRef {
    session_id: EntityId,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallInput {
    session_id: EntityId,
    /// Wheel files canonical-contained under `<data>/repl/wheels`.
    #[serde(default)]
    wheels: Vec<PathBuf>,
    #[serde(default)]
    packages: Vec<PackageInput>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageInput {
    name: String,
    pinned: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CellInput {
    op: CellOp,
    session_id: EntityId,
    /// Base64 cell source bytes.
    source_b64: String,
    /// Caller-declared replay boundary; default `unknown` (never upgraded).
    #[serde(default)]
    cell_kind: CellKindInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CellKindInput {
    Pure,
    DeclaredEffect,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CellOp {
    Execute,
}

// ---------------------------------------------------------------------------
// Frozen inputs and bounded results (sealed into CAS).
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct SessionFrozenInput {
    op: String,
    kernel_id: Option<String>,
    module: Option<PathBuf>,
    module_digest: Option<String>,
    session_id: Option<EntityId>,
    limits: Option<LimitInput>,
}

#[derive(Debug, Serialize)]
struct CellFrozenInput {
    op: String,
    session_id: EntityId,
    source_len: usize,
    source_digest: String,
    cell_kind: String,
}

#[derive(Debug, Serialize)]
struct SessionResult {
    session_id: Option<EntityId>,
    outcome: Option<String>,
    kernel_id: Option<String>,
    runtime: Option<String>,
    backend: Option<String>,
    session_model: Option<String>,
    recovery_level: Option<String>,
    cells_executed: Option<u64>,
    environment_generation: Option<String>,
    detail: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct CellResultView {
    session_id: EntityId,
    seq: u64,
    cell: EntityId,
    outcome: String,
    exit_code: Option<i32>,
    stdout_b64: Option<String>,
    stdout_bytes: Option<u64>,
    stdout_ref: Option<ObjectRef>,
    stderr_b64: Option<String>,
    stderr_bytes: Option<u64>,
    stderr_ref: Option<ObjectRef>,
    driver_report: Option<DriverReportView>,
    truncated: bool,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct DriverReportView {
    ok: bool,
    error: Option<String>,
    dropped: Vec<String>,
    state_bytes: u64,
    state_verified: bool,
}

#[derive(Debug, Serialize)]
struct ListKernelsResult {
    targets: Vec<KernelTargetView>,
}

#[derive(Debug, Serialize)]
struct KernelTargetView {
    name: String,
    runtime: String,
    abi: String,
    availability: String,
    note: String,
}

// ---------------------------------------------------------------------------
// Session store: durable session directories plus the in-memory live cache.
// ---------------------------------------------------------------------------

/// Owns the durable session namespace under `<data>/repl` and the in-memory
/// cache of live sessions. A session's durable state is its directory; the
/// cache only keeps the owned interpreter process alive between ops. A
/// controller restart loses the cache and loses nothing durable — recovery
/// reopens from disk.
pub struct SessionStore {
    data_dir: PathBuf,
    sessions: parking_lot::Mutex<BTreeMap<EntityId, Arc<std::sync::Mutex<crate::session::WasiReplSession>>>>,
    quiesced: parking_lot::Mutex<Vec<(EntityId, bool)>>,
    upgrading: std::sync::atomic::AtomicBool,
    cancellations: parking_lot::Mutex<BTreeMap<EntityId, Arc<parking_lot::Mutex<Option<shenko_wasm::WasiCancellation>>>>>,
}

#[derive(Debug, Serialize)]
pub struct QuiesceReport {
    pub stopped_sessions: Vec<EntityId>,
    pub idle_sessions: Vec<EntityId>,
}

impl SessionStore {
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        for sub in ["sessions", "kernels", "wheels"] {
            fs::create_dir_all(data_dir.join(sub))?;
        }
        Ok(Self { data_dir, sessions: parking_lot::Mutex::new(BTreeMap::new()),
            quiesced: parking_lot::Mutex::new(Vec::new()), upgrading: std::sync::atomic::AtomicBool::new(false),
            cancellations: Default::default() })
    }

    pub fn quiesce_for_upgrade(&self) -> Result<QuiesceReport> {
        let sessions = self.sessions.lock();
        self.upgrading.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut quiesced = self.quiesced.lock();
        let mut report = QuiesceReport { stopped_sessions: Vec::new(), idle_sessions: Vec::new() };
        for (id, session) in sessions.iter() {
            let mut session = session.try_lock()
                .map_err(|_| anyhow::anyhow!("REPL_SESSION_BUSY: session {id} still owns execution"))?;
            let known = quiesced.iter().any(|(known, _)| known == id);
            if !known { quiesced.push((*id, true)); }
            let stopped = session.quiesce_for_upgrade()?;
            if let Some((_, restore)) = quiesced.iter_mut().find(|(known, _)| known == id) {
                *restore = if known { *restore || stopped } else { stopped };
            }
            if stopped {
                report.stopped_sessions.push(*id);
            } else {
                report.idle_sessions.push(*id);
            }
        }
        Ok(report)
    }

    pub fn resume_after_upgrade_abort(&self) -> Result<()> {
        let sessions = self.sessions.lock();
        let mut quiesced = self.quiesced.lock();
        while let Some((id, stopped)) = quiesced.last().copied() {
            let session = sessions.get(&id).context("quiesced session missing from ownership map")?;
            let mut session = session.lock()
                .map_err(|_| anyhow::anyhow!("REPL_SESSION_POISONED: {id}"))?;
            session.resume_after_upgrade_abort(stopped)?;
            quiesced.pop();
        }
        self.upgrading.store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn sessions_root(&self) -> PathBuf { self.data_dir.join("sessions") }
    fn request_cancel(&self, id: EntityId) -> Result<bool> {
        let slot = self.cancellations.lock().get(&id).cloned();
        let slot = match slot {
            Some(slot) => slot,
            None => {
                self.open(id)?;
                self.cancellations.lock().get(&id).cloned()
                    .context("REPL_SESSION_UNAVAILABLE: session has no cancellation owner")?
            },
        };
        let token = slot.lock();
        if let Some(token) = token.as_ref() { token.cancel(); Ok(true) } else { Ok(false) }
    }
    fn kernels_root(&self) -> PathBuf { self.data_dir.join("kernels") }
    fn wheels_root(&self) -> PathBuf { self.data_dir.join("wheels") }

    /// Canonical-containment admission: the real path must live under the
    /// root; both sides are canonical before comparison, so no pathname
    /// trick can escape it.
    fn admit_under(root: &Path, path: &Path) -> Result<PathBuf> {
        let root = root.canonicalize().context("canonicalize admission root")?;
        let path = path.canonicalize().with_context(||
            format!("REPL_KERNEL_UNAVAILABLE: {} is not readable", path.display()))?;
        if !path.starts_with(&root) {
            anyhow::bail!("REPL_KERNEL_UNAVAILABLE: {} is outside the admitted {}", path.display(), root.display());
        }
        Ok(path)
    }

    pub fn admit_module(&self, path: &Path) -> Result<PathBuf> {
        Self::admit_under(&self.kernels_root(), path)
    }

    pub fn admit_wheel(&self, path: &Path) -> Result<PathBuf> {
        Self::admit_under(&self.wheels_root(), path)
    }

    fn session_dir(&self, session_id: EntityId) -> PathBuf {
        self.sessions_root().join(session_id.to_string())
    }

    /// Cached live session, or reopen the durable directory. Reopen never
    /// replays cells; an interrupted restore is fenced by the journal.
    pub fn open(&self, session_id: EntityId) -> Result<Arc<std::sync::Mutex<crate::session::WasiReplSession>>> {
        let mut sessions = self.sessions.lock();
        if self.upgrading.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("REPL_QUIESCED: upgrade owns admission");
        }
        if let Some(session) = sessions.get(&session_id) {
            return Ok(Arc::clone(session));
        }
        let dir = self.session_dir(session_id);
        if !dir.join(crate::journal::SESSION_MANIFEST).is_file() {
            anyhow::bail!("REPL_SESSION_UNAVAILABLE: no session at {}", dir.display());
        }
        let provider = crate::session::WasiReplProvider::new(self.sessions_root());
        let session = provider.reopen(&dir)?;
        self.cancellations.lock().insert(session_id, session.cancellation_handle());
        let session = Arc::new(std::sync::Mutex::new(session));
        sessions.insert(session_id, Arc::clone(&session));
        Ok(session)
    }

    /// Cache a freshly created session; its id is its directory name.
    pub fn insert(&self, session: crate::session::WasiReplSession) -> Result<EntityId> {
        let mut sessions = self.sessions.lock();
        if self.upgrading.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("REPL_QUIESCED: upgrade owns admission");
        }
        let id = session.session_dir().file_name().and_then(|name| name.to_str())
            .and_then(|name| name.parse().ok())
            .context("REPL_SESSION_INVALID: session directory has no id")?;
        self.cancellations.lock().insert(id, session.cancellation_handle());
        sessions.insert(id, Arc::new(std::sync::Mutex::new(session)));
        Ok(id)
    }

    pub fn remove(&self, session_id: EntityId) {
        self.sessions.lock().remove(&session_id);
        self.cancellations.lock().remove(&session_id);
    }

    pub fn sessions_root_cloned(&self) -> PathBuf { self.sessions_root() }
}

// ---------------------------------------------------------------------------
// Shared handler machinery (mirror of the wasi.exec ledger pattern).
// ---------------------------------------------------------------------------

struct HandlerCore {
    store: Arc<Store>,
    kind: &'static str,
    operation_name: &'static str,
    contract: &'static [u8],
    plugin_id: EntityId,
    role_revision: &'static str,
    authority_ref: &'static str,
}

impl HandlerCore {
    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        match self.store.cas.seal_object(
            &serde_json::to_vec(value)?,
            "application/json",
            "repl-effects",
            schema,
            1,
        ) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn new_operation(
        &self,
        invocation: &NativeTaskInvocation,
        input_ref: ObjectRef,
        resource: ObjectRef,
        acceptance: ObjectRef,
        retry: ObjectRef,
    ) -> (OperationRecord, AttemptRecord) {
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let contract_digest = Digest::blake3(self.contract);
        let operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: "native-handler".into(),
            operation_name: self.operation_name.into(),
            input_schema_version: 1,
            output_schema_version: 1,
            final_input_ref: input_ref.clone(),
            input_digest: input_ref.digest.clone(),
            contract_ref: OperationContractRef {
                plugin_id: self.plugin_id,
                artifact_digest: contract_digest.clone(),
                contract_digest,
                receipt_reader_ref: self.version_ref(),
            },
            resource_bindings_ref: resource,
            authority_ref: invocation.authority.clone(),
            acceptance_predicate_ref: acceptance,
            retry_contract_ref: retry,
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::Unknown,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: zero,
            evidence_through: None,
        };
        let attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: invocation.operation_id,
            attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id,
            worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id,
            epoch: invocation.task_invocation.epoch,
            implementation_ref: self.version_ref(),
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: None,
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        (operation, attempt)
    }

    fn version_ref(&self) -> VersionRef {
        let contract_digest = Digest::blake3(self.contract);
        VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: format!("builtin:{}", self.kind),
            },
            symbol: format!("{}.execute", self.kind),
            schema: SchemaRef {
                name: format!("{}-observation-v1", self.kind.replace('.', "-")),
                major: 1,
                digest: contract_digest,
            },
        }
    }

    fn evidence(
        &self,
        invocation: &NativeTaskInvocation,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        raw: ObjectRef,
        normalized: Option<ObjectRef>,
        decoded: bool,
        outcome: Option<OperationOutcome>,
        effect: EffectStatus,
        event_key: Option<String>,
    ) -> Result<OperationRecord> {
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(invocation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: format!("{}.v1", self.kind),
            source_instance_id: invocation.task_invocation.node_id.to_string(),
            source_event_key: event_key,
            producer_epoch: Some(invocation.task_invocation.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: if decoded { DecodeStatus::Decoded } else { DecodeStatus::Uninterpreted },
            observation_level: if decoded { Some(ObservationLevel::RemoteCompleted) } else { None },
            normalized_ref: normalized,
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            invocation.operation_id, attempt.attempt_id, evidence, outcome, effect,
        )
    }

    /// An armed session/cell op whose outcome the controller never observed
    /// stays E1 unknown: a restart is not proof the interpreter stopped, and
    /// no second attempt is armed during reconcile.
    async fn reconcile_unknown(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id && operation.operation_name == self.operation_name,
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::InFlight {
            let attempt = self.store.external_operation_attempt(operation_id)?;
            if attempt.dispatch_stage != AttemptDispatchStage::Ended {
                let raw = self.seal(
                    &serde_json::json!({
                        "operation_id": operation_id,
                        "attempt_id": attempt.attempt_id,
                        "observation": "controller recovered without a conclusive session result",
                        "process_stop_proven": false,
                    }),
                    "repl-recovery",
                )?;
                let evidence = EvidenceReceipt {
                    record_schema_version: 1,
                    receipt_id: EntityId::new(),
                    operation_id: Some(operation_id),
                    attempt_id: Some(attempt.attempt_id),
                    source_id: format!("{}.v1", self.kind),
                    source_instance_id: self.store.capabilities.incarnation.to_string(),
                    source_event_key: Some(format!("controller-recovery:{}", attempt.attempt_id)),
                    producer_epoch: None,
                    producer_schema_version: Some("1".into()),
                    raw_ref: raw,
                    decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
                    decode_status: DecodeStatus::Decoded,
                    observation_level: None,
                    normalized_ref: None,
                    observed_at: Some(chrono::Utc::now()),
                    received_commit: operation.prepared_commit,
                };
                return self.store.record_external_evidence(
                    operation_id, attempt.attempt_id, evidence, None, EffectStatus::Unknown,
                );
            }
        }
        Ok(operation)
    }

    fn descriptor(&self, prompt_section: String) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: self.kind.into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: self.plugin_id,
                artifact_digest: Digest::blake3(self.contract),
                registry_revision: 0,
            },
            role_revision: self.role_revision.into(),
            authority_refs: vec![self.authority_ref.into()],
            prompt_section: Some(prompt_section),
        }
    }
}

fn outcome_of(outcome: &crate::contract::CellOutcome) -> (String, Option<i32>) {
    match outcome {
        crate::contract::CellOutcome::Completed { exit_code } =>
            ("completed".into(), Some(*exit_code)),
        crate::contract::CellOutcome::Exited { exit_code } =>
            ("exited".into(), Some(*exit_code)),
        crate::contract::CellOutcome::Cancelled => ("cancelled".into(), None),
        crate::contract::CellOutcome::DeadlineExceeded => ("deadline_exceeded".into(), None),
        crate::contract::CellOutcome::Failed { .. } => ("failed".into(), None),
    }
}

fn operation_outcome_of(outcome: &crate::contract::CellOutcome) -> OperationOutcome {
    match outcome {
        crate::contract::CellOutcome::Completed { .. } => OperationOutcome::Succeeded,
        crate::contract::CellOutcome::Cancelled | crate::contract::CellOutcome::DeadlineExceeded =>
            OperationOutcome::Cancelled,
        crate::contract::CellOutcome::Exited { .. } | crate::contract::CellOutcome::Failed { .. } =>
            OperationOutcome::Failed,
    }
}

/// Seal one journaled stream file: full bytes into CAS by streaming, plus a
/// bounded head preview (the only bytes ever resident in memory).
fn seal_stream(store: &Store, session_dir: &Path, relative: &Path) -> Result<(ObjectRef, Vec<u8>, bool)> {
    let mut file = fs::File::open(session_dir.join(relative))?;
    let sealed = match store.cas.seal_reader(
        &mut file, "application/octet-stream", "repl-streams", "repl-stream", 1,
    ) {
        Ok(sealed) => sealed,
        Err(error) => {
            store.close_external_writer();
            return Err(error.into());
        }
    };
    let mut preview = Vec::new();
    file.read_to_end(&mut preview)?;
    let truncated = preview.len() > MAX_PREVIEW_BYTES;
    preview.truncate(MAX_PREVIEW_BYTES);
    Ok((sealed.object, preview, truncated))
}

// ---------------------------------------------------------------------------
// repl.session v1
// ---------------------------------------------------------------------------

pub struct ReplSessionHandler {
    core: HandlerCore,
    sessions: Arc<SessionStore>,
}

type SessionOpOutcome = (
    SessionFrozenInput,
    serde_json::Value,
    bool,
    Option<OperationOutcome>,
    EffectStatus,
    Option<String>,
);

impl ReplSessionHandler {
    pub fn new(store: Arc<Store>, data_dir: PathBuf) -> Result<Self> {
        Ok(Self {
            core: HandlerCore {
                store,
                kind: "repl.session",
                operation_name: "repl.session.v1",
                contract: SESSION_CONTRACT,
                plugin_id: "3b3f2c94-7d1e-4f6a-9c3d-8a2e5f1b6d40".parse().expect("fixed plugin UUID"),
                role_revision: "native-repl-session/1",
                authority_ref: "repl.managed_session.execute/v1",
            },
            sessions: Arc::new(SessionStore::new(data_dir)?),
        })
    }

    /// The shared session namespace, handed to the cell handler so both
    /// kinds operate on the same durable sessions and live cache.
    pub fn sessions(&self) -> Arc<SessionStore> {
        Arc::clone(&self.sessions)
    }

    async fn run(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: SessionInput = serde_json::from_value(invocation.input.clone())
            .map_err(|error| anyhow::anyhow!("REPL_SESSION_INVALID: {error}"))?;
        let sessions = Arc::clone(&self.sessions);
        let payload = invocation.input.clone();
        let task_id = invocation.task_invocation.task_id;
        let worker_id = invocation.task_invocation.worker_id;
        let prepared: Result<SessionOpOutcome> =
            tokio::task::spawn_blocking(move ||
                run_session_op(&sessions, &payload, input, task_id, worker_id)).await?;
        let (frozen, value, decoded, outcome, effect, event_key) = match prepared {
            Ok(parts) => parts,
            Err(error) => {
                let message = format!("{error:#}");
                let refused = message.contains("REPL_SESSION_UNAVAILABLE")
                    || message.contains("REPL_KERNEL_UNAVAILABLE")
                    || message.contains("REPL_KERNEL_INVALID");
                if refused {
                    return self.record_refusal(&invocation, message).await;
                }
                return Err(error);
            }
        };
        let raw = self.core.seal(&serde_json::json!({
            "operation_id": invocation.operation_id,
            "result": value,
        }), "repl-session-observation")?;
        let normalized = self.core.seal(&value, "repl-session-result")?;
        let store = Arc::clone(&self.core.store);
        let (mut operation, mut attempt) = {
            let input_ref = self.core.seal(&frozen, "repl-session-frozen-input")?;
            let bindings = ResourceBindings {
                provider: "repl.session.v1".into(),
                account_identity: String::new(),
                runtime_node: invocation.task_invocation.node_id.to_string(),
                mount_generation: 0,
                capability_level: "managed_session_home".into(),
                extra: serde_json::json!({"session_id": frozen.session_id, "op": frozen.op}),
            };
            let resource = self.core.seal(&bindings, "repl-session-binding")?;
            let acceptance = self.core.seal(
                &"session op completed against the durable session journal; refusals carry an explicit REPL_* reason",
                "repl-session-acceptance",
            )?;
            let retry = self.core.seal(
                &"an armed session op without a conclusive observation remains unknown; no automatic replay",
                "repl-session-retry",
            )?;
            self.core.new_operation(&invocation, input_ref, resource, acceptance, retry)
        };
        if let Some(existing) = store.arm_external_operation(
            &invocation.task_invocation, &mut operation, &mut attempt,
        )? {
            return Ok(existing);
        }
        self.core.evidence(&invocation, &operation, &attempt, raw, Some(normalized),
            decoded, outcome, effect, event_key)
    }

    /// Record an explicit REPL_* resolution refusal as the task's real outcome.
    async fn record_refusal(&self, invocation: &NativeTaskInvocation, detail: String)
        -> Result<OperationRecord>
    {
        let frozen = SessionFrozenInput { op: "refused".into(), kernel_id: None, module: None,
            module_digest: None, session_id: None, limits: None };
        let value = serde_json::to_value(SessionResult {
            session_id: None, outcome: Some("refused".into()), kernel_id: None, runtime: None,
            backend: None, session_model: None, recovery_level: None, cells_executed: None,
            environment_generation: None, detail: None, error: Some(detail.clone()),
        })?;
        let store = Arc::clone(&self.core.store);
        let (mut operation, mut attempt) = {
            let input_ref = self.core.seal(&frozen, "repl-session-frozen-input")?;
            let bindings = ResourceBindings {
                provider: "repl.session.v1".into(),
                account_identity: String::new(),
                runtime_node: invocation.task_invocation.node_id.to_string(),
                mount_generation: 0,
                capability_level: "resolution".into(),
                extra: serde_json::json!({"error": detail}),
            };
            let resource = self.core.seal(&bindings, "repl-session-binding")?;
            let acceptance = self.core.seal(&"explicit resolution refusal", "repl-session-acceptance")?;
            let retry = self.core.seal(&"no execution attempted", "repl-session-retry")?;
            self.core.new_operation(invocation, input_ref, resource, acceptance, retry)
        };
        if let Some(existing) = store.arm_external_operation(
            &invocation.task_invocation, &mut operation, &mut attempt,
        )? {
            return Ok(existing);
        }
        let raw = self.core.seal(&serde_json::json!({
            "operation_id": invocation.operation_id,
            "error": detail,
            "executed": false,
        }), "repl-session-observation")?;
        let normalized = self.core.seal(&value, "repl-session-result")?;
        self.core.evidence(invocation, &operation, &attempt, raw, Some(normalized),
            true, Some(OperationOutcome::Failed), EffectStatus::None, None)
    }
}

/// Execute one session op with the session mutex held. Blocking by design
/// (real process I/O); the caller wraps this in `spawn_blocking`.
fn run_session_op(
    sessions: &Arc<SessionStore>,
    payload: &serde_json::Value,
    input: SessionInput,
    task_id: EntityId,
    worker_id: EntityId,
) -> Result<SessionOpOutcome> {
    match input.op {
        SessionOp::ListKernels => {
            let targets = crate::kernel::named_targets().iter().map(|target| {
                let availability = match &target.availability {
                    crate::kernel::KernelAvailability::Bindable { via } =>
                        format!("bindable:{}", serde_json::to_string(via).unwrap_or_default()),
                    crate::kernel::KernelAvailability::Unavailable { reason } =>
                        format!("unavailable:{reason}"),
                };
                KernelTargetView {
                    name: target.name.clone(),
                    runtime: format!("{:?}", target.runtime),
                    abi: target.abi.clone(),
                    availability,
                    note: target.note.clone(),
                }
            }).collect();
            let value = serde_json::to_value(ListKernelsResult { targets })?;
            Ok((SessionFrozenInput { op: "list_kernels".into(), kernel_id: None, module: None,
                module_digest: None, session_id: None, limits: None }, value, true,
                Some(OperationOutcome::Succeeded), EffectStatus::None, None))
        }
        SessionOp::Create => {
            let create: CreateInput = serde_json::from_value(payload.clone())
                .map_err(|error| anyhow::anyhow!("REPL_KERNEL_INVALID: create payload: {error}"))?;
            let (profile, module_digest) = build_kernel(sessions, &create)?;
            let provider = crate::session::WasiReplProvider::new(sessions.sessions_root_cloned());
            let session = provider.create(crate::contract::CreateSession {
                kernel: profile.clone(),
                bind: crate::contract::TaskBinding {
                    task: Some(task_id),
                    worker: Some(worker_id),
                    scope: None,
                },
                limits: crate::contract::ResourceLimits {
                    max_wall_clock_ms: create.limits.max_wall_clock_ms,
                    max_fuel: create.limits.max_fuel,
                    max_memory_bytes: create.limits.max_memory_bytes,
                    enforced: false,
                },
                preview_bytes: create.preview_bytes,
            })?;
            let session_id: EntityId = session.session_dir().file_name().and_then(|n| n.to_str())
                .and_then(|n| n.parse().ok())
                .context("REPL_SESSION_INVALID: session directory has no id")?;
            sessions.insert(session)?;
            let value = serde_json::to_value(SessionResult {
                session_id: Some(session_id),
                outcome: Some("created".into()),
                kernel_id: Some(profile.kernel_id.clone()),
                runtime: Some(format!("{:?}", profile.runtime)),
                backend: Some(format!("{:?}", profile.backend)),
                session_model: Some(format!("{:?}", profile.session_model())),
                recovery_level: Some(format!("{:?}", profile.recovery_level())),
                cells_executed: Some(0),
                environment_generation: Some(crate::kernel::environment_generation(&profile).0),
                detail: None,
                error: None,
            })?;
            Ok((SessionFrozenInput { op: "create".into(), kernel_id: Some(profile.kernel_id.clone()),
                module: Some(match &profile.module {
                    crate::kernel::ModuleSource::ModulePath(path) => path.clone(),
                }), module_digest: Some(module_digest), session_id: Some(session_id),
                limits: Some(create.limits) }, value, true, Some(OperationOutcome::Succeeded),
                EffectStatus::Unknown, None))
        }
        SessionOp::Cancel => {
            let reference: SessionRef = serde_json::from_value(payload.clone())
                .map_err(|error| anyhow::anyhow!("REPL_SESSION_INVALID: {error}"))?;
            let session_id = reference.session_id;
            let requested = sessions.request_cancel(session_id)?;
            let frozen = SessionFrozenInput { op: "cancel".into(), kernel_id: None,
                module: None, module_digest: None, session_id: Some(session_id), limits: None };
            let value = serde_json::to_value(SessionResult {
                session_id: Some(session_id),
                outcome: Some(if requested { "interrupt_requested" } else { "nothing_running" }.into()),
                kernel_id: None, runtime: None, backend: None, session_model: None,
                recovery_level: None, cells_executed: None, environment_generation: None,
                detail: None, error: None,
            })?;
            Ok((frozen, value, true, Some(OperationOutcome::Succeeded), EffectStatus::Unknown, None))
        }
        SessionOp::Inspect | SessionOp::Checkpoint | SessionOp::Restore | SessionOp::Fork
        | SessionOp::Reset | SessionOp::Dispose | SessionOp::InstallDependencies => {
            let reference: SessionRef = serde_json::from_value(payload.clone())
                .map_err(|error| anyhow::anyhow!("REPL_SESSION_INVALID: {error}"))?;
            let session_id = reference.session_id;
            let handle = sessions.open(session_id)?;
            let frozen = SessionFrozenInput { op: format!("{:?}", input.op).to_lowercase(),
                kernel_id: None, module: None, module_digest: None, session_id: Some(session_id),
                limits: None };
            let mut session = handle.lock()
                .map_err(|_| anyhow::anyhow!("REPL_SESSION_INVALID: session lock poisoned"))?;
            match input.op {
                SessionOp::Inspect => {
                    let info = session.inspect()?;
                    let value = serde_json::to_value(SessionResult {
                        session_id: Some(session_id),
                        outcome: Some(format!("{:?}", info.state)),
                        kernel_id: Some(info.kernel_id.clone()),
                        runtime: Some(format!("{:?}", info.runtime)),
                        backend: Some(format!("{:?}", info.backend)),
                        session_model: Some(format!("{:?}", info.session_model)),
                        recovery_level: Some(format!("{:?}", info.recovery_level)),
                        cells_executed: Some(info.cells_executed),
                        environment_generation: Some(info.environment_generation.0.clone()),
                        detail: Some(format!("interpreter_state_persists={} live_process_running={} enforcement={:?} driver={:?}",
                            info.interpreter_state_persists, info.live_process_running,
                            info.enforcement, info.driver)),
                        error: None,
                    })?;
                    Ok((frozen, value, true, Some(OperationOutcome::Succeeded), EffectStatus::Unknown, None))
                }
                SessionOp::Checkpoint => {
                    let checkpoint = session.checkpoint()?;
                    let value = serde_json::to_value(SessionResult {
                        session_id: Some(session_id),
                        outcome: Some(format!("{:?}", checkpoint.level)),
                        kernel_id: None, runtime: None, backend: None, session_model: None,
                        recovery_level: None,
                        cells_executed: Some(checkpoint.cells),
                        environment_generation: None,
                        detail: Some(format!("journal_blake3={} state_files={}",
                            checkpoint.journal_blake3, checkpoint.state_files.len())),
                        error: None,
                    })?;
                    Ok((frozen, value, true, Some(OperationOutcome::Succeeded), EffectStatus::Unknown, None))
                }
                SessionOp::Restore => {
                    let report = session.restore()?;
                    let ready = !matches!(session.inspect()?.state, crate::contract::SessionState::RecoveryRequired);
                    let replayed = report.entries.iter()
                        .filter(|entry| matches!(entry.recovery,
                            crate::contract::CellRecovery::Replayed { .. })).count();
                    let value = serde_json::to_value(SessionResult {
                        session_id: Some(session_id),
                        outcome: Some(if ready { "restored" } else { "recovery_required" }.into()),
                        kernel_id: None, runtime: None, backend: None, session_model: None,
                        recovery_level: None,
                        cells_executed: Some(replayed as u64),
                        environment_generation: None,
                        detail: Some(serde_json::to_string(&report)?),
                        error: (!ready).then(|| "REPL_RECOVERY_REQUIRED: final live state has no verified R1 checkpoint".into()),
                    })?;
                    Ok((frozen, value, true, Some(if ready { OperationOutcome::Succeeded } else { OperationOutcome::Failed }), EffectStatus::Unknown, None))
                }
                SessionOp::Fork => {
                    let forked = session.fork(&sessions.sessions_root_cloned())?;
                    let forked_id = forked.session_dir().file_name().and_then(|n| n.to_str())
                        .and_then(|n| n.parse().ok())
                        .context("REPL_SESSION_INVALID: fork directory has no id")?;
                    let info = forked.inspect()?;
                    // Cache the forked session so its live process persists.
                    let handle = sessions.open(forked_id)?;
                    drop(handle.lock().map_err(|_| anyhow::anyhow!("REPL_SESSION_INVALID: session lock poisoned"))?);
                    let value = serde_json::to_value(SessionResult {
                        session_id: Some(forked_id),
                        outcome: Some("forked".into()),
                        kernel_id: Some(info.kernel_id.clone()),
                        runtime: Some(format!("{:?}", info.runtime)),
                        backend: Some(format!("{:?}", info.backend)),
                        session_model: Some(format!("{:?}", info.session_model)),
                        recovery_level: Some(format!("{:?}", info.recovery_level)),
                        cells_executed: Some(info.cells_executed),
                        environment_generation: Some(info.environment_generation.0.clone()),
                        detail: None,
                        error: None,
                    })?;
                    Ok((frozen, value, true, Some(OperationOutcome::Succeeded), EffectStatus::Unknown, None))
                }
                SessionOp::Reset => {
                    session.reset()?;
                    let value = serde_json::to_value(SessionResult {
                        session_id: Some(session_id),
                        outcome: Some("reset".into()),
                        kernel_id: None, runtime: None, backend: None, session_model: None,
                        recovery_level: None, cells_executed: None, environment_generation: None,
                        detail: None, error: None,
                    })?;
                    Ok((frozen, value, true, Some(OperationOutcome::Succeeded), EffectStatus::Unknown, None))
                }
                SessionOp::Dispose => {
                    session.dispose()?;
                    drop(session);
                    sessions.remove(session_id);
                    let value = serde_json::to_value(SessionResult {
                        session_id: Some(session_id),
                        outcome: Some("disposed".into()),
                        kernel_id: None, runtime: None, backend: None, session_model: None,
                        recovery_level: None, cells_executed: None, environment_generation: None,
                        detail: None, error: None,
                    })?;
                    Ok((frozen, value, true, Some(OperationOutcome::Succeeded), EffectStatus::Unknown, None))
                }
                SessionOp::InstallDependencies => {
                    let install: InstallInput = serde_json::from_value(payload.clone())
                        .map_err(|error| anyhow::anyhow!("REPL_INSTALL_INVALID: {error}"))?;
                    let mut wheels = Vec::new();
                    for wheel in &install.wheels {
                        wheels.push(sessions.admit_wheel(wheel)?);
                    }
                    let packages = install.packages.iter().map(|package| {
                        crate::kernel::PackageSpec { name: package.name.clone(), pinned: package.pinned.clone() }
                    }).collect::<Vec<_>>();
                    let outcome = session.install_dependencies(crate::contract::InstallRequest { wheels, packages })?;
                    let (outcome_str, error) = match &outcome {
                        crate::contract::InstallOutcome::Installed { installed, environment_generation, .. } =>
                            (format!("installed:{}:{}", installed.len(), environment_generation.0), None),
                        crate::contract::InstallOutcome::Refused { reason } =>
                            ("refused".into(), Some(reason.clone())),
                        crate::contract::InstallOutcome::Unsupported { reason } =>
                            ("unsupported".into(), Some(reason.clone())),
                    };
                    let value = serde_json::to_value(SessionResult {
                        session_id: Some(session_id),
                        outcome: Some(outcome_str),
                        kernel_id: None, runtime: None, backend: None, session_model: None,
                        recovery_level: None, cells_executed: None, environment_generation: None,
                        detail: None, error,
                    })?;
                    let operation_outcome = match outcome {
                        crate::contract::InstallOutcome::Installed { .. } => OperationOutcome::Succeeded,
                        _ => OperationOutcome::Failed,
                    };
                    Ok((frozen, value, true, Some(operation_outcome), EffectStatus::Unknown, None))
                }
                SessionOp::ListKernels | SessionOp::Create | SessionOp::Cancel => unreachable!("routed above"),
            }
        }
    }
}

/// Kernel construction for `create`, shared by the blocking op runner.
fn build_kernel(sessions: &SessionStore, create: &CreateInput)
    -> Result<(crate::kernel::KernelProfile, String)>
{
    ensure!(!create.kernel_id.is_empty() && create.kernel_id.len() <= MAX_NAME_BYTES,
        "REPL_KERNEL_INVALID: kernel_id");
    let module = sessions.admit_module(&create.module)?;
    let digest = crate::journal::hash_bytes(&fs::read(&module)?);
    let stdlib = match &create.stdlib {
        Some(path) => Some(sessions.admit_module(path)?),
        None => None,
    };
    let profile = match create.kernel_id.as_str() {
        "python-wasi" => crate::adapters::python_wasi(create.kernel_id.clone(), module, stdlib),
        "quickjs-wasm" => {
            if stdlib.is_some() {
                anyhow::bail!("REPL_KERNEL_INVALID: stdlib is a python-wasi-only field");
            }
            crate::adapters::quickjs_wasi(create.kernel_id.clone(), module)
        }
        "python-native-host" | "node-native-host" | "bun-native-host" => {
            if stdlib.is_some() {
                anyhow::bail!("REPL_KERNEL_INVALID: stdlib is a python-wasi-only field");
            }
            match create.kernel_id.as_str() {
                "python-native-host" =>
                    crate::adapters::python_native(create.kernel_id.clone(), module),
                "node-native-host" =>
                    crate::adapters::node_native(create.kernel_id.clone(), module),
                _ => crate::adapters::bun_native(create.kernel_id.clone(), module),
            }
        }
        other => anyhow::bail!(
            "REPL_KERNEL_UNAVAILABLE: named target {other:?} is not bound in this installation"),
    };
    crate::kernel::validate_profile(&profile)?;
    Ok((profile, digest))
}

#[async_trait]
impl NativeTaskHandler for ReplSessionHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        self.core.descriptor(
            "repl.session/v1 管理真实长生命周期 REPL 会话（WASI 或原生解释器进程，变量在进程存活期间保留）：\
             [delegate:repl.session:1] {\"op\":\"create\",\"kernel_id\":\"python-wasi|quickjs-wasm|python-native-host|node-native-host|bun-native-host\",\
             \"module\":\"<data>/repl/kernels 下的解释器路径\",\"limits\":{\"max_wall_clock_ms\":60000}}；\
             其余 op（inspect/checkpoint/restore/fork/reset/cancel/dispose/install_dependencies）带 {\"session_id\":\"...\"}。\
             restore 通过重放 Pure cell 重建状态，DeclaredEffect 只保留凭据不重放；install_dependencies 提交候选环境并推进 generation；\
             会话进程中断后会话进入 recovery-required。无 R2。"
                .into(),
        )
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        self.core.reconcile_unknown(task, operation_id).await
    }
}

// ---------------------------------------------------------------------------
// repl.cell v1
// ---------------------------------------------------------------------------

pub struct ReplCellHandler {
    core: HandlerCore,
    sessions: Arc<SessionStore>,
}

impl ReplCellHandler {
    pub fn new(store: Arc<Store>, sessions: Arc<SessionStore>) -> Self {
        Self {
            core: HandlerCore {
                store,
                kind: "repl.cell",
                operation_name: "repl.cell.v1",
                contract: CELL_CONTRACT,
                plugin_id: "9a1d5e20-3b8c-4e7f-2a6d-1c4e9b0f3a58".parse().expect("fixed plugin UUID"),
                role_revision: "native-repl-cell/1",
                authority_ref: "repl.managed_session.execute_cell/v1",
            },
            sessions,
        }
    }

    fn prepare(&self, invocation: &NativeTaskInvocation)
        -> Result<(CellFrozenInput, Vec<u8>, CellKindInput)>
    {
        let parsed = (|| -> Result<(CellInput, Vec<u8>)> {
            let input: CellInput = serde_json::from_value(invocation.input.clone())?;
            ensure!(input.op == CellOp::Execute, "REPL_CELL_INVALID: unsupported cell op");
            let source = B64.decode(&input.source_b64)
                .map_err(|error| anyhow::anyhow!("source_b64: {error}"))?;
            ensure!(source.len() <= MAX_SOURCE_BYTES, "REPL_CELL_INVALID: cell source exceeds {MAX_SOURCE_BYTES} bytes");
            ensure!(!source.contains(&0), "REPL_CELL_INVALID: NUL in cell source");
            Ok((input, source))
        })();
        let (input, source) = parsed?;
        let frozen = CellFrozenInput {
            op: "execute".into(),
            session_id: input.session_id,
            source_len: source.len(),
            source_digest: crate::journal::hash_bytes(&source),
            cell_kind: format!("{:?}", input.cell_kind).to_lowercase(),
        };
        Ok((frozen, source, input.cell_kind))
    }

    async fn run(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let (frozen, source, cell_kind) = self.prepare(&invocation)?;
        let cell_kind_contract = match cell_kind {
            CellKindInput::Pure => crate::contract::CellKind::Pure,
            CellKindInput::DeclaredEffect => crate::contract::CellKind::DeclaredEffect,
            CellKindInput::Unknown => crate::contract::CellKind::Unknown,
        };
        let sessions = Arc::clone(&self.sessions);
        let store = Arc::clone(&self.core.store);
        let session_id = frozen.session_id;
        let task_label = invocation.task_invocation.task_id;
        let prepared: Result<(
            crate::contract::CellResult,
            crate::contract::CellOutcome,
            (ObjectRef, Vec<u8>, bool),
            (ObjectRef, Vec<u8>, bool),
        )> = tokio::task::spawn_blocking(move || {
            let handle = sessions.open(session_id)?;
            let mut session = handle.lock()
                .map_err(|_| anyhow::anyhow!("REPL_SESSION_INVALID: session lock poisoned"))?;
            let result = session.execute(crate::contract::CellRequest {
                source,
                cell_kind: cell_kind_contract,
                labels: BTreeMap::from([("task".into(), task_label.to_string())]),
            })?;
            let dir = session.session_dir().to_path_buf();
            let outcome = result.record.outcome.clone();
            let stdout = seal_stream(&store, &dir, &result.record.stdout_path)?;
            let stderr = seal_stream(&store, &dir, &result.record.stderr_path)?;
            Ok((result, outcome, stdout, stderr))
        }).await?;

        let (cell, outcome, stdout_evidence, stderr_evidence) = match prepared {
            Ok(parts) => parts,
            Err(error) => {
                let message = format!("{error:#}");
                if message.contains("REPL_SESSION_UNAVAILABLE") || message.contains("REPL_RECOVERY_REQUIRED") {
                    return self.record_refusal(&invocation, message).await;
                }
                return Err(error);
            }
        };

        let (outcome_str, exit_code) = outcome_of(&outcome);
        let truncated = stdout_evidence.2 || stderr_evidence.2;
        let result_view = CellResultView {
            session_id,
            seq: cell.record.seq,
            cell: cell.record.cell,
            outcome: outcome_str,
            exit_code,
            stdout_b64: Some(B64.encode(&stdout_evidence.1)),
            stdout_bytes: Some(cell.record.stdout_bytes),
            stdout_ref: Some(stdout_evidence.0),
            stderr_b64: Some(B64.encode(&stderr_evidence.1)),
            stderr_bytes: Some(cell.record.stderr_bytes),
            stderr_ref: Some(stderr_evidence.0),
            driver_report: cell.record.driver_report.as_ref().map(|report| DriverReportView {
                ok: report.ok,
                error: report.error.clone(),
                dropped: report.dropped.clone(),
                state_bytes: report.state_bytes,
                state_verified: report.state_verified,
            }),
            truncated,
            error: match &outcome {
                crate::contract::CellOutcome::Failed { error } => Some(error.clone()),
                _ => None,
            },
        };

        let store = Arc::clone(&self.core.store);
        let (mut operation, mut attempt) = {
            let input_ref = self.core.seal(&frozen, "repl-cell-frozen-input")?;
            let bindings = ResourceBindings {
                provider: "repl.cell.v1".into(),
                account_identity: String::new(),
                runtime_node: invocation.task_invocation.node_id.to_string(),
                mount_generation: 0,
                capability_level: "managed_session_cell".into(),
                extra: serde_json::json!({"session_id": session_id, "seq": cell.record.seq}),
            };
            let resource = self.core.seal(&bindings, "repl-cell-binding")?;
            let acceptance = self.core.seal(
                &"cell completed under the session journal; stream digests and the driver report bind the outcome",
                "repl-cell-acceptance",
            )?;
            let retry = self.core.seal(
                &"an armed cell without a conclusive observation remains unknown; no automatic replay",
                "repl-cell-retry",
            )?;
            self.core.new_operation(&invocation, input_ref, resource, acceptance, retry)
        };
        if let Some(existing) = store.arm_external_operation(
            &invocation.task_invocation, &mut operation, &mut attempt,
        )? {
            return Ok(existing);
        }
        let raw = self.core.seal(&serde_json::json!({
            "operation_id": invocation.operation_id,
            "session_id": session_id,
            "seq": cell.record.seq,
            "outcome": result_view.outcome,
        }), "repl-cell-observation")?;
        let normalized = self.core.seal(&result_view, "repl-cell-result")?;
        self.core.evidence(&invocation, &operation, &attempt, raw, Some(normalized),
            true, Some(operation_outcome_of(&outcome)), EffectStatus::Unknown, None)
    }

    async fn record_refusal(&self, invocation: &NativeTaskInvocation, detail: String)
        -> Result<OperationRecord>
    {
        let frozen = CellFrozenInput { op: "refused".into(), session_id: EntityId::nil(),
            source_len: 0, source_digest: crate::journal::hash_bytes(b""), cell_kind: "unknown".into() };
        let value = CellResultView {
            session_id: EntityId::nil(), seq: 0, cell: EntityId::nil(),
            outcome: "refused".into(), exit_code: None,
            stdout_b64: None, stdout_bytes: None, stdout_ref: None,
            stderr_b64: None, stderr_bytes: None, stderr_ref: None,
            driver_report: None, truncated: false, error: Some(detail.clone()),
        };
        let store = Arc::clone(&self.core.store);
        let (mut operation, mut attempt) = {
            let input_ref = self.core.seal(&frozen, "repl-cell-frozen-input")?;
            let bindings = ResourceBindings {
                provider: "repl.cell.v1".into(),
                account_identity: String::new(),
                runtime_node: invocation.task_invocation.node_id.to_string(),
                mount_generation: 0,
                capability_level: "resolution".into(),
                extra: serde_json::json!({"error": detail}),
            };
            let resource = self.core.seal(&bindings, "repl-cell-binding")?;
            let acceptance = self.core.seal(&"explicit resolution refusal", "repl-cell-acceptance")?;
            let retry = self.core.seal(&"no cell executed", "repl-cell-retry")?;
            self.core.new_operation(invocation, input_ref, resource, acceptance, retry)
        };
        if let Some(existing) = store.arm_external_operation(
            &invocation.task_invocation, &mut operation, &mut attempt,
        )? {
            return Ok(existing);
        }
        let raw = self.core.seal(&serde_json::json!({
            "operation_id": invocation.operation_id,
            "error": detail,
            "executed": false,
        }), "repl-cell-observation")?;
        let normalized = self.core.seal(&value, "repl-cell-result")?;
        self.core.evidence(invocation, &operation, &attempt, raw, Some(normalized),
            true, Some(OperationOutcome::Failed), EffectStatus::None, None)
    }
}

#[async_trait]
impl NativeTaskHandler for ReplCellHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        self.core.descriptor(
            "repl.cell/v1 在一个已创建的 REPL 会话上执行一个 cell（真实解释器进程内，变量跨 cell 保留；WASI 会话经帧协议，原生会话同名协议）：\
             [delegate:repl.cell:1] {\"op\":\"execute\",\"session_id\":\"...\",\"source_b64\":\"...\",\"cell_kind\":\"pure|declared_effect|unknown\"}。\
             cell_kind 是调用方声明的重放边界：pure 可在恢复时重放重建状态，declared_effect 永不重放（只保留凭据），unknown 是诚实的默认值。\
             返回 cell 序号、流摘要与驱动报告（可序列化绑定的 R1 边界）；会话不可用返回显式 REPL_* 拒绝。"
                .into(),
        )
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        self.core.reconcile_unknown(task, operation_id).await
    }
}
