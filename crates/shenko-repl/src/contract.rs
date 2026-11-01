//! The §19/§19.1 REPL contract shared by every kernel backend.
//!
//! Two real session models exist behind one contract:
//!
//! * `SessionModel::LiveProcess` — the session owns a real long-lived
//!   interpreter process (a persistent WASI guest or a native host process).
//!   While it lives, variables, functions, modules and loaded packages stay
//!   in interpreter memory; cells are delivered on a framed stdin protocol
//!   and per-cell streams land in guest-visible files. Interrupting a cell
//!   kills the process: effects already committed stand, the session reports
//!   recovery-required, and `restore()` rebuilds a fresh process from the
//!   journaled work record. This is the model driver-bound kernels use.
//! * `SessionModel::OneShot` — each cell runs as one independent process
//!   (raw WASI kernels without a driver). Live variables do not survive
//!   between cells; recovery replays journaled `Pure` cells to rebuild the
//!   declared state file. This is an R0 work record, labeled as such.
//!
//! Recovery levels stay the §19 ladder and are claimed per kernel, never per
//! wish: R0 work record, R1 explicitly serialized objects (driver state
//! file), R2 full resumable VM state (not implemented by any kernel here).

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
    /// A native host process kernel (the additional native route; it never
    /// satisfies a WASM target).
    NativeHost,
    /// A kernel reached over an SSH seat (not implemented by this crate).
    RemoteSsh,
    /// Emscripten/JS-host runtime route (not implemented by this crate).
    EmscriptenHost,
}

/// How a session actually keeps interpreter state between cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionModel {
    /// One process per cell; live variables do not survive between cells.
    /// Recovery replays journaled `Pure` cells. R0 work record only.
    OneShot,
    /// The session owns one real long-lived interpreter process; variables,
    /// closures and loaded modules persist in interpreter memory while it
    /// lives, and recovery rebuilds a fresh process from the journal plus the
    /// explicitly serialized state file.
    LiveProcess,
}

/// Recovery level a backend actually guarantees for a session (§19 R0–R2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryLevel {
    /// Cell source, outputs, outcomes, order and environment metadata are
    /// durable; nothing is claimed about live interpreter objects.
    R0WorkRecord,
    /// Explicitly serialized objects can also be restored by the kernel
    /// (the driver state file), plus journal replay for `Pure` cells.
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

/// Which declared limits are backed by a real mechanism, per field. Fuel and
/// memory reservation are declared but not enforced anywhere yet; the
/// wall-clock bound is enforced per cell (epoch interrupt on WASI, host-side
/// kill on native) whenever the caller sets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitEnforcement {
    pub wall_clock: bool,
    pub fuel: bool,
    pub memory: bool,
}

/// Declared resource limits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    pub max_wall_clock_ms: Option<u64>,
    pub max_fuel: Option<u64>,
    pub max_memory_bytes: Option<u64>,
    /// Legacy persisted aggregate flag; inspect reports individual mechanisms.
    #[serde(default)]
    pub enforced: bool,
}

impl ResourceLimits {
    /// Honest per-field enforcement report for `inspect`.
    pub fn enforcement(&self, backend: BackendKind) -> LimitEnforcement {
        LimitEnforcement {
            wall_clock: self.max_wall_clock_ms.is_some(),
            fuel: self.max_fuel.is_some() && backend == BackendKind::WasiPreview1,
            memory: self.max_memory_bytes.is_some()
                && matches!(backend, BackendKind::WasiPreview1 | BackendKind::NativeHost),
        }
    }
}

/// Binding of a session to its delegating context (§19: each REPL binds a
/// task, worker and scope; free-form fields keep this adapter-neutral).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CellOutcome {
    /// `_start` returned normally (exit code 0).
    Completed { exit_code: i32 },
    /// The guest called `proc_exit(n)` with `n != 0`; a normal interpreter
    /// "cell raised" path, still a completed record.
    Exited { exit_code: i32 },
    /// The guest was interrupted by a cancel request at an epoch checkpoint
    /// (WASI) or killed (native) before the cell finished. Effects the cell
    /// already committed stand; the cell has no complete record and the
    /// session requires `restore()` before further execution.
    Cancelled,
    /// The per-cell wall-clock budget elapsed; the same effect-preservation
    /// and recovery rules as `Cancelled` apply.
    DeadlineExceeded,
    /// The guest trapped or the host failed to run it; `error` is the real
    /// failure text. Stream bytes written before the trap are still durable.
    Failed { error: String },
}

/// Structured trailer a driver writes as the last line of its report file
/// after each cell. This is what makes the R1 claim auditable: whether the
/// state file actually persisted, and which bindings were dropped because
/// they are not serializable (the honest R1 boundary).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverReport {
    /// Observed inside the executing interpreter, never guessed by host OS.
    #[serde(default)]
    pub compatible_wheel_tags: Vec<String>,
    pub ok: bool,
    pub error: Option<String>,
    /// Names the driver could not serialize into the state file.
    pub dropped: Vec<String>,
    pub state_bytes: u64,
    /// True when the driver verified the state file round-trips (loaded the
    /// just-written state back). False means the R1 claim is downgraded to
    /// R0 for this cell even on a driver kernel.
    pub state_verified: bool,
}

/// Durable record of one executed cell (the R0 journal line).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellRecord {
    #[serde(default)]
    pub object_checkpoint: Option<crate::state::Baseline>,
    /// Absent for historical/unrecorded cells; absence never authorizes fresh
    /// host input during replay.
    #[serde(default)]
    pub io_receipt: Option<shenko_wasm::replay_io::IoReceipt>,
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
    /// BLAKE3 hex of the exact source bytes delivered to the guest.
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
    /// Driver trailer for this cell, when the kernel runs a driver.
    pub driver_report: Option<DriverReport>,
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
    pub session_model: SessionModel,
    pub state: SessionState,
    /// Highest recovery level this backend can deliver for this session.
    pub recovery_level: RecoveryLevel,
    /// True only when guest interpreter state outlives one cell process.
    /// True for `LiveProcess` sessions while the owned process lives.
    pub interpreter_state_persists: bool,
    /// Whether the owned live process is currently running (LiveProcess
    /// model only).
    pub live_process_running: bool,
    pub cells_executed: u64,
    pub environment_generation: RuntimeGeneration,
    pub bind: TaskBinding,
    pub limits: ResourceLimits,
    pub enforcement: LimitEnforcement,
    /// Driver identity (id@blake3) when the kernel runs an embedded driver.
    pub driver: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Ready,
    Busy,
    /// An interrupted restore must be retried before another cell can run.
    RecoveryRequired,
    Disposed,
}

/// An R0/R1 checkpoint is a durable citation of the journal plus the
/// declared state-file digests; nothing here claims interpreter objects
/// survive beyond what the driver serialized.
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
    /// An explicit immutable R1 object checkpoint was restored; no source
    /// or external effect was executed to obtain this state.
    ObjectRestored { checkpoint_blake3: String, dropped: Vec<String> },
    /// Pure cell re-executed; `replayed_as` is the replay record index.
    Replayed { replayed_as: u64 },
    /// Declared-effect cell preserved as a record; its effect is not rerun.
    ReceiptOnly,
    /// Unclassifiable cell left for reconciliation; not replayed.
    NeedsReconciliation,
    /// Historical work retained, but excluded from the current state by reset.
    BeforeReset { reset: EntityId },
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

/// Honest answer to `cancel`. Interrupting a cell kills the session's live
/// process (or traps the one-shot guest); it is a request honored at the
/// next interrupt point, never proof that all work stopped — a guest blocked
/// inside a blocking host call can outlive the request, and effects already
/// committed stand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelOutcome {
    /// The named cell is executing; an interrupt was requested at the next
    /// epoch checkpoint (WASI) or a kill was delivered (native). Not proof
    /// the cell's work fully stopped.
    InterruptRequested { cell: EntityId },
    /// The named cell is executing but this backend has no interrupt
    /// mechanism at all.
    NotInterruptible { cell: EntityId },
    /// No cell is currently executing; nothing to cancel.
    NothingRunning,
    /// The session is disposed.
    Disposed,
}

/// One dependency entry the caller asked to install (R083 surface).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    /// Host paths to pinned wheel files the caller already resolved. Wheel
    /// filenames must follow `{dist}-{version}(-{build})?-{python}-{abi}-{platform}.whl`.
    pub wheels: Vec<PathBuf>,
    /// Registry packages for installers that have one (npm/bun); `pinned`
    /// carries the exact version or dist-tag the caller resolved.
    pub packages: Vec<crate::kernel::PackageSpec>,
}

/// A package the installer refused, with the real reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedPackage {
    pub name: String,
    pub pinned: String,
    pub reason: String,
}

/// Honest answer to `install_dependencies` (R083 surface).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallOutcome {
    /// Packages landed in a candidate environment that was verified and
    /// committed: the session now runs on `environment_generation`, and the
    /// journal carries an `env` record binding the transition to the cell
    /// sequence. The previous environment directory stays on disk so a fork
    /// of the old generation remains reproducible.
    Installed {
        installed: Vec<String>,
        skipped: Vec<SkippedPackage>,
        previous_generation: RuntimeGeneration,
        environment_generation: RuntimeGeneration,
    },
    /// Nothing was installed; the candidate environment was discarded and
    /// the session keeps running on its previous generation.
    Refused { reason: String },
    /// The kernel backend has no installer (e.g. raw QuickJS without a
    /// package route); the generation is unchanged.
    Unsupported { reason: String },
}

/// One cell to execute.
#[derive(Debug, Clone)]
pub struct CellRequest {
    /// Exact bytes delivered to the guest as the cell program.
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
    fn create(&self, request: CreateSession) -> Result<Self::Session>;
    /// Reopen an existing session directory without replaying cells.
    fn reopen(&self, session_dir: &std::path::Path) -> anyhow::Result<Self::Session>;
}

/// One live REPL session.
pub trait ReplSession {
    /// Execute one cell on the real backend and journal it durably.
    fn execute(&mut self, request: CellRequest) -> anyhow::Result<CellResult>;
    /// Current session state and honest capability report.
    fn inspect(&self) -> anyhow::Result<SessionInfo>;
    /// Durable R0/R1 citation of the journal plus declared state-file digests.
    fn checkpoint(&mut self) -> anyhow::Result<ReplCheckpoint>;
    /// Rebuild this session's interpreter state by re-executing journaled
    /// Pure cells into a fresh owned process (LiveProcess) or fresh one-shot
    /// runs (OneShot). Declared-effect cells keep their durable record as
    /// the receipt; Unknown cells are reconciliation gaps. External effects
    /// are never re-executed (§19 replay boundary, A15). Rebuild starts from
    /// the session's immutable baseline (empty after reset), not surviving
    /// state.
    fn restore(&mut self) -> anyhow::Result<RecoverReport>;
    /// Create a new session whose guest-visible state is a copy of this
    /// session's; the new session keeps its own journal from scratch.
    fn fork(&self, sessions_root: &std::path::Path) -> anyhow::Result<Box<dyn ReplSession>>;
    /// Report the real interrupt ability; never claims work stopped.
    fn cancel(&self) -> anyhow::Result<CancelOutcome>;
    /// Durably mark a replay boundary, then remove declared state. Historical
    /// work remains in the journal but is not resurrected by recovery. A failed
    /// state removal is retryable, including after reopen.
    fn reset(&mut self) -> anyhow::Result<()>;
    /// R083 surface: install into a candidate environment, verify, and commit
    /// a new `environment_generation`; on any failure the candidate is
    /// discarded and the previous environment keeps serving the session.
    fn install_dependencies(
        &mut self,
        request: InstallRequest,
    ) -> anyhow::Result<InstallOutcome>;
    /// End the session; the owned live process is terminated and the journal
    /// and outputs stay on disk for recovery.
    fn dispose(&mut self) -> anyhow::Result<()>;
    fn session_dir(&self) -> &std::path::Path;
}
