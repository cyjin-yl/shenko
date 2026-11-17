//! Native task handlers exposing REPL sessions to the subject runtime.
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
//! work under `spawn_blocking` only after its complete input and resource
//! binding have been sealed and T1 succeeds. Observations and bounded results
//! then settle the original attempt through `record_external_evidence`.
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
use shenko_kernel::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
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
    b"shenko.repl.session/v1: install a durable named kernel binding (install_kernel) and manage a \
      real long-lived REPL session (list_sessions/create/inspect/checkpoint/restore/fork/reset/\
      cancel/dispose/install_dependencies) with a durable R0 journal, an R1 driver state file where \
      the kernel declares one, candidate-environment dependency commits, and no R2 claim anywhere. \
      A binding pins one interpreter artifact (and its stdlib tree) by digest; create resolves it \
      before T1 instead of trusting the caller's path. Internal request schema 2 freezes \
      the complete request and resource identity before any session work; an existing \
      operation is a factual read and never permission to repeat the effect";
const CELL_CONTRACT: &[u8] =
    b"shenko.repl.cell/v1: execute one journaled cell on the session's real engine (live owned \
      interpreter process or one-shot WASI guest), preserve the R0 work record with stream \
      digests and the driver report, and never replay declared-effect cells during recovery. \
      Internal request schema 2 freezes the complete source and cell identity before T1; \
      only its fresh admission starts the cell, and recovery never repeats that attempt";

// ---------------------------------------------------------------------------
// Input schemas (model-facing, deny-unknown-fields).
// ---------------------------------------------------------------------------

/// First-stage parse: the op discriminator only. The remaining fields are
/// enforced per operation in the second stage (`op_args`), so an op's
/// required arguments are admitted while genuinely unknown fields are still
/// refused. `deny_unknown_fields` deliberately does NOT live here — the other
/// fields belong to the op, not to the discriminator.
#[derive(Debug, Deserialize)]
struct SessionInput {
    op: SessionOp,
}

/// Arguments shared by every zero-argument op: none at all. Strict so a
/// stray field on `list_kernels` refuses instead of being silently dropped.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionOp {
    ListKernels,
    ListSessions,
    Create,
    InstallKernel,
    Inspect,
    Checkpoint,
    Restore,
    Fork,
    Reset,
    Cancel,
    Dispose,
    InstallDependencies,
}

impl SessionOp {
    /// The wire `op` spelling recorded in the sealed frozen input.
    fn wire_name(&self) -> &'static str {
        match self {
            SessionOp::ListKernels => "list_kernels",
            SessionOp::ListSessions => "list_sessions",
            SessionOp::Create => "create",
            SessionOp::InstallKernel => "install_kernel",
            SessionOp::Inspect => "inspect",
            SessionOp::Checkpoint => "checkpoint",
            SessionOp::Restore => "restore",
            SessionOp::Fork => "fork",
            SessionOp::Reset => "reset",
            SessionOp::Cancel => "cancel",
            SessionOp::Dispose => "dispose",
            SessionOp::InstallDependencies => "install_dependencies",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInput {
    /// Installed kernel binding (see `list_kernels`) to create from. Resolves
    /// the profile, re-verifies the artifact digests, and takes no other
    /// binding fields.
    #[serde(default)]
    entry: Option<String>,
    /// Named adapter target (see `list_kernels`); required without `entry`.
    #[serde(default)]
    kernel_id: Option<String>,
    /// Interpreter artifact path, canonical-contained under
    /// `<data>/repl/kernels`; required without `entry`, which already records
    /// its own artifact.
    #[serde(default)]
    module: Option<PathBuf>,
    /// Optional CPython stdlib directory (python-wasi only).
    #[serde(default)]
    stdlib: Option<PathBuf>,
    #[serde(default)]
    limits: LimitInput,
    #[serde(default)]
    preview_bytes: usize,
}

/// Publish or replace one durable kernel binding. The artifact must already
/// exist under the admitted kernels tree; nothing is downloaded or built here.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallKernelInput {
    /// Entry name the caller will `create` by.
    entry: String,
    /// Named target this binding is built from.
    target: String,
    /// Interpreter artifact, canonical-contained under `<data>/repl/kernels`.
    module: PathBuf,
    /// Referenced standard library (python-wasi only); admitted the same way.
    #[serde(default)]
    stdlib: Option<PathBuf>,
    /// Replace an existing binding of the same name. The previous binding is
    /// retired only after the new manifest is durable.
    #[serde(default)]
    replace: bool,
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

/// Route-only view of a session-targeted request: extracts the operation's
/// target before its own strict argument set is enforced. NOT
/// `deny_unknown_fields` — per-op arguments (e.g. `wheels`) would otherwise
/// be refused here before the op schema ever sees them.
#[derive(Debug, Deserialize)]
struct SessionRoute {
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

/// Second-stage parse: the op's own arguments are the request object minus
/// the `op` discriminator, deserialized under the argument struct's strict
/// `deny_unknown_fields` schema. Missing required fields refuse with the
/// schema's `missing field` error; unknown fields refuse outright — a partial
/// operation is never admitted.
fn op_args<T: serde::de::DeserializeOwned>(payload: &serde_json::Value) -> serde_json::Result<T> {
    let args = match payload {
        serde_json::Value::Object(map) => {
            let mut map = map.clone();
            map.remove("op");
            serde_json::Value::Object(map)
        }
        other => other.clone(),
    };
    serde_json::from_value(args)
}

/// A resolution refusal is a caller-resolvable answer about *this
/// installation* — no session, no binding, an artifact that no longer matches
/// its recorded digest, a target this build cannot bind. It is recorded as the
/// task's explicit result instead of parking the operation, and it never
/// claims an effect. Anything else is a real failure and propagates.
fn resolution_refusal(message: &str) -> bool {
    message.contains("REPL_SESSION_UNAVAILABLE")
        || message.contains("REPL_KERNEL_UNAVAILABLE")
        || message.contains("REPL_KERNEL_INVALID")
        || message.contains("REPL_ENTRY_")
}

// ---------------------------------------------------------------------------
// Frozen inputs and bounded results (sealed into CAS).
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct SessionObservedBinding {
    op: String,
    kernel_id: Option<String>,
    /// Installed binding the op resolved, when it used one.
    entry: Option<String>,
    module: Option<PathBuf>,
    module_digest: Option<String>,
    session_id: Option<EntityId>,
    limits: Option<LimitInput>,
}

#[derive(Debug, Serialize)]
struct CellFrozenInput {
    op: String,
    session_id: EntityId,
    cell: EntityId,
    source_ref: ObjectRef,
    source_len: usize,
    source_digest: String,
    cell_kind: String,
    session_binding: crate::session::SessionExecutionBinding,
}

#[derive(Debug, Clone, Serialize)]
struct ArtifactBinding {
    path: PathBuf,
    digest: String,
    directory: bool,
}

impl ArtifactBinding {
    fn read(path: PathBuf) -> Result<Self> {
        let directory = path.is_dir();
        let digest = if directory {
            crate::install::tree_digest(&path)?
        } else {
            crate::journal::hash_reader(fs::File::open(&path)?)?.0
        };
        Ok(Self { path, digest, directory })
    }

    fn verify(&self) -> Result<()> {
        let current = Self::read(self.path.clone())?;
        ensure!(current.digest == self.digest && current.directory == self.directory,
            "REPL_BINDING_STALE: artifact changed after T1 preparation");
        Ok(())
    }
}

/// All resolution here is read-only. In particular `SessionStore::open` may
/// repair a journal and must stay after T1, while cancellation keeps its
/// independent token path instead of waiting for a running cell's mutex.
struct SessionPreparation {
    input: SessionInput,
    payload: serde_json::Value,
    create_kernel: Option<(crate::kernel::KernelProfile, String)>,
    created_session_id: Option<EntityId>,
    session_binding: Option<crate::session::SessionExecutionBinding>,
    artifacts: Vec<ArtifactBinding>,
}

impl SessionPreparation {
    fn new(sessions: &SessionStore, payload: &serde_json::Value) -> Result<Self> {
        let input: SessionInput = serde_json::from_value(payload.clone())
            .map_err(|error| anyhow::anyhow!("REPL_SESSION_INVALID: {error}"))?;
        let mut prepared = Self {
            input, payload: payload.clone(), create_kernel: None, created_session_id: None,
            session_binding: None, artifacts: Vec::new(),
        };
        match prepared.input.op {
            SessionOp::ListKernels | SessionOp::ListSessions => {
                let _: NoArgs = op_args(payload)
                    .map_err(|error| anyhow::anyhow!("REPL_SESSION_INVALID: {error}"))?;
            }
            SessionOp::Create => {
                let create: CreateInput = op_args(payload)
                    .map_err(|error| anyhow::anyhow!("REPL_KERNEL_INVALID: create payload: {error}"))?;
                let (profile, digest) = build_kernel(sessions, &create)?;
                let module = ArtifactBinding::read(crate::entry::profile_module(&profile).to_path_buf())?;
                ensure!(module.digest == digest, "REPL_BINDING_STALE: interpreter changed during preparation");
                prepared.artifacts.push(module);
                for mount in &profile.extra_mounts {
                    prepared.artifacts.push(ArtifactBinding::read(mount.source.clone())?);
                }
                prepared.create_kernel = Some((profile, digest));
                prepared.created_session_id = Some(EntityId::new());
            }
            SessionOp::InstallKernel => {
                let install: InstallKernelInput = op_args(payload)
                    .map_err(|error| anyhow::anyhow!("REPL_ENTRY_INVALID: install_kernel payload: {error}"))?;
                crate::entry::validate_entry_name(&install.entry)?;
                ensure!(install.replace || !sessions.entries_root().join(&install.entry).exists(),
                    "REPL_ENTRY_EXISTS: kernel entry already exists; explicit replacement is required");
                let module = sessions.admit_module(&install.module)?;
                let stdlib = install.stdlib.as_ref().map(|path| sessions.admit_module(path)).transpose()?;
                let profile = crate::adapters::profile_for_target(
                    &install.target, install.entry.clone(), module.clone(), stdlib.clone())?;
                crate::kernel::validate_module_mode(&profile, &module)?;
                prepared.artifacts.push(ArtifactBinding::read(module.clone())?);
                prepared.payload["module"] = serde_json::to_value(module)?;
                if let Some(stdlib) = stdlib {
                    prepared.artifacts.push(ArtifactBinding::read(stdlib.clone())?);
                    prepared.payload["stdlib"] = serde_json::to_value(stdlib)?;
                }
            }
            op => {
                let route: SessionRoute = op_args(payload)
                    .map_err(|error| anyhow::anyhow!("REPL_SESSION_INVALID: {error}"))?;
                if op == SessionOp::InstallDependencies {
                    let install: InstallInput = op_args(payload)
                        .map_err(|error| anyhow::anyhow!("REPL_INSTALL_INVALID: {error}"))?;
                    let mut wheels = Vec::new();
                    for wheel in install.wheels {
                        let wheel = sessions.admit_wheel(&wheel)?;
                        prepared.artifacts.push(ArtifactBinding::read(wheel.clone())?);
                        wheels.push(wheel);
                    }
                    prepared.payload["wheels"] = serde_json::to_value(wheels)?;
                } else {
                    let _: SessionRef = op_args(payload)
                        .map_err(|error| anyhow::anyhow!("REPL_SESSION_INVALID: {error}"))?;
                }
                prepared.session_binding = Some(crate::session::SessionExecutionBinding::read(
                    &sessions.session_dir(route.session_id))?);
                if op == SessionOp::Fork { prepared.created_session_id = Some(EntityId::new()); }
            }
        }
        Ok(prepared)
    }

    fn binding(&self) -> serde_json::Value {
        serde_json::json!({
            "execution_request": self.payload, "create_kernel": self.create_kernel,
            "created_session_id": self.created_session_id,
            "session": self.session_binding, "artifacts": self.artifacts,
        })
    }

    fn verify(&self, sessions: &SessionStore) -> Result<()> {
        for artifact in &self.artifacts { artifact.verify()?; }
        if let Some(binding) = &self.session_binding {
            let route: SessionRoute = op_args(&self.payload)?;
            binding.verify_manifest(&sessions.session_dir(route.session_id))?;
        }
        Ok(())
    }
}

/// Positive local evidence that no requested session/cell work was started.
/// Other post-T1 failures remain uncertain and never arm another attempt.
#[derive(Debug)]
struct BeforeReplExecution(anyhow::Error);

impl std::fmt::Display for BeforeReplExecution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl std::error::Error for BeforeReplExecution {}

struct CancelExecutionOnDrop(crate::session::ExecutionControl);

impl Drop for CancelExecutionOnDrop {
    fn drop(&mut self) { self.0.cancel(); }
}

fn lock_session<'a>(session: &'a std::sync::Mutex<crate::session::WasiReplSession>,
    execution: &crate::session::ExecutionControl)
    -> Result<std::sync::MutexGuard<'a, crate::session::WasiReplSession>>
{
    loop {
        execution.check()?;
        match session.try_lock() {
            Ok(session) => return Ok(session),
            Err(std::sync::TryLockError::Poisoned(_)) =>
                anyhow::bail!("REPL_SESSION_INVALID: session lock poisoned"),
            Err(std::sync::TryLockError::WouldBlock) =>
                std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
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
    /// Durable bindings this installation actually has. Listing reports the
    /// recorded digest and only a cheap artifact-length check; `create`
    /// re-verifies the bytes.
    entries: Vec<crate::entry::EntryListing>,
}

/// Durable sessions this installation still has, including ones whose
/// controller process is gone. Reading a listing never opens an interpreter.
#[derive(Debug, Serialize)]
struct ListSessionsResult {
    sessions: Vec<crate::session::SessionListing>,
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
        for sub in ["sessions", "kernels", "wheels", "entries"] {
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
    /// Durable kernel bindings (replaceable kernel plugins). Kept apart
    /// from `kernels`, which holds the admitted interpreter artifacts the
    /// bindings point at.
    fn entries_root(&self) -> PathBuf { self.data_dir.join("entries") }

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
    fn seal_request(
        &self, invocation: &NativeTaskInvocation, binding: &impl Serialize, schema: &str,
    ) -> Result<ObjectRef> {
        self.seal(&serde_json::json!({
            "schema_version": 2,
            "task_id": invocation.task_invocation.task_id,
            "operation_id": invocation.operation_id,
            "task_input_ref": invocation.task.input_ref,
            "request": invocation.input,
            "binding": binding,
        }), schema)
    }

    /// An original result is a factual read before any discovery, opening or
    /// execution. A request with a different body cannot reuse its identity.
    /// Historical schema-1 inputs stay available through `reconcile`; their
    /// incomplete execution-time envelopes cannot be upgraded into new work.
    fn existing(&self, invocation: &NativeTaskInvocation) -> Result<Option<OperationRecord>> {
        let Some(operation) = self.store.find_external_operation(invocation.operation_id)? else {
            return Ok(None);
        };
        ensure!(operation.task_id == invocation.task_invocation.task_id
            && operation.operation_name == self.operation_name
            && operation.logical_key == "native-handler",
            "original_operation_contract_mismatch");
        ensure!(operation.input_schema_version == 2,
            "REPL_RECOVERY_REQUIRED: historical operation must use its original reconciliation path");
        let bytes = self.store.cas.read_object(&operation.final_input_ref.digest)?;
        ensure!(bytes.len() as u64 == operation.final_input_ref.bytes
            && Digest::blake3(&bytes) == operation.input_digest
            && operation.input_digest == operation.final_input_ref.digest,
            "REPL_RECEIPT_CORRUPT: original request seal does not verify");
        let sealed: serde_json::Value = serde_json::from_slice(&bytes)?;
        ensure!(sealed["schema_version"] == 2
            && sealed["task_id"] == serde_json::to_value(invocation.task_invocation.task_id)?
            && sealed["operation_id"] == serde_json::to_value(invocation.operation_id)?
            && sealed["task_input_ref"] == serde_json::to_value(&invocation.task.input_ref)?
            && sealed["request"] == invocation.input,
            "operation_conflict: original REPL request differs");
        Ok(Some(operation))
    }

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
            input_schema_version: 2,
            output_schema_version: 1,
            final_input_ref: input_ref.clone(),
            input_digest: input_ref.digest.clone(),
            contract_ref: OperationContractRef {
                plugin_id: self.plugin_id,
                artifact_digest: shenko_kernel::native_artifact::native_artifact_digest(),
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
        VersionRef {
            artifact: ArtifactRef {
                digest: shenko_kernel::native_artifact::native_artifact_digest(),
                locator: format!("builtin:{}", self.kind),
            },
            symbol: format!("{}.execute", self.kind),
            schema: SchemaRef {
                name: format!("{}-observation-v1", self.kind.replace('.', "-")),
                major: 1,
                digest: shenko_kernel::native_artifact::native_schema_digest(&format!("{}-observation-v1", self.kind.replace('.', "-")), 1),
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
                artifact_digest: shenko_kernel::native_artifact::native_artifact_digest(),
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

/// The honest effect settlement for a cell that already ran: its record and
/// streams are durable in the session journal, so a completed or exited cell
/// is fully applied; a cancelled, deadlined or failed cell may have changed
/// guest state partway — partial, never unknown: the work was observed.
fn cell_effect_of(outcome: &crate::contract::CellOutcome) -> EffectStatus {
    match outcome {
        crate::contract::CellOutcome::Completed { .. } | crate::contract::CellOutcome::Exited { .. } =>
            EffectStatus::Complete,
        crate::contract::CellOutcome::Cancelled
        | crate::contract::CellOutcome::DeadlineExceeded
        | crate::contract::CellOutcome::Failed { .. } => EffectStatus::Partial,
    }
}

/// Seal one journaled stream file: full bytes into CAS by streaming, plus a
/// bounded head preview (the only bytes ever resident in memory).
fn seal_stream(store: &Store, session_dir: &Path, relative: &Path) -> Result<(ObjectRef, Vec<u8>, bool)> {
    // Capture from the very same reads CAS seals. seal_reader consumes its
    // input to EOF; a second read of that file would silently return empty.
    struct WithPreview<R> { reader: R, preview: Vec<u8> }
    impl<R: Read> Read for WithPreview<R> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let count = self.reader.read(bytes)?;
            let retained = count.min((MAX_PREVIEW_BYTES + 1).saturating_sub(self.preview.len()));
            self.preview.extend_from_slice(&bytes[..retained]);
            Ok(count)
        }
    }
    let mut stream = WithPreview {
        reader: fs::File::open(session_dir.join(relative))?, preview: Vec::new(),
    };
    let sealed = match store.cas.seal_reader(
        &mut stream, "application/octet-stream", "repl-streams", "repl-stream", 1,
    ) {
        Ok(sealed) => sealed,
        Err(error) => {
            store.close_external_writer();
            return Err(error.into());
        }
    };
    let mut preview = stream.preview;
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
    SessionObservedBinding,
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
                role_revision: "native-repl-session/2",
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
        if let Some(existing) = self.core.existing(&invocation)? { return Ok(existing); }
        let prepared = match SessionPreparation::new(&self.sessions, &invocation.input) {
            Ok(prepared) => prepared,
            Err(error) => {
                let message = format!("{error:#}");
                if resolution_refusal(&message) {
                    return self.record_refusal(&invocation, message).await;
                }
                return Err(error);
            }
        };
        let store = Arc::clone(&self.core.store);
        let (mut operation, mut attempt) = {
            let binding = prepared.binding();
            let input_ref = self.core.seal_request(&invocation, &binding, "repl-session-request-v2")?;
            let bindings = ResourceBindings {
                provider: "repl.session.v1".into(),
                account_identity: String::new(),
                runtime_node: invocation.task_invocation.node_id.to_string(),
                mount_generation: 0,
                capability_level: "managed_session_home".into(),
                extra: binding,
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
        // No session open, signal, filesystem mutation or interpreter worker
        // is reached until this exact source invocation wins durable T1.
        let sessions = Arc::clone(&self.sessions);
        let task_id = invocation.task_invocation.task_id;
        let worker_id = invocation.task_invocation.worker_id;
        let execution = crate::session::ExecutionControl::default();
        let _cancel_on_drop = CancelExecutionOnDrop(execution.clone());
        if invocation.cancellation.is_cancelled() { execution.cancel(); }
        let worker_execution = execution.clone();
        let mut worker = tokio::task::spawn_blocking(move || {
            worker_execution.check().map_err(BeforeReplExecution)?;
            prepared.verify(&sessions).map_err(BeforeReplExecution)?;
            run_session_op(&sessions, &prepared, task_id, worker_id, &worker_execution)
        });
        let observed = tokio::select! {
            _result = &mut worker => _result?,
            _ = invocation.cancellation.cancelled() => {
                execution.cancel();
                worker.await?
            }
        };
        let (binding, value, decoded, outcome, effect, event_key) = match observed {
            Ok(observed) => observed,
            Err(error) if error.is::<BeforeReplExecution>() => {
                return self.refusal_evidence(&invocation, &operation, &attempt, format!("{error:#}"));
            }
            Err(error) => return Err(error),
        };
        let raw = self.core.seal(&serde_json::json!({
            "operation_id": invocation.operation_id,
            "execution_binding": binding,
            "result": value,
        }), "repl-session-observation")?;
        let normalized = self.core.seal(&value, "repl-session-result")?;
        self.core.evidence(&invocation, &operation, &attempt, raw, Some(normalized),
            decoded, outcome, effect, event_key)
    }

    /// Record an explicit REPL_* resolution refusal as the task's real outcome.
    async fn record_refusal(&self, invocation: &NativeTaskInvocation, detail: String)
        -> Result<OperationRecord>
    {
        let store = Arc::clone(&self.core.store);
        let (mut operation, mut attempt) = {
            let input_ref = self.core.seal_request(invocation, &serde_json::json!({"resolution_refusal":detail}),
                "repl-session-request-v2")?;
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
        self.refusal_evidence(invocation, &operation, &attempt, detail)
    }

    fn refusal_evidence(
        &self, invocation: &NativeTaskInvocation, operation: &OperationRecord,
        attempt: &AttemptRecord, detail: String,
    ) -> Result<OperationRecord> {
        let value = serde_json::to_value(SessionResult {
            session_id: None, outcome: Some("refused".into()), kernel_id: None, runtime: None,
            backend: None, session_model: None, recovery_level: None, cells_executed: None,
            environment_generation: None, detail: None, error: Some(detail.clone()),
        })?;
        let raw = self.core.seal(&serde_json::json!({
            "operation_id": invocation.operation_id,
            "error": detail,
            "executed": false,
        }), "repl-session-observation")?;
        let normalized = self.core.seal(&value, "repl-session-result")?;
        self.core.evidence(invocation, operation, attempt, raw, Some(normalized),
            true, Some(OperationOutcome::Failed), EffectStatus::None, None)
    }
}

/// Execute one session op with the session mutex held. Blocking by design
/// (real process I/O); the caller wraps this in `spawn_blocking`.
fn run_session_op(
    sessions: &Arc<SessionStore>,
    prepared: &SessionPreparation,
    task_id: EntityId,
    worker_id: EntityId,
    execution: &crate::session::ExecutionControl,
) -> Result<SessionOpOutcome> {
    execution.check().map_err(BeforeReplExecution)?;
    let payload = &prepared.payload;
    let input = &prepared.input;
    match input.op {
        SessionOp::ListKernels => {
            let _strict: NoArgs = op_args(payload)
                .map_err(|error| anyhow::anyhow!("REPL_SESSION_INVALID: {error}"))?;
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
            // Installed bindings are part of discovery: a restarted node's
            // caller must not have to remember interpreter paths.
            let entries = crate::entry::list(&sessions.entries_root())?;
            let value = serde_json::to_value(ListKernelsResult { targets, entries })?;
            Ok((SessionObservedBinding { op: "list_kernels".into(), kernel_id: None, module: None,
                entry: None, module_digest: None, session_id: None, limits: None }, value, true,
                Some(OperationOutcome::Succeeded), EffectStatus::None, None))
        }
        SessionOp::ListSessions => {
            let _strict: NoArgs = op_args(payload)
                .map_err(|error| anyhow::anyhow!("REPL_SESSION_INVALID: {error}"))?;
            // A restarted controller must be able to tell a caller which
            // durable sessions still exist. Listing reads and validates each
            // manifest only: no interpreter is opened and no journal is
            // repaired, so this observation changes nothing.
            let listings = crate::session::list_sessions(&sessions.sessions_root_cloned())?;
            let value = serde_json::to_value(ListSessionsResult { sessions: listings })?;
            Ok((SessionObservedBinding { op: "list_sessions".into(), kernel_id: None, entry: None,
                module: None, module_digest: None, session_id: None, limits: None }, value, true,
                Some(OperationOutcome::Succeeded), EffectStatus::None, None))
        }
        SessionOp::Create => {
            let create: CreateInput = op_args(payload)
                .map_err(|error| anyhow::anyhow!("REPL_KERNEL_INVALID: create payload: {error}"))?;
            let (profile, module_digest) = prepared.create_kernel.clone()
                .context("REPL_KERNEL_INVALID: no frozen create binding")?;
            let provider = crate::session::WasiReplProvider::new(sessions.sessions_root_cloned());
            let session = provider.create_bound(crate::contract::CreateSession {
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
            }, prepared.created_session_id.context("REPL_SESSION_INVALID: no frozen new session id")?,
                Some(&module_digest))?;
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
            Ok((SessionObservedBinding { op: "create".into(), kernel_id: Some(profile.kernel_id.clone()),
                entry: create.entry.clone(), module: Some(match &profile.module {
                    crate::kernel::ModuleSource::ModulePath(path) => path.clone(),
                }), module_digest: Some(module_digest), session_id: Some(session_id),
                limits: Some(create.limits) }, value, true, Some(OperationOutcome::Succeeded),
                EffectStatus::Complete, None))
        }
        SessionOp::InstallKernel => {
            let install: InstallKernelInput = op_args(payload)
                .map_err(|error| anyhow::anyhow!("REPL_ENTRY_INVALID: install_kernel payload: {error}"))?;
            let module = sessions.admit_module(&install.module)?;
            let stdlib = match &install.stdlib {
                Some(path) => Some(sessions.admit_module(path)?),
                None => None,
            };
            let entry = crate::entry::install(&sessions.entries_root(), &crate::entry::InstallEntry {
                entry: install.entry.clone(),
                target: install.target.clone(),
                module,
                stdlib,
                replace: install.replace,
            })?;
            let value = serde_json::to_value(SessionResult {
                session_id: None,
                outcome: Some("installed".into()),
                kernel_id: Some(entry.entry.clone()),
                runtime: Some(format!("{:?}", entry.profile.runtime)),
                backend: Some(format!("{:?}", entry.profile.backend)),
                session_model: Some(format!("{:?}", entry.session_model())),
                recovery_level: Some(format!("{:?}", entry.recovery_level())),
                cells_executed: None,
                environment_generation: Some(crate::kernel::environment_generation(&entry.profile).0),
                detail: Some(format!(
                    "target={} artifact={} bytes={} replacement_requested={}",
                    entry.target, entry.module_path.display(), entry.module_bytes, install.replace
                )),
                error: None,
            })?;
            Ok((SessionObservedBinding {
                op: "install_kernel".into(),
                kernel_id: Some(entry.entry.clone()),
                entry: Some(entry.entry.clone()),
                module: Some(entry.module_path.clone()),
                module_digest: Some(entry.module_blake3.clone()),
                session_id: None,
                limits: None,
            }, value, true, Some(OperationOutcome::Succeeded), EffectStatus::Complete, None))
        }
        SessionOp::Cancel => {
            let reference: SessionRef = op_args(payload)
                .map_err(|error| anyhow::anyhow!("REPL_SESSION_INVALID: {error}"))?;
            let session_id = reference.session_id;
            let requested = sessions.request_cancel(session_id)?;
            let frozen = SessionObservedBinding { op: "cancel".into(), kernel_id: None,
                entry: None, module: None, module_digest: None, session_id: Some(session_id),
                limits: None };
            let value = serde_json::to_value(SessionResult {
                session_id: Some(session_id),
                outcome: Some(if requested { "interrupt_requested" } else { "nothing_running" }.into()),
                kernel_id: None, runtime: None, backend: None, session_model: None,
                recovery_level: None, cells_executed: None, environment_generation: None,
                detail: None, error: None,
            })?;
            Ok((frozen, value, true, Some(OperationOutcome::Succeeded),
                if requested { EffectStatus::Complete } else { EffectStatus::None }, None))
        }
        SessionOp::Inspect | SessionOp::Checkpoint | SessionOp::Restore | SessionOp::Fork
        | SessionOp::Reset | SessionOp::Dispose | SessionOp::InstallDependencies => {
            // Route on the session id first, then enforce the op's own strict
            // argument set before the session is opened: a malformed or
            // over-supplied request is refused, never admitted against a real
            // session and never fabricates one.
            let route: SessionRoute = op_args(payload)
                .map_err(|error| anyhow::anyhow!("REPL_SESSION_INVALID: {error}"))?;
            let install = match input.op {
                SessionOp::InstallDependencies =>
                    Some(op_args::<InstallInput>(payload)
                        .map_err(|error| anyhow::anyhow!("REPL_INSTALL_INVALID: {error}"))?),
                _ => {
                    let _strict: SessionRef = op_args(payload)
                        .map_err(|error| anyhow::anyhow!("REPL_SESSION_INVALID: {error}"))?;
                    None
                }
            };
            let session_id = route.session_id;
            let handle = sessions.open(session_id)?;
            let frozen = SessionObservedBinding { op: input.op.wire_name().into(),
                kernel_id: None, entry: None, module: None, module_digest: None,
                session_id: Some(session_id), limits: None };
            let mut session = lock_session(&handle, execution)?;
            session.verify_native_binding(prepared.session_binding.as_ref()
                .context("REPL_SESSION_INVALID: no frozen session binding")?)?;
            let observed = session.with_execution_control(execution.clone(), |session| match input.op {
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
                    // A read-only observation: nothing about the session's
                    // durable state changed.
                    Ok((frozen, value, true, Some(OperationOutcome::Succeeded), EffectStatus::None, None))
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
                    // A checkpoint is a durable citation of what already
                    // exists — it mutates nothing.
                    Ok((frozen, value, true, Some(OperationOutcome::Succeeded), EffectStatus::None, None))
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
                    // A verified restore is fully applied; an unready one
                    // durably kept the active-restore fence and whatever
                    // object restores already landed — partial, never
                    // unknown: the work was observed.
                    Ok((frozen, value, true,
                        Some(if ready { OperationOutcome::Succeeded } else { OperationOutcome::Failed }),
                        if ready { EffectStatus::Complete } else { EffectStatus::Partial }, None))
                }
                SessionOp::Fork => {
                    let forked = session.fork_identified(&sessions.sessions_root_cloned(),
                        prepared.created_session_id.context("REPL_SESSION_INVALID: no frozen fork id")?)?;
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
                    Ok((frozen, value, true, Some(OperationOutcome::Succeeded), EffectStatus::Complete, None))
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
                    Ok((frozen, value, true, Some(OperationOutcome::Succeeded), EffectStatus::Complete, None))
                }
                SessionOp::Dispose => {
                    session.dispose()?;
                    let value = serde_json::to_value(SessionResult {
                        session_id: Some(session_id),
                        outcome: Some("disposed".into()),
                        kernel_id: None, runtime: None, backend: None, session_model: None,
                        recovery_level: None, cells_executed: None, environment_generation: None,
                        detail: None, error: None,
                    })?;
                    Ok((frozen, value, true, Some(OperationOutcome::Succeeded), EffectStatus::Complete, None))
                }
                SessionOp::InstallDependencies => {
                    let install = install.expect("install_dependencies routed its arguments");
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
                    // An installed generation is committed durably; a refused
                    // or unsupported install committed nothing.
                    let (operation_outcome, effect) = match &outcome {
                        crate::contract::InstallOutcome::Installed { .. } =>
                            (OperationOutcome::Succeeded, EffectStatus::Complete),
                        _ => (OperationOutcome::Failed, EffectStatus::None),
                    };
                    Ok((frozen, value, true, Some(operation_outcome), effect, None))
                }
                SessionOp::ListKernels | SessionOp::ListSessions | SessionOp::Create
                | SessionOp::InstallKernel | SessionOp::Cancel => unreachable!("routed above"),
            })?;
            if input.op == SessionOp::Dispose {
                drop(session);
                sessions.remove(session_id);
            }
            Ok(observed)
        }
    }
}

/// Kernel construction for `create`, shared by the blocking op runner.
fn build_kernel(sessions: &SessionStore, create: &CreateInput)
    -> Result<(crate::kernel::KernelProfile, String)>
{
    let explicit = create.kernel_id.is_some() || create.module.is_some() || create.stdlib.is_some();
    if let Some(entry) = &create.entry {
        ensure!(
            !explicit,
            "REPL_KERNEL_INVALID: create takes one binding — either \"entry\" or \
             \"kernel_id\"+\"module\", never both"
        );
        return resolve_entry(sessions, entry);
    }
    let (Some(kernel_id), Some(module)) = (&create.kernel_id, &create.module) else {
        anyhow::bail!(
            "REPL_KERNEL_INVALID: create needs either \"entry\" (an installed kernel binding) or \
             \"kernel_id\"+\"module\""
        );
    };
    ensure!(!kernel_id.is_empty() && kernel_id.len() <= MAX_NAME_BYTES,
        "REPL_KERNEL_INVALID: kernel_id");
    let module = sessions.admit_module(module)?;
    let digest = crate::journal::hash_bytes(&fs::read(&module)?);
    let stdlib = match &create.stdlib {
        Some(path) => Some(sessions.admit_module(path)?),
        None => None,
    };
    // One mapping for both routes: an installed binding and an explicit
    // create can never describe the same named target differently.
    let profile = crate::adapters::profile_for_target(
        kernel_id, kernel_id.clone(), module, stdlib)?;
    Ok((profile, digest))
}

/// Create-time resolution of a durable kernel binding. The entry must still
/// verify byte-for-byte, and both its artifact and its stdlib must live in the
/// admitted kernels tree: a tampered manifest cannot run an interpreter from
/// outside the tree the operator installed into.
fn resolve_entry(sessions: &SessionStore, entry: &str)
    -> Result<(crate::kernel::KernelProfile, String)>
{
    let (profile, record) = crate::entry::resolve(&sessions.entries_root(), entry)?;
    let module = sessions.admit_module(crate::entry::profile_module(&profile))?;
    ensure!(
        module == record.module_path,
        "REPL_ENTRY_CORRUPT: kernel entry {entry:?} resolves outside the admitted kernels tree"
    );
    if let Some(stdlib) = &record.stdlib_path {
        ensure!(
            sessions.admit_module(stdlib)? == *stdlib,
            "REPL_ENTRY_CORRUPT: kernel entry {entry:?} stdlib is outside the admitted kernels tree"
        );
    }
    Ok((profile, record.module_blake3))
}

#[async_trait]
impl NativeTaskHandler for ReplSessionHandler {
    fn joins_execution_on_cancel(&self) -> bool { true }

    fn descriptor(&self) -> NativeTaskDescriptor {
        self.core.descriptor(
            "repl.session/v1 管理真实长生命周期 REPL 会话（WASI 或原生解释器进程，变量在进程存活期间保留）：\
             先看已安装的 kernel 绑定与具名目标 {\"op\":\"list_kernels\"}（targets = 本构建能绑定/不能绑定的运行时，\
             entries = 本机真实安装的绑定及其 artifact 状态；列目录只核对长度，真正执行前会重新核对摘要）。\
             重启后先 {\"op\":\"list_sessions\"} 看本机真实存续的会话（只校验 manifest，不开解释器、不修复 journal），\
             再用返回的 session_id 继续 inspect/restore。\
             安装绑定：[delegate:repl.session:1] {\"op\":\"install_kernel\",\"entry\":\"python-3.13\",\
             \"target\":\"python-wasi|quickjs-wasm|python-native-host|node-native-host|bun-native-host\",\
             \"module\":\"<data>/repl/kernels 下的解释器路径\",\"stdlib\":\"可选 <data>/repl/kernels 下的标准库目录\",\
             \"replace\":false}；同名已有绑定需显式 replace，旧绑定在新清单持久后才退场。\
             创建会话可用绑定名 {\"op\":\"create\",\"entry\":\"python-3.13\",\"limits\":{\"max_wall_clock_ms\":60000}}，\
             也可显式给 {\"op\":\"create\",\"kernel_id\":\"...\",\"module\":\"...\"}；两者不可混用。\
             其余 op（inspect/checkpoint/restore/fork/reset/cancel/dispose/install_dependencies）带 {\"session_id\":\"...\"}。\
             restore 通过重放 Pure cell 重建状态，DeclaredEffect 只保留凭据不重放；install_dependencies 提交候选环境并推进 generation；\
             node-wasm/bun-wasm/pyodide 具名不可用，不会被 quickjs 或宿主解释器顶替；会话进程中断后会话进入 recovery-required。无 R2。"
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
                role_revision: "native-repl-cell/2",
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
        let session_binding = crate::session::SessionExecutionBinding::read(
            &self.sessions.session_dir(input.session_id))?;
        let source_ref = match self.core.store.cas.seal_object(
            &source, "application/octet-stream", "repl-effects", "repl-cell-source", 1,
        ) {
            Ok(sealed) => sealed.object,
            Err(error) => {
                self.core.store.close_external_writer();
                return Err(error.into());
            }
        };
        let frozen = CellFrozenInput {
            op: "execute".into(),
            session_id: input.session_id,
            cell: EntityId::new(),
            source_ref,
            source_len: source.len(),
            source_digest: crate::journal::hash_bytes(&source),
            cell_kind: match input.cell_kind {
                CellKindInput::Pure => "pure", CellKindInput::DeclaredEffect => "declared_effect",
                CellKindInput::Unknown => "unknown",
            }.into(),
            session_binding,
        };
        Ok((frozen, source, input.cell_kind))
    }

    fn prepare_operation(
        &self, invocation: &NativeTaskInvocation, frozen: &CellFrozenInput,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let input_ref = self.core.seal_request(invocation, frozen, "repl-cell-request-v2")?;
        let bindings = ResourceBindings {
            provider: "repl.cell.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "managed_session_cell".into(),
            extra: serde_json::json!({
                "session": frozen.session_binding,
                "cell": frozen.cell,
                "source_ref": frozen.source_ref,
            }),
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
        Ok(self.core.new_operation(invocation, input_ref, resource, acceptance, retry))
    }

    async fn run(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        if let Some(existing) = self.core.existing(&invocation)? { return Ok(existing); }
        let (frozen, source, cell_kind) = match self.prepare(&invocation) {
            Ok(prepared) => prepared,
            Err(error) if resolution_refusal(&format!("{error:#}")) => {
                return self.record_refusal(&invocation, format!("{error:#}")).await;
            }
            Err(error) => return Err(error),
        };
        let (mut operation, mut attempt) = self.prepare_operation(&invocation, &frozen)?;
        if let Some(existing) = self.core.store.arm_external_operation(
            &invocation.task_invocation, &mut operation, &mut attempt,
        )? {
            return Ok(existing);
        }
        let cell_kind_contract = match cell_kind {
            CellKindInput::Pure => crate::contract::CellKind::Pure,
            CellKindInput::DeclaredEffect => crate::contract::CellKind::DeclaredEffect,
            CellKindInput::Unknown => crate::contract::CellKind::Unknown,
        };
        let sessions = Arc::clone(&self.sessions);
        let store = Arc::clone(&self.core.store);
        let session_id = frozen.session_id;
        let task_label = invocation.task_invocation.task_id;
        let execution = crate::session::ExecutionControl::default();
        let _cancel_on_drop = CancelExecutionOnDrop(execution.clone());
        if invocation.cancellation.is_cancelled() { execution.cancel(); }
        let worker_execution = execution.clone();
        let mut worker = tokio::task::spawn_blocking(move || {
            worker_execution.check().map_err(BeforeReplExecution)?;
            frozen.session_binding.verify_manifest(&sessions.session_dir(session_id))
                .map_err(BeforeReplExecution)?;
            let handle = sessions.open(session_id)?;
            let mut session = lock_session(&handle, &worker_execution)?;
            // Opening can already heal a manifest or finish a reset. Any
            // later refusal therefore keeps this original attempt unknown;
            // it cannot prove that no session mutation happened.
            let result = session.with_execution_control(worker_execution, |session| {
                session.check_native_cell(&frozen.session_binding)?;
                session.execute_identified(crate::contract::CellRequest {
                    source,
                    cell_kind: cell_kind_contract,
                    labels: BTreeMap::from([("task".into(), task_label.to_string())]),
                }, frozen.cell)
            })?;
            let dir = session.session_dir().to_path_buf();
            let outcome = result.record.outcome.clone();
            let stdout = seal_stream(&store, &dir, &result.record.stdout_path)?;
            let stderr = seal_stream(&store, &dir, &result.record.stderr_path)?;
            Ok((result, outcome, stdout, stderr))
        });
        let prepared: Result<(
            crate::contract::CellResult,
            crate::contract::CellOutcome,
            (ObjectRef, Vec<u8>, bool),
            (ObjectRef, Vec<u8>, bool),
        )> = tokio::select! {
            _result = &mut worker => _result?,
            _ = invocation.cancellation.cancelled() => {
                execution.cancel();
                worker.await?
            }
        };

        let (cell, outcome, stdout_evidence, stderr_evidence) = match prepared {
            Ok(parts) => parts,
            Err(error) if error.is::<BeforeReplExecution>() => {
                return self.refusal_evidence(&invocation, &operation, &attempt, format!("{error:#}"));
            }
            Err(error) => return Err(error),
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

        let raw = self.core.seal(&serde_json::json!({
            "operation_id": invocation.operation_id,
            "session_id": session_id,
            "cell": cell.record.cell,
            "seq": cell.record.seq,
            "outcome": result_view.outcome,
        }), "repl-cell-observation")?;
        let normalized = self.core.seal(&result_view, "repl-cell-result")?;
        self.core.evidence(&invocation, &operation, &attempt, raw, Some(normalized),
            true, Some(operation_outcome_of(&outcome)), cell_effect_of(&outcome), None)
    }

    async fn record_refusal(&self, invocation: &NativeTaskInvocation, detail: String)
        -> Result<OperationRecord>
    {
        let store = Arc::clone(&self.core.store);
        let (mut operation, mut attempt) = {
            let input_ref = self.core.seal_request(invocation, &serde_json::json!({"resolution_refusal":detail}),
                "repl-cell-request-v2")?;
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
        self.refusal_evidence(invocation, &operation, &attempt, detail)
    }

    fn refusal_evidence(
        &self, invocation: &NativeTaskInvocation, operation: &OperationRecord,
        attempt: &AttemptRecord, detail: String,
    ) -> Result<OperationRecord> {
        let value = CellResultView {
            session_id: invocation.input.get("session_id").cloned()
                .and_then(|value| serde_json::from_value(value).ok()).unwrap_or_else(EntityId::nil),
            seq: 0, cell: EntityId::nil(),
            outcome: "refused".into(), exit_code: None,
            stdout_b64: None, stdout_bytes: None, stdout_ref: None,
            stderr_b64: None, stderr_bytes: None, stderr_ref: None,
            driver_report: None, truncated: false, error: Some(detail.clone()),
        };
        let raw = self.core.seal(&serde_json::json!({
            "operation_id": invocation.operation_id,
            "error": detail,
            "executed": false,
        }), "repl-cell-observation")?;
        let normalized = self.core.seal(&value, "repl-cell-result")?;
        self.core.evidence(invocation, operation, attempt, raw, Some(normalized),
            true, Some(OperationOutcome::Failed), EffectStatus::None, None)
    }
}

#[async_trait]
impl NativeTaskHandler for ReplCellHandler {
    fn joins_execution_on_cancel(&self) -> bool { true }

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

// ---------------------------------------------------------------------------
// Native-handler regressions: every advertised session op through the real
// registered handler, ledger T1 and CAS receipts — never a session fabricated
// by the parser.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_types::task::TaskInvocation;
    use shenko_types::agent::CallContext;
    use shenko_types::store::WriteBatch;

    /// A real resident WASI guest for the live-session ops: like a driver
    /// interpreter it stays alive on the session's framed stdin channel,
    /// blocking in `fd_read` until the host closes the pipe (shutdown/reset/
    /// drop), then exits cleanly. `restore` spawns it for real; a guest that
    /// dies early makes `inspect` report `recovery_required`, so the asserts
    /// below fail loudly rather than matching whatever happened.
    const RESIDENT_GUEST: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_read"
    (func $read (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start")
    (local $nread i32)
    (loop $again
      ;; iov {buf=64, len=512} at 0; nread lands at 8.
      (i32.store (i32.const 0) (i32.const 64))
      (i32.store (i32.const 4) (i32.const 512))
      ;; Any read error ends the guest rather than spinning.
      (if (call $read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 8))
        (then (return)))
      (local.set $nread (i32.load (i32.const 8)))
      (br_if $again (i32.ne (local.get $nread) (i32.const 0))))))
"#;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("shenko-repl-task-{tag}-{}", EntityId::new()))
    }

    struct Harness {
        store: Arc<Store>,
        handler: ReplSessionHandler,
        data_dir: PathBuf,
    }

    impl Harness {
        fn new(tag: &str) -> Result<(Self, PathBuf)> {
            let root = temp_dir(tag);
            let data_dir = root.join("repl");
            for sub in ["kernels", "wheels"] {
                fs::create_dir_all(data_dir.join(sub))?;
            }
            let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
            let handler = ReplSessionHandler::new(store.clone(), data_dir.clone())?;
            Ok((Self { store, handler, data_dir }, root))
        }

        /// Register one Running task in the real ledger (T1 admission needs
        /// its exact revision/invocation) and invoke the handler through the
        /// registered `NativeTaskHandler::execute` path.
        async fn run(&self, input: serde_json::Value) -> Result<OperationRecord> {
            invoke(&self.store, &self.handler.core, &self.handler, "repl.session",
                "repl-session-request", input, true).await
        }

        /// The sealed bounded result of exactly one settled observation.
        fn normalized(&self, operation: &OperationRecord) -> Result<serde_json::Value> {
            normalized_result(&self.store, operation)
        }
    }

    /// Register one Running task in the real ledger and invoke *that* handler.
    /// Shared by both handler kinds so the cell surface is exercised through
    /// the same T1/CAS path the session surface is.
    async fn invoke(
        store: &Arc<Store>,
        core: &HandlerCore,
        handler: &dyn NativeTaskHandler,
        kind: &str,
        schema: &str,
        input: serde_json::Value,
        commit_task: bool,
    ) -> Result<OperationRecord> {
        handler.execute(prepare_invocation(store, core, handler, kind, schema, input, commit_task)?).await
    }

    fn registered_invocation(
        store: &Arc<Store>, core: &HandlerCore, handler: &dyn NativeTaskHandler,
        kind: &str, schema: &str, input: serde_json::Value,
    ) -> Result<NativeTaskInvocation> {
        prepare_invocation(store, core, handler, kind, schema, input, true)
    }

    fn prepare_invocation(
        store: &Arc<Store>, core: &HandlerCore, handler: &dyn NativeTaskHandler,
        kind: &str, schema: &str, input: serde_json::Value, commit_task: bool,
    ) -> Result<NativeTaskInvocation> {
        let (task_id, invocation_id) = (EntityId::new(), EntityId::new());
        let epoch = ControllerEpoch(1);
        let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
        let input_ref = core.seal(&input, schema)?;
        let task = TaskRecord {
            record_schema_version: 1,
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            agent_id: EntityId::new(),
            conversation_id: EntityId::new(),
            owner: TaskOwner::Conversation { id: EntityId::new() },
            background: false,
            kind: kind.into(),
            definition_version: 1,
            implementation_ref: handler.descriptor().implementation_ref,
            input_ref,
            state: TaskState::Running {
                checkpoint_ref: None,
                invocation_id,
                mode: RunMode::Run,
                epoch,
            },
            abort_request: None,
            memos: serde_json::Map::new(),
            residual_case_ids: vec![],
            created_by_step_id: None,
            dispatch_eligibility: DispatchEligibility::Ready,
            no_progress_crashes: 0,
            first_run_at: None,
            terminal_at: None,
            created_commit: zero,
            updated_commit: zero,
        };
        if commit_task {
            let mut batch = WriteBatch::new();
            batch.task_upserts.push(task.clone());
            store.commit(batch)?;
        }
        let task_invocation = TaskInvocation {
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            invocation_id,
            epoch,
            software_generation: RuntimeGeneration::default(),
            worker_id: EntityId::new(),
            node_id: EntityId::new(),
        };
        let mut context = CallContext::new_system(RuntimeGeneration::default());
        context.task_id = Some(task_id);
        context.invocation_id = Some(invocation_id);
        context.controller_epoch = epoch;
        Ok(NativeTaskInvocation {
            context,
            task_invocation,
            task,
            operation_id: EntityId::new(),
            authority: AuthorityRef {
                delegation_id: task_id,
                delegation_revision: RecordRevision(0),
                decision_ref: EntityId::new(),
            },
            input,
            child_tools: None,
            cancellation: Default::default(),
        })
    }

    fn repeat(invocation: &NativeTaskInvocation) -> NativeTaskInvocation {
        NativeTaskInvocation {
            context: invocation.context.clone(),
            task_invocation: invocation.task_invocation.clone(),
            task: invocation.task.clone(), operation_id: invocation.operation_id,
            authority: invocation.authority.clone(), input: invocation.input.clone(),
            child_tools: None,
            cancellation: invocation.cancellation.clone(),
        }
    }

    /// The sealed bounded result of exactly one settled observation.
    fn normalized_result(store: &Store, operation: &OperationRecord) -> Result<serde_json::Value> {
        let evidence = store.external_operation_evidence(operation.operation_id)?;
        ensure!(evidence.len() == 1, "expected one observation, got {}", evidence.len());
        let normalized = evidence[0].normalized_ref.as_ref().context("no bounded result")?;
        Ok(serde_json::from_slice(&store.cas.read_object(&normalized.digest)?)?)
    }

    /// The cell surface over the same durable sessions and the same ledger.
    struct CellHarness {
        store: Arc<Store>,
        handler: ReplCellHandler,
    }

    impl CellHarness {
        fn new(store: Arc<Store>, sessions: Arc<SessionStore>) -> Self {
            Self { store: store.clone(), handler: ReplCellHandler::new(store, sessions) }
        }

        async fn run(&self, input: serde_json::Value) -> Result<OperationRecord> {
            invoke(&self.store, &self.handler.core, &self.handler, "repl.cell",
                "repl-cell-request", input, true).await
        }

        fn normalized(&self, operation: &OperationRecord) -> Result<serde_json::Value> {
            normalized_result(&self.store, operation)
        }
    }

    fn session_id_of(result: &serde_json::Value) -> EntityId {
        result["session_id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .expect("result carries a session id")
    }

    /// One durable journey through denied admission, an observed effect,
    /// duplicate invocation and an armed-before-worker crash. The guest
    /// appends one real byte per execution; recovery must never append again.
    #[test]
    fn native_repl_t1_precedes_effect_and_recovery_never_reexecutes() -> Result<()> {
        tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
            let (harness, root) = Harness::new("t1-before-effect")?;
            let module = harness.data_dir.join("kernels/effect.wat");
            fs::write(&module, r#"
                (module
                  (import "wasi_snapshot_preview1" "path_open"
                    (func $open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
                  (import "wasi_snapshot_preview1" "fd_write"
                    (func $write (param i32 i32 i32 i32) (result i32)))
                  (import "wasi_snapshot_preview1" "fd_close"
                    (func $close (param i32) (result i32)))
                  (memory (export "memory") 1)
                  (data (i32.const 64) "effects")
                  (data (i32.const 80) "x")
                  (func (export "_start")
                    (if (call $open (i32.const 3) (i32.const 0) (i32.const 64)
                        (i32.const 7) (i32.const 1) (i64.const 64) (i64.const 0)
                        (i32.const 1) (i32.const 16)) (then unreachable))
                    (i32.store (i32.const 0) (i32.const 80))
                    (i32.store (i32.const 4) (i32.const 1))
                    (if (call $write (i32.load (i32.const 16)) (i32.const 0)
                        (i32.const 1) (i32.const 8)) (then unreachable))
                    (drop (call $close (i32.load (i32.const 16))))
                    (if (call $write (i32.const 2) (i32.const 0)
                        (i32.const 1) (i32.const 8)) (then unreachable))
                    (i32.store (i32.const 0) (i32.const 128))
                    (i32.store (i32.const 4) (i32.const 16385))
                    (if (call $write (i32.const 1) (i32.const 0)
                        (i32.const 1) (i32.const 8)) (then unreachable))))
            "#)?;

            let create = registered_invocation(&harness.store, &harness.handler.core, &harness.handler,
                "repl.session", "repl-session-request", serde_json::json!({
                    "op": "create", "kernel_id": "python-wasi", "module": module,
                }))?;
            let mut fenced = repeat(&create);
            fenced.task_invocation.task_revision = TaskRevision(1);
            let rejected = harness.handler.execute(fenced).await;
            // The original invocation is rejected by the Store's authority
            // preparation, before its final-transaction stale_task check.
            assert!(matches!(&rejected,
                Err(error) if error.to_string() == "stale or inadmissible T1 invocation"),
                "session T1 must fence the stale task revision, got {rejected:#?}");
            assert!(harness.store.find_external_operation(create.operation_id)?.is_none());
            assert_eq!(fs::read_dir(harness.data_dir.join("sessions"))?.count(), 0);
            let created = harness.handler.execute(repeat(&create)).await?;
            assert_eq!(created.outcome, Some(OperationOutcome::Succeeded));
            let created_id = session_id_of(&harness.normalized(&created)?);
            let create_seal: serde_json::Value = serde_json::from_slice(
                &harness.store.cas.read_object(&created.final_input_ref.digest)?)?;
            assert_eq!(create_seal["binding"]["created_session_id"], serde_json::to_value(created_id)?);
            let duplicate = harness.handler.execute(repeat(&create)).await?;
            assert_eq!(duplicate.prepared_commit, created.prepared_commit);
            assert_eq!(fs::read_dir(harness.data_dir.join("sessions"))?.count(), 1);

            // A raw one-shot WASI session supplies the actual counted effect.
            // This fixture does not claim Python/JS language delivery.
            let provider = crate::session::WasiReplProvider::new(harness.handler.sessions.sessions_root());
            let session = provider.create(crate::contract::CreateSession {
                kernel: crate::kernel::KernelProfile {
                    kernel_id: "effect-guest".into(), runtime: crate::contract::ReplRuntime::Other,
                    runtime_label: "counted WASI effect".into(), backend: crate::contract::BackendKind::WasiPreview1,
                    module: crate::kernel::ModuleSource::ModulePath(module), argv: vec!["effect-guest".into()],
                    env: vec![], workdir_guest: "/home".into(), persistence: crate::kernel::StatePersistence::None,
                    extra_mounts: vec![], dependency_lock: vec![], driver: crate::kernel::KernelDriver::None,
                },
                bind: crate::contract::TaskBinding { task: None, worker: None, scope: Some("T1 effect count".into()) },
                limits: crate::contract::ResourceLimits::default(), preview_bytes: 0,
            })?;
            let session_dir = session.session_dir().to_path_buf();
            let session_id: EntityId = session_dir.file_name().and_then(|name| name.to_str())
                .context("session id")?.parse()?;
            harness.handler.sessions.insert(session)?;
            let cell_handler = ReplCellHandler::new(harness.store.clone(), harness.handler.sessions());
            let cell = registered_invocation(&harness.store, &cell_handler.core, &cell_handler,
                "repl.cell", "repl-cell-request", serde_json::json!({
                    "op": "execute", "session_id": session_id,
                    "source_b64": B64.encode(b"count one effect"), "cell_kind": "declared_effect",
                }))?;
            let effects = session_dir.join("home/effects");
            let mut fenced = repeat(&cell);
            fenced.task_invocation.task_revision = TaskRevision(1);
            let rejected = cell_handler.execute(fenced).await;
            assert!(matches!(&rejected,
                Err(error) if error.to_string() == "stale or inadmissible T1 invocation"),
                "cell T1 must fence the stale task revision, got {rejected:#?}");
            assert!(!effects.exists());
            assert!(harness.store.find_external_operation(cell.operation_id)?.is_none());
            let completed = cell_handler.execute(repeat(&cell)).await?;
            assert_eq!(completed.outcome, Some(OperationOutcome::Succeeded));
            assert_eq!(fs::metadata(&effects)?.len(), 1);
            let result = normalized_result(&harness.store, &completed)?;
            assert_eq!(B64.decode(result["stdout_b64"].as_str().context("stdout preview")?)?,
                vec![0; MAX_PREVIEW_BYTES]);
            assert_eq!(B64.decode(result["stderr_b64"].as_str().context("stderr preview")?)?, b"x");
            assert_eq!(result["stdout_bytes"], MAX_PREVIEW_BYTES + 1);
            assert_eq!(result["truncated"], true);
            let stdout_ref: ObjectRef = serde_json::from_value(result["stdout_ref"].clone())?;
            assert_eq!(harness.store.cas.read_object(&stdout_ref.digest)?, vec![0; MAX_PREVIEW_BYTES + 1]);
            let seal: serde_json::Value = serde_json::from_slice(
                &harness.store.cas.read_object(&completed.final_input_ref.digest)?)?;
            assert_eq!(completed.input_schema_version, 2);
            assert_eq!(seal["request"], cell.input);
            assert_eq!(seal["binding"]["cell"], result["cell"]);
            assert!(seal["binding"].get("seq").is_none());
            let source_ref: ObjectRef = serde_json::from_value(seal["binding"]["source_ref"].clone())?;
            assert_eq!(harness.store.cas.read_object(&source_ref.digest)?, b"count one effect");
            assert_eq!(cell_handler.execute(repeat(&cell)).await?.prepared_commit, completed.prepared_commit);
            let mut different = repeat(&cell);
            different.input["source_b64"] = serde_json::json!(B64.encode(b"another effect"));
            let rejected = cell_handler.execute(different).await;
            assert!(matches!(&rejected,
                Err(error) if error.to_string() == "operation_conflict: original REPL request differs"),
                "a changed source must conflict with its original operation, got {rejected:#?}");
            assert_eq!(fs::metadata(&effects)?.len(), 1);

            // Stop exactly after production preparation and durable T1, before
            // there is any worker result. A new controller cannot infer that the
            // old worker did not run and cannot turn its original admission into
            // a new permission to execute.
            let unknown = registered_invocation(&harness.store, &cell_handler.core, &cell_handler,
                "repl.cell", "repl-cell-request", cell.input.clone())?;
            let (frozen, _, _) = cell_handler.prepare(&unknown)?;
            let (mut armed, mut attempt) = cell_handler.prepare_operation(&unknown, &frozen)?;
            assert!(harness.store.arm_external_operation(
                &unknown.task_invocation, &mut armed, &mut attempt)?.is_none());
            let attempt_id = attempt.attempt_id;
            let data_dir = harness.data_dir.clone();
            drop(cell_handler);
            drop(harness);
            let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
            let sessions = Arc::new(SessionStore::new(data_dir)?);
            let recovered = ReplCellHandler::new(store.clone(), sessions);
            let existing = recovered.execute(repeat(&unknown)).await?;
            assert_eq!(existing.prepared_commit, armed.prepared_commit);
            assert_eq!(existing.effect_status, EffectStatus::Unknown);
            let reconciled = recovered.reconcile(&unknown.task, unknown.operation_id).await?;
            assert_eq!(reconciled.effect_status, EffectStatus::Unknown);
            assert!(reconciled.outcome.is_none());
            assert_eq!(store.external_operation_attempt(unknown.operation_id)?.attempt_id, attempt_id);
            let evidence = store.external_operation_evidence(unknown.operation_id)?;
            assert_eq!(evidence.len(), 1);
            assert_eq!(evidence[0].attempt_id, Some(attempt_id));
            assert_eq!(recovered.execute(repeat(&cell)).await?.prepared_commit, completed.prepared_commit);
            assert_eq!(fs::metadata(&effects)?.len(), 1);
            drop(recovered);
            drop(store);
            fs::remove_dir_all(root)?;
            Ok(())
        })
    }

    /// Every advertised session op is dispatched with its own arguments:
    /// `op` is the discriminator, never the whole request. `create` and the
    /// session-targeted ops succeed against the durable session journal;
    /// `install_dependencies` records the kernel's own installer refusal
    /// (resolved, never a parse rejection).
    #[tokio::test]
    async fn advertised_session_operations_dispatch_with_their_arguments() -> Result<()> {
        let (harness, root) = Harness::new("ops")?;
        let module = harness.data_dir.join("kernels").join("python.wat");
        fs::write(&module, RESIDENT_GUEST)?;
        let wheel = harness.data_dir.join("wheels").join("pkg-1.0-py3-none-any.whl");
        fs::write(&wheel, b"not-a-wheel")?;

        // list_kernels: zero arguments, real named targets.
        let op = harness.run(serde_json::json!({"op": "list_kernels"})).await?;
        assert_eq!(op.state, OperationState::Resolved);
        assert_eq!(op.outcome, Some(OperationOutcome::Succeeded));
        let result = harness.normalized(&op)?;
        assert!(result["targets"].as_array().context("targets")?.iter()
            .any(|target| target["name"] == "python-wasi"), "{result}");

        // create with kernel_id + module + limits: a real durable session.
        let op = harness.run(serde_json::json!({
            "op": "create",
            "kernel_id": "python-wasi",
            "module": module,
            "limits": {"max_wall_clock_ms": 60000},
        })).await?;
        assert_eq!(op.state, OperationState::Resolved);
        assert_eq!(op.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(op.effect_status, EffectStatus::Complete);
        let result = harness.normalized(&op)?;
        assert_eq!(result["outcome"], "created");
        assert_eq!(result["kernel_id"], "python-wasi");
        let session_id = session_id_of(&result);
        let sid = serde_json::json!(session_id);

        // Every session-targeted op reaches the durable session by its id.
        for (op_name, expected_outcome) in [
            ("inspect", "Ready"),
            ("checkpoint", "R1ObjectCheckpoint"),
            ("restore", "restored"),
            ("reset", "reset"),
        ] {
            let op = harness
                .run(serde_json::json!({"op": op_name, "session_id": sid.clone()}))
                .await?;
            assert_eq!(op.state, OperationState::Resolved, "{op_name}");
            assert_eq!(op.outcome, Some(OperationOutcome::Succeeded), "{op_name}");
            if op_name == "inspect" || op_name == "checkpoint" {
                assert_eq!(op.effect_status, EffectStatus::None, "{op_name}");
            } else {
                assert_eq!(op.effect_status, EffectStatus::Complete, "{op_name}");
            }
            let result = harness.normalized(&op)?;
            assert_eq!(result["session_id"], session_id.to_string(), "{op_name}");
            assert_eq!(result["outcome"], expected_outcome, "{op_name}");
        }

        // cancel reports the session's real run state, not an error.
        let op = harness
            .run(serde_json::json!({"op": "cancel", "session_id": sid.clone()}))
            .await?;
        assert_eq!(op.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(harness.normalized(&op)?["outcome"], "nothing_running");

        // fork returns the new session; dispose then retires it by id.
        let op = harness
            .run(serde_json::json!({"op": "fork", "session_id": sid.clone()}))
            .await?;
        assert_eq!(op.outcome, Some(OperationOutcome::Succeeded));
        let forked = session_id_of(&harness.normalized(&op)?);
        assert_ne!(forked, session_id);
        let op = harness
            .run(serde_json::json!({"op": "dispose", "session_id": forked}))
            .await?;
        assert_eq!(op.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(harness.normalized(&op)?["outcome"], "disposed");

        // install_dependencies accepts its own argument set (session_id +
        // wheels) and records the kernel's real installer outcome — the
        // CPython-WASI route requires ABI tags observed from a live cell, so
        // the honest settled result is the installer's refusal, not a parse
        // rejection and never a fabricated session.
        let op = harness.run(serde_json::json!({
            "op": "install_dependencies",
            "session_id": sid.clone(),
            "wheels": [wheel],
        })).await?;
        assert_eq!(op.state, OperationState::Resolved);
        assert_eq!(op.outcome, Some(OperationOutcome::Failed));
        // A refused install committed no environment — none, not unknown.
        assert_eq!(op.effect_status, EffectStatus::None);
        let result = harness.normalized(&op)?;
        assert_eq!(result["session_id"], session_id.to_string());
        assert_eq!(result["outcome"], "refused", "{result}");
        assert!(result["error"].as_str().context("install error")?
            .contains("REPL_INSTALL_FAILED"), "{result}");

        // An unknown session is an explicit refusal: no session is invented.
        let op = harness.run(serde_json::json!({
            "op": "cancel",
            "session_id": EntityId::new(),
        })).await?;
        assert_eq!(op.state, OperationState::Resolved);
        assert_eq!(op.outcome, Some(OperationOutcome::Failed));
        assert_eq!(harness.normalized(&op)?["outcome"], "refused");

        // The same resident session now owns a real cell that never writes a
        // report. Cancelling a restore queued behind it must retire only that
        // invocation, and cancelling the cell must close/join its blocked
        // stdin interpreter before reporting a stopped cell.
        let cells = Arc::new(ReplCellHandler::new(harness.store.clone(), harness.handler.sessions()));
        let running = registered_invocation(&harness.store, &cells.core, cells.as_ref(),
            "repl.cell", "repl-cell-request", serde_json::json!({
                "op": "execute", "session_id": session_id,
                "source_b64": B64.encode(b"resident fixture waits for cancellation"),
                "cell_kind": "unknown",
            }))?;
        let running_stop = running.cancellation.clone();
        let running = tokio::spawn(async move { cells.execute(running).await });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let slot = harness.handler.sessions.cancellations.lock().get(&session_id).cloned();
                if slot.is_some_and(|slot| slot.lock().is_some()) { break; }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await.context("resident cell did not acquire its guest")?;
        let waiting = registered_invocation(&harness.store, &harness.handler.core, &harness.handler,
            "repl.session", "repl-session-request", serde_json::json!({
                "op": "restore", "session_id": session_id,
            }))?;
        let waiting_id = waiting.operation_id;
        let waiting_stop = waiting.cancellation.clone();
        let (waiting_result, running_was_owned, guest_was_cancelled) = {
            let waiting = harness.handler.execute(waiting);
            tokio::pin!(waiting);
            tokio::select! {
                _result = &mut waiting => panic!("restore bypassed the executing cell: {_result:?}"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(40)) => {},
            }
            waiting_stop.cancel();
            let waiting_result = tokio::time::timeout(std::time::Duration::from_secs(2), &mut waiting).await;
            let running_was_owned = !running.is_finished();
            let guest_was_cancelled = harness.handler.sessions.cancellations.lock().get(&session_id)
                .is_some_and(|slot| slot.lock().as_ref().is_some_and(shenko_wasm::WasiCancellation::is_cancelled));
            (waiting_result, running_was_owned, guest_was_cancelled)
        };
        running_stop.cancel();
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(3), running).await
            .context("cancelled resident interpreter was not joined")???;
        let waiting_error = waiting_result.context("queued restore did not observe cancellation")?
            .expect_err("queued restore must retain its already armed uncertain attempt");
        assert!(waiting_error.to_string().contains("REPL_EXECUTION_CANCELLED"), "{waiting_error:#}");
        assert!(running_was_owned && !guest_was_cancelled,
            "cancelling a queued operation must not cancel the current interpreter");
        assert_eq!(harness.store.external_operation(waiting_id)?.effect_status, EffectStatus::Unknown);
        assert_eq!(cancelled.outcome, Some(OperationOutcome::Cancelled));
        let session = harness.handler.sessions.open(session_id)?;
        let info = session.lock().map_err(|_| anyhow::anyhow!("test session poisoned"))?.inspect()?;
        assert!(!info.live_process_running, "Cancelled is published only after the actual interpreter join");

        drop(harness);
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    /// The strict argument contract still holds: genuinely unknown fields and
    /// missing required arguments refuse before any session work; the refusal
    /// is either a recorded resolved failure or an immediate REPL_* error, in
    /// each case with an explicit reason.
    #[tokio::test]
    async fn session_ops_refuse_unknown_fields_and_missing_arguments() -> Result<()> {
        let (harness, root) = Harness::new("refusals")?;
        let module = harness.data_dir.join("kernels").join("python.wat");
        fs::write(&module, "(module)")?;

        // A stray field on a zero-argument op refuses.
        match harness.run(serde_json::json!({"op": "list_kernels", "verbose": true})).await {
            Err(error) => {
                let message = format!("{error:#}");
                assert!(message.contains("REPL_SESSION_INVALID")
                    && message.contains("unknown field `verbose`"), "{message}");
            }
            Ok(op) => panic!("stray field dispatched: {op:?}"),
        }

        // An unknown field on a session-targeted op refuses.
        match harness.run(serde_json::json!({
            "op": "inspect", "session_id": EntityId::new(), "bogus": 1,
        })).await {
            Err(error) => {
                let message = format!("{error:#}");
                assert!(message.contains("REPL_SESSION_INVALID")
                    && message.contains("unknown field `bogus`"), "{message}");
            }
            Ok(op) => panic!("unknown field dispatched: {op:?}"),
        }

        // A missing required argument on a session-targeted op refuses
        // without touching any session.
        match harness.run(serde_json::json!({"op": "inspect"})).await {
            Err(error) => {
                let message = format!("{error:#}");
                assert!(message.contains("REPL_SESSION_INVALID")
                    && message.contains("missing field `session_id`"), "{message}");
            }
            Ok(op) => panic!("partial inspect dispatched: {op:?}"),
        }
        match harness.run(serde_json::json!({"op": "install_dependencies"})).await {
            Err(error) => {
                let message = format!("{error:#}");
                assert!(message.contains("REPL_SESSION_INVALID")
                    && message.contains("missing field `session_id`"), "{message}");
            }
            Ok(op) => panic!("partial install dispatched: {op:?}"),
        }

        // create without its required arguments is an explicit recorded
        // refusal (REPL_KERNEL_INVALID), never a fabricated session.
        let op = harness.run(serde_json::json!({"op": "create"})).await?;
        assert_eq!(op.state, OperationState::Resolved);
        assert_eq!(op.outcome, Some(OperationOutcome::Failed));
        let result = harness.normalized(&op)?;
        assert_eq!(result["outcome"], "refused");
        assert!(result["session_id"].is_null(), "{result}");
        assert!(result["error"].as_str().context("refusal error")?
            .contains("REPL_KERNEL_INVALID"), "{result}");

        // An unknown op variant refuses; an unknown op together with a
        // fabricated session id never opens a session.
        match harness.run(serde_json::json!({"op": "fabricate_session"})).await {
            Err(error) => {
                let message = format!("{error:#}");
                assert!(message.contains("REPL_SESSION_INVALID")
                    && message.contains("unknown variant"), "{message}");
            }
            Ok(op) => panic!("unknown op dispatched: {op:?}"),
        }

        drop(harness);
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    /// A durable kernel binding is the replaceable-kernel entry: install
    /// it once, discover it from `list_kernels`, create a session from the
    /// name, and fail closed when the artifact no longer matches. The guest
    /// below is the resident synthetic one, so this proves the binding and
    /// session-admission path — never CPython/Node/Bun language delivery.
    #[tokio::test]
    async fn kernel_binding_is_installed_listed_and_created_by_name() -> Result<()> {
        let (harness, root) = Harness::new("entry")?;
        let module = harness.data_dir.join("kernels").join("python.wat");
        fs::write(&module, RESIDENT_GUEST)?;

        let op = harness.run(serde_json::json!({
            "op": "install_kernel",
            "entry": "python-3.13",
            "target": "python-wasi",
            "module": module,
        })).await?;
        assert_eq!(op.state, OperationState::Resolved);
        assert_eq!(op.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(op.effect_status, EffectStatus::Complete);
        let result = harness.normalized(&op)?;
        assert_eq!(result["outcome"], "installed");
        assert_eq!(result["kernel_id"], "python-3.13");
        assert_eq!(result["session_model"], "LiveProcess");
        assert_eq!(result["recovery_level"], "R1ObjectCheckpoint");

        // Discovery reports the binding beside the named targets: available
        // and unavailable targets stay listed, and the installed entry carries
        // its own artifact state.
        let op = harness.run(serde_json::json!({"op": "list_kernels"})).await?;
        let result = harness.normalized(&op)?;
        let entry = result["entries"].as_array().context("entries")?
            .iter()
            .find(|entry| entry["ok"]["entry"]["entry"] == "python-3.13")
            .with_context(|| format!("installed binding missing from {result}"))?;
        assert_eq!(entry["ok"]["entry"]["target"], "python-wasi");
        assert_eq!(entry["ok"]["artifact"], "size_matches");
        let targets = result["targets"].as_array().context("targets")?;
        assert!(targets.iter().any(|target| target["name"] == "node-wasm"), "{result}");
        assert!(targets.iter().any(|target| {
            target["name"] == "node-wasm"
                && target["availability"].as_str().unwrap_or_default().starts_with("unavailable")
        }), "{result}");

        // create resolves the binding: same profile, same admission, and a
        // real durable session — the caller never names the interpreter path.
        let op = harness.run(serde_json::json!({"op": "create", "entry": "python-3.13"})).await?;
        assert_eq!(op.outcome, Some(OperationOutcome::Succeeded));
        let result = harness.normalized(&op)?;
        assert_eq!(result["outcome"], "created");
        assert_eq!(result["kernel_id"], "python-3.13");
        let session_id = session_id_of(&result);
        let op = harness
            .run(serde_json::json!({"op": "inspect", "session_id": session_id}))
            .await?;
        let inspected = harness.normalized(&op)?;
        assert_eq!(inspected["kernel_id"], "python-3.13");
        assert_eq!(inspected["outcome"], "Ready", "{inspected}");

        // A same-length replacement is a different interpreter: create
        // re-verifies the digest, refuses explicitly, and creates no session.
        let sessions_before = fs::read_dir(harness.data_dir.join("sessions"))?.count();
        fs::write(&module, RESIDENT_GUEST.replace("i32.const 512", "i32.const 513"))?;
        let op = harness.run(serde_json::json!({"op": "create", "entry": "python-3.13"})).await?;
        assert_eq!(op.state, OperationState::Resolved);
        assert_eq!(op.outcome, Some(OperationOutcome::Failed));
        assert_eq!(op.effect_status, EffectStatus::None);
        let result = harness.normalized(&op)?;
        assert_eq!(result["outcome"], "refused");
        assert!(result["session_id"].is_null(), "{result}");
        assert!(result["error"].as_str().context("error")?
            .contains("REPL_ENTRY_DIGEST_MISMATCH"), "{result}");
        assert_eq!(fs::read_dir(harness.data_dir.join("sessions"))?.count(), sessions_before);

        drop(harness);
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    /// Every binding refusal is explicit, publishes nothing, and never
    /// substitutes another runtime for a named-but-unavailable target.
    #[tokio::test]
    async fn binding_refusals_stay_explicit_and_publish_nothing() -> Result<()> {
        let (harness, root) = Harness::new("entry-refusals")?;
        let module = harness.data_dir.join("kernels").join("node.wat");
        fs::write(&module, RESIDENT_GUEST)?;
        // A native binding executes its artifact, so admission requires the
        // execute bit (kernel::validate_module_mode).
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&module, fs::Permissions::from_mode(0o755))?;
        }
        let outside = root.join("outside.wat");
        fs::write(&outside, RESIDENT_GUEST)?;

        let refusals = [
            // node-wasm is a named target with no verified WASM binding; the
            // native Node route is a different target, never a substitute.
            (serde_json::json!({"op": "install_kernel", "entry": "node", "target": "node-wasm",
                "module": module}), "REPL_KERNEL_UNAVAILABLE"),
            // The artifact must already live in the admitted kernels tree.
            (serde_json::json!({"op": "install_kernel", "entry": "node", "target": "node-native-host",
                "module": outside}), "REPL_KERNEL_UNAVAILABLE"),
            // An entry name cannot escape its directory.
            (serde_json::json!({"op": "install_kernel", "entry": "../node", "target": "node-native-host",
                "module": module}), "REPL_ENTRY_INVALID"),
            // No binding exists under that name.
            (serde_json::json!({"op": "create", "entry": "no-such-kernel"}),
                "REPL_ENTRY_UNAVAILABLE"),
            // One binding per create, never two.
            (serde_json::json!({"op": "create", "entry": "node", "kernel_id": "node-native-host",
                "module": module}), "REPL_KERNEL_INVALID"),
        ];
        for (input, expected) in refusals {
            let op = harness.run(input.clone()).await?;
            assert_eq!(op.state, OperationState::Resolved, "{input}");
            assert_eq!(op.outcome, Some(OperationOutcome::Failed), "{input}");
            assert_eq!(op.effect_status, EffectStatus::None, "{input}");
            let result = harness.normalized(&op)?;
            assert_eq!(result["outcome"], "refused", "{input}");
            assert!(result["error"].as_str().context("error")?.contains(expected),
                "{input}: {result}");
        }
        // Nothing was published by any refusal.
        let op = harness.run(serde_json::json!({"op": "list_kernels"})).await?;
        assert!(harness.normalized(&op)?["entries"].as_array().context("entries")?.is_empty());

        // A published binding is replaced only on explicit request, and the
        // replacement is the single remaining binding.
        let op = harness.run(serde_json::json!({
            "op": "install_kernel", "entry": "node", "target": "node-native-host", "module": module,
        })).await?;
        assert_eq!(op.outcome, Some(OperationOutcome::Succeeded));
        let op = harness.run(serde_json::json!({
            "op": "install_kernel", "entry": "node", "target": "node-native-host", "module": module,
        })).await?;
        assert_eq!(op.outcome, Some(OperationOutcome::Failed));
        assert!(harness.normalized(&op)?["error"].as_str().context("error")?
            .contains("REPL_ENTRY_EXISTS"));
        let op = harness.run(serde_json::json!({
            "op": "install_kernel", "entry": "node", "target": "node-native-host", "module": module,
            "replace": true,
        })).await?;
        assert_eq!(op.outcome, Some(OperationOutcome::Succeeded));
        let op = harness.run(serde_json::json!({"op": "list_kernels"})).await?;
        let entries = harness.normalized(&op)?["entries"].as_array().cloned().context("entries")?;
        assert_eq!(entries.len(), 1, "{entries:?}");

        drop(harness);
        fs::remove_dir_all(root).ok();
        Ok(())
    }

    /// A restarted caller can enumerate the durable sessions it may resume.
    /// Listing validates manifests only — no interpreter is opened, no journal
    /// is repaired — and a corrupt session directory is reported rather than
    /// disappearing from the list.
    #[tokio::test]
    async fn durable_sessions_are_listed_for_resumption() -> Result<()> {
        let (harness, root) = Harness::new("sessions")?;
        let module = harness.data_dir.join("kernels").join("python.wat");
        fs::write(&module, RESIDENT_GUEST)?;
        let op = harness.run(serde_json::json!({
            "op": "install_kernel", "entry": "python-3.13", "target": "python-wasi", "module": module,
        })).await?;
        assert_eq!(op.outcome, Some(OperationOutcome::Succeeded));
        let op = harness.run(serde_json::json!({"op": "create", "entry": "python-3.13"})).await?;
        let created = session_id_of(&harness.normalized(&op)?);

        // A fresh view over the same data directory — what a restarted
        // controller has — reports the session from its durable manifest
        // without opening an interpreter or resuming anything.
        let durable = crate::session::list_sessions(&harness.data_dir.join("sessions"))?;
        assert!(
            durable.iter().any(|listing| matches!(
                listing,
                crate::session::SessionListing::Ok(summary)
                    if summary.session_id == created && summary.kernel_id == "python-3.13"
            )),
            "{durable:?}"
        );
        let op = harness.run(serde_json::json!({"op": "list_sessions"})).await?;
        assert_eq!(op.state, OperationState::Resolved);
        assert_eq!(op.effect_status, EffectStatus::None);
        let result = harness.normalized(&op)?;
        let listed = result["sessions"].as_array().context("sessions")?
            .iter()
            .find(|session| session["ok"]["session_id"] == created.to_string())
            .with_context(|| format!("session missing from {result}"))?;
        assert_eq!(listed["ok"]["kernel_id"], "python-3.13");
        assert!(listed["ok"]["disposed_unix_ms"].is_null());
        // The durable session is bound to the exact bytes the installed
        // binding records — that is what makes it resumable against a known
        // runtime, and it is a fact, not a rendering of an enum.
        let op = harness.run(serde_json::json!({"op": "list_kernels"})).await?;
        let bound = harness.normalized(&op)?;
        let binding = bound["entries"].as_array().context("entries")?
            .iter()
            .find(|entry| entry["ok"]["entry"]["entry"] == "python-3.13")
            .map(|entry| &entry["ok"]["entry"]["module_blake3"])
            .with_context(|| format!("binding missing from {bound}"))?;
        assert_eq!(&listed["ok"]["module_blake3"], binding, "{listed}");
        assert!(listed["ok"]["environment_generation"]
            .as_str()
            .is_some_and(|generation| !generation.is_empty()));

        // A directory that is not a session is reported, never hidden.
        let corrupt = harness.data_dir.join("sessions").join(EntityId::new().to_string());
        fs::create_dir_all(&corrupt)?;
        fs::write(corrupt.join(crate::journal::SESSION_MANIFEST), b"{\"schema_version\":3}")?;
        let op = harness.run(serde_json::json!({"op": "list_sessions"})).await?;
        let result = harness.normalized(&op)?;
        assert!(result["sessions"].as_array().context("sessions")?.iter()
            .any(|session| session["corrupt"]["session"]
                == corrupt.file_name().unwrap().to_string_lossy().as_ref()), "{result}");

        drop(harness);
        fs::remove_dir_all(root).ok();
        Ok(())
    }
    /// Admission is the only permission to run real interpreter work. Without
    /// a committed task T1 refuses; neither a session create nor a cell may
    /// touch the external session journal. Before the T1-order fix the first
    /// create happened and the cell ran before the late arm was rejected.
    #[test]
    fn refused_t1_does_not_create_a_session_or_execute_a_cell() -> Result<()> {
        tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
            let (harness, root) = Harness::new("repl-before-t1")?;
            let module = harness.data_dir.join("kernels").join("python.wat");
            fs::write(&module, RESIDENT_GUEST)?;
            let create = serde_json::json!({
                "op": "create", "kernel_id": "python-wasi", "module": module,
                "limits": {"max_wall_clock_ms": 500}
            });

            // The uncommitted task exists only in this invocation's memory, not
            // in the authoritative Store. The attempted create must stop at T1.
            let refused = invoke(&harness.store, &harness.handler.core, &harness.handler,
                "repl.session", "repl-session-request", create.clone(), false).await;
            assert!(refused.is_err(), "uncommitted task must not acquire T1");
            assert_eq!(fs::read_dir(harness.data_dir.join("sessions"))?.count(), 0,
                "T1 refusal cannot create even one durable session");

            let created = harness.run(create).await?;
            assert_eq!(created.outcome, Some(OperationOutcome::Succeeded));
            let session_id = session_id_of(&harness.normalized(&created)?);
            let sessions = harness.handler.sessions();
            let cells_before = sessions.open(session_id)?.lock()
                .map_err(|_| anyhow::anyhow!("test session lock poisoned"))?.inspect()?.cells_executed;

            let cell = CellHarness::new(harness.store.clone(), Arc::clone(&sessions));
            let input = serde_json::json!({
                "op": "execute", "session_id": session_id,
                "source_b64": B64.encode(b"print(7)"),
                "cell_kind": "declared_effect"
            });
            let refused = invoke(&cell.store, &cell.handler.core, &cell.handler,
                "repl.cell", "repl-cell-request", input, false).await;
            assert!(refused.is_err(), "uncommitted cell must not acquire T1");
            let cells_after = sessions.open(session_id)?.lock()
                .map_err(|_| anyhow::anyhow!("test session lock poisoned"))?.inspect()?.cells_executed;
            assert_eq!(cells_after, cells_before,
                "a rejected cell must not mutate the original session journal");

            drop(cell);
            drop(sessions);
            drop(harness);
            fs::remove_dir_all(root).ok();
            Ok(())
        })
    }

}
