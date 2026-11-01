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

/// What interrupted an in-flight cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellInterrupt {
    CancelRequested,
    DeadlineExceeded,
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
    stdin: File,
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
                // No whole-run deadline: the guest lives for the session.
                // Mid-cell interrupts use the cancel token; per-cell
                // wall-clock bounds are enforced by the caller.
                command.control = WasiRunControl {
                    deadline: None,
                    cancel: Some(run_cancel),
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
            stdin: stdin_write,
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
                    run_native_child(argv, env, workdir, guest_log, guest_err_log, stdin_read, run_cancel);
                *thread_outcome.lock().expect("live outcome lock") = Some(recorded);
                thread_dead.store(true, Ordering::SeqCst);
            })
            .context("spawn native session thread")?;
        Ok(Self {
            cancel,
            stdin: stdin_write,
            outcome,
            dead,
            join: Some(join),
        })
    }

    pub fn is_dead(&self) -> bool { self.dead.load(Ordering::SeqCst) }

    pub fn cancel(&self) { self.cancel.cancel(); }

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

    /// Deliver one cell and wait for its report. On interrupt the process is
    /// killed and `Err(CellInterrupt)` is returned after death is observed;
    /// the caller journals the partial record from the durable stream files.
    pub fn run_cell(
        &mut self,
        header: &CellHeader,
        source: &[u8],
        report_path: &Path,
        deadline: Option<Duration>,
    ) -> std::result::Result<(), CellInterrupt> {
        if self.is_dead() {
            return Err(CellInterrupt::CancelRequested);
        }
        let header_json = serde_json::to_vec(header).expect("cell header serializes");
        let framed = (|| -> std::io::Result<()> {
            self.stdin.write_all(&(header_json.len() as u64).to_be_bytes())?;
            self.stdin.write_all(&header_json)?;
            self.stdin.write_all(&(source.len() as u64).to_be_bytes())?;
            self.stdin.write_all(source)?;
            self.stdin.flush()?;
            Ok(())
        })();
        if framed.is_err() {
            // Broken pipe: the guest died between cells.
            self.dead.store(true, Ordering::SeqCst);
            return Err(CellInterrupt::CancelRequested);
        }

        let started = Instant::now();
        loop {
            if report_path.exists() {
                return Ok(());
            }
            if self.cancel.is_cancelled() {
                self.await_death(DEATH_TIMEOUT);
                return Err(CellInterrupt::CancelRequested);
            }
            if deadline.is_some_and(|limit| started.elapsed() > limit) {
                self.cancel.cancel();
                self.await_death(DEATH_TIMEOUT);
                return Err(CellInterrupt::DeadlineExceeded);
            }
            if self.is_dead() {
                return Err(CellInterrupt::CancelRequested);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Terminate the owned process: close stdin (guest EOF exit), join, and
    /// give up on a guest that ignores EOF past the drain budget (the native
    /// child is killed by the cancel path if it stays stuck). Journal and
    /// outputs on disk are untouched.
    pub fn shutdown(mut self) -> Option<ProcessOutcome> {
        drop(self.stdin);
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

fn run_native_child(
    argv: Vec<String>,
    env: Vec<(String, String)>,
    workdir: PathBuf,
    guest_log: File,
    guest_err_log: File,
    stdin_read: File,
    cancel: WasiCancellation,
) -> ProcessOutcome {
    let mut command = std::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(env)
        .current_dir(&workdir)
        .stdin(std::process::Stdio::from(stdin_read))
        .stdout(std::process::Stdio::from(guest_log))
        .stderr(std::process::Stdio::from(guest_err_log));
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
                    let _ = child.kill();
                    let _ = child.wait();
                    return ProcessOutcome { outcome: Some(WasiRunOutcome::Cancelled), transport_error: None };
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return ProcessOutcome {
                    outcome: None,
                    transport_error: Some(format!("wait {}: {error}", argv[0])),
                }
            }
        }
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
