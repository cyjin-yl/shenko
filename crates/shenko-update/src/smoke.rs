//! Candidate smoke launch — SPEC §9.5 (smoke), §35.7.
//!
//! The smoke step starts the candidate program against its own state copy
//! and checks that it can actually read that state. It never sends a real
//! message, never drives a real seat, never replays an outbox and never
//! starts a second unbounded subject loop: the candidate runs a bounded
//! readiness command that only reads.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use shenko_types::ids::RuntimeGeneration;
use shenko_types::object::Digest;

/// The schema version of the smoke report.
pub const SMOKE_SCHEMA_VERSION: u32 = 1;

/// How much of the candidate's output is kept.
const OUTPUT_TAIL_BYTES: usize = 8 * 1024;

/// How much of one stream is captured before the rest is drained and dropped.
const STREAM_BYTES: usize = 1024 * 1024;

/// How long a candidate has to answer the bounded readiness command.
///
/// The smoke runs while the working runtime is already quiesced (a rollback
/// relaunch of the retained executable, or a prepared candidate before any
/// hand-over), so a candidate that never answers would hold the installation
/// paused forever. The command is stopped and the smoke fails instead; a
/// timeout is never a pass.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the stopped candidate gets to be reaped and its pipes to close.
/// A descendant that escaped the process group cannot hold this open.
const RETIRE_JOIN: Duration = Duration::from_secs(10);

/// One smoke check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmokeCheckOutcome {
    pub name: String,
    pub passed: bool,
    pub detail: Option<String>,
}

/// The report the candidate produces for its own state copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmokeReport {
    pub schema_version: u32,
    pub generation: String,
    pub checks: Vec<SmokeCheckOutcome>,
    pub passed: bool,
    pub exit_code: i32,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub launched_at: String,
}

impl SmokeReport {
    pub fn failures(&self) -> impl Iterator<Item = &SmokeCheckOutcome> {
        self.checks.iter().filter(|check| !check.passed)
    }
}

/// Errors of the smoke launch.
#[derive(Debug, thiserror::Error)]
pub enum SmokeError {
    /// The slot the smoke was asked to qualify does not describe the program
    /// that would be launched: a missing or malformed descriptor, another
    /// generation, another program, or bytes that differ from the recorded
    /// digest.
    #[error("the smoke slot descriptor does not name this program: {0}")]
    Manifest(String),
    #[error("the candidate program could not be started: {0}")]
    Launch(#[source] std::io::Error),
    /// The candidate was still running when the bounded smoke expired. It is
    /// stopped; its pass/fail is unknown, which is not success.
    #[error("{0}")]
    Timeout(SmokeTimeout),
    #[error("the candidate program reported a malformed smoke report: {0}")]
    Malformed(String),
    /// The candidate's own outcome was not a pass: the report says so, the
    /// program exited non-zero, or no readable report was printed at all.
    /// The detail names the exit code, the failed checks and the captured
    /// stderr tail; a missing report and a failed report are never called
    /// a pass.
    #[error("the candidate's smoke report did not pass: {0}")]
    Failed(String),
}

/// The bounded smoke expired, and what stopping the candidate also failed to
/// do.
///
/// A cleanup failure never replaces the reason the smoke failed: the timeout is
/// what the installation acts on, and the note says the tree it started may
/// still have members that the next hand-over has to deal with.
#[derive(Debug)]
pub struct SmokeTimeout {
    /// How long the candidate was given before it was stopped.
    pub seconds: u64,
    /// What stopping the owned tree also failed to do, when it did fail.
    pub cleanup: Option<String>,
}

impl std::fmt::Display for SmokeTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the candidate program did not answer the bounded smoke within {}s and was stopped; no report is sealed",
            self.seconds
        )?;
        if let Some(cleanup) = &self.cleanup {
            write!(formatter, "; stopping it also failed: {cleanup}")?;
        }
        Ok(())
    }
}

/// Start the candidate against its own state copy and read its report.
///
/// The candidate is asked for a bounded, read-only readiness check. A
/// candidate that does not understand the command, exits non-zero or reports
/// a failed check is an error naming the failed outcome: `Ok` is returned
/// only when the report actually passed, so a failed or absent report can
/// never be sealed as activation evidence. The launch itself is bounded: a
/// candidate that does not answer in time is stopped rather than left holding
/// a quiesced installation.
pub async fn launch_smoke(
    artifact: &Path,
    slot_dir: &Path,
    generation: &RuntimeGeneration,
) -> Result<SmokeReport, SmokeError> {
    verify_slot_manifest(artifact, slot_dir, generation)?;
    let (exit_code, stdout, stderr) = collect_bounded(artifact, slot_dir).await?;

    let parsed = extract_json(&stdout)
        .and_then(|json| serde_json::from_str::<SmokeReport>(&json).ok());
    let mut report = match parsed {
        Some(report) => report,
        None => SmokeReport {
            schema_version: SMOKE_SCHEMA_VERSION,
            generation: generation.0.clone(),
            checks: vec![SmokeCheckOutcome {
                name: "candidate_smoke_report".into(),
                passed: false,
                detail: Some(
                    "the candidate did not print a readable smoke report".to_string(),
                ),
            }],
            passed: false,
            exit_code,
            stdout_tail: tail(&stdout),
            stderr_tail: tail(&stderr),
            launched_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        },
    };
    report.exit_code = exit_code;
    if report.stdout_tail.is_empty() {
        report.stdout_tail = tail(&stdout);
    }
    if report.stderr_tail.is_empty() {
        report.stderr_tail = tail(&stderr);
    }
    if exit_code != 0 {
        report.passed = false;
    }
    // The report qualifies one generation. A program that answered for
    // another slot's generation did not read the state copy this smoke was
    // launched against, so its pass says nothing about this one.
    if report.generation != generation.0 {
        return Err(SmokeError::Failed(format!(
            "the report of {} names generation {}, not {}; it did not qualify this state copy",
            artifact.display(),
            report.generation,
            generation.0
        )));
    }
    if !report.passed {
        return Err(SmokeError::Failed(describe_failure(&report)));
    }
    Ok(report)
}

/// Run the candidate's bounded readiness command and collect what it printed.
///
/// The launch owns a tree, not one process. On Unix the child leads its own
/// process group, so a candidate that forks and leaves descendants holding the
/// pipes is stopped as a tree instead of leaving children running against a
/// state copy this hand-over is qualifying. On Windows the child is created
/// suspended, assigned to a job object and only then resumed, so it belongs to
/// the tree before it can create its first descendant. Both streams are read
/// while the child runs (bounded, drained past the cap so it can never block on
/// a full pipe), the wait is bounded, and the timeout path stops the tree,
/// reaps the leader and settles the readers with their own deadline before the
/// smoke fails. A stream that is still open at that point is reported as held
/// by something outside the tree, never waited on indefinitely.
async fn collect_bounded(
    artifact: &Path,
    slot_dir: &Path,
) -> Result<(i32, String, String), SmokeError> {
    let mut command = Command::new(artifact);
    command
        .arg("--data-dir")
        .arg(slot_dir)
        .arg("update")
        .arg("smoke")
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A child that outlives the deadline must not keep running against a
        // state copy this hand-over is about to qualify.
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    // On Windows the candidate is created suspended and started by `adopt`
    // once it is inside the job, so it cannot fork an escaped descendant.
    #[cfg(windows)]
    {
        win32::create_suspended(&mut command);
    }
    // The owner exists before the candidate does, so no candidate process can
    // be created outside the tree.
    let mut tree = OwnedTree::before_spawn()?;
    let mut child = command.spawn().map_err(SmokeError::Launch)?;
    // `child` is declared after `tree`: if this future is dropped instead of
    // finishing, the leader goes first and the tree second. A cancelled future
    // cannot join anything, so that path signals the leader through
    // `kill_on_drop` and, on Windows, leaves the rest to the job's last handle
    // closing, which terminates whatever else the candidate started.
    tree.adopt(&child)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SmokeError::Launch(std::io::Error::other("the candidate's stdout was not piped")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| SmokeError::Launch(std::io::Error::other("the candidate's stderr was not piped")))?;
    let reader_out = tokio::spawn(read_capped(stdout, STREAM_BYTES));
    let reader_err = tokio::spawn(read_capped(stderr, STREAM_BYTES));
    let status = match tokio::time::timeout(SMOKE_TIMEOUT, child.wait()).await {
        Ok(status) => status.map_err(SmokeError::Launch)?,
        Err(_) => {
            // The candidate never answered. Stop every process it started and
            // reap the leader; what that also failed to do is reported beside
            // the timeout instead of being hidden by it.
            let mut notes = tree.retire(&mut child).await;
            // The stopped tree closed the pipes. A stream the tree cannot
            // reach is the only thing left holding one, so the readers are
            // settled with a deadline of their own and the pipe is named.
            for (name, reader) in [("stdout", reader_out), ("stderr", reader_err)] {
                match settle_stream(name, reader).await {
                    StreamSettlement::Settled(_) => {}
                    StreamSettlement::Failed(detail) => notes.add(detail),
                    StreamSettlement::Pending(reader) => {
                        reader.abort();
                        notes.add(format!(
                            "the candidate's {name} reader was still holding its pipe {}-second after the tree was stopped; something outside the owned tree kept it open",
                            RETIRE_JOIN.as_secs()
                        ));
                    }
                }
            }
            return Err(SmokeError::Timeout(SmokeTimeout {
                seconds: SMOKE_TIMEOUT.as_secs(),
                cleanup: notes.describe(),
            }));
        }
    };
    // The leader exited, so `status` is the candidate's own. A descendant it
    // forked can still hold a copy of the pipes; the readers are settled with
    // the retirement deadline and the tree is signalled only when a stream is
    // provably still open.
    let mut notes = CleanupNotes::default();
    let (stdout, stdout_truncated) =
        settle_stream_or_stop("stdout", reader_out, &mut tree, &mut notes).await?;
    let (stderr, stderr_truncated) =
        settle_stream_or_stop("stderr", reader_err, &mut tree, &mut notes).await?;
    if stdout_truncated || stderr_truncated {
        tracing::warn!(
            stdout_truncated,
            stderr_truncated,
            limit = STREAM_BYTES as u64,
            "the candidate printed more than the smoke retains; the report is read from the kept prefix"
        );
    }
    Ok((
        status.code().unwrap_or(-1),
        String::from_utf8_lossy(&stdout).to_string(),
        String::from_utf8_lossy(&stderr).to_string(),
    ))
}

/// Read one candidate stream, keeping at most `limit` bytes.
async fn read_capped(
    mut pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        let count = pipe.read(&mut buffer).await?;
        if count == 0 {
            return Ok((kept, truncated));
        }
        let keep = count.min(limit.saturating_sub(kept.len()));
        kept.extend_from_slice(&buffer[..keep]);
        truncated |= keep != count;
    }
}

/// How one candidate stream ended.
enum StreamSettlement {
    /// The reader finished: the bytes it kept, and whether it hit the cap.
    Settled((Vec<u8>, bool)),
    /// The reader stopped with this detail.
    Failed(String),
    /// The pipe was still held open at the retirement deadline. The reader is
    /// untouched, so the caller decides whether the pipe is worth waiting for
    /// again.
    Pending(tokio::task::JoinHandle<std::io::Result<(Vec<u8>, bool)>>),
}

/// Join one candidate stream within the retirement deadline.
///
/// A reader that has not finished by the deadline is handed back rather than
/// aborted here: only the caller knows whether the tree was already stopped.
async fn settle_stream(
    name: &'static str,
    mut reader: tokio::task::JoinHandle<std::io::Result<(Vec<u8>, bool)>>,
) -> StreamSettlement {
    match tokio::time::timeout(RETIRE_JOIN, &mut reader).await {
        Ok(Ok(Ok(read))) => StreamSettlement::Settled(read),
        Ok(Ok(Err(error))) => {
            StreamSettlement::Failed(format!("the candidate's {name} reader failed: {error}"))
        }
        Ok(Err(error)) => {
            StreamSettlement::Failed(format!("the candidate's {name} reader stopped: {error}"))
        }
        Err(_) => StreamSettlement::Pending(reader),
    }
}

/// Settle one candidate stream, stopping the tree if a descendant is holding
/// the pipe open.
///
/// A leader that exited cannot still be holding its own pipe, so a reader that
/// is pending after the retirement deadline belongs to a process the candidate
/// forked. That process is stopped and the stream is given one more deadline. A
/// stream that still does not settle fails the smoke naming the pipe: a
/// candidate that leaves its output with a process this smoke cannot reach has
/// not finished its bounded readiness command.
async fn settle_stream_or_stop(
    name: &'static str,
    reader: tokio::task::JoinHandle<std::io::Result<(Vec<u8>, bool)>>,
    tree: &mut OwnedTree,
    notes: &mut CleanupNotes,
) -> Result<(Vec<u8>, bool), SmokeError> {
    match settle_stream(name, reader).await {
        StreamSettlement::Settled(read) => Ok(read),
        StreamSettlement::Failed(detail) => Err(SmokeError::Launch(std::io::Error::other(detail))),
        StreamSettlement::Pending(reader) => {
            tree.stop(notes);
            match settle_stream(name, reader).await {
                StreamSettlement::Settled(read) => Ok(read),
                StreamSettlement::Failed(detail) => Err(SmokeError::Launch(
                    std::io::Error::other(format!("{detail}{}", notes.suffix())),
                )),
                StreamSettlement::Pending(reader) => {
                    reader.abort();
                    Err(SmokeError::Launch(std::io::Error::other(format!(
                        "the candidate's {name} was still open {}-second after the smoke's tree was stopped{}",
                        RETIRE_JOIN.as_secs(),
                        notes.suffix()
                    ))))
                }
            }
        }
    }
}

/// What owns every process one smoke launch started.
///
/// Unix: the process group the candidate leads — `process_group(0)` at spawn
/// makes the candidate its own group leader, so one group signal reaches the
/// descendants it forked.
///
/// Windows: a job object whose only limit is terminate-on-last-close. Job
/// membership is inherited, so every process the candidate creates belongs to
/// it, and closing the last handle is what stops the tree — which also covers
/// the paths no code runs on, such as a dropped smoke future.
#[cfg(unix)]
type TreeOwner = Option<u32>;
#[cfg(windows)]
type TreeOwner = Option<win32::Job>;
#[cfg(not(any(unix, windows)))]
type TreeOwner = ();

/// The processes one smoke launch started, and the kernel object that owns
/// them.
struct OwnedTree {
    owner: TreeOwner,
    /// The tree is stopped at most once: the second stop would report an
    /// error for work the first one already did.
    stopped: bool,
}

impl OwnedTree {
    /// Create the owner before the candidate exists, so nothing the candidate
    /// starts can be created outside the tree.
    fn before_spawn() -> Result<Self, SmokeError> {
        Ok(Self { owner: create_owner()?, stopped: false })
    }

    /// Bind the spawned candidate to the owner it was spawned into.
    ///
    /// On Windows this is where the candidate starts running: it was created
    /// suspended, it is assigned to the job there, and only then resumed.
    fn adopt(&mut self, child: &tokio::process::Child) -> Result<(), SmokeError> {
        adopt_owner(&mut self.owner, child)
    }

    /// Stop the tree, recording what could not be stopped.
    ///
    /// Every stream this launch settles asks for the tree to be stopped, so
    /// the first stop is remembered: signalling an already stopped tree would
    /// be reported as a cleanup failure that never happened.
    fn stop(&mut self, notes: &mut CleanupNotes) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        stop_owner(&self.owner, notes);
    }

    /// Stop every process this smoke started, then reap the leader.
    ///
    /// The stop and the reap are both reported: a leader that does not finish
    /// after being signalled leaves a process that still holds the state copy,
    /// and a caller that is already failing must see that rather than a silent
    /// success.
    async fn retire(&mut self, child: &mut tokio::process::Child) -> CleanupNotes {
        let mut notes = CleanupNotes::default();
        self.stop(&mut notes);
        // Stopping the tree reaches the leader too — the group signal on Unix,
        // the job termination on Windows — so a leader that is already gone is
        // the state this asks for, not a failure to signal it.
        if let Err(detail) = signal_leader(child) {
            notes.add(detail);
        }
        match tokio::time::timeout(RETIRE_JOIN, child.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => notes.add(format!("the smoke leader could not be reaped: {error}")),
            Err(_) => notes.add(format!(
                "the smoke leader did not finish within the {}-second retirement deadline",
                RETIRE_JOIN.as_secs()
            )),
        }
        notes
    }
}

/// Signal the leader if stopping the tree did not already end it.
///
/// The tree stop terminates the leader as well, so only a leader that is still
/// running after it, and cannot then be signalled, is a cleanup failure. Its
/// state is read first because a leader the stop already killed is exactly
/// what retirement wanted.
fn signal_leader(child: &mut tokio::process::Child) -> Result<(), String> {
    match child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(error) => {
            return Err(format!("the smoke leader's state could not be read: {error}"));
        }
    }
    match child.start_kill() {
        Ok(()) => Ok(()),
        // The leader exited between the check and the signal, which is again
        // the state retirement asks for.
        Err(error) => match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            _ => Err(format!("the smoke leader could not be signalled: {error}")),
        },
    }
}

/// What stopping the smoke's tree also failed to do.
///
/// A cleanup failure never replaces the reason the smoke failed: it is reported
/// after it, so both the primary outcome and the state of the tree reach the
/// caller.
#[derive(Debug, Default)]
struct CleanupNotes(Vec<String>);

impl CleanupNotes {
    fn add(&mut self, detail: String) {
        self.0.push(detail);
    }

    /// The notes as one fragment, or nothing when stopping did not fail.
    fn describe(&self) -> Option<String> {
        (!self.0.is_empty()).then(|| self.0.join("; "))
    }

    /// The same notes as the tail of a longer sentence.
    fn suffix(&self) -> String {
        self.describe()
            .map(|detail| format!("; stopping the tree also failed: {detail}"))
            .unwrap_or_default()
    }
}

/// The candidate is spawned with `process_group(0)`, so this smoke has no
/// kernel object of its own to create.
#[cfg(unix)]
fn create_owner() -> Result<TreeOwner, SmokeError> {
    Ok(None)
}

/// The candidate leads the group whose id is its own pid.
#[cfg(unix)]
fn adopt_owner(owner: &mut TreeOwner, child: &tokio::process::Child) -> Result<(), SmokeError> {
    *owner = child.id();
    Ok(())
}

/// One group signal reaches the descendants the candidate forked. A group
/// whose leader has been reaped and whose members are gone reports `ESRCH`,
/// which is the state this asks for rather than an error.
///
/// The group's identity is a bare number, and the kernel does not free it
/// while any member is still in the group — which is exactly the case the
/// signal does work in. The two paths that stop a tree differ in what they can
/// rely on: on the timeout path the leader is still unreaped, so its pid and
/// therefore the group id cannot be reused at all; on the path where the
/// leader already exited the group is signalled because a descendant is still
/// holding a pipe, and a candidate that instead left the group while keeping
/// the pipe open is reached only by the stream deadline that follows.
#[cfg(unix)]
fn stop_owner(owner: &TreeOwner, notes: &mut CleanupNotes) {
    let Some(group) = *owner else { return };
    let Some(pid) = rustix::process::Pid::from_raw(group as i32) else {
        notes.add(format!("the smoke's process group {group} is not a valid pid"));
        return;
    };
    match rustix::process::kill_process_group(pid, rustix::process::Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
        Err(error) => notes.add(format!(
            "the candidate's smoke process group {group} could not be signalled: {error}"
        )),
    }
}

/// The job object is created before the candidate, and it terminates its
/// members when its last handle closes.
#[cfg(windows)]
fn create_owner() -> Result<TreeOwner, SmokeError> {
    win32::Job::create().map(Some).map_err(SmokeError::Launch)
}

/// Assign the suspended candidate to the job, then let it run.
#[cfg(windows)]
fn adopt_owner(owner: &mut TreeOwner, child: &tokio::process::Child) -> Result<(), SmokeError> {
    let Some(job) = owner.as_ref() else {
        return Err(SmokeError::Launch(std::io::Error::other(
            "the smoke's job object is missing",
        )));
    };
    job.assign_and_resume(child).map_err(SmokeError::Launch)
}

/// Terminating the job reaches every process the candidate created, including
/// the leader.
#[cfg(windows)]
fn stop_owner(owner: &TreeOwner, notes: &mut CleanupNotes) {
    if let Some(job) = owner {
        job.terminate(notes);
    }
}

/// This platform has neither a process group nor a job object in the current
/// dependency set: only the direct child is stopped, and descendants it forked
/// may survive. The limitation is stated, not hidden.
#[cfg(not(any(unix, windows)))]
fn create_owner() -> Result<TreeOwner, SmokeError> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn adopt_owner(_owner: &mut TreeOwner, _child: &tokio::process::Child) -> Result<(), SmokeError> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn stop_owner(_owner: &TreeOwner, _notes: &mut CleanupNotes) {}

/// Check that the slot being smoked declares exactly the program being run.
///
/// A smoke report is sealed as activation evidence, and for a rollback it is
/// the only qualification of the retained executable. Launching whatever
/// bytes happen to sit at a path would qualify a program the installation
/// never recorded, so the descriptor's generation, program path and digest
/// must all agree with what is about to be executed. A descriptor that is
/// absent, unreadable, incomplete or about another generation is an error,
/// never an implicit pass.
fn verify_slot_manifest(
    artifact: &Path,
    slot_dir: &Path,
    generation: &RuntimeGeneration,
) -> Result<(), SmokeError> {
    let manifest = crate::slots::read_slot_manifest(slot_dir).map_err(|error| {
        SmokeError::Manifest(format!(
            "the slot {} has no readable descriptor: {error}",
            slot_dir.display()
        ))
    })?;
    if manifest.generation != generation.0 {
        return Err(SmokeError::Manifest(format!(
            "the slot {} describes generation {}, not the {} this smoke was asked to qualify",
            slot_dir.display(),
            manifest.generation,
            generation.0
        )));
    }
    let declared = manifest.artifact_path.as_deref().ok_or_else(|| {
        SmokeError::Manifest(format!(
            "the slot {} names no program artifact",
            slot_dir.display()
        ))
    })?;
    // A recorded relative path is relative to its own slot, not to whatever
    // directory this process happens to run in.
    let recorded_path = PathBuf::from(declared);
    let recorded_path = if recorded_path.is_absolute() {
        recorded_path
    } else {
        slot_dir.join(recorded_path)
    };
    if !same_path(&recorded_path, artifact) {
        return Err(SmokeError::Manifest(format!(
            "the slot {} names {} as its program, not the {} this smoke was asked to run",
            slot_dir.display(),
            declared,
            artifact.display()
        )));
    }
    let recorded = manifest.artifact_digest.as_ref().ok_or_else(|| {
        SmokeError::Manifest(format!(
            "the slot {} records no digest for its program",
            slot_dir.display()
        ))
    })?;
    let bytes = std::fs::read(artifact).map_err(|error| {
        SmokeError::Manifest(format!(
            "the program {} named by the slot {} could not be read: {error}",
            artifact.display(),
            slot_dir.display()
        ))
    })?;
    let actual = Digest::blake3(&bytes);
    if &actual != recorded {
        return Err(SmokeError::Manifest(format!(
            "the program {} does not match the digest the slot records: recorded {recorded}, found {actual}",
            artifact.display()
        )));
    }
    Ok(())
}

/// Two paths name the same program when they are equal, or when both resolve
/// to the same file (a recorded relative path, a symlinked installation root).
fn same_path(declared: &Path, artifact: &Path) -> bool {
    if declared == artifact {
        return true;
    }
    match (declared.canonicalize(), artifact.canonicalize()) {
        (Ok(declared), Ok(artifact)) => declared == artifact,
        _ => false,
    }
}

/// The refusal detail of a failed smoke report: its exit code, every failed
/// check (including the synthesized "no readable report" check when the
/// candidate printed nothing usable) and the bounded stderr tail.
pub(crate) fn describe_failure(report: &SmokeReport) -> String {
    let mut detail = format!("exit code {}", report.exit_code);
    let failures: Vec<String> = report
        .failures()
        .map(|check| match &check.detail {
            Some(detail) => format!("{}: {}", check.name, detail),
            None => check.name.clone(),
        })
        .collect();
    if failures.is_empty() {
        detail.push_str("; no failed checks recorded");
    } else {
        detail.push_str(&format!("; failed checks: {}", failures.join(", ")));
    }
    if !report.stderr_tail.is_empty() {
        detail.push_str(&format!("; stderr tail: {}", tail(&report.stderr_tail)));
    }
    detail
}

/// The JSON object the candidate prints on stdout.
fn extract_json(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    Some(text[start..=end].to_string())
}

fn tail(text: &str) -> String {
    if text.len() <= OUTPUT_TAIL_BYTES {
        return text.to_string();
    }
    let start = text.len() - OUTPUT_TAIL_BYTES;
    let start = text
        .char_indices()
        .find(|(index, _)| *index >= start)
        .map(|(index, _)| index)
        .unwrap_or(start);
    text[start..].to_string()
}

/// The job object that owns one smoke launch.
///
/// A job is inherited by the processes a member creates, but a descendant
/// created *before* the assignment is not retroactively captured. Spawning
/// first and assigning afterwards therefore leaves a window in which a
/// candidate can fork something outside the tree, so the candidate is created
/// suspended, assigned, and only then resumed. `terminate-on-last-close` makes
/// the job's own lifetime the termination condition: whenever the last handle
/// goes away — including a dropped smoke future that runs no cleanup code —
/// every process still in the job is killed.
///
/// The smoke never asks for `CREATE_BREAKAWAY_FROM_JOB`: a candidate is only
/// ever run inside this job. When the process running the smoke is itself in a
/// job, the candidate is nested inside this one (Windows 8 and later); an
/// assignment the platform refuses is a launch failure, never a candidate left
/// running outside the tree.
#[cfg(windows)]
mod win32 {
    use std::ffi::c_void;
    use std::mem::size_of;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
    };

    use super::CleanupNotes;

    /// Create the candidate suspended, so it is inside the job before it runs
    /// and before it can create anything of its own.
    pub(super) fn create_suspended(command: &mut tokio::process::Command) {
        command.creation_flags(CREATE_SUSPENDED);
    }

    /// One job object, closed when this value is dropped.
    ///
    /// The handle is created without inheritable attributes and with default
    /// security, so it is not passed to any child: the smoke's own descendants
    /// inherit job membership, not the ability to close the job.
    ///
    /// Nothing else this launch opens on Windows is inheritable either — the
    /// job exists before the candidate does and must not be reachable from it,
    /// and the snapshot and thread handles are opened with `bInheritHandle`
    /// false. Handles the smoke process itself was handed stay the standard
    /// `CreateProcess` inheritance surface; this launch adds none of its own.
    pub(super) struct Job(HANDLE);

    // SAFETY: a job handle is a process-wide kernel object reference that may
    // be used and closed from any thread, and this value owns the only handle
    // it was created with, so no second owner can close it.
    unsafe impl Send for Job {}
    // SAFETY: as above; the value has no interior mutability and every method
    // takes `&self`.
    unsafe impl Sync for Job {}

    impl Job {
        pub(super) fn create() -> std::io::Result<Self> {
            // SAFETY: a null attribute pointer and a null name ask for an
            // unnamed, non-inheritable job with default security.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let job = Job(handle);
            // Terminate-on-last-close is the only limit: the job neither caps
            // the candidate's memory nor its process count, it only makes the
            // job's own lifetime the tree's lifetime.
            let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                    LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    ..Default::default()
                },
                ..Default::default()
            };
            // SAFETY: the value is the structure the information class names,
            // and the length is that structure's own size.
            let configured = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if configured == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(job)
        }

        /// Assign the suspended candidate to the job, then let it run.
        ///
        /// The assignment is what makes every descendant the candidate creates
        /// a member, so a failure here is a launch failure: the candidate has
        /// not run, and it is left suspended for the caller to kill.
        pub(super) fn assign_and_resume(&self, child: &tokio::process::Child) -> std::io::Result<()> {
            let process = child.raw_handle().ok_or_else(|| {
                std::io::Error::other("the candidate exited before it could be assigned to the smoke's job")
            })? as HANDLE;
            // SAFETY: `process` is the live process handle of the child and the
            // job handle is held by `self`.
            if unsafe { AssignProcessToJobObject(self.0, process) } == 0 {
                let error = std::io::Error::last_os_error();
                return Err(std::io::Error::new(
                    error.kind(),
                    format!(
                        "the candidate could not be added to the smoke's job object, so it would not belong to the tree this smoke stops: {error}"
                    ),
                ));
            }
            let pid = child.id().ok_or_else(|| {
                std::io::Error::other("the suspended candidate has no process id")
            })?;
            resume_suspended(pid).map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!(
                        "the assigned candidate could not be resumed inside the smoke's job object: {error}"
                    ),
                )
            })
        }

        /// Terminate every process in the job. The leader is in it too.
        pub(super) fn terminate(&self, notes: &mut CleanupNotes) {
            // SAFETY: the handle is held by `self` for the whole call.
            if unsafe { TerminateJobObject(self.0, 1) } == 0 {
                notes.add(format!(
                    "the smoke's job object could not be terminated: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // Closing the last handle is what
            // `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` acts on, so this close is
            // the termination of whatever is still in the job. A failure here
            // has nowhere to be reported and nothing left to report it to.
            // SAFETY: the handle is owned by this value and closed exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Resume the single thread a suspended process was created with.
    ///
    /// `std::process` hands back the process, not its primary thread, so the
    /// thread is found by snapshot. A `CREATE_SUSPENDED` process has exactly
    /// one thread and has not run yet, so it cannot have created another.
    fn resume_suspended(pid: u32) -> std::io::Result<()> {
        // SAFETY: a system-wide thread snapshot takes no input handles.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let resumed = find_thread(snapshot, pid).unwrap_or_else(|| {
            Err(std::io::Error::other(format!(
                "the suspended candidate {pid} has no thread to resume"
            )))
        });
        // SAFETY: this call owns the snapshot and closes it exactly once.
        unsafe { CloseHandle(snapshot) };
        resumed
    }

    fn find_thread(snapshot: HANDLE, pid: u32) -> Option<std::io::Result<()>> {
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        // SAFETY: `entry` is the structure this API fills in, and its `dwSize`
        // names its own size.
        let mut more = unsafe { Thread32First(snapshot, &mut entry) };
        while more != 0 {
            if entry.th32OwnerProcessID == pid {
                return Some(resume_thread(entry.th32ThreadID));
            }
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            // SAFETY: same snapshot and same structure as above.
            more = unsafe { Thread32Next(snapshot, &mut entry) };
        }
        None
    }

    fn resume_thread(thread_id: u32) -> std::io::Result<()> {
        // SAFETY: opening a thread by id asks only for the right to resume it.
        let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
        if thread.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the handle is live and owned by this call.
        let previous = unsafe { ResumeThread(thread) };
        // SAFETY: closed exactly once, after the resume.
        unsafe { CloseHandle(thread) };
        if previous == u32::MAX {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}
