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
use tokio::process::Command;

use shenko_types::ids::RuntimeGeneration;
use shenko_types::object::Digest;

/// The schema version of the smoke report.
pub const SMOKE_SCHEMA_VERSION: u32 = 1;

/// How much of the candidate's output is kept.
const OUTPUT_TAIL_BYTES: usize = 8 * 1024;

/// How long a candidate has to answer the bounded readiness command.
///
/// The smoke runs while the working runtime is already quiesced (a rollback
/// relaunch of the retained executable, or a prepared candidate before any
/// hand-over), so a candidate that never answers would hold the installation
/// paused forever. The command is stopped and the smoke fails instead; a
/// timeout is never a pass.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(120);

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
    #[error("the candidate program did not answer the bounded smoke within {0}s and was stopped; no report is sealed")]
    Timeout(u64),
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
    let output = match tokio::time::timeout(SMOKE_TIMEOUT, command.output()).await {
        Ok(output) => output.map_err(SmokeError::Launch)?,
        Err(_) => return Err(SmokeError::Timeout(SMOKE_TIMEOUT.as_secs())),
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

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
