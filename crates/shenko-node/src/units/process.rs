//! Bounded command trials and their process, output-reader and owner lifetimes.
//!
//! Attempts are durable before spawning. Reconciliation retains Unknown when a
//! child may have run, and a live foreign owner is never retired as a crash.

use anyhow::{Context, Result, ensure};
use crate::Node;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use super::codec::MAX_TEXT_BYTES;
use super::installation::{InstalledUnit, now, string_list};
use super::materialization::verify_artifact;
use super::trials::TrialOutcome;

pub const MAX_ARGV: usize = 64;
pub const MAX_ARG_BYTES: usize = 4096;
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub const DEFAULT_TRIAL_TIMEOUT_MS: u64 = 30_000;
pub const MAX_TRIAL_TIMEOUT_MS: u64 = 600_000;

/// One program probe of the local environment.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProgramProbe {
    pub program: String,
    pub resolved: Option<String>,
    pub size: Option<u64>,
    pub modified_unix: Option<u64>,
}

/// The environment a trial was bound to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TrialEnvironment {
    pub os: String,
    pub arch: String,
    pub kernel: Option<String>,
    pub programs: Vec<ProgramProbe>,
    pub digest: Digest,
}

/// Append-only trial evidence of one attempt.
///
/// A trial is either an investigation of the environment or a readiness proof
/// for the installed artifact. Only an artifact-bound trial, whose artifact
/// digest was verified before and after the run, can prove readiness.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TrialEvidence {
    pub attempt_id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub outcome: TrialOutcome,
    pub environment_digest: Digest,
    pub environment: TrialEnvironment,
    /// The command that actually ran, after the artifact placeholder was
    /// expanded.
    pub argv: Vec<String>,
    /// The command the caller asked for, before expansion.
    pub requested_argv: Vec<String>,
    pub cwd: Option<String>,
    pub risk: String,
    pub benefit: String,
    pub consequence: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub interrupted: bool,
    /// The run referenced the materialized artifact of this unit.
    pub artifact_bound: bool,
    pub artifact_path: Option<String>,
    /// The materialized artifact matched the installed digest before the run.
    pub artifact_verified_before: bool,
    /// The materialized artifact still matched the installed digest after it.
    pub artifact_verified_after: bool,
    /// The output could not be read completely, so the result is not trusted.
    pub reader_failed: bool,
    /// The whole process group was signalled on the deadline.
    pub group_signalled: bool,
    /// Descendants outside the signalled group may still be running.
    pub descendants_unknown: bool,
    pub owner_pid: i64,
    pub owner_runtime: String,
    pub reason: String,
}

/// Command arguments, paths, environment probes, descriptions and captured
/// stdout/stderr remain local rather than being redacted by key-name rules.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CommandTrialProvenance {
    pub attempt_id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub outcome: TrialOutcome,
    pub environment_digest: Digest,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub interrupted: bool,
    pub artifact_bound: bool,
    pub artifact_verified_before: bool,
    pub artifact_verified_after: bool,
    pub reader_failed: bool,
    pub group_signalled: bool,
    pub descendants_unknown: bool,
}

/// Whether a command trial's evidence records that its command really ran.
///
/// A command that could not be started still writes a trial fact, and an
/// attempt retired without its owner leaves one with no output at all;
/// neither records an execution. A real run leaves an exit status, a signal,
/// or a deadline that produced output or signalled the process group.
pub(super) fn command_ran(evidence: &Value) -> bool {
    let status = ["exit_code", "signal"]
        .iter()
        .any(|key| evidence.get(*key).is_some_and(|value| !value.is_null()));
    if status {
        return true;
    }
    let captured = |key: &str| {
        evidence
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty())
    };
    evidence.get("interrupted").and_then(Value::as_bool) == Some(true)
        && (evidence.get("group_signalled").and_then(Value::as_bool) == Some(true)
            || captured("stdout")
            || captured("stderr"))
}

/// Resolve the program a trial will run in its actual child working directory.
fn resolve_program(program: &str, cwd: &Path) -> Option<String> {
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        let path = Path::new(program);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        return if path.is_file() {
            Some(path.to_string_lossy().into_owned())
        } else {
            None
        };
    }
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(program);
        let candidate = if candidate.is_absolute() {
            candidate
        } else {
            cwd.join(candidate)
        };
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// The environment a trial is bound to. It changes when the interpreter, the
/// operating system or the architecture changes, so a previous result cannot
/// be inherited silently.
pub(super) fn probe_environment(program: &str, cwd: &Path) -> Result<TrialEnvironment> {
    let resolved = resolve_program(program, cwd);
    let mut probe = ProgramProbe {
        program: program.to_string(),
        resolved: resolved.clone(),
        size: None,
        modified_unix: None,
    };
    if let Some(path) = &resolved {
        if let Ok(metadata) = std::fs::metadata(path) {
            probe.size = Some(metadata.len());
            probe.modified_unix = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|delta| delta.as_secs());
        }
    }
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|value| value.trim().to_string());
    let digest = Digest::blake3(&serde_json::to_vec(&json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "kernel": kernel,
        "programs": [&probe],
    }))?);
    Ok(TrialEnvironment {
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        kernel,
        programs: vec![probe],
        digest,
    })
}

/// Read at most `cap` bytes. The third value reports a failed read, which
/// makes the captured output untrustworthy.
fn drain<R: Read>(mut reader: R, cap: usize) -> (Vec<u8>, bool, bool) {
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    let mut failed = false;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                let room = cap.saturating_sub(kept.len());
                if room > 0 {
                    let take = room.min(count);
                    kept.extend_from_slice(&chunk[..take]);
                    if take < count {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    (kept, truncated, failed)
}

/// How long a trial waits for its captured output before it stops trusting it.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolve the effective child working directory before probing or spawning.
/// Persisting this canonical path makes later environment rechecks independent
/// of the Node process's subsequent working directory.
fn trial_workdir(cwd: Option<&str>, unit: &InstalledUnit) -> Result<PathBuf> {
    let path = match cwd {
        Some(value) => PathBuf::from(value),
        None => unit
            .materialization
            .as_ref()
            .map(|materialization| PathBuf::from(&materialization.path))
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| {
                path.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| path.clone())
            })
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let path = std::fs::canonicalize(path)?;
    ensure!(path.is_dir(), "trial working directory is not a directory");
    Ok(path)
}

/// Recover the effective directory retained with an older default-cwd trial.
/// A relative historical value is not guessed against a later Node process.
pub(super) fn evidence_workdir(evidence: &TrialEvidence) -> Option<PathBuf> {
    let path = match &evidence.cwd {
        Some(cwd) => PathBuf::from(cwd),
        None => evidence
            .artifact_path
            .as_ref()
            .and_then(|artifact| Path::new(artifact).parent().map(Path::to_path_buf))?,
    };
    path.is_absolute().then_some(path)
}

/// Expand the `{artifact}` placeholder to the materialized package path.
fn expand_artifact(argv: &[String], artifact: Option<&str>) -> Result<Vec<String>> {
    let mut expanded = Vec::with_capacity(argv.len());
    for argument in argv {
        if argument.contains("{artifact}") {
            let path = artifact.context(
                "the unit is not materialized in a package root, so {artifact} cannot be expanded",
            )?;
            expanded.push(argument.replace("{artifact}", path));
        } else {
            expanded.push(argument.clone());
        }
    }
    Ok(expanded)
}

/// Whether a command references the materialized artifact of a unit.
fn references_artifact(argv: &[String], artifact: &str) -> bool {
    argv.iter()
        .any(|argument| argument.split_whitespace().any(|token| token == artifact))
}

/// Stop a whole process group. A descendant that left the group may survive.
fn signal_process_group(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) == 0 }
}

fn process_group_gone(pid: u32) -> bool {
    if pid == 0 {
        return true;
    }
    let rc = unsafe { libc::kill(-(pid as libc::pid_t), 0) };
    if rc == 0 {
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
}

struct OwnerIncarnation<'a> {
    pid: i64,
    start_ticks: u64,
    boot_id: Option<&'a str>,
}

fn parse_owner_incarnation(runtime: &str) -> Option<OwnerIncarnation<'_>> {
    // New identities append a boot id. Schema-1 identities are still parsed
    // by pid/start ticks so a live pre-upgrade command is not retired merely
    // for having the older representation.
    let parts: Vec<&str> = runtime.rsplitn(4, ':').collect();
    if parts.len() == 4 {
        Some(OwnerIncarnation {
            pid: parts[2].parse().ok()?,
            start_ticks: parts[1].parse().ok()?,
            boot_id: Some(parts[0]),
        })
    } else if parts.len() == 3 {
        Some(OwnerIncarnation {
            pid: parts[1].parse().ok()?,
            start_ticks: parts[0].parse().ok()?,
            boot_id: None,
        })
    } else {
        None
    }
}

/// Whether the persisted owner incarnation, rather than merely its numeric
/// pid, is provably gone. EPERM or an unavailable /proc record leaves a live
/// candidate untouched because it cannot prove crash recovery is safe.
pub(super) fn process_gone(pid: i64, owner_runtime: &str) -> bool {
    if pid <= 0 {
        return true;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
        return true;
    }
    let Some(expected) = parse_owner_incarnation(owner_runtime) else {
        // A malformed owner coordinate cannot keep an attempt pending forever.
        return true;
    };
    if expected.pid != pid {
        return true;
    }
    if let Some(expected_boot) = expected.boot_id.filter(|boot| *boot != "unknown") {
        let Some(actual_boot) = boot_id() else {
            return false;
        };
        if actual_boot != expected_boot {
            return true;
        }
    }
    if expected.start_ticks == 0 {
        return false;
    }
    match process_start_ticks_for(pid) {
        Some(actual) => actual != expected.start_ticks,
        None => false,
    }
}

/// Identity of the running command. It stays the same for the lifetime of the
/// process, so a reused process id cannot be mistaken for the same command.
pub(super) fn runtime_id(conn: &rusqlite::Connection) -> Result<String> {
    runtime_id_for_process(conn, i64::from(std::process::id()))
}

fn runtime_id_for_process(conn: &rusqlite::Connection, pid: i64) -> Result<String> {
    let installation: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k='installation'", [], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(format!(
        "{}:{}:{}:{}",
        installation.unwrap_or_else(|| "uninitialized".to_string()),
        pid,
        process_start_ticks_for(pid).unwrap_or(0),
        boot_id().unwrap_or_else(|| "unknown".to_string())
    ))
}

/// The start time of this process in clock ticks since boot.
fn process_start_ticks_for(pid: i64) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat.rsplit_once(") ")?.1;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    fields.get(19)?.parse().ok()
}

fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|value| value.trim().to_string())
}

/// A minimal environment record for evidence whose original was lost.
fn fallback_environment(digest: Digest) -> TrialEnvironment {
    TrialEnvironment {
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        kernel: None,
        programs: vec![],
        digest,
    }
}

impl Node {
    // ----------------------------------------------------------- trial -----

    /// Run a bounded, caller-selected command as the real trial of a unit.
    ///
    /// The command is executed directly, without a shell. There is no
    /// allowlist and no approval step: the caller states the risk, the
    /// expected benefit and the consequence, and the run is recorded with its
    /// real exit status and output.
    ///
    /// A run that references the materialized artifact of this unit, either by
    /// passing its path or by using the `{artifact}` placeholder, is a
    /// readiness proof: the artifact digest is verified before and after the
    /// run. Any other command is an investigation of the environment. It is
    /// allowed, it is recorded, and it never promotes readiness.
    pub(super) fn unit_trial(
        &mut self,
        unit_id: &str,
        argv: &[String],
        cwd: Option<&str>,
        timeout_ms: Option<u64>,
        risk: &str,
        benefit: &str,
        consequence: &str,
    ) -> Result<Value> {
        ensure!(
            !argv.is_empty() && argv.len() <= MAX_ARGV,
            "trial argv must hold 1..=64 entries"
        );
        for argument in argv {
            ensure!(
                argument.len() <= MAX_ARG_BYTES,
                "trial argument exceeds the size bound"
            );
            ensure!(!argument.contains('\0'), "trial argument contains NUL");
        }
        ensure!(
            risk.trim().len() >= 4 && risk.len() <= MAX_TEXT_BYTES,
            "the trial must state its risk"
        );
        ensure!(
            benefit.trim().len() >= 4 && benefit.len() <= MAX_TEXT_BYTES,
            "the trial must state its expected benefit"
        );
        ensure!(
            consequence.trim().len() >= 4 && consequence.len() <= MAX_TEXT_BYTES,
            "the trial must state its consequence"
        );
        let timeout = timeout_ms
            .unwrap_or(DEFAULT_TRIAL_TIMEOUT_MS)
            .min(MAX_TRIAL_TIMEOUT_MS);
        ensure!(timeout > 0, "trial timeout must be positive");
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        let artifact = unit
            .materialization
            .as_ref()
            .filter(|m| m.state == "materialized")
            .map(|m| m.path.clone())
            .filter(|path| !path.is_empty());
        let expanded = expand_artifact(argv, artifact.as_deref())?;
        let artifact_bound = match &artifact {
            Some(path) => references_artifact(&expanded, path),
            None => false,
        };
        let workdir = trial_workdir(cwd, &unit)?;
        let workdir_text = workdir.to_string_lossy().into_owned();
        let environment = probe_environment(&expanded[0], &workdir)?;
        let attempt_id = EntityId::new().to_string();
        let started_at = now();
        let owner_pid = i64::from(std::process::id());
        let owner_runtime = runtime_id(&self.conn)?;
        let attempt_data = json!({
            "argv": expanded,
            "requested_argv": argv,
            "cwd": workdir_text,
            "environment": environment,
            "risk": risk,
            "benefit": benefit,
            "consequence": consequence,
            "artifact_bound": artifact_bound,
            "artifact_path": artifact,
            "timeout_ms": timeout,
        });
        // The attempt is durable before any child can exist.
        {
            let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
            tx.execute(
                "INSERT INTO unit_attempts(attempt_id,unit_id,revision,artifact_digest,environment_digest,state,started_at,finished_at,owner_pid,owner_runtime,data) \
                 VALUES(?1,?2,?3,?4,?5,'prepared',?6,NULL,?7,?8,?9)",
                params![
                    attempt_id,
                    unit.unit_id,
                    unit.revision,
                    unit.artifact_digest.0,
                    environment.digest.0,
                    started_at,
                    owner_pid,
                    owner_runtime,
                    serde_json::to_string(&attempt_data)?
                ],
            )?;
            tx.commit()?;
        }
        // From here a child may exist, so the attempt is already unresolved.
        {
            let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
            tx.execute(
                "UPDATE unit_attempts SET state='spawning' WHERE attempt_id=?1",
                params![attempt_id],
            )?;
            tx.commit()?;
        }
        // A readiness proof only counts when the materialized artifact really
        // is the installed artifact.
        let artifact_verified_before = match (&artifact, artifact_bound) {
            (Some(path), true) => verify_artifact(path, &unit.artifact_digest),
            _ => false,
        };
        let mut command = Command::new(&expanded[0]);
        command
            .args(&expanded[1..])
            .current_dir(&workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The child leads its own process group, so the deadline can stop the
        // whole tree instead of only the direct child.
        command.process_group(0);
        let spawn = command.spawn();
        let mut child = match spawn {
            Ok(child) => child,
            Err(e) => {
                let evidence = TrialEvidence {
                    attempt_id: attempt_id.clone(),
                    started_at: started_at.clone(),
                    finished_at: Some(now()),
                    outcome: TrialOutcome::Failed,
                    environment_digest: environment.digest.clone(),
                    environment: environment.clone(),
                    argv: expanded.clone(),
                    requested_argv: argv.to_vec(),
                    cwd: Some(workdir_text.clone()),
                    risk: risk.to_string(),
                    benefit: benefit.to_string(),
                    consequence: consequence.to_string(),
                    exit_code: None,
                    signal: None,
                    stdout: String::new(),
                    stderr: format!("{e}"),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    interrupted: false,
                    artifact_bound,
                    artifact_path: artifact.clone(),
                    artifact_verified_before,
                    artifact_verified_after: false,
                    reader_failed: false,
                    group_signalled: false,
                    descendants_unknown: false,
                    owner_pid,
                    owner_runtime: owner_runtime.clone(),
                    reason: "the trial command could not be started".to_string(),
                };
                self.record_trial(&attempt_id, &unit, started_at, &evidence, "failed")?;
                let (readiness, reason) = self.readiness_of(&unit)?;
                return Ok(json!({
                    "attempt_id": attempt_id,
                    "outcome": "failed",
                    "error": format!("{e}"),
                    "artifact_bound": artifact_bound,
                    "readiness_proof": false,
                    "readiness": readiness,
                    "reason": reason,
                }));
            }
        };
        {
            let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
            tx.execute(
                "UPDATE unit_attempts SET state='running' WHERE attempt_id=?1",
                params![attempt_id],
            )?;
            tx.commit()?;
        }
        let stdout = child.stdout.take().context("trial stdout pipe missing")?;
        let stderr = child.stderr.take().context("trial stderr pipe missing")?;
        let (out_tx, out_rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();
        let _out_reader = std::thread::spawn(move || {
            let result = drain(stdout, MAX_OUTPUT_BYTES);
            let _ = out_tx.send(result);
        });
        let _err_reader = std::thread::spawn(move || {
            let result = drain(stderr, MAX_OUTPUT_BYTES);
            let _ = err_tx.send(result);
        });
        let deadline = Instant::now() + Duration::from_millis(timeout);
        let mut timed_out = false;
        let mut group_signalled = false;
        let mut wait_error: Option<String> = None;
        let mut status = None;
        let mut out_result = None;
        let mut err_result = None;
        loop {
            if status.is_none() {
                match child.try_wait() {
                    Ok(Some(done)) => status = Some(done),
                    Ok(None) => {}
                    Err(e) => {
                        wait_error = Some(format!("{e}"));
                        group_signalled = signal_process_group(child.id());
                        let _ = child.kill();
                        status = child.wait().ok();
                        break;
                    }
                }
            }
            if out_result.is_none() {
                match out_rx.try_recv() {
                    Ok(result) => out_result = Some(result),
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        out_result = Some((Vec::new(), false, true));
                    }
                }
            }
            if err_result.is_none() {
                match err_rx.try_recv() {
                    Ok(result) => err_result = Some(result),
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        err_result = Some((Vec::new(), false, true));
                    }
                }
            }
            // A departed group leader is not completion while a descendant
            // remains or a reader is still waiting for captured output.
            let group_finished = status.is_some() && process_group_gone(child.id());
            if status.is_some() && group_finished && out_result.is_some() && err_result.is_some() {
                break;
            }
            if Instant::now() >= deadline {
                timed_out = true;
                group_signalled = signal_process_group(child.id());
                if status.is_none() {
                    let _ = child.kill();
                    status = child.wait().ok();
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // After a group stop, reader teardown remains bounded. An escaped
        // descendant can still hold a pipe, but its result is unknown rather
        // than an unbounded wait or a successful proof.
        if out_result.is_none() {
            out_result = Some(out_rx.recv_timeout(READ_TIMEOUT).unwrap_or((
                Vec::new(),
                false,
                true,
            )));
        }
        if err_result.is_none() {
            err_result = Some(err_rx.recv_timeout(READ_TIMEOUT).unwrap_or((
                Vec::new(),
                false,
                true,
            )));
        }
        let (out_bytes, out_truncated, out_failed) =
            out_result.unwrap_or((Vec::new(), false, true));
        let (err_bytes, err_truncated, err_failed) =
            err_result.unwrap_or((Vec::new(), false, true));
        let reader_failed = out_failed || err_failed;
        let descendants_unknown = timed_out || group_signalled || reader_failed;
        let artifact_verified_after = match (&artifact, artifact_bound) {
            (Some(path), true) => verify_artifact(path, &unit.artifact_digest),
            _ => false,
        };
        let (outcome, exit_code, signal) = match &status {
            Some(status) if timed_out => (TrialOutcome::TimedOut, status.code(), status.signal()),
            Some(status) if status.code() == Some(0) && reader_failed => {
                (TrialOutcome::Unknown, Some(0), None)
            }
            Some(status) if status.code() == Some(0) => (TrialOutcome::Succeeded, Some(0), None),
            Some(status) => match status.code() {
                Some(code) => (TrialOutcome::Failed, Some(code), None),
                None => (TrialOutcome::Signaled, None, status.signal()),
            },
            None => (TrialOutcome::Failed, None, None),
        };
        let stdout_text = String::from_utf8_lossy(&out_bytes).into_owned();
        let mut stderr_text = String::from_utf8_lossy(&err_bytes).into_owned();
        if let Some(error) = &wait_error {
            stderr_text.push_str(&format!("\nwait error: {error}"));
        }
        let reason = match outcome {
            TrialOutcome::Succeeded => "the trial command ran and reported success",
            TrialOutcome::Failed => "the trial command ran and reported failure",
            TrialOutcome::Signaled => "the trial command was terminated by a signal",
            TrialOutcome::TimedOut => {
                "the trial exceeded its bounded deadline while waiting for the command, descendants, or captured output"
            }
            TrialOutcome::Unknown => {
                "the trial result could not be read completely, so its outcome is unknown"
            }
        };
        let evidence = TrialEvidence {
            attempt_id: attempt_id.clone(),
            started_at: started_at.clone(),
            finished_at: Some(now()),
            outcome,
            environment_digest: environment.digest.clone(),
            environment: environment.clone(),
            argv: expanded.clone(),
            requested_argv: argv.to_vec(),
            cwd: Some(workdir_text.clone()),
            risk: risk.to_string(),
            benefit: benefit.to_string(),
            consequence: consequence.to_string(),
            exit_code,
            signal,
            stdout: stdout_text.clone(),
            stderr: stderr_text.clone(),
            stdout_truncated: out_truncated,
            stderr_truncated: err_truncated,
            interrupted: timed_out,
            artifact_bound,
            artifact_path: artifact.clone(),
            artifact_verified_before,
            artifact_verified_after,
            reader_failed,
            group_signalled,
            descendants_unknown,
            owner_pid,
            owner_runtime: owner_runtime.clone(),
            reason: reason.to_string(),
        };
        let readiness_proof = artifact_bound
            && artifact_verified_before
            && artifact_verified_after
            && outcome == TrialOutcome::Succeeded;
        self.record_trial(&attempt_id, &unit, started_at, &evidence, outcome.as_str())?;
        let (readiness, readiness_reason) = self.readiness_of(&unit)?;
        Ok(json!({
            "attempt_id": attempt_id,
            "outcome": outcome.as_str(),
            "exit_code": exit_code,
            "signal": signal,
            "argv": expanded,
            "cwd": workdir_text,
            "artifact_bound": artifact_bound,
            "artifact_path": artifact,
            "artifact_verified_before": artifact_verified_before,
            "artifact_verified_after": artifact_verified_after,
            "readiness_proof": readiness_proof,
            "reader_failed": reader_failed,
            "group_signalled": group_signalled,
            "descendants_unknown": descendants_unknown,
            "stdout": stdout_text,
            "stderr": stderr_text,
            "stdout_truncated": out_truncated,
            "stderr_truncated": err_truncated,
            "interrupted": timed_out,
            "environment": environment,
            "readiness": readiness,
            "reason": readiness_reason,
        }))
    }

    /// Retire attempts whose owning process is provably gone.
    ///
    /// An attempt that another live command still owns is left untouched: a
    /// concurrent command on the same installation is not a crash, and its
    /// attempt must not be relabelled as one.
    pub fn retire_unknown_trials(&mut self) -> Result<u32> {
        let my_runtime = runtime_id(&self.conn)?;
        let my_pid = i64::from(std::process::id());
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
        let pending: Vec<(String, String, String, String, String, String, String, i64, String)> = tx
            .prepare(
                "SELECT attempt_id,unit_id,revision,artifact_digest,environment_digest,started_at,data,owner_pid,owner_runtime \
                 FROM unit_attempts WHERE state IN ('prepared','spawning','running') ORDER BY started_at",
            )?
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if pending.is_empty() {
            tx.commit()?;
            return Ok(0);
        }
        let finished = now();
        let mut retired = 0u32;
        for (
            attempt_id,
            unit_id,
            revision,
            digest,
            environment,
            started_at,
            data,
            owner_pid,
            owner_runtime,
        ) in pending
        {
            let mine = owner_runtime == my_runtime && owner_pid == my_pid;
            if !mine && !process_gone(owner_pid, &owner_runtime) {
                // Another command may still be running this attempt.
                continue;
            }
            let stored: Value = serde_json::from_str(&data).unwrap_or_else(|_| json!({}));
            let environment_digest = Digest(environment.clone());
            let evidence = TrialEvidence {
                attempt_id: attempt_id.clone(),
                started_at: started_at.clone(),
                finished_at: Some(finished.clone()),
                outcome: TrialOutcome::Unknown,
                environment_digest: environment_digest.clone(),
                environment: stored
                    .get("environment")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                    .unwrap_or_else(|| fallback_environment(environment_digest)),
                argv: string_list(stored.get("argv")),
                requested_argv: string_list(stored.get("requested_argv")),
                cwd: stored
                    .get("cwd")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string()),
                risk: stored
                    .get("risk")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                benefit: stored
                    .get("benefit")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                consequence: stored
                    .get("consequence")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                exit_code: None,
                signal: None,
                stdout: String::new(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                interrupted: true,
                artifact_bound: stored
                    .get("artifact_bound")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
                artifact_path: stored
                    .get("artifact_path")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string()),
                artifact_verified_before: false,
                artifact_verified_after: false,
                reader_failed: false,
                group_signalled: false,
                descendants_unknown: true,
                owner_pid,
                owner_runtime: owner_runtime.clone(),
                reason: "the owning command ended before this attempt was resolved; a child may have run, so the outcome is unknown and is never replayed automatically"
                    .to_string(),
            };
            tx.execute(
                "INSERT INTO unit_trials(attempt_id,unit_id,revision,artifact_digest,environment_digest,started_at,finished_at,outcome,artifact_bound,artifact_verified,owner_pid,owner_runtime,data) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,'unknown',?8,0,?9,?10,?11)",
                params![
                    attempt_id,
                    unit_id,
                    revision,
                    digest,
                    environment,
                    started_at,
                    finished,
                    i64::from(evidence.artifact_bound),
                    owner_pid,
                    owner_runtime,
                    serde_json::to_string(&evidence)?
                ],
            )?;
            tx.execute(
                "UPDATE unit_attempts SET state='unknown',finished_at=?2 WHERE attempt_id=?1",
                params![attempt_id, finished],
            )?;
            retired += 1;
        }
        tx.commit()?;
        Ok(retired)
    }

    fn record_trial(
        &mut self,
        attempt_id: &str,
        unit: &InstalledUnit,
        started_at: String,
        evidence: &TrialEvidence,
        state: &str,
    ) -> Result<()> {
        let finished = now();
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
        tx.execute(
            "INSERT INTO unit_trials(attempt_id,unit_id,revision,artifact_digest,environment_digest,started_at,finished_at,outcome,artifact_bound,artifact_verified,owner_pid,owner_runtime,data) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                attempt_id,
                unit.unit_id,
                unit.revision,
                unit.artifact_digest.0,
                evidence.environment_digest.0,
                started_at,
                finished,
                state,
                i64::from(evidence.artifact_bound),
                i64::from(
                    evidence.artifact_verified_before && evidence.artifact_verified_after
                ),
                evidence.owner_pid,
                evidence.owner_runtime,
                serde_json::to_string(evidence)?
            ],
        )?;
        tx.execute(
            "UPDATE unit_attempts SET state=?2,finished_at=?3 WHERE attempt_id=?1",
            params![attempt_id, state, finished],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::Node;
    use crate::units::UnitRequest;
    use crate::units::installation::now;
    use crate::units::test_support::{export, package, scratch, trial};
    use rusqlite::params;
    use serde_json::{Value, json};
    use shenko_types::ids::EntityId;
    use shenko_types::object::Digest;
    use std::path::Path;
    use super::{probe_environment, runtime_id, runtime_id_for_process};

    #[test]
    fn trial_records_real_success_and_real_failure() {
        let dir = scratch("trial-node");
        let root = scratch("trial-packages");
        let mut node = Node::open(&dir).unwrap();
        node.handle_unit_request(UnitRequest::Import {
            export: export(vec![package(
                "demo.skill.trial",
                "1.0.0",
                b"trial-bytes",
                vec![],
            )]),
            package_root: root.to_string_lossy().into_owned(),
            enable: None,
        })
        .unwrap();
        let good = node
            .handle_unit_request(trial(
                "demo.skill.trial",
                vec!["/bin/sh", "-c", "cat {artifact}"],
            ))
            .unwrap();
        assert_eq!(good["outcome"], json!("succeeded"));
        assert_eq!(good["readiness_proof"], json!(true));
        assert_eq!(good["readiness"], json!("ready"));

        let bad = node
            .handle_unit_request(trial(
                "demo.skill.trial",
                vec!["/bin/sh", "-c", "exit 3; cat {artifact}"],
            ))
            .unwrap();
        assert_eq!(bad["outcome"], json!("failed"));
        assert_eq!(bad["exit_code"], json!(3));
        assert_eq!(bad["readiness"], json!("failed"));

        let missing = node
            .handle_unit_request(trial(
                "demo.skill.trial",
                vec!["/nonexistent/shenko-units-probe"],
            ))
            .unwrap();
        assert_eq!(missing["outcome"], json!("failed"));
        assert_eq!(missing["artifact_bound"], json!(false));
        // An unbound failure does not change the readiness of the artifact.
        assert_eq!(missing["readiness"], json!("failed"));

        let evidence = node
            .handle_unit_request(UnitRequest::Evidence {
                unit_id: Some("demo.skill.trial".into()),
                after: "0".into(),
                limit: 32,
            })
            .unwrap();
        let rows = evidence["evidence"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["outcome"], json!("succeeded"));
        assert_eq!(rows[1]["outcome"], json!("failed"));
        assert_eq!(rows[2]["outcome"], json!("failed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unresolved_attempt_becomes_unknown_and_is_never_replayed() {
        let dir = scratch("crash-node");
        let root = scratch("crash-packages");
        let mut node = Node::open(&dir).unwrap();
        node.handle_unit_request(UnitRequest::Import {
            export: export(vec![package(
                "demo.skill.crash",
                "1.0.0",
                b"crash-bytes",
                vec![],
            )]),
            package_root: root.to_string_lossy().into_owned(),
            enable: None,
        })
        .unwrap();
        let environment = probe_environment("/bin/sh", Path::new("/")).unwrap();
        let digest = Digest::blake3(b"crash-bytes");
        let artifact_path = node
            .installed_unit("demo.skill.crash")
            .unwrap()
            .unwrap()
            .materialization
            .unwrap()
            .path;
        let runtime = runtime_id(&node.conn).unwrap();
        let foreign_runtime = runtime_id_for_process(&node.conn, 1).unwrap();
        let mine = EntityId::new().to_string();
        let foreign = EntityId::new().to_string();
        let data = serde_json::to_string(&json!({
            "argv": ["/bin/sh", "-c", "cat {artifact}"],
            "requested_argv": ["/bin/sh", "-c", "cat {artifact}"],
            "cwd": Value::Null,
            "environment": environment,
            "risk": "runs a trivial interpreter command",
            "benefit": "proves the unit can run in this environment",
            "consequence": "a failed command leaves the unit unready here",
            "artifact_bound": true,
            "artifact_path": artifact_path,
        }))
        .unwrap();
        // An attempt of this command, whose child may have run.
        node.conn
            .execute(
                "INSERT INTO unit_attempts(attempt_id,unit_id,revision,artifact_digest,environment_digest,state,started_at,finished_at,owner_pid,owner_runtime,data) \
                 VALUES(?1,'demo.skill.crash','1.0.0',?2,?3,'spawning',?4,NULL,?5,?6,?7)",
                params![
                    mine,
                    digest.0,
                    environment.digest.0,
                    now(),
                    i64::from(std::process::id()),
                    runtime,
                    data
                ],
            )
            .unwrap();
        // An attempt of another command that is still alive.
        node.conn
            .execute(
                "INSERT INTO unit_attempts(attempt_id,unit_id,revision,artifact_digest,environment_digest,state,started_at,finished_at,owner_pid,owner_runtime,data) \
                 VALUES(?1,'demo.skill.crash','1.0.0',?2,?3,'spawning',?4,NULL,1,?5,?6)",
                params![
                    foreign,
                    digest.0,
                    environment.digest.0,
                    now(),
                    foreign_runtime,
                    data
                ],
            )
            .unwrap();
        // The next request retires only the attempt whose owner is gone.
        let status = node
            .handle_unit_request(UnitRequest::Status {
                unit_id: "demo.skill.crash".into(),
            })
            .unwrap();
        assert_eq!(status["readiness"]["state"], json!("unknown"));
        assert_eq!(status["local_trials"], json!(1));
        let evidence = node
            .handle_unit_request(UnitRequest::Evidence {
                unit_id: Some("demo.skill.crash".into()),
                after: "0".into(),
                limit: 32,
            })
            .unwrap();
        let rows = evidence["evidence"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["outcome"], json!("unknown"));
        assert_eq!(rows[0]["attempt_id"], json!(mine));
        // Nothing was replayed, and the live attempt of the other command was
        // not relabelled as a crash.
        let attempts: i64 = node
            .conn
            .query_row("SELECT count(*) FROM unit_attempts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(attempts, 2);
        let pending: i64 = node
            .conn
            .query_row(
                "SELECT count(*) FROM unit_attempts WHERE state='spawning'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1);
        // The facts are durable in the node's own store.
        let store = rusqlite::Connection::open(dir.join("node.sqlite3")).unwrap();
        let unknown: i64 = store
            .query_row(
                "SELECT count(*) FROM unit_trials WHERE outcome='unknown'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unknown, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
