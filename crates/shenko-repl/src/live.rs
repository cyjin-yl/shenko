//! The live session engine: one real long-lived interpreter process per
//! session (SPEC §19 "有状态会话在进程存活时保留变量").
//!
//! The host owns the process; cells travel to it over a framed stdin
//! protocol while per-cell streams and the structured driver report land in
//! guest-visible files under `/home/.shenko/cells/` (rename-ordered so the
//! report file appearing means the streams are complete and flushed):
//!
//! ```text
//! host -> guest  [u64be header_len][header JSON][u64be payload_len][cell source]
//! guest          writes <cell_dir>/<seq>.out / .err, then renames
//!                <cell_dir>/<seq>.report.json into place last
//! ```
//!
//! header JSON: {"seq": u64, "cell_dir": path, "state_path": path,
//! "env_dir": path}. Paths are guest paths (`/home/...`) for the WASI
//! backend and host paths for the native backend; drivers are backend
//! neutral and just use what the header says.
//!
//! Interrupting a cell kills the owned process (epoch trap on WASI, kill on
//! native): effects already committed stand, no complete report exists, and
//! the session reports recovery-required until `restore()` rebuilds a fresh
//! process from the durable journal. A cancel request is honored at the next
//! interrupt point and is never proof that all work stopped — a guest
//! blocked inside a blocking host call can outlive the request.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use shenko_wasm::{
    InputFile, OutputFile, WasiCancellation, WasiCommand, WasiRunControl, WasiRunOutcome,
    WasmRuntime,
};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Poll granularity while waiting for a cell report.
const POLL_INTERVAL: Duration = Duration::from_millis(2);
/// How long shutdown waits for a clean guest exit before giving up on join.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an interrupted guest gets to reach the observed death state.
const DEATH_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-cell protocol paths handed to the driver in the header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellHeader {
    /// 1-based cell sequence; the guest counts identically during replay.
    pub seq: u64,
    /// Directory (guest path on WASI, host path on native) holding the
    /// per-cell `<seq>.out`, `<seq>.err` and `<seq>.report.json` files.
    pub cell_dir: String,
    /// Driver state file path (kernel-declared persistence target).
    pub state_path: String,
    /// Environment directory for this cell's generation (site-packages /
    /// node_modules parent). Changed by `install_dependencies` commits.
    pub env_dir: String,
}

/// Why an in-flight cell did not produce a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellFailure {
    /// A caller cancellation was requested and the process was killed.
    CancelRequested,
    /// The per-cell wall-clock budget elapsed.
    DeadlineExceeded,
    /// The owned interpreter ended on its own: no cancel and no deadline was
    /// requested. `detail` retains the real exit status or the transport
    /// error, so a driver that died at startup (missing module, unsupported
    /// `node:repl`, `sys.exit()` in a cell) is never reported as a cancel the
    /// caller never asked for.
    ProcessExited { detail: String },
    /// The frame could not be delivered and the interpreter was still alive
    /// after the death-observation window. It is killed rather than left
    /// executing a partially written frame; the cell's completion is unknown
    /// and this is not a caller cancel.
    DeliveryFailed { detail: String },
}

/// Why the owned process is gone.
#[derive(Debug, Clone)]
pub struct ProcessOutcome {
    pub outcome: Option<WasiRunOutcome>,
    pub transport_error: Option<String>,
}

/// One owned long-lived interpreter process plus its protocol handles.
pub struct LiveProcess {
    cancel: WasiCancellation,
    stdin: Option<File>,
    outcome: Arc<Mutex<Option<ProcessOutcome>>>,
    dead: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for LiveProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveProcess")
            .field("dead", &self.dead.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl LiveProcess {
    /// Spawn a WASI preview1 guest that stays alive across cells.
    ///
    /// `module` is the exact interpreter module bytes (already digest-bound
    /// by the caller). The guest's own stdout/stderr go to durable session
    /// logs; the cell protocol uses stdin framing plus per-cell files.
    pub fn spawn_wasi(
        mut runtime: WasmRuntime,
        module: Vec<u8>,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        guest_log: File,
        guest_err_log: File,
        limits: shenko_wasm::WasiResourceLimits,
    ) -> Result<Self> {
        let (stdin_read, stdin_write) = pipe().context("create WASI session stdin pipe")?;
        let cancel = WasiCancellation::default();
        let run_cancel = cancel.clone();
        let outcome: Arc<Mutex<Option<ProcessOutcome>>> = Arc::new(Mutex::new(None));
        let thread_outcome = Arc::clone(&outcome);
        let dead = Arc::new(AtomicBool::new(false));
        let thread_dead = Arc::clone(&dead);
        let join = std::thread::Builder::new()
            .name("shenko-repl-live".into())
            .spawn(move || {
                let mut command = WasiCommand::new(
                    InputFile::new(stdin_read),
                    OutputFile::new(guest_log),
                    OutputFile::new(guest_err_log),
                );
                command.arguments = argv;
                command.environment = env;
                command.limits = limits;
                // No whole-run deadline: the guest lives for the session.
                // Mid-cell interrupts use the cancel token; per-cell
                // wall-clock bounds are enforced by the caller.
                command.control = WasiRunControl {
                    deadline: None,
                    cancel: Some(run_cancel),
                    revocation: None,
                };
                let result = runtime.run_wasi_module(&module, command);
                let recorded = match result {
                    Ok(outcome) => ProcessOutcome {
                        outcome: Some(outcome),
                        transport_error: None,
                    },
                    Err(error) => ProcessOutcome {
                        outcome: None,
                        transport_error: Some(format!("{error:#}")),
                    },
                };
                *thread_outcome.lock().expect("live outcome lock") = Some(recorded);
                thread_dead.store(true, Ordering::SeqCst);
            })
            .context("spawn live WASI session thread")?;
        Ok(Self {
            cancel,
            stdin: Some(stdin_write),
            outcome,
            dead,
            join: Some(join),
        })
    }

    /// Spawn a native host interpreter process with the same protocol.
    pub fn spawn_native(
        executable: PathBuf,
        driver_path: PathBuf,
        extra_argv: Vec<String>,
        env: Vec<(String, String)>,
        workdir: PathBuf,
        guest_log: File,
        guest_err_log: File,
        max_memory_bytes: Option<u64>,
    ) -> Result<Self> {
        let (stdin_read, stdin_write) = pipe().context("create native session stdin pipe")?;
        let cancel = WasiCancellation::default();
        let run_cancel = cancel.clone();
        let outcome: Arc<Mutex<Option<ProcessOutcome>>> = Arc::new(Mutex::new(None));
        let thread_outcome = Arc::clone(&outcome);
        let dead = Arc::new(AtomicBool::new(false));
        let thread_dead = Arc::clone(&dead);
        let mut argv = vec![
            executable.to_string_lossy().into_owned(),
            driver_path.to_string_lossy().into_owned(),
        ];
        argv.extend(extra_argv);
        let join = std::thread::Builder::new()
            .name("shenko-repl-live".into())
            .spawn(move || {
                let recorded =
                    run_native_child(argv, env, workdir, guest_log, guest_err_log, stdin_read, run_cancel, max_memory_bytes);
                *thread_outcome.lock().expect("live outcome lock") = Some(recorded);
                thread_dead.store(true, Ordering::SeqCst);
            })
            .context("spawn native session thread")?;
        Ok(Self {
            cancel,
            stdin: Some(stdin_write),
            outcome,
            dead,
            join: Some(join),
        })
    }

    pub fn is_dead(&self) -> bool { self.dead.load(Ordering::SeqCst) }

    pub fn cancel(&self) { self.cancel.cancel(); }
    pub fn cancellation(&self) -> WasiCancellation { self.cancel.clone() }

    /// Wait (briefly) for process death after an interrupt.
    pub fn await_death(&self, timeout: Duration) {
        let start = Instant::now();
        while !self.dead.load(Ordering::SeqCst) && start.elapsed() < timeout {
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    pub fn outcome(&self) -> Option<ProcessOutcome> {
        self.outcome.lock().expect("live outcome lock").clone()
    }

    /// Classify an interpreter end that no interrupt asked for. The real exit
    /// status (or the transport error that ended the process) is retained; an
    /// unobserved status is reported as unobserved rather than invented.
    fn death_failure(&self) -> CellFailure {
        if let Some(recorded) = self.outcome() {
            if let Some(error) = recorded.transport_error {
                return CellFailure::ProcessExited {
                    detail: format!("interpreter failed: {error}"),
                };
            }
            match recorded.outcome {
                Some(WasiRunOutcome::Exited(code)) => return CellFailure::ProcessExited {
                    detail: format!(
                        "interpreter exited with status {code} without completing the cell"
                    ),
                },
                Some(WasiRunOutcome::Cancelled) => return CellFailure::ProcessExited {
                    detail: "the runtime cancelled the interpreter without a host interrupt request"
                        .into(),
                },
                Some(WasiRunOutcome::DeadlineExceeded) => return CellFailure::ProcessExited {
                    detail: "the runtime's own deadline ended the interpreter".into(),
                },
                None => {}
            }
        }
        CellFailure::ProcessExited {
            detail: "interpreter ended and its exit status was not observed".into(),
        }
    }

    /// Deliver one cell and wait for its report. On interrupt the process is
    /// killed and the failure is returned after death is observed;
    /// the caller journals the partial record from the durable stream files.
    /// A process that ends on its own is reported as exactly that, with its
    /// real status, never as a caller interrupt.
    pub fn run_cell(
        &mut self,
        header: &CellHeader,
        source: &[u8],
        report_path: &Path,
        deadline: Option<Duration>,
    ) -> std::result::Result<(), CellFailure> {
        if self.is_dead() {
            return Err(self.death_failure());
        }
        let header_json = serde_json::to_vec(header).expect("cell header serializes");
        let started = Instant::now();
        let framed = (|| -> std::io::Result<()> {
            let header_len = (header_json.len() as u64).to_be_bytes();
            let source_len = (source.len() as u64).to_be_bytes();
            for mut remaining in [&header_len[..], &header_json[..], &source_len[..], source] {
                while !remaining.is_empty() {
                    if self.cancel.is_cancelled() || deadline.is_some_and(|limit| started.elapsed() >= limit) {
                        self.cancel.cancel();
                        return Err(std::io::ErrorKind::TimedOut.into());
                    }
                    match self.stdin.as_mut().ok_or(std::io::ErrorKind::BrokenPipe)?.write(remaining) {
                        Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                        Ok(written) => remaining = &remaining[written..],
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(POLL_INTERVAL),
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {},
                        Err(error) => return Err(error),
                    }
                }
            }
            Ok(())
        })();
        if framed.is_err() {
            // Delivery failure is not by itself observed worker termination.
            // A deadline or an explicit cancel owns the classification; an
            // unexplained broken pipe means the interpreter closed its own
            // stdin (it ended), so retain whatever status it left behind.
            if deadline.is_some_and(|limit| started.elapsed() >= limit) {
                self.cancel.cancel();
                self.await_death(DEATH_TIMEOUT);
                return Err(CellFailure::DeadlineExceeded);
            }
            if self.cancel.is_cancelled() {
                self.await_death(DEATH_TIMEOUT);
                return Err(CellFailure::CancelRequested);
            }
            self.await_death(DEATH_TIMEOUT);
            if self.is_dead() {
                return Err(self.death_failure());
            }
            // Still alive with a half-written frame: never let it run a
            // partially delivered cell. Kill the owned process and report the
            // undelivered frame; completion is unknown, and this is not a
            // caller cancel.
            self.cancel.cancel();
            self.await_death(DEATH_TIMEOUT);
            return Err(CellFailure::DeliveryFailed {
                detail: "the cell frame was not delivered completely; the interpreter was killed \
                         and the cell's completion is unknown"
                    .into(),
            });
        }

        loop {
            if report_path.exists() {
                return Ok(());
            }
            if deadline.is_some_and(|limit| started.elapsed() > limit) {
                self.cancel.cancel();
                self.await_death(DEATH_TIMEOUT);
                return Err(CellFailure::DeadlineExceeded);
            }
            if self.cancel.is_cancelled() {
                self.await_death(DEATH_TIMEOUT);
                return Err(CellFailure::CancelRequested);
            }
            if self.is_dead() {
                return Err(self.death_failure());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Terminate the owned process: close stdin (guest EOF exit), join, and
    /// give up on a guest that ignores EOF past the drain budget (the native
    /// child is killed by the cancel path if it stays stuck). Journal and
    /// outputs on disk are untouched.
    pub fn shutdown(mut self) -> Option<ProcessOutcome> {
        drop(self.stdin.take());
        if let Some(join) = self.join.take() {
            let start = Instant::now();
            while !self.dead.load(Ordering::SeqCst) && start.elapsed() < DRAIN_TIMEOUT {
                std::thread::sleep(POLL_INTERVAL);
            }
            if !self.dead.load(Ordering::SeqCst) {
                self.cancel();
                self.await_death(DEATH_TIMEOUT);
            }
            let _ = join.join();
        }
        self.outcome()
    }
}

impl Drop for LiveProcess {
    fn drop(&mut self) {
        drop(self.stdin.take());
        self.cancel.cancel();
        if let Some(join) = self.join.take() {
            // A blocked hostcall retains this owner rather than silently
            // detaching a still-running interpreter and releasing capacity.
            let _ = join.join();
        }
    }
}

fn run_native_child(
    argv: Vec<String>,
    env: Vec<(String, String)>,
    workdir: PathBuf,
    guest_log: File,
    guest_err_log: File,
    stdin_read: File,
    cancel: WasiCancellation,
    max_memory_bytes: Option<u64>,
) -> ProcessOutcome {
    let mut command = std::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .process_group(0)
        .env_clear()
        .envs(env)
        .current_dir(&workdir)
        .stdin(std::process::Stdio::from(stdin_read))
        .stdout(std::process::Stdio::from(guest_log))
        .stderr(std::process::Stdio::from(guest_err_log));
    if let Some(bytes) = max_memory_bytes {
        // pre_exec performs only async-signal-safe setrlimit; the limit is
        // inherited by descendants and cannot be raised past this hard cap.
        unsafe {
            command.pre_exec(move || {
                let cap = libc::rlimit { rlim_cur: bytes as libc::rlim_t,
                    rlim_max: bytes as libc::rlim_t };
                if libc::setrlimit(libc::RLIMIT_AS, &cap) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ProcessOutcome {
                outcome: None,
                transport_error: Some(format!("spawn {}: {error}", argv[0])),
            }
        }
    };
    // Poll: cancel -> kill; otherwise reap the exit status. The caller's
    // per-cell deadline also funnels through `cancel`.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                terminate_process_group(child.id());
                let code = status.code().unwrap_or(-1);
                let outcome = if cancel.is_cancelled() && code != 0 {
                    WasiRunOutcome::Cancelled
                } else {
                    WasiRunOutcome::Exited(code)
                };
                return ProcessOutcome { outcome: Some(outcome), transport_error: None };
            }
            Ok(None) => {
                if cancel.is_cancelled() {
                    // Kill the owned group, not only the interpreter: a cell
                    // may have spawned subprocesses. Do not report death or
                    // release the worker until the interpreter is reaped.
                    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL); }
                    match child.wait() {
                        Ok(_) => {
                            terminate_process_group(child.id());
                            return ProcessOutcome {
                                outcome: Some(WasiRunOutcome::Cancelled), transport_error: None,
                            };
                        },
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            // Unknown termination is not permission to detach.
                            std::thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                // Preserve ownership while termination cannot be observed.
                // ECHILD means the OS no longer has an owned child to reap.
                if error.raw_os_error() == Some(libc::ECHILD) {
                    terminate_process_group(child.id());
                    return ProcessOutcome { outcome: None,
                        transport_error: Some(format!("wait {}: {error}", argv[0])) };
                }
                unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL); }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

pub(crate) fn terminate_process_group(id: u32) {
    let group = -(id as libc::pid_t);
    loop {
        unsafe { libc::kill(group, libc::SIGKILL); }
        if unsafe { libc::kill(group, 0) } < 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return;
        }
        // Descendants, including zombies not yet reaped by their OS owner,
        // retain the group's capacity until absence is actually observed.
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Create an OS pipe and return (read_end, write_end) as Files.
pub fn pipe() -> std::io::Result<(File, File)> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe(2) fills the array with two fresh fds; both are uniquely
    // owned here and wrapped in Files that close them on drop.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Only the host write end is nonblocking. Guest reads retain normal
    // blocking semantics; delivery polls deadline/cancellation on backpressure.
    let flags = unsafe { libc::fcntl(fds[1], libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(fds[0]); libc::close(fds[1]); }
        return Err(error);
    }
    // SAFETY: fds[0] is the freshly created pipe read end, uniquely owned.
    let read = unsafe { File::from_raw_fd(fds[0]) };
    // SAFETY: fds[1] is the freshly created pipe write end, uniquely owned.
    let write = unsafe { File::from_raw_fd(fds[1]) };
    Ok((read, write))
}

/// Append-mode durable log file for one guest stream.
pub fn open_guest_log(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open guest log {}", path.display()))?)
}

/// Read and parse the driver report file (one bounded JSON line), when present.
pub fn read_driver_report(path: &Path) -> Result<Option<crate::contract::DriverReport>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
    };
    let mut buf = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut buf)?;
    if buf.is_empty() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&buf);
    let line = text.lines().next().unwrap_or("");
    Ok(Some(serde_json::from_str(line).context("parse driver report")?))
}
