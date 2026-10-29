//! The §19/§19.1 REPL contract shared by every kernel backend.
//!
//! This crate ships one real backend (`session::WasiReplProvider`); the types
//! here are runtime-neutral so a Pyodide profile, a boxed native kernel or an
//! SSH kernel can implement the same session semantics without pretending to
//! be the same runtime.

use serde::{Deserialize, Serialize};
use shenko_types::ids::{EntityId, RuntimeGeneration};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Actual interpreter/runtime family a kernel runs on (SPEC §19.1: report the
/// real language/runtime/ABI; QuickJS is not Node, a JS engine is not Bun).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplRuntime {
    /// CPython compiled to a WASI target (e.g. `python.wasm`, preview1).
    CPythonWasi,
    /// Pyodide/Emscripten build of CPython; a different ABI route that needs a
    /// JS-host or an Emscripten-WASI capable backend. Not served by this crate.
    Pyodide,
    /// Node.js itself compiled to WASM. No verified upstream target is bound.
    NodeWasm,
    /// Bun itself compiled to WASM. No verified upstream target exists.
    BunWasm,
    /// A JavaScript engine embedded in a WASM module (Javy/QuickJS style).
    /// JavaScript-in-WASM, explicitly not Node or Bun.
    JsWasm,
    /// Any other real runtime; `runtime_label` in the kernel profile carries
    /// the human-readable identity.
    Other,
}

/// Host-side execution environment of a kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// WASI preview1 command module run by `shenko-wasm` (wasmtime).
    WasiPreview1,
    /// A native host process kernel (not implemented by this crate).
    NativeHost,
    /// A kernel reached over an SSH seat (not implemented by this crate).
    RemoteSsh,
    /// Emscripten/JS-host runtime route (not implemented by this crate).
    EmscriptenHost,
}

/// Recovery level a backend actually guarantees for a session (§19 R0–R2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryLevel {
    /// Cell source, outputs, outcomes, order and environment metadata are
    /// durable; nothing is claimed about live interpreter objects.
    R0WorkRecord,
    /// Explicitly serialized objects can also be restored by the kernel.
    R1ObjectCheckpoint,
    /// Full resumable VM state for a specific engine/module/ABI, including
    /// external-resource rebinding. Copying linear memory does not reach R2.
    R2VmState,
}

/// Caller-declared replay boundary of a cell (§19: replaying code to rebuild
/// state must never re-execute an external effect that already happened).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellKind {
    /// No declared external effect; safe to re-execute during recovery.
    Pure,
    /// Carries an external effect (message, remote write, payment…). Never
    /// re-executed by recovery; its durable record plus an optional receipt
    /// reference is the result substitute.
    DeclaredEffect,
    /// The caller cannot classify the cell. Recovery never replays it and
    /// reports it as needing reconciliation instead of guessing.
    Unknown,
}

/// Declared resource limits. The WASI backend currently cannot enforce any
/// of them; `inspect` returns them with `enforced: false` rather than
/// pretending a guard exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub max_wall_clock_ms: Option<u64>,
    pub max_fuel: Option<u64>,
    pub max_memory_bytes: Option<u64>,
    /// Always false until a real mechanism enforces the values above.
    #[serde(default)]
    pub enforced: bool,
}

/// Binding of a session to its delegating context (§19: each REPL binds a
/// task, worker and scope; free-form fields keep this adapter-neutral).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBinding {
    pub task: Option<EntityId>,
    pub worker: Option<EntityId>,
    pub scope: Option<String>,
}

/// Output declared by the caller for preview purposes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamPreview {
    /// Lossy prefix of the stream, at most `preview_bytes` of it.
    pub text: String,
    /// Total bytes in the durable stream file.
    pub total_bytes: u64,
    /// Whether `text` does not cover the whole stream.
    pub truncated: bool,
    /// BLAKE3 hex of the full stream file, so a reader can verify readback.
    pub blake3: String,
    /// Session-relative path of the durable stream file.
    pub path: PathBuf,
}

/// What actually happened when one cell ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellOutcome {
    /// `_start` returned normally (exit code 0).
    Completed { exit_code: i32 },
    /// The guest called `proc_exit(n)` with `n != 0`; a normal interpreter
    /// "cell raised" path, still a completed record.
    Exited { exit_code: i32 },
    /// The guest trapped or the host failed to run it; `error` is the real
    /// failure text. Stream bytes written before the trap are still durable.
    Failed { error: String },
}

/// Durable record of one executed cell (the R0 journal line).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellRecord {
    /// "cell"; distinguishes record kinds in one JSONL journal.
    pub kind: String,
    /// Journal schema for independent readers; bump on incompatible change.
    pub schema_version: u32,
    pub session_id: EntityId,
    /// 1-based monotonic position within the session's cell history.
    pub seq: u64,
    pub cell: EntityId,
    /// Replay boundary declared at execution time; never edited afterwards.
    pub cell_kind: CellKind,
    /// BLAKE3 hex of the exact source bytes fed to stdin.
    pub source_blake3: String,
    pub source_bytes: u64,
    /// Session-relative path of the durable source file.
    pub source_path: PathBuf,
    pub outcome: CellOutcome,
    pub stdout_blake3: String,
    pub stdout_bytes: u64,
    pub stderr_blake3: String,
    pub stderr_bytes: u64,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    /// BLAKE3 of the interpreter module actually executed for this cell.
    pub module_blake3: String,
    pub environment_generation: RuntimeGeneration,
    pub wall_clock_ms: u64,
    pub completed_unix_ms: i64,
}

/// Result returned to the caller of `execute`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellResult {
    pub record: CellRecord,
    pub stdout: StreamPreview,
    pub stderr: StreamPreview,
}

/// Session snapshot for `inspect`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: EntityId,
    pub kernel_id: String,
    pub runtime: ReplRuntime,
    pub backend: BackendKind,
    pub state: SessionState,
    /// Highest recovery level this backend can deliver for this session.
    pub recovery_level: RecoveryLevel,
    /// True only when guest interpreter state outlives one cell process.
    /// Always false for the process-per-cell WASI backend.
    pub interpreter_state_persists: bool,
    pub cells_executed: u64,
    pub environment_generation: RuntimeGeneration,
    pub bind: TaskBinding,
    pub limits: ResourceLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Ready,
    Busy,
    Disposed,
}

/// An R0 checkpoint is a durable citation of the journal; nothing here claims
/// interpreter objects survive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplCheckpoint {
    pub checkpoint: EntityId,
    pub session_id: EntityId,
    pub level: RecoveryLevel,
    /// BLAKE3 of the journal file at checkpoint time.
    pub journal_blake3: String,
    pub cells: u64,
    /// Per-entry blake3 of the interpreter-state files the kernel declared.
    pub state_files: BTreeMap<String, String>,
    pub created_unix_ms: i64,
}

/// What recovery decided for each journaled cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellRecovery {
    /// Pure cell re-executed; `replayed_as` is the replay record index.
    Replayed { replayed_as: u64 },
    /// Declared-effect cell preserved as a record; its effect is not rerun.
    ReceiptOnly,
    /// Unclassifiable cell left for reconciliation; not replayed.
    NeedsReconciliation,
    /// Record could not be trusted (missing/corrupt stream or source file).
    Corrupt { detail: String },
}

/// Per-cell entry of a recovery report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoverEntry {
    pub cell: EntityId,
    pub seq: u64,
    pub cell_kind: CellKind,
    pub recovery: CellRecovery,
}

/// Result of `restore`: which cells rebuilt state by replay and which are
/// receipts or gaps. External effects are never re-executed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoverReport {
    pub session_id: EntityId,
    pub entries: Vec<RecoverEntry>,
    /// Byte offset of a torn trailing journal line that was ignored, if any.
    pub torn_tail_at: Option<u64>,
}

/// Honest answer to `cancel`: the WASI preview1 backend has no interrupt
/// mechanism, so a running cell cannot be stopped through this interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelOutcome {
    /// The named cell is executing and nothing can interrupt it.
    NotInterruptible { cell: EntityId },
    /// No cell is currently executing; nothing to cancel.
    NothingRunning,
    /// The session is disposed.
    Disposed,
}

/// Honest answer to `install_dependencies` (R083 surface): no package
/// installer is bound, so the call fails instead of fabricating a lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallOutcome {
    /// The kernel backend has no installer; generation is unchanged.
    Unsupported { reason: String },
}

/// One cell to execute.
#[derive(Debug, Clone)]
pub struct CellRequest {
    /// Exact bytes delivered to the guest's stdin as the cell program.
    pub source: Vec<u8>,
    /// Replay boundary declared by the caller; `Unknown` is the honest
    /// default and is never silently upgraded to `Pure`.
    pub cell_kind: CellKind,
    /// Free-form labels for the caller's own bookkeeping (task, worker…).
    pub labels: BTreeMap<String, String>,
}

/// Request to create a session.
#[derive(Debug, Clone)]
pub struct CreateSession {
    pub kernel: crate::kernel::KernelProfile,
    pub bind: TaskBinding,
    pub limits: ResourceLimits,
    /// Upper bound for the stdout/stderr preview text in `CellResult`.
    pub preview_bytes: usize,
}

/// Provider contract (§19). `restore`/`fork` return boxed sessions so a
/// recovered session can come back through any conforming backend.
pub trait ReplProvider {
    type Session: ReplSession;
    /// Report every named kernel runtime and its actual binding state.
    fn kernels(&self) -> Vec<crate::kernel::KernelTarget>;
    /// Create a session. Fails with a stable error when the kernel's module
    /// is not bound/readable — never falls back to a different runtime.
    fn create(&self, request: CreateSession) -> anyhow::Result<Self::Session>;
    /// Reopen an existing session directory without replaying cells.
    fn reopen(&self, session_dir: &std::path::Path) -> anyhow::Result<Self::Session>;
}

/// One live REPL session.
pub trait ReplSession {
    /// Execute one cell on the real backend and journal it durably.
    fn execute(&mut self, request: CellRequest) -> anyhow::Result<CellResult>;
    /// Current session state and honest capability report.
    fn inspect(&self) -> anyhow::Result<SessionInfo>;
    /// Durable R0 citation of the journal plus declared state-file digests.
    fn checkpoint(&mut self) -> anyhow::Result<ReplCheckpoint>;
    /// Rebuild this session's interpreter state by re-executing journaled
    /// Pure cells. Declared-effect cells keep their durable record as the
    /// receipt; Unknown cells are reconciliation gaps. External effects are
    /// never re-executed (§19 replay boundary, A15).
    fn restore(&mut self) -> anyhow::Result<RecoverReport>;
    /// Create a new session whose guest-visible state is a copy of this
    /// session's; the new session keeps its own journal from scratch.
    fn fork(&self, sessions_root: &std::path::Path) -> anyhow::Result<Box<dyn ReplSession>>;
    /// Report the real interrupt ability; never claims work stopped.
    fn cancel(&self) -> anyhow::Result<CancelOutcome>;
    /// Remove kernel-declared interpreter state files; journaled work is
    /// retained. Sessions without declared state files fail, never pretend.
    fn reset(&mut self) -> anyhow::Result<()>;
    /// R083 surface; currently always reports Unsupported.
    fn install_dependencies(
        &mut self,
        packages: &[crate::kernel::PackageSpec],
    ) -> anyhow::Result<InstallOutcome>;
    /// End the session; journal and outputs stay on disk for recovery.
    fn dispose(&mut self) -> anyhow::Result<()>;
    fn session_dir(&self) -> &std::path::Path;
}
