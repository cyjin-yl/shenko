use crate::mount::{MountMode, MountSpec, validate_mounts};
use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tracing::info;
use wasmtime::{Config, Engine, Instance, Linker, Module, Store, UpdateDeadline, Val};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::{DirPerms, FilePerms, I32Exit, WasiCtxBuilder};
pub use wasmtime_wasi::cli::{InputFile, OutputFile, StdinStream, StdoutStream};

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
/// A guest blocked inside a blocking host call (for example `fd_read` on an
/// empty pipe, or `poll_oneoff` on a WASI clock subscription) stays blocked
/// until that host call returns; the deadline is a bound on guest-executed
/// work and the host-side run, not a promise to abort blocked host calls.
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
}

/// How often the watcher advances the controlled engine's epoch, in ticks.
/// One tick equals one `Instant`-clock interval of this duration. Cancellation
/// and deadline checks run when guest code reaches an epoch checkpoint after
/// a tick, so observed latency is at most one interval plus one guest
/// checkpoint for CPU-bound guests.
const WASI_EPOCH_CHECK_INTERVAL: Duration = Duration::from_millis(10);

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
    /// Optional execution controls; `WasiRunControl::default()` runs without
    /// a deadline or cancellation, exactly as before.
    pub control: WasiRunControl,
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
            control: WasiRunControl::default(),
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
            if value.contains('\0') {
                bail!("WASI_COMMAND_INVALID: environment value {index} contains NUL");
            }
        }
        Ok(())
    }
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
    /// Create a runtime with a default-config engine and a fresh empty store.
    pub fn new() -> Result<Self> {
        info!("WASM runtime init (wasmtime-45)");
        let engine = Engine::default();
        let store = Store::new(&engine, ());
        Ok(Self {
            engine,
            store: Some(store),
            home_dir: PathBuf::new(),
            mounts: Vec::new(),
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
    /// comes from `self.mounts` with its declared RO/RW permissions. Wasmtime
    /// currently reopens the checked pathname, so an ancestor swap can still
    /// redirect a preopen; do not treat validation as a capability binding.
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
    /// controls interrupt guest code only; a guest blocked inside a blocking
    /// host call stays blocked until that call returns.
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

/// Per-store state for a controlled run: the WASI context plus the stop
/// bookkeeping consulted by the epoch-deadline callback. `stop` records the
/// typed outcome the callback committed to, so the caller can distinguish a
/// deliberate kill from an ordinary guest trap.
struct ControlledCtx {
    wasi: WasiP1Ctx,
    cancel: Option<WasiCancellation>,
    deadline: Option<Instant>,
    stop: Option<WasiRunOutcome>,
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

/// Instantiate a WASI preview1 command with the home preopen and run `_start`.
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
    let cancel = command.control.cancel.clone();
    let canonical_sources = validate_mounts(mounts, home)?;
    let module =
        Module::new(engine, wasm_or_wat).map_err(|e| anyhow!("compile WASI module: {e}"))?;

    let mut builder = WasiCtxBuilder::new();
    builder
        .args(&command.arguments)
        .envs(&command.environment)
        .stdin(command.stdin)
        .stdout(command.stdout)
        .stderr(command.stderr);
    for (mount, source) in mounts.iter().zip(canonical_sources.iter()) {
        let (dir_perms, file_perms) = match mount.mode {
            MountMode::ReadOnly => (DirPerms::READ, FilePerms::READ),
            MountMode::ReadWrite => (DirPerms::all(), FilePerms::all()),
            MountMode::CopyOnWrite => unreachable!("validated COW must fail before preopen"),
        };
        // wasmtime 45 has its own `Error` type (not `std::error::Error`), so
        // `anyhow::Context` does not apply here; embed the binding in the message.
        builder
            .preopened_dir(source, &mount.guest_path, dir_perms, file_perms)
            .map_err(|e| {
                anyhow!(
                    "preopen {} at {} ({:?}): {e}",
                    source.display(),
                    mount.guest_path,
                    mount.mode
                )
            })?;
    }
    let wasi = builder.build_p1();

    if !controlled {
        let mut linker: Linker<WasiP1Ctx> = Linker::new(engine);
        p1::add_to_linker_sync(&mut linker, |cx| cx)
            .map_err(|e| anyhow!("configure wasi preview1 linker: {e}"))?;

        let mut store = Store::new(engine, wasi);
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
            wasi,
            cancel,
            deadline: deadline_at,
            stop: None,
        },
    );
    let mut linker: Linker<ControlledCtx> = Linker::new(engine);
    p1::add_to_linker_sync(&mut linker, |cx| &mut cx.wasi)
        .map_err(|e| anyhow!("configure wasi preview1 linker: {e}"))?;

    // The epoch deadline is one check interval, not the caller's wall-clock
    // deadline: at every checkpoint the callback re-evaluates cancellation
    // (an explicit request wins deterministically) and the real deadline
    // against `Instant`. The watcher thread only advances engine epochs; all
    // stop decisions happen on the execution thread in this callback, so a
    // committed `stop` always corresponds to the trap that follows.
    store.epoch_deadline_callback(|mut cx| {
        let data = cx.data_mut();
        let kill = if data
            .cancel
            .as_ref()
            .is_some_and(WasiCancellation::is_cancelled)
        {
            Some(WasiRunOutcome::Cancelled)
        } else if data
            .deadline
            .is_some_and(|d| Instant::now() >= d)
        {
            Some(WasiRunOutcome::DeadlineExceeded)
        } else {
            None
        };
        match kill {
            Some(outcome) => {
                cx.data_mut().stop = Some(outcome);
                Err(anyhow!("WASI run stopped: {outcome:?}"))
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
    {
        let data = store.data();
        let pre = if data
            .cancel
            .as_ref()
            .is_some_and(WasiCancellation::is_cancelled)
        {
            Some(WasiRunOutcome::Cancelled)
        } else if data
            .deadline
            .is_some_and(|d| Instant::now() >= d)
        {
            Some(WasiRunOutcome::DeadlineExceeded)
        } else {
            None
        };
        if let Some(outcome) = pre {
            return Ok(outcome);
        }
    }

    let stopped = |store: &Store<ControlledCtx>| store.data().stop;
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

    /// One focused mount-semantics scenario: a real guest cannot create a file
    /// through a RO preopen, and requesting unimplemented COW fails instead of
    /// silently becoming direct RW.
    #[test]
    fn read_only_and_cow_mounts_keep_their_semantics() -> Result<()> {
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

        let error = WasmRuntime::with_mounts(vec![MountSpec::new(
            home,
            HOME_GUEST_PATH,
            MountMode::CopyOnWrite,
            true,
            true,
        )])
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
        assert!(error.contains("MOUNT_MODE_UNSUPPORTED"), "{error}");
        assert!(error.contains("no direct-RW fallback"), "{error}");
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

    /// Count this process's live threads whose comm starts with `shenko-wasi`
    /// (worker + epoch watcher; Linux comm is truncated to 15 bytes, so the
    /// prefix covers `shenko-wasi-epo`). Runs on the project dev container,
    /// which is Linux; on other platforms the check is skipped.
    #[cfg(target_os = "linux")]
    fn live_shenko_wasi_threads() -> usize {
        fs::read_dir("/proc/self/task")
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                fs::read_to_string(entry.path().join("comm"))
                    .map(|comm| comm.trim_start().starts_with("shenko-wasi"))
                    .unwrap_or(false)
            })
            .count()
    }

    #[cfg(not(target_os = "linux"))]
    fn live_shenko_wasi_threads() -> usize {
        0
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
    /// outcome, keeps output already written, and leaves the runtime usable
    /// for the next command with no surviving worker or watcher threads.
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
        command.control.deadline = Some(Duration::from_millis(100));
        let started = Instant::now();
        let outcome = rt.run_wasi_module(SPIN_WAT.as_bytes(), command)?;
        assert_eq!(outcome, WasiRunOutcome::DeadlineExceeded);
        assert_eq!(
            stdout.contents().as_ref(),
            b"spin",
            "output committed before the deadline is retained"
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "an unbounded guest must be interrupted, not joined forever"
        );
        assert_eq!(
            live_shenko_wasi_threads(),
            0,
            "worker and watcher threads must not survive the call"
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

        // Mid-run: the guest spins until the cloned handle cancels it.
        let token = WasiCancellation::default();
        let later = MemoryOutputPipe::new(64);
        let mut running = WasiCommand::new(std::io::empty(), later.clone(), std::io::empty());
        running.control.cancel = Some(token.clone());
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            token.cancel();
        });
        let started = Instant::now();
        assert_eq!(
            rt.run_wasi_module(SPIN_WAT.as_bytes(), running)?,
            WasiRunOutcome::Cancelled
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "cancellation must reach the guest at an epoch checkpoint"
        );
        assert_eq!(
            later.contents().as_ref(),
            b"spin",
            "output committed before cancellation is retained"
        );
        assert_eq!(live_shenko_wasi_threads(), 0);
        canceller.join().expect("canceller thread");
        Ok(())
    }
}
