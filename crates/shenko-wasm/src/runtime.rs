use crate::mount::{MountMode, MountSpec, admit_mounts, validate_mounts};
use crate::p1::{P1Host, StdioStreams};
use crate::revoke::WasiRevocation;
use crate::state::{
    CapturedWasmState, ExternalTransportEvidence, WasmBackendIdentity, WasmStateManifest,
    WasmStateProvider, WasmStateRestore,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use shenko_store::cas::ContentStore;
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tracing::info;
use wasmtime::{
    Config, Engine, Error as WasmtimeError, Instance, Linker, Module, Store, UpdateDeadline,
    Val,
};
use wasmtime_wasi::I32Exit;
pub use wasmtime_wasi::cli::{InputFile, OutputFile, StdinStream, StdoutStream};
use std::io;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
/// Bounded in-memory streams for callers that must cap guest output
/// (Playground backend receipts) without binding the whole crate to a
/// preview-2 path. These upstream pipes expose write errors to the guest;
/// callers needing an independently checked bound use BoundedOutputPipe.
pub use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};

/// Bounded guest output capture with host-visible rejection flags.
///
/// [`MemoryOutputPipe`] reports capacity overflow only as a guest-visible
/// stream error. A guest that ignores the error can still write a valid
/// small JSON and exit 0 — the receipt would look complete while bytes were
/// actually rejected. This pipe records every rejected/failed write
/// independently, so the caller's success check includes stream integrity
/// instead of relying on the trap or the retained length.
#[derive(Clone, Debug)]
pub struct BoundedOutputPipe {
    inner: Arc<BoundedOutputInner>,
}

#[derive(Debug)]
struct BoundedOutputInner {
    bytes: parking_lot::Mutex<bytes::BytesMut>,
    limit: usize,
    /// A write was rejected because it would exceed the bound.
    overflowed: AtomicBool,
    /// Any write failed, including a capacity rejection.
    failed: AtomicBool,
}

impl BoundedOutputPipe {
    /// A capture whose retained output can never exceed `limit` bytes.
    pub fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new(BoundedOutputInner {
                bytes: parking_lot::Mutex::new(bytes::BytesMut::new()),
                limit,
                overflowed: AtomicBool::new(false),
                failed: AtomicBool::new(false),
            }),
        }
    }
    /// The retained output; potentially truncated iff `did_overflow()`.
    pub fn contents(&self) -> bytes::Bytes {
        let guard = self.inner.bytes.lock();
        bytes::Bytes::copy_from_slice(&guard)
    }
    /// True once any guest write had to be rejected for capacity.
    pub fn did_overflow(&self) -> bool {
        self.inner.overflowed.load(Ordering::SeqCst)
    }
    /// True once a guest write was rejected, independently of its exit code.
    pub fn did_fail(&self) -> bool {
        self.inner.failed.load(Ordering::SeqCst)
    }
}

impl wasmtime_wasi::cli::IsTerminal for BoundedOutputPipe {
    fn is_terminal(&self) -> bool { false }
}

impl StdoutStream for BoundedOutputPipe {
    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        Box::new(BoundedOutputWriter { inner: Arc::clone(&self.inner) })
    }
}

struct BoundedOutputWriter {
    inner: Arc<BoundedOutputInner>,
}

impl tokio::io::AsyncWrite for BoundedOutputWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut bytes = self.inner.bytes.lock();
        if buf.len() > self.inner.limit.saturating_sub(bytes.len()) {
            self.inner.overflowed.store(true, Ordering::SeqCst);
            self.inner.failed.store(true, Ordering::SeqCst);
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "bounded guest output capacity exceeded",
            )));
        }
        bytes.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Shared cancellation request for a controlled WASI execution.
///
/// Cloning a handle keeps the same request; `cancel()` is idempotent and can
/// be called from any thread, including the same thread before the run
/// starts. A cancellation is observed by the controlled run at guest epoch
/// checkpoints (see `WasiRunControl`), never by rewinding effects the guest
/// already committed.
#[derive(Debug, Clone, Default)]
pub struct WasiCancellation {
    cancelled: Arc<AtomicBool>,
}

impl WasiCancellation {
    /// Request cancellation. Returns immediately; the run reports
    /// `WasiRunOutcome::Cancelled` once the guest reaches an epoch checkpoint.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation was already requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

/// Optional per-invocation execution controls (R007: cancel + timeout on top
/// of the existing argv/env/stdio contract). `WasiRunControl::default()`
/// requests no controls and keeps the previous unbounded behavior.
///
/// Guarantees are deliberately bounded and documented rather than implied:
/// both `deadline` and `cancel` interrupt **guest code** at wasmtime epoch
/// checkpoints, with a check granularity of `WASI_EPOCH_CHECK_INTERVAL`.
/// A guest blocked inside a host call that has no stop observation (for
/// example `fd_read` on an empty pipe, or `poll_oneoff` on a WASI clock
/// subscription) stays blocked until that host call returns. Network host
/// calls are the exception: they poll in bounded slices against the same
/// [`RunStop`], so a blocked socket read ends with the run.
/// `deadline` is measured from the start of the run and includes module
/// compilation and mount setup, so an elapsed budget reports
/// `WasiRunOutcome::DeadlineExceeded` before the guest executes at all.
#[derive(Debug, Clone, Default)]
pub struct WasiRunControl {
    /// Maximum total duration of the run, from run start through `_start`
    /// returning.
    pub deadline: Option<Duration>,
    /// Optional cancellation handle observed at epoch checkpoints.
    pub cancel: Option<WasiCancellation>,
    /// Optional live write-withdrawal request (SPEC §8.3, §41.3). Another
    /// thread may withdraw write capability while the guest runs; the guest's
    /// already-open writable descriptors then refuse further writes at the
    /// next hostcall boundary, while reads keep the same merged view. Unlike
    /// `cancel`, this does not stop the run.
    pub revocation: Option<WasiRevocation>,
}

/// How often the watcher advances the controlled engine's epoch, in ticks.
/// One tick equals one `Instant`-clock interval of this duration. Cancellation
/// and deadline checks run when guest code reaches an epoch checkpoint after
/// a tick, so observed latency is at most one interval plus one guest
/// checkpoint for CPU-bound guests.
const WASI_EPOCH_CHECK_INTERVAL: Duration = Duration::from_millis(10);

/// Stop conditions shared by the epoch callback (guest execution) and the
/// preview1 host (blocking host I/O).
///
/// A run has exactly one `RunStop`; every clone observes and commits the same
/// first stop. Cancellation outranks the deadline, matching the epoch
/// callback's decision order, and the first committed outcome is never
/// overwritten — a host call that terminates on a cancellation cannot later
/// relabel the run as a deadline expiry.
#[derive(Clone, Default)]
pub struct RunStop {
    committed: Arc<parking_lot::Mutex<Option<WasiRunOutcome>>>,
    cancel: Option<WasiCancellation>,
    deadline: Option<Instant>,
}

impl RunStop {
    pub fn new(cancel: Option<WasiCancellation>, deadline: Option<Instant>) -> Self {
        Self {
            committed: Arc::new(parking_lot::Mutex::new(None)),
            cancel,
            deadline,
        }
    }

    /// Commit the first live stop condition and return the committed outcome.
    pub fn observe(&self) -> Option<WasiRunOutcome> {
        let live = if self
            .cancel
            .as_ref()
            .is_some_and(WasiCancellation::is_cancelled)
        {
            Some(WasiRunOutcome::Cancelled)
        } else if self.deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            Some(WasiRunOutcome::DeadlineExceeded)
        } else {
            None
        };
        if let Some(outcome) = live {
            self.commit(outcome);
        }
        self.committed()
    }

    /// Record `outcome` when nothing was recorded yet, and return whatever is
    /// now committed.
    pub fn commit(&self, outcome: WasiRunOutcome) -> Option<WasiRunOutcome> {
        let mut slot = self.committed.lock();
        if slot.is_none() {
            *slot = Some(outcome);
        }
        *slot
    }

    /// The first committed stop, if any.
    pub fn committed(&self) -> Option<WasiRunOutcome> {
        *self.committed.lock()
    }

    /// Whether a stop was already committed.
    pub fn is_stopped(&self) -> bool {
        self.committed.lock().is_some()
    }

    /// Whether this run carries a deadline at all.
    pub fn has_deadline(&self) -> bool {
        self.deadline.is_some()
    }

    /// Time left before the deadline; `None` without a deadline or once it
    /// has already elapsed.
    pub fn remaining(&self) -> Option<Duration> {
        let deadline = self.deadline?;
        deadline.checked_duration_since(Instant::now())
    }
}

/// Typed, caller-visible result of a real WASI execution (R007 task
/// lifecycle). Host-side failures (invalid commands, mount validation,
/// compile/instantiate/linker errors and guest traps other than `proc_exit`)
/// remain `Err` exactly as before; this enum covers successful runs and
/// deliberate stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasiRunOutcome {
    /// `_start` returned (0) or the guest called `proc_exit(n)`. Writes
    /// already made to caller streams and mounts remain effects.
    Exited(i32),
    /// The configured deadline elapsed before the guest finished. The guest
    /// was trapped at an epoch checkpoint; effects already committed stand.
    DeadlineExceeded,
    /// `WasiCancellation::cancel` was observed before the guest finished.
    /// Same effect-preservation rule as `DeadlineExceeded`.
    Cancelled,
}

/// Guest path where the persistent WASM home is mounted for WASI commands
/// (SPEC §8.3: every mount declares its source path and guest path; the home
/// mount is the one preopen a WASI session always gets).
pub const HOME_GUEST_PATH: &str = "/home";

/// Per-invocation preview1 arguments, environment, and standard streams.
/// Arguments include argv[0] only when the caller supplies it. Neither
/// arguments, environment, nor stdio are inherited from the host implicitly.
///
/// The caller owns the policy and lifetime of each stream. For example,
/// `InputFile` and `OutputFile` bind already-open files without buffering the
/// whole output in the runtime. Keep another handle to an output destination
/// if its bytes are needed after a nonzero exit or a trap.
pub struct WasiCommand<I, O, E> {
    pub arguments: Vec<String>,
    pub environment: Vec<(String, String)>,
    /// Total guest instruction fuel and linear-memory ceiling for this store.
    pub limits: WasiResourceLimits,
    pub replay_io: Option<crate::replay_io::ReplayIo>,
    /// Optional execution controls; `WasiRunControl::default()` runs without
    /// a deadline or cancellation, exactly as before.
    pub control: WasiRunControl,
    /// Exact network reach for this run, or `None` for a guest with no
    /// transport. The host opens every admitted socket before the guest
    /// exists and publishes it through the host-owned `SHENKO_NET_*`
    /// descriptors (see [`crate::net::NetworkAdmission`]); a guest can never
    /// create, re-point or widen one. A networked run must also carry a
    /// deadline or a cancellation handle, so no host socket call can park the
    /// run indefinitely.
    pub network: Option<crate::net::NetworkAdmission>,
    stdin: I,
    stdout: O,
    stderr: E,
}

impl<I, O, E> WasiCommand<I, O, E> {
    /// Explicitly choose all three streams, including any intentional sinks.
    pub fn new(
        stdin: I,
        stdout: O,
        stderr: E,
    ) -> Self {
        Self {
            arguments: Vec::new(),
            environment: Vec::new(),
            limits: WasiResourceLimits::default(),
            replay_io: None,
            control: WasiRunControl::default(),
            network: None,
            stdin,
            stdout,
            stderr,
        }
    }

    fn validate(&self) -> Result<()> {
        for (index, argument) in self.arguments.iter().enumerate() {
            if argument.contains('\0') {
                bail!("WASI_COMMAND_INVALID: argument {index} contains NUL");
            }
        }
        for (index, (key, value)) in self.environment.iter().enumerate() {
            if key.is_empty() || key.contains('\0') || key.contains('=') {
                bail!("WASI_COMMAND_INVALID: environment key {index} is empty or contains NUL or '='");
            }
            ensure!(
                !crate::net::is_reserved_env_key(key),
                "WASI_COMMAND_INVALID: environment key {index} is in the host-owned \
                 {} namespace; the host alone publishes installed socket descriptors",
                crate::net::GUEST_ENV_ROOT
            );
            if value.contains('\0') {
                bail!("WASI_COMMAND_INVALID: environment value {index} contains NUL");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WasiResourceLimits {
    pub max_fuel: Option<u64>,
    pub max_memory_bytes: Option<usize>,
}

/// Real WASM runtime backed by wasmtime-45.
///
/// Holds the engine and an owned store. Modules are compiled by the engine and
/// instantiated against the store; function calls dispatch through the store's
/// context. A configured mount list binds the persistent home and any other
/// explicitly granted host directories. Only file home/metadata recovery is
/// represented here; live module memory, KV and sockets are not checkpoints.
pub struct WasmRuntime {
    pub engine: Engine,
    pub store: Option<Store<()>>,
    pub home_dir: PathBuf,
    pub mounts: Vec<MountSpec>,
    /// Store of the retained stateful instance, when one was instantiated with
    /// [`WasmRuntime::instantiate_stateful`]. Only one instance is retained per
    /// runtime, and `&mut self` serializes calls, which is what makes "capture
    /// is quiescent" a fact rather than an assumption.
    stateful: Option<Store<ControlledCtx>>,
    /// Private epoch-interruption engine used only by controlled runs.
    ///
    /// Epoch engines require every store to declare an epoch deadline before
    /// guest code may execute, so the public `engine` must keep running
    /// uncontrolled stores (`instantiate_module`, plain `run_wasi_module`)
    /// without epoch instrumentation. A dedicated, never-exported engine also
    /// keeps each run's epoch private: nobody else can tick it (no premature
    /// deadlines) and its ticks cannot trip stores owned by `self.engine` or
    /// by an `Engine` cloned out of this runtime (no cross-run cancellation).
    /// `&mut self` already serializes runs on one runtime, and the epoch is
    /// bookkeeping-relative, so one lazily-built engine serves all of this
    /// runtime's controlled runs without recompilation of the engine itself.
    control_engine: Option<Engine>,
}

impl WasmRuntime {
    pub fn invocation_digest(&self, arguments: &[String], environment: &[(String, String)]) -> Result<String> {
        crate::replay_io::invocation_digest(arguments, environment, &self.mounts, &self.home_dir)
    }
    /// Create a runtime with a default-config engine and a fresh empty store.
    pub fn new() -> Result<Self> {
        info!("WASM runtime init (wasmtime-45)");
        // Compile serially on the calling thread: wasmtime's parallel
        // compilation pool is a process-global rayon pool whose workers inherit
        // the comm of whichever thread first compiles. Built from an invocation
        // worker named "shenko-wasi", that pool would outlive the run and
        // masquerade as leaked invocation threads.
        let mut config = Config::new();
        config.parallel_compilation(false);
        config.consume_fuel(true);
        let engine = Engine::new(&config)?;
        let mut store = Store::new(&engine, ());
        store.set_fuel(u64::MAX).map_err(|e| anyhow!("initialize module fuel: {e}"))?;
        Ok(Self {
            engine,
            store: Some(store),
            home_dir: PathBuf::new(),
            mounts: Vec::new(),
            stateful: None,
            control_engine: None,
        })
    }

    /// Construct the real WASI home from a managed manifest. There is no
    /// implicit host-directory fallback and no silent COW-to-RW downgrade.
    pub fn with_mounts(mounts: Vec<MountSpec>) -> Result<Self> {
        let home_dir = mounts
            .iter()
            .find(|mount| mount.guest_path == HOME_GUEST_PATH)
            .map(|mount| mount.source.clone())
            .ok_or_else(|| anyhow!("WASM_PROFILE_UNAVAILABLE: /home mount is missing"))?;
        validate_mounts(&mounts, &home_dir)?;
        let mut runtime = Self::new()?;
        runtime.home_dir = home_dir;
        runtime.mounts = mounts;
        Ok(runtime)
    }

    /// Compile and instantiate a WAT module against the owned store.
    pub fn instantiate_module(&mut self, wat: &str) -> Result<Instance> {
        let store = self.store.as_mut().context("wasm store not initialized")?;
        let module = Module::new(&self.engine, wat.as_bytes())
            .map_err(|e| anyhow!("compile WAT module: {e}"))?;
        let instance =
            Instance::new(store, &module, &[]).map_err(|e| anyhow!("instantiate module: {e}"))?;
        Ok(instance)
    }

    /// Call an exported function by name on the given instance.
    pub fn call_func(&mut self, instance: &Instance, name: &str, args: &[Val]) -> Result<Vec<Val>> {
        let store = self.store.as_mut().context("wasm store not initialized")?;
        let func = instance
            .get_func(&mut *store, name)
            .ok_or_else(|| anyhow!("exported function `{}` not found", name))?;
        let result_count = func.ty(&*store).results().len();
        // wasmtime 45 uses the out-parameter call ABI: allocate a slot per
        // declared result, then let the engine overwrite each slot with the
        // produced value (initial values are placeholders, never read).
        let mut results = vec![Val::I32(0); result_count];
        func.call(store, args, &mut results)
            .map_err(|e| anyhow!("call function: {e}"))?;
        Ok(results)
    }

    /// Run a WASI preview1 command module and return its typed outcome.
    ///
    /// `wasm_or_wat` may be either a binary module or WAT text. Every preopen
    /// comes from `self.mounts` with its declared RO/RW/COW permissions. Each
    /// mount is admitted once into an owned directory capability before the
    /// guest exists, and the guest's preview1 host resolves every path from
    /// those handles: no host pathname is re-resolved for the guest, so an
    /// ancestor swap between validation and use cannot redirect a preopen
    /// (SPEC §8.3, A08).
    /// The command supplies exact argv, environment and streams, without a
    /// shell or inherited process state. A normal return is exit code 0 and a
    /// `proc_exit(n)` trap is exit code `n`; other traps are errors, and any
    /// writes already made to caller-supplied streams or mounts remain effects.
    ///
    /// `command.control` optionally bounds the run (R007): a `deadline`
    /// measured from run start and/or a `WasiCancellation` handle observed at
    /// guest epoch checkpoints report the typed `WasiRunOutcome` variants
    /// instead of hanging the caller. Controlled runs execute on this
    /// runtime's private epoch engine, so ticking one run can never interrupt
    /// stores on the shared `engine` or on engines other runtimes own, and a
    /// stopped run leaves the runtime fully usable for the next call. These
    /// controls interrupt guest code at epoch checkpoints, and a guest blocked
    /// in a *network* host call observes the same stop in bounded poll slices
    /// (see [`crate::net`]); other blocking host calls (an empty stdin pipe)
    /// still stay blocked until they return.
    pub fn run_wasi_module<I, O, E>(
        &mut self,
        wasm_or_wat: &[u8],
        command: WasiCommand<I, O, E>,
    ) -> Result<WasiRunOutcome>
    where
        I: StdinStream + 'static,
        O: StdoutStream + 'static,
        E: StdoutStream + 'static,
    {
        if self.home_dir.as_os_str().is_empty() {
            return Err(anyhow!(
                "WASM_PROFILE_UNAVAILABLE: call with_mounts before running WASI"
            ));
        }
        command.validate()?;
        let control = command.control.clone();

        // A networked guest owns host sockets, so it must also own a bound:
        // refusing here (before any mount, compile or socket work) keeps every
        // socket host call interruptible instead of trusting the peer. A
        // deterministic transcript cannot represent a live socket either, so
        // replay and transport are refused together rather than silently
        // mixing a recorded run with real I/O.
        if command.network.is_some() {
            ensure!(
                control.deadline.is_some() || control.cancel.is_some(),
                "WASI_NETWORK_RUN_UNBOUNDED: a networked guest needs a run deadline or a \
                 cancellation handle; an unbounded socket run could never be stopped"
            );
            ensure!(
                command.replay_io.is_none(),
                "WASI_NETWORK_REPLAY_UNSUPPORTED: a live socket is not a deterministic replay \
                 input; run the networked guest without replay_io"
            );
        }

        // Deterministic boundary checks, before any mount, compile or spawn
        // work: an already-cancelled run must not produce guest effects, and a
        // deadline that has already elapsed (zero duration, or a duration that
        // cannot even be represented relative to now) is reported rather than
        // silently changed into a different policy.
        if control
            .cancel
            .as_ref()
            .is_some_and(WasiCancellation::is_cancelled)
        {
            return Ok(WasiRunOutcome::Cancelled);
        }
        if control.deadline.is_some_and(|d| d.is_zero()) {
            return Ok(WasiRunOutcome::DeadlineExceeded);
        }
        // Resolve the caller-relative duration into one absolute deadline
        // now: a duration that cannot be represented is rejected instead of
        // silently downgraded to a different bound.
        let deadline_at = control
            .deadline
            .map(|d| {
                Instant::now().checked_add(d).ok_or_else(|| {
                    anyhow!(
                        "WASI_RUN_CONTROL_INVALID: deadline duration exceeds representable clock range"
                    )
                })
            })
            .transpose()?;

        let controlled = control.deadline.is_some() || control.cancel.is_some();
        let engine = if controlled {
            if self.control_engine.is_none() {
                let mut config = Config::new();
                config.epoch_interruption(true);
                config.parallel_compilation(false);
                config.consume_fuel(true);
                self.control_engine = Some(
                    Engine::new(&config)
                        .map_err(|e| anyhow!("create epoch-enabled engine: {e}"))?,
                );
            }
            self.control_engine
                .as_ref()
                .expect("control engine just built")
                .clone()
        } else {
            self.engine.clone()
        };
        let home = &self.home_dir;
        let mounts = &self.mounts;
        // Bind before the closures: the worker borrows the selected engine
        // while the watcher gets its own cheap clone (Engine is Arc-backed).
        let engine_ref = &engine;
        let ticker = engine.clone();
        // wasmtime-wasi's synchronous shims call `Handle::block_on`, which
        // panics when the calling thread already drives a tokio runtime. Run
        // on a fresh thread with no ambient runtime so the shims fall back to
        // wasmtime-wasi's own runtime; this keeps the API sync-safe from async
        // callers (the Shenko main loop) as well as from plain tests. Scoped
        // borrowing avoids copying the module and mount list for each run.
        std::thread::scope(|scope| {
            // The watcher exists only for controlled runs and only ticks this
            // run's private engine, so it can never cancel unrelated stores.
            // It is spawned first on purpose: if this spawn fails there is no
            // unbounded worker to abandon, and if the worker spawn below fails
            // the dropped `stop_tx` lets the watcher exit within one interval
            // before `scope` joins it. The watcher is always joined below on
            // success, error or worker panic — nothing outlives the call.
            let watcher = controlled.then(|| {
                let (stop_tx, stop_rx) = mpsc::channel::<()>();
                std::thread::Builder::new()
                    .name("shenko-wasi-epoch".into())
                    .spawn_scoped(scope, move || loop {
                        match stop_rx.recv_timeout(WASI_EPOCH_CHECK_INTERVAL) {
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                ticker.increment_epoch();
                            }
                            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    })
                    .context("spawn wasi epoch watcher thread")
                    .map(|watcher| (watcher, stop_tx))
            }).transpose()?;

            let worker = std::thread::Builder::new()
                .name("shenko-wasi".into())
                .spawn_scoped(scope, move || {
                    run_wasi_blocking(engine_ref, home, mounts, wasm_or_wat, command, deadline_at)
                })
                .context("spawn wasi execution thread")?;

            let run = worker
                .join()
                .map_err(|_| anyhow!("wasi execution thread panicked"))?;
            if let Some((watcher, stop_tx)) = watcher {
                // Dropping the sender ends the watcher's next recv_timeout;
                // joining bounds the cleanup before the typed result returns.
                drop(stop_tx);
                watcher
                    .join()
                    .map_err(|_| anyhow!("wasi epoch watcher thread panicked"))?;
            }
            run
        })
    }
}

/// Per-store state for a run: the WASI context plus the stop cell consulted by
/// the epoch-deadline callback and by every blocking host socket call. The
/// first committed outcome is what the caller sees, so a deliberate kill is
/// never confused with an ordinary guest trap.
struct ControlledCtx {
    host: P1Host,
    limits: wasmtime::StoreLimits,
    stop: RunStop,
}

fn store_limits(limits: WasiResourceLimits) -> wasmtime::StoreLimits {
    let mut builder = wasmtime::StoreLimitsBuilder::new();
    if let Some(bytes) = limits.max_memory_bytes {
        builder = builder.memory_size(bytes).memories(1);
    }
    builder.trap_on_grow_failure(true).build()
}

/// Map a `_start` call result onto an exit code, preserving the preexisting
/// contract: normal return is 0, `proc_exit(n)` is `n` (an `I32Exit` trap),
/// anything else is a guest trap error.
fn wasi_exit(result: wasmtime::Result<()>) -> Result<WasiRunOutcome> {
    match result {
        Ok(()) => Ok(WasiRunOutcome::Exited(0)),
        Err(e) => match e.downcast_ref::<I32Exit>() {
            Some(exit) => Ok(WasiRunOutcome::Exited(exit.0)),
            None => Err(anyhow!("run WASI `_start`: {e:?}")),
        },
    }
}

/// Instantiate a WASI preview1 command with the admitted mount closure and
/// run `_start`.
///
/// The mount closure is admitted once, before the guest exists: every mount
/// is opened into an owned directory capability and the guest's preview1
/// host resolves paths from those handles only. No host pathname is
/// re-resolved for the guest, so an ancestor swap between validation and
/// use cannot redirect a preopen (SPEC §8.3, A08).
fn run_wasi_blocking<I, O, E>(
    engine: &Engine,
    home: &Path,
    mounts: &[MountSpec],
    wasm_or_wat: &[u8],
    command: WasiCommand<I, O, E>,
    deadline_at: Option<Instant>,
) -> Result<WasiRunOutcome>
where
    I: StdinStream + 'static,
    O: StdoutStream + 'static,
    E: StdoutStream + 'static,
{
    let controlled = deadline_at.is_some() || command.control.cancel.is_some();
    let resource_limits = command.limits;
    // One stop cell per run: the epoch callback and every host socket call
    // observe and commit the same first outcome.
    let stop = RunStop::new(command.control.cancel.clone(), deadline_at);
    if let Some(io) = &command.replay_io {
        let invocation = crate::replay_io::invocation_digest(&command.arguments, &command.environment, mounts, home)?;
        io.verify_execution(wasm_or_wat, &invocation)?;
    }
    validate_mounts(mounts, home)?;
    let module =
        Module::new(engine, wasm_or_wat).map_err(|e| anyhow!("compile WASI module: {e}"))?;

    let bindings = admit_mounts(mounts, home)?;
    let stdio = StdioStreams::from_async(command.stdin.async_stream(),
        command.stdout.async_stream(), command.stderr.async_stream());
    let mut host = P1Host::new(bindings, stdio, command.arguments, command.environment)?;
    host.set_replay_io(command.replay_io);
    host.set_revocation(command.control.revocation.as_ref().map(WasiRevocation::state));
    // Sockets are opened from the admission before the guest exists, so the
    // guest can only ever reach an endpoint the caller was granted. A stop
    // observed while connecting ends the run here, with no guest code run.
    if let Some(admission) = command.network {
        match host.install_network(admission, stop.clone())? {
            crate::net::NetworkStart::Installed => {}
            crate::net::NetworkStart::Stopped(outcome) => return Ok(outcome),
        }
    }

    if !controlled {
        let mut linker: Linker<ControlledCtx> = Linker::new(engine);
        crate::p1::add_to_linker_sync(&mut linker, |cx| &mut cx.host)
            .map_err(|e| anyhow!("configure wasi preview1 linker: {e}"))?;

        let mut store = Store::new(engine, ControlledCtx {
            host, limits: store_limits(resource_limits), stop,
        });
        store.limiter(|cx| &mut cx.limits);
        store.set_fuel(resource_limits.max_fuel.unwrap_or(u64::MAX))
            .map_err(|e| anyhow!("set WASI fuel: {e}"))?;
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| anyhow!("instantiate WASI module: {e}"))?;
        let start = instance
            .get_typed_func::<(), ()>(&mut store, "_start")
            .map_err(|e| anyhow!("WASI command must export `_start`: {e}"))?;

        return wasi_exit(start.call(&mut store, ()));
    }

    let mut store = Store::new(
        engine,
        ControlledCtx {
            host,
            limits: store_limits(resource_limits),
            stop,
        },
    );
    store.limiter(|cx| &mut cx.limits);
    store.set_fuel(resource_limits.max_fuel.unwrap_or(u64::MAX))
        .map_err(|e| anyhow!("set WASI fuel: {e}"))?;
    let mut linker: Linker<ControlledCtx> = Linker::new(engine);
    crate::p1::add_to_linker_sync(&mut linker, |cx| &mut cx.host)
        .map_err(|e| anyhow!("configure wasi preview1 linker: {e}"))?;

    // The epoch deadline is one check interval, not the caller's wall-clock
    // deadline: at every checkpoint the callback re-evaluates cancellation
    // (an explicit request wins deterministically) and the real deadline
    // against `Instant`. The watcher thread only advances engine epochs; all
    // stop decisions happen on the execution thread — in this callback, or in
    // a host socket call that cannot make progress — and they commit to the
    // same `RunStop`, so the reported outcome is the first condition that was
    // actually observed.
    store.epoch_deadline_callback(|mut cx| {
        match cx.data().stop.observe() {
            Some(outcome) => {
                // The callback's error type is wasmtime's own Error (not
                // anyhow::Error): it is what unwinds guest execution into the
                // caller-visible trap for this store.
                Err(WasmtimeError::msg(format!(
                    "WASI run stopped: {outcome:?}"
                )))
            }
            None => Ok(UpdateDeadline::Continue(1)),
        }
    });
    // On an epoch engine a store that never declares a deadline traps
    // immediately, so this deadline must be installed before any guest code
    // executes. The callback re-arms it one tick at a time.
    store.set_epoch_deadline(1);

    // Deterministic pre-execution check: cancellation or an elapsed deadline
    // observed after compilation/setup but before instantiation means no
    // guest code (including `start` sections) ever runs for this call.
    if let Some(outcome) = store.data().stop.observe() {
        return Ok(outcome);
    }

    let stopped = |store: &Store<ControlledCtx>| store.data().stop.committed();
    let instance = match linker.instantiate(&mut store, &module) {
        Ok(instance) => instance,
        // A stop committed by the epoch callback outranks the trap it raised.
        Err(e) => match stopped(&store) {
            Some(outcome) => return Ok(outcome),
            None => return Err(anyhow!("instantiate WASI module: {e}")),
        },
    };
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|e| anyhow!("WASI command must export `_start`: {e}"))?;

    let result = start.call(&mut store, ());
    match stopped(&store) {
        Some(outcome) => Ok(outcome),
        None => wasi_exit(result),
    }
}

/// A retained WASI preview1 instance that can be driven across calls and
/// snapshotted between them (SPEC §51.4 runtime state).
///
/// This is not a second engine or a parallel runtime: it is the same
/// `ControlledCtx`/`P1Host`/`Linker` wiring as [`WasmRuntime::run_wasi_module`],
/// with the store kept alive instead of being dropped when one command returns.
/// `WasmRuntime::capture_state` runs only while no call is executing, because
/// every operation takes `&mut WasmRuntime`.
pub struct StatefulInstance {
    instance: Instance,
    pub instance_id: EntityId,
    /// Exact bytes the instance was created from; a captured state is bound to
    /// their digest and a restore refuses a different module.
    module_bytes: Vec<u8>,
    mounts: Vec<MountSpec>,
    pub captures: u64,
}

impl StatefulInstance {
    pub fn module_digest(&self) -> Digest {
        Digest::blake3(&self.module_bytes)
    }

    pub fn mounts(&self) -> &[MountSpec] {
        &self.mounts
    }
}

impl WasmRuntime {
    /// Instantiate a WASI preview1 command that stays alive for repeated
    /// exports. `_start` is NOT called: the caller drives whichever exports it
    /// needs, so a state can be captured between two calls.
    pub fn instantiate_stateful<I, O, E>(
        &mut self,
        wasm_or_wat: &[u8],
        command: WasiCommand<I, O, E>,
    ) -> Result<StatefulInstance>
    where
        I: StdinStream + 'static,
        O: StdoutStream + 'static,
        E: StdoutStream + 'static,
    {
        command.validate()?;
        // A retained instance does not run on the epoch engine, so a deadline or
        // cancellation could not be enforced. Refusing is honest; bounded runs
        // go through `run_wasi_module`.
        ensure!(
            command.control.deadline.is_none() && command.control.cancel.is_none(),
            "STATEFUL_INSTANCE_UNSUPPORTED: a retained instance cannot enforce a deadline or cancellation; use run_wasi_module for bounded runs"
        );
        validate_mounts(&self.mounts, &self.home_dir)?;
        let module = Module::new(&self.engine, wasm_or_wat)
            .map_err(|error| anyhow!("compile stateful WASI module: {error}"))?;
        let bindings = admit_mounts(&self.mounts, &self.home_dir)?;
        let stdio = StdioStreams::from_async(
            command.stdin.async_stream(),
            command.stdout.async_stream(),
            command.stderr.async_stream(),
        );
        let mut host = P1Host::new(bindings, stdio, command.arguments, command.environment)?;
        host.set_replay_io(command.replay_io);
        host.set_revocation(command.control.revocation.as_ref().map(WasiRevocation::state));
        let mut store = Store::new(
            &self.engine,
            ControlledCtx {
                host,
                limits: store_limits(command.limits),
                stop: RunStop::new(None, None),
            },
        );
        store.limiter(|ctx| &mut ctx.limits);
        store
            .set_fuel(command.limits.max_fuel.unwrap_or(u64::MAX))
            .map_err(|error| anyhow!("set stateful WASI fuel: {error}"))?;
        let mut linker: Linker<ControlledCtx> = Linker::new(&self.engine);
        crate::p1::add_to_linker_sync(&mut linker, |ctx| &mut ctx.host)
            .map_err(|error| anyhow!("configure stateful wasi preview1 linker: {error}"))?;
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|error| anyhow!("instantiate stateful WASI module: {error}"))?;
        // Only one retained instance per runtime: a second one would make
        // "the store I snapshot" ambiguous.
        self.stateful = Some(store);
        Ok(StatefulInstance {
            instance,
            instance_id: EntityId::new(),
            module_bytes: wasm_or_wat.to_vec(),
            mounts: self.mounts.clone(),
            captures: 0,
        })
    }

    /// Call one export on the retained instance. Guest traps are errors, exactly
    /// like [`WasmRuntime::call_func`]; effects already committed stand.
    pub fn call_stateful(
        &mut self,
        handle: &StatefulInstance,
        name: &str,
        args: &[Val],
    ) -> Result<Vec<Val>> {
        let store = self
            .stateful
            .as_mut()
            .context("STATEFUL_INSTANCE_MISSING: no retained stateful instance")?;
        let func = handle
            .instance
            .get_func(&mut *store, name)
            .ok_or_else(|| anyhow!("exported function `{name}` not found"))?;
        let results = func.ty(&*store).results().len();
        let mut out = vec![Val::I32(0); results];
        func.call(&mut *store, args, &mut out)
            .map_err(|error| anyhow!("call stateful `{name}`: {error}"))?;
        Ok(out)
    }

    /// Capture the retained instance's linear memory, continuation and object
    /// graph into this installation's CAS.
    ///
    /// Capture is only legal between calls: `&mut self` plus the retained store
    /// means no guest code is executing, and the manifest records that
    /// (`QuiesceReceipt`). A guest without the continuation protocol still has
    /// its memory captured; the manifest then says `NotCapturable` and the state
    /// is not resumable.
    pub fn capture_state(
        &mut self,
        handle: &mut StatefulInstance,
        cas: &ContentStore,
        transport: &[ExternalTransportEvidence],
    ) -> Result<CapturedWasmState> {
        let store = self
            .stateful
            .as_mut()
            .context("STATEFUL_INSTANCE_MISSING: no retained stateful instance")?;
        let provider = WasmStateProvider::new(cas);
        let sequence = handle.captures;
        let captured = provider.capture(
            store,
            &handle.instance,
            &handle.module_bytes,
            handle.instance_id,
            &handle.mounts,
            transport,
            sequence,
        )?;
        handle.captures = handle.captures.saturating_add(1);
        Ok(captured)
    }

    /// Instantiate the same module again and restore a verified state onto it.
    ///
    /// The caller supplies the module bytes: a state is only restorable onto the
    /// module it was captured from, and the digest is compared before anything
    /// is overwritten. `WasmStateRestore` reports whether the guest actually
    /// re-imported its continuation and reproduced the captured graph digest,
    /// which is the difference between "memory copied" and "execution resumed".
    pub fn restore_state<I, O, E>(
        &mut self,
        cas: &ContentStore,
        manifest: &WasmStateManifest,
        module_bytes: &[u8],
        command: WasiCommand<I, O, E>,
    ) -> Result<(StatefulInstance, WasmStateRestore)>
    where
        I: StdinStream + 'static,
        O: StdoutStream + 'static,
        E: StdoutStream + 'static,
    {
        let supplied = Digest::blake3(module_bytes);
        ensure!(
            supplied == manifest.module && module_bytes.len() as u64 == manifest.module_bytes,
            "WASM_STATE_INCOMPLETE: state was captured from module {} ({} bytes), the supplied module is {supplied} ({} bytes)",
            manifest.module,
            manifest.module_bytes,
            module_bytes.len()
        );
        let mut handle = self.instantiate_stateful(module_bytes, command)?;
        let store = self
            .stateful
            .as_mut()
            .ok_or_else(|| anyhow!("STATEFUL_INSTANCE_MISSING: no retained stateful instance"))?;
        let provider = WasmStateProvider::new(cas);
        let backend = WasmBackendIdentity::current();
        let restored = provider.restore(store, &handle.instance, manifest, &backend)?;
        handle.captures = manifest.quiesce.instance_sequence.saturating_add(1);
        Ok((handle, restored))
    }
}

impl Default for WasmRuntime {
    fn default() -> Self {
        Self::new().expect("WASM runtime init failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

    fn no_stdio() -> WasiCommand<std::io::Empty, std::io::Empty, std::io::Empty> {
        WasiCommand::new(std::io::empty(), std::io::empty(), std::io::empty())
    }

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shenko-wasm-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create temp home");
        dir
    }

    #[test]
    fn basic_instantiate() -> Result<()> {
        let mut rt = WasmRuntime::new()?;
        // A module that exports an identity function over i32.
        let wat = r#"
            (module
              (func (export "identity") (param i32) (result i32)
                local.get 0)
            )
        "#;
        let instance = rt.instantiate_module(wat)?;
        let results = rt.call_func(&instance, "identity", &[Val::I32(42)])?;
        assert_eq!(results.len(), 1, "identity should return one value");
        match results[0] {
            Val::I32(v) => assert_eq!(v, 42, "identity(42) should be 42"),
            ref other => panic!("expected i32 result, got {:?}", other),
        }
        Ok(())
    }

    /// Proves `_start` is actually executed: a clean command exits 0 and
    /// `proc_exit(n)` is reported as exit code `n` rather than an error.
    #[test]
    fn wasi_start_runs_and_reports_exit_code() -> Result<()> {
        let home = temp_home("start");
        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home.clone(),
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;

        let plain = r#"(module (func (export "_start")))"#;
        assert_eq!(
            rt.run_wasi_module(plain.as_bytes(), no_stdio())?,
            WasiRunOutcome::Exited(0),
            "empty _start exits 0"
        );

        let exit7 = r#"
            (module
              (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
              ;; real WASI commands export their linear memory; wiggle's host
              ;; glue requires it even when only proc_exit is imported
              (memory (export "memory") 1)
              (func (export "_start") (call $proc_exit (i32.const 7)))
            )
        "#;
        assert_eq!(
            rt.run_wasi_module(exit7.as_bytes(), no_stdio())?,
            WasiRunOutcome::Exited(7),
            "proc_exit(7) surfaces as exit code 7"
        );

        fs::remove_dir_all(&home).ok();
        Ok(())
    }

    /// Proves the persistent home is mounted: fd 3 is a preopen named `/home`,
    /// and the guest can create and write a file there through WASI, which the
    /// host then reads back from the same directory. Each failure exits with a
    /// distinct code so a broken stage is identifiable.
    #[test]
    fn home_dir_preopened_and_guest_writable() -> Result<()> {
        let home = temp_home("preopen");
        fs::write(home.join("host-visible.txt"), b"persistent")?;
        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home.clone(),
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;

        let wat = r#"
            (module
              (import "wasi_snapshot_preview1" "fd_prestat_get"
                (func $fd_prestat_get (param i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "fd_prestat_dir_name"
                (func $fd_prestat_dir_name (param i32 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "path_open"
                (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "proc_exit"
                (func $proc_exit (param i32)))
              (memory (export "memory") 1)
              (data (i32.const 200) "guest-out.txt")
              (data (i32.const 240) "hi\n")
              (func (export "_start")
                ;; fd 3 is the first preopen; a non-zero errno means it is missing.
                (if (i32.ne (call $fd_prestat_get (i32.const 3) (i32.const 0)) (i32.const 0))
                  (then (call $proc_exit (i32.const 41))))
                ;; The u8 tag is at offset 0, so pr_name_len is at offset 4.
                (if (i32.ne (i32.load (i32.const 4)) (i32.const 5))
                  (then (call $proc_exit (i32.const 42))))
                (if (i32.ne (call $fd_prestat_dir_name (i32.const 3) (i32.const 64) (i32.const 5))
                            (i32.const 0))
                  (then (call $proc_exit (i32.const 43))))
                ;; "/home" as little-endian u32 plus the trailing 'e'.
                (if (i32.ne (i32.load (i32.const 64)) (i32.const 0x6d6f682f))
                  (then (call $proc_exit (i32.const 44))))
                (if (i32.ne (i32.load8_u (i32.const 68)) (i32.const 0x65))
                  (then (call $proc_exit (i32.const 45))))
                ;; iovec {buf=240, len=3} at offset 300.
                (i32.store (i32.const 300) (i32.const 240))
                (i32.store (i32.const 304) (i32.const 3))
                ;; path_open(dirfd=3, dirflags=0, path=200 "guest-out.txt" len=13,
                ;;           oflags=CREAT|TRUNC=9, rights_base=FD_WRITE=64,
                ;;           rights_inheriting=0, fdflags=0, opened_fd=310)
                (if (i32.ne
                      (call $path_open
                        (i32.const 3) (i32.const 0) (i32.const 200) (i32.const 13)
                        (i32.const 9) (i64.const 64) (i64.const 0)
                        (i32.const 0) (i32.const 312))
                      (i32.const 0))
                  (then (call $proc_exit (i32.const 51))))
                ;; fd_write(fd=[310], iovs=300, iovs_len=1, nwritten=320)
                (if (i32.ne
                      (call $fd_write (i32.load (i32.const 312)) (i32.const 300)
                                      (i32.const 1) (i32.const 320))
                      (i32.const 0))
                  (then (call $proc_exit (i32.const 52))))
                (if (i32.ne (i32.load (i32.const 320)) (i32.const 3))
                  (then (call $proc_exit (i32.const 53))))
              )
            )
        "#;

        let code = rt.run_wasi_module(wat.as_bytes(), no_stdio())?;
        assert_eq!(
            code,
            WasiRunOutcome::Exited(0),
            "guest check failed at stage {code:?}"
        );
        assert_eq!(
            fs::read_to_string(home.join("guest-out.txt"))?,
            "hi\n",
            "guest write should land in the host home directory"
        );
        assert!(home.join("host-visible.txt").exists());
        fs::remove_dir_all(&home).ok();
        Ok(())
    }

    /// A real guest cannot create a file through a read-only preopen.
    #[test]
    fn read_only_mount_rejects_guest_writes() -> Result<()> {
        let home = temp_home("modes-home");
        let read_only = temp_home("modes-ro");
        let mut rt = WasmRuntime::with_mounts(vec![
            MountSpec::new(
                home.clone(),
                HOME_GUEST_PATH,
                MountMode::ReadWrite,
                true,
                false,
            ),
            MountSpec::new(
                read_only.clone(),
                "/read-only",
                MountMode::ReadOnly,
                false,
                false,
            ),
        ])?;
        let write_attempt = r#"
            (module
              (import "wasi_snapshot_preview1" "path_open"
                (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
              (memory (export "memory") 1)
              (data (i32.const 200) "blocked.txt")
              (func (export "_start")
                ;; The second preopen is fd 4. A successful create on a RO
                ;; mount is a contract violation; any WASI errno is rejected.
                (if (i32.eq
                      (call $path_open
                        (i32.const 4) (i32.const 0) (i32.const 200) (i32.const 11)
                        (i32.const 9) (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 300))
                      (i32.const 0))
                  (then (call $proc_exit (i32.const 90))))
              )
            )
        "#;
        assert_eq!(
            rt.run_wasi_module(write_attempt.as_bytes(), no_stdio())?,
            WasiRunOutcome::Exited(0)
        );
        assert!(!read_only.join("blocked.txt").exists());
        Ok(())
    }

    #[test]
    fn command_passes_arguments_environment_and_file_streams_to_guest() -> Result<()> {
        let home = temp_home("command-streams").canonicalize()?;
        let input_path = home.join("stdin.bin");
        let output_path = home.join("stdout.bin");
        let error_path = home.join("stderr.bin");
        fs::write(&input_path, b"in\0put")?;
        let mut command = WasiCommand::new(
            InputFile::new(fs::File::open(input_path)?),
            OutputFile::new(fs::File::create(&output_path)?),
            OutputFile::new(fs::File::create(&error_path)?),
        );
        command.arguments = vec!["tool".into(), "a b".into(), "".into(), "你好".into()];
        command.environment = vec![("KEY".into(), "guest=one".into()), ("EMPTY".into(), "".into())];
        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home,
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;

        // Write the exact WASI argv buffer and then the raw stdin bytes to
        // stdout; write the exact environment buffer to stderr. Lengths come
        // from WASI, not from the fixture, so unexpected inherited entries
        // cannot go unnoticed.
        let wat = r#"
            (module
              (import "wasi_snapshot_preview1" "args_sizes_get"
                (func $args_sizes_get (param i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "args_get"
                (func $args_get (param i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "environ_sizes_get"
                (func $environ_sizes_get (param i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "environ_get"
                (func $environ_get (param i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "fd_read"
                (func $fd_read (param i32 i32 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
              (memory (export "memory") 1)
              (func (export "_start")
                (if (call $args_sizes_get (i32.const 0) (i32.const 4))
                  (then (call $exit (i32.const 20))))
                (if (i32.ne (i32.load (i32.const 0)) (i32.const 4))
                  (then (call $exit (i32.const 21))))
                (if (call $args_get (i32.const 64) (i32.const 256))
                  (then (call $exit (i32.const 22))))
                (if (call $environ_sizes_get (i32.const 8) (i32.const 12))
                  (then (call $exit (i32.const 23))))
                (if (i32.ne (i32.load (i32.const 8)) (i32.const 2))
                  (then (call $exit (i32.const 24))))
                (if (call $environ_get (i32.const 96) (i32.const 512))
                  (then (call $exit (i32.const 25))))
                (i32.store (i32.const 128) (i32.const 256))
                (i32.store (i32.const 132) (i32.load (i32.const 4)))
                (if (call $fd_write (i32.const 1) (i32.const 128) (i32.const 1) (i32.const 140))
                  (then (call $exit (i32.const 26))))
                (i32.store (i32.const 128) (i32.const 1024))
                (i32.store (i32.const 132) (i32.const 64))
                (if (call $fd_read (i32.const 0) (i32.const 128) (i32.const 1) (i32.const 140))
                  (then (call $exit (i32.const 27))))
                (i32.store (i32.const 132) (i32.load (i32.const 140)))
                (if (call $fd_write (i32.const 1) (i32.const 128) (i32.const 1) (i32.const 140))
                  (then (call $exit (i32.const 28))))
                (i32.store (i32.const 128) (i32.const 512))
                (i32.store (i32.const 132) (i32.load (i32.const 12)))
                (if (call $fd_write (i32.const 2) (i32.const 128) (i32.const 1) (i32.const 140))
                  (then (call $exit (i32.const 29))))
                (call $exit (i32.const 7))))
        "#;
        assert_eq!(
            rt.run_wasi_module(wat.as_bytes(), command)?,
            WasiRunOutcome::Exited(7)
        );
        let mut expected_stdout = "tool\0a b\0\0你好\0".as_bytes().to_vec();
        expected_stdout.extend_from_slice(b"in\0put");
        assert_eq!(fs::read(output_path)?, expected_stdout);
        assert_eq!(fs::read(error_path)?, b"KEY=guest=one\0EMPTY=\0");
        Ok(())
    }

    #[test]
    fn command_stream_effects_remain_visible_after_a_trap() -> Result<()> {
        let home = temp_home("command-trap").canonicalize()?;
        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home,
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;
        let stdout = MemoryOutputPipe::new(64);
        let stderr = MemoryOutputPipe::new(64);
        let command = WasiCommand::new(std::io::empty(), stdout.clone(), stderr.clone());
        let wat = r#"
            (module
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
              (memory (export "memory") 1)
              (data (i32.const 100) "prefix")
              (data (i32.const 120) "diag")
              (func (export "_start")
                (i32.store (i32.const 0) (i32.const 100))
                (i32.store (i32.const 4) (i32.const 6))
                (if (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8))
                  (then (call $exit (i32.const 90))))
                (i32.store (i32.const 0) (i32.const 120))
                (i32.store (i32.const 4) (i32.const 4))
                (if (call $fd_write (i32.const 2) (i32.const 0) (i32.const 1) (i32.const 8))
                  (then (call $exit (i32.const 91))))
                unreachable))
        "#;
        let error = rt.run_wasi_module(wat.as_bytes(), command).unwrap_err();
        assert!(error.to_string().contains("run WASI `_start`"), "{error}");
        assert_eq!(stdout.contents().as_ref(), b"prefix");
        assert_eq!(stderr.contents().as_ref(), b"diag");
        Ok(())
    }

    #[test]
    fn consecutive_commands_do_not_share_process_state() -> Result<()> {
        let home = temp_home("command-isolation").canonicalize()?;
        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home,
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;
        let wat = r#"
            (module
              (import "wasi_snapshot_preview1" "args_sizes_get"
                (func $args (param i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "environ_sizes_get"
                (func $env (param i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
              (memory (export "memory") 1)
              (func (export "_start")
                (if (call $args (i32.const 0) (i32.const 4))
                  (then (call $exit (i32.const 90))))
                (if (call $env (i32.const 8) (i32.const 12))
                  (then (call $exit (i32.const 91))))
                (call $exit (i32.add (i32.mul (i32.load (i32.const 0)) (i32.const 10))
                                     (i32.load (i32.const 8))))))
        "#;
        let mut first = no_stdio();
        first.arguments = vec!["one".into(), "two".into()];
        first.environment = vec![("A".into(), "one".into())];
        assert_eq!(
            rt.run_wasi_module(wat.as_bytes(), first)?,
            WasiRunOutcome::Exited(21)
        );
        assert_eq!(
            rt.run_wasi_module(wat.as_bytes(), no_stdio())?,
            WasiRunOutcome::Exited(0)
        );
        Ok(())
    }

    #[test]
    fn invalid_command_strings_fail_before_guest_effects() -> Result<()> {
        let home = temp_home("command-invalid").canonicalize()?;
        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home,
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;
        let wat = r#"
            (module
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 100) "started")
              (func (export "_start")
                (i32.store (i32.const 0) (i32.const 100))
                (i32.store (i32.const 4) (i32.const 7))
                (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))))
        "#;
        let mut reject = |arguments, environment| {
            let stdout = MemoryOutputPipe::new(64);
            let mut command = WasiCommand::new(std::io::empty(), stdout.clone(), std::io::empty());
            command.arguments = arguments;
            command.environment = environment;
            let error = rt.run_wasi_module(wat.as_bytes(), command).unwrap_err();
            assert!(error.to_string().contains("WASI_COMMAND_INVALID"), "{error}");
            assert!(stdout.contents().is_empty());
        };
        reject(vec!["bad\0arg".into()], vec![]);
        reject(vec![], vec![("BAD=KEY".into(), "value".into())]);
        reject(vec![], vec![("KEY".into(), "bad\0value".into())]);
        Ok(())
    }

    /// A guest that writes a marker then spins forever. Without execution
    /// controls this run could never return.
    const SPIN_WAT: &str = r#"
        (module
          (import "wasi_snapshot_preview1" "fd_write"
            (func $fd_write (param i32 i32 i32 i32) (result i32)))
          (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
          (memory (export "memory") 1)
          (data (i32.const 100) "spin")
          (func (export "_start")
            (i32.store (i32.const 0) (i32.const 100))
            (i32.store (i32.const 4) (i32.const 4))
            (if (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8))
              (then (call $exit (i32.const 80))))
            (loop (br 0))))
    "#;

    /// The deadline turns an unbounded CPU-loop guest into a bounded typed
    /// outcome and keeps output already written. The deadline is measured
    /// from run start and covers compilation, so it is generously larger than
    /// the marker+spin portion, but setup may legitimately consume the whole
    /// budget before any guest code executes. Marker-synchronized cancellation
    /// separately verifies that prior output survives an interruption. After a
    /// stopped run the same runtime serves the next command normally.
    /// `Duration::ZERO` is a distinct boundary: the run reports
    /// `DeadlineExceeded` before any guest code can execute.
    #[test]
    fn deadline_stops_cpu_bound_guest_and_runtime_recovers() -> Result<()> {
        let home = temp_home("deadline").canonicalize()?;
        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home,
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;

        let stdout = MemoryOutputPipe::new(64);
        let mut command = WasiCommand::new(std::io::empty(), stdout.clone(), std::io::empty());
        command.control.deadline = Some(Duration::from_secs(5));
        let started = Instant::now();
        let outcome = rt.run_wasi_module(SPIN_WAT.as_bytes(), command)?;
        assert_eq!(outcome, WasiRunOutcome::DeadlineExceeded);
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the deadline must interrupt the infinite guest well inside the \
             guard window; a joined-forever run would exceed it"
        );

        // The same runtime — including its epoch bookkeeping — serves the
        // next command normally, so a stopped run is not a poisoned engine.
        let plain = r#"(module (func (export "_start")))"#;
        assert_eq!(
            rt.run_wasi_module(plain.as_bytes(), no_stdio())?,
            WasiRunOutcome::Exited(0)
        );

        // A zero deadline elapses before any guest work: the writer guest
        // must not even produce its first write.
        let fresh = MemoryOutputPipe::new(64);
        let mut zeroed = WasiCommand::new(std::io::empty(), fresh.clone(), std::io::empty());
        zeroed.control.deadline = Some(Duration::ZERO);
        assert_eq!(
            rt.run_wasi_module(SPIN_WAT.as_bytes(), zeroed)?,
            WasiRunOutcome::DeadlineExceeded
        );
        assert!(fresh.contents().is_empty());
        Ok(())
    }

    /// Cancellation is deterministic in both directions: a handle cancelled
    /// before the call reports `Cancelled` without guest effects, and a
    /// mid-run cancel stops a CPU-bound guest while retaining prior output.
    #[test]
    fn cancellation_stops_guest_before_and_during_execution() -> Result<()> {
        let home = temp_home("cancel").canonicalize()?;
        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home,
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;

        // Pre-cancelled: rejected before mounts/compile/spawn, so the guest
        // cannot have produced its write.
        let token = WasiCancellation::default();
        token.cancel();
        let early = MemoryOutputPipe::new(64);
        let mut cancelled = WasiCommand::new(std::io::empty(), early.clone(), std::io::empty());
        cancelled.control.cancel = Some(token);
        assert_eq!(
            rt.run_wasi_module(SPIN_WAT.as_bytes(), cancelled)?,
            WasiRunOutcome::Cancelled
        );
        assert!(early.contents().is_empty());

        // Mid-run: the guest announces it actually started by writing its
        // marker. Only then cancel — a fixed sleep would race module
        // compilation and could legitimately cancel before any guest code,
        // leaving the marker unwritten and the assertion meaningless.
        let token = WasiCancellation::default();
        let later = MemoryOutputPipe::new(64);
        let mut running = WasiCommand::new(std::io::empty(), later.clone(), std::io::empty());
        running.control.cancel = Some(token.clone());
        let observed = later.clone();
        let canceller = std::thread::spawn(move || {
            // Poll until the guest's real marker lands (guest actually
            // running), bounded so a broken run cannot hang the test.
            let waited = Instant::now();
            while observed.contents().as_ref() != b"spin"
                && waited.elapsed() < Duration::from_secs(60)
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            token.cancel();
        });
        let started = Instant::now();
        assert_eq!(
            rt.run_wasi_module(SPIN_WAT.as_bytes(), running)?,
            WasiRunOutcome::Cancelled
        );
        assert!(
            started.elapsed() < Duration::from_secs(90),
            "cancellation must reach the guest at an epoch checkpoint"
        );
        assert_eq!(
            later.contents().as_ref(),
            b"spin",
            "the cancel raced with a running guest, so its committed output \
             is retained"
        );
        canceller.join().expect("canceller thread");
        Ok(())
    }
}
