//! Candidate smoke launch — SPEC §9.5 (smoke), §35.7.
//!
//! The smoke step starts the candidate program against its own state copy
//! and checks that it can actually read that state. It never sends a real
//! message, never drives a real seat, never replays an outbox and never
//! starts a second unbounded subject loop: the candidate runs a bounded
//! readiness command that only reads.

use std::path::Path;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use shenko_types::ids::RuntimeGeneration;

/// The schema version of the smoke report.
pub const SMOKE_SCHEMA_VERSION: u32 = 1;

/// How much of the candidate's output is kept.
const OUTPUT_TAIL_BYTES: usize = 8 * 1024;

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
    #[error("the candidate program could not be started: {0}")]
    Launch(#[source] std::io::Error),
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
/// never be sealed as activation evidence.
pub async fn launch_smoke(
    artifact: &Path,
    slot_dir: &Path,
    generation: &RuntimeGeneration,
) -> Result<SmokeReport, SmokeError> {
    let output = Command::new(artifact)
        .arg("--data-dir")
        .arg(slot_dir)
        .arg("update")
        .arg("smoke")
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(SmokeError::Launch)?;

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
    if !report.passed {
        return Err(SmokeError::Failed(describe_failure(&report)));
    }
    Ok(report)
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
