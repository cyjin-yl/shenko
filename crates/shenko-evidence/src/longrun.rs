//! Long-run observation admission — S5 and COMPLETION §2「长期性」.
//!
//! S5 requires a 7/30-day report formed against a named candidate and
//! environment, and COMPLETION §4 forbids splicing different candidates or
//! accelerated runs into a stable 30-day claim. The sampler
//! (`scripts/observe_longrun.py`) already refuses gapped, unverifiable or
//! damaged observation bytes; this module is the *consumer* side: it admits a
//! retained sampler report only when
//!
//! * the caller supplied the digest the report is expected to have,
//! * the report is a real observation of *this* candidate (commit, measured
//!   binary digest and runtime profile),
//! * the sampler's own integrity section is intact and the report is eligible
//!   as product evidence,
//! * the run was terminally sealed with `stop_reason: completed`, so the byte
//!   stream cannot later grow a gap that invalidates the claimed window,
//! * at least one declared window is observed, with corroborated qualifying
//!   seconds at least equal to that window's declared duration. Every other
//!   declared window stays in the summary as unobserved.
//! * a window carrying the SPEC's own name (`7d`, `30d`) requires at least the
//!   SPEC's duration, so a shorter run cannot borrow the name; and several
//!   admitted reports only add their windows up when they measured the same
//!   binary digest in the same profile.
//!
//! Duration is never inferred from the presence of a tool, a directory or a
//! report file. An admitted report is also not proof of behaviour *between*
//! samples, of physical power-loss durability, or of semantic correctness;
//! those are separate observations. When no report is bound to the candidate
//! the obligation report says `duration_status: unobserved`, which is the
//! honest state of every candidate that has not actually run for the declared
//! windows.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

const MAX_JSON_BYTES: u64 = 8 * 1024 * 1024;
const REPORT_KIND: &str = "c19_long_run_observation_report";
/// Schema 5 binds live samples to SHA-256 measurements of the open executed
/// file. Older reports cannot establish that binding and stay as explicit
/// rejections, including sealed schema-4 reports based only on size/mtime.
const REPORT_SCHEMA: u64 = 5;
const REAL_OBSERVATION: &str = "real_observation";
const OBSERVED_VERDICT: &str = "all_declared_windows_observed";
const INCOMPLETE_VERDICT: &str = "incomplete_observation";
const COMPLETED: &str = "completed";
/// The two windows SPEC S5 / COMPLETION §2 name as the duration obligation.
///
/// A report may declare its own windows, but naming one of these while
/// requiring less time would be declaring a different obligation under the
/// SPEC's name, so the name and the duration have to agree.
const SPEC_WINDOWS: [(&str, f64); 2] = [("7d", 604_800.0), ("30d", 2_592_000.0)];

/// A retained sampler report the caller wants admitted, with the digest the
/// caller expects it to have. A path alone is never proof of its content.
#[derive(Debug, Clone)]
pub struct LongRunInput<'a> {
    pub path: &'a Path,
    pub sha256: &'a str,
}

/// One declared observation window from a sampler report.
#[derive(Debug, Clone, PartialEq)]
pub struct LongRunWindow {
    pub name: String,
    pub required_seconds: f64,
    /// Corroborated elapsed time inside one intact, gap-free candidate run.
    pub qualifying_seconds: f64,
}

impl LongRunWindow {
    pub fn observed(&self) -> bool {
        self.required_seconds > 0.0 && self.qualifying_seconds >= self.required_seconds
    }
}

/// A durable mid-run checkpoint recorded by the sampler.
#[derive(Debug, Clone, PartialEq)]
pub struct LongRunCheckpoint {
    pub checkpoint_id: String,
    /// The sample sequence this checkpoint was taken at; it cannot name a
    /// sample the run had not written yet.
    pub sample_seq: u64,
    pub epoch: f64,
}

/// An admitted long-run report.
#[derive(Debug, Clone, PartialEq)]
pub struct LongRunAdmission {
    pub path: PathBuf,
    pub sha256: String,
    pub run_id: String,
    pub candidate_commit: String,
    /// The runtime/build profile the observation belongs to.
    pub profile: String,
    /// Digest of the candidate binary the sampler measured at `init`.
    pub binary_sha256: String,
    pub evidence_class: String,
    pub stop_reason: String,
    pub stop_sample_seq: u64,
    pub stop_epoch: f64,
    pub checkpoints: Vec<LongRunCheckpoint>,
    pub windows: Vec<LongRunWindow>,
}

impl LongRunAdmission {
    pub fn observed_windows(&self) -> Vec<&str> {
        self.windows
            .iter()
            .filter(|window| window.observed())
            .map(|window| window.name.as_str())
            .collect()
    }
}

/// What the obligation report can honestly say about 7/30-day duration.
#[derive(Debug, Clone, Default)]
pub struct LongRunSummary {
    pub admitted: Vec<LongRunAdmission>,
    /// Rejected reports, keyed by path, with the original failure.
    pub rejections: BTreeMap<String, String>,
    /// Every declared window name, with whether an admitted report observed it.
    pub windows: BTreeMap<String, bool>,
    /// Distinct runtime/build profiles among the admitted reports.
    pub profiles: BTreeSet<String>,
    /// Distinct measured candidate binary digests among the admitted reports.
    pub binaries: BTreeSet<String>,
}

impl LongRunSummary {
    /// Declared windows covered by admitted reports of *different* artifacts
    /// or profiles.
    ///
    /// A duration obligation belongs to one candidate artifact observed in one
    /// runtime profile. A 7-day report of binary A and a 30-day report of
    /// binary B are two observations; adding them would claim a 30-day run
    /// nobody performed. The conflict is reported by its exact digests and
    /// profile names instead of being resolved in favor of one of them.
    pub fn identity_conflicts(&self) -> Vec<String> {
        let mut conflicts = Vec::new();
        if self.binaries.len() > 1 {
            conflicts.push(format!(
                "admitted reports measure different candidate binaries: {}",
                self.binaries.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if self.profiles.len() > 1 {
            conflicts.push(format!(
                "admitted reports belong to different profiles: {}",
                self.profiles.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        conflicts
    }

    /// `observed` only when at least one report was admitted, every declared
    /// window is observed by an admitted report, and those reports describe
    /// one artifact in one profile. The named 7-day and 30-day windows must
    /// be among them: another window's coverage is a different observation.
    pub fn duration_status(&self) -> &'static str {
        let spec_covered = SPEC_WINDOWS
            .iter()
            .all(|(name, _)| self.windows.get(*name).is_some_and(|observed| *observed));
        if self.admitted.is_empty()
            || !self.identity_conflicts().is_empty()
            || !spec_covered
            || self.windows.values().any(|observed| !observed)
        {
            "unobserved"
        } else {
            "observed"
        }
    }

    pub fn unobserved_windows(&self) -> Vec<String> {
        let mut unobserved: Vec<String> = SPEC_WINDOWS
            .iter()
            .filter(|(name, _)| !self.windows.contains_key(*name))
            .map(|(name, _)| (*name).to_string())
            .collect();
        unobserved.extend(
            self.windows
                .iter()
                .filter(|(_, observed)| !**observed)
                .map(|(name, _)| name.clone()),
        );
        unobserved.sort();
        unobserved.dedup();
        unobserved
    }

    /// Machine-readable view.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "report_schema": REPORT_SCHEMA,
            "duration_status": self.duration_status(),
            "windows": self.windows,
            "unobserved_windows": self.unobserved_windows(),
            "obligation_windows": SPEC_WINDOWS
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<&str>>(),
            "candidate_binaries": self.binaries,
            "profiles": self.profiles,
            "identity_conflicts": self.identity_conflicts(),
            "admitted_reports": self.admitted.iter().map(|report| serde_json::json!({
                "path": report.path,
                "sha256": report.sha256,
                "run_id": report.run_id,
                "candidate_commit": report.candidate_commit,
                "profile": report.profile,
                "binary_sha256": report.binary_sha256,
                "evidence_class": report.evidence_class,
                "stop_reason": report.stop_reason,
                "stop_sample_seq": report.stop_sample_seq,
                "stop_epoch": report.stop_epoch,
                "checkpoint_count": report.checkpoints.len(),
                "checkpoints": report.checkpoints.iter().map(|checkpoint| serde_json::json!({
                    "checkpoint_id": checkpoint.checkpoint_id,
                    "sample_seq": checkpoint.sample_seq,
                    "epoch": checkpoint.epoch,
                })).collect::<Vec<_>>(),
                "windows": report.windows.iter().map(|window| serde_json::json!({
                    "name": window.name,
                    "required_seconds": window.required_seconds,
                    "qualifying_seconds": window.qualifying_seconds,
                    "status": if window.observed() { "observed" } else { "unobserved" },
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "rejections": self.rejections,
            "verification_boundary": "sampler reports are points-in-time observations of one intact, \
                terminally stopped candidate run; they do not prove behaviour between samples, \
                physical power-loss durability, or semantic correctness; windows only add up \
                when every admitted report measures the same candidate binary in the same profile",
        })
    }
}

/// Admit the retained long-run reports against one candidate.
pub fn summarize<'a>(
    inputs: impl IntoIterator<Item = &'a LongRunInput<'a>>,
    candidate_commit: Option<&str>,
) -> LongRunSummary {
    let mut summary = LongRunSummary::default();
    let mut declared = BTreeSet::new();
    for input in inputs {
        let key = input.path.to_string_lossy().into_owned();
        match admit(input, candidate_commit) {
            Ok(report) => {
                summary.profiles.insert(report.profile.clone());
                summary.binaries.insert(report.binary_sha256.clone());
                for window in &report.windows {
                    declared.insert(window.name.clone());
                    let observed = summary.windows.entry(window.name.clone()).or_insert(false);
                    *observed |= window.observed();
                }
                summary.admitted.push(report);
            }
            Err(error) => {
                // A rejected report still declares the windows the caller
                // asked about; they stay visible as unobserved instead of
                // disappearing from the denominator.
                if let Ok(bytes) = read_bytes(input.path)
                    && let Some(names) = declared_windows(&bytes)
                {
                    declared.extend(names);
                }
                summary.rejections.insert(key, error);
            }
        }
    }
    for name in declared {
        summary.windows.entry(name).or_insert(false);
    }
    summary
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let file =
        File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    if !file.metadata().map_err(|error| error.to_string())?.is_file() {
        return Err(format!("not a regular file: {}", path.display()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return Err(format!(
            "report exceeds {MAX_JSON_BYTES} bytes: {}",
            path.display()
        ));
    }
    Ok(bytes)
}

/// Window names a report declares, readable even when the report itself is
/// rejected so the claim stays in the denominator.
fn declared_windows(bytes: &[u8]) -> Option<Vec<String>> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    Some(
        value
            .get("declared_windows")?
            .as_array()?
            .iter()
            .filter_map(|window| window.get("window")?.as_str().map(str::to_string))
            .collect(),
    )
}

fn digest_matches(bytes: &[u8], expected: &str) -> bool {
    let actual = Sha256::digest(bytes);
    expected.len() == actual.len() * 2
        && expected
            .as_bytes()
            .chunks_exact(2)
            .zip(actual)
            .all(|(hex, byte)| {
                let digit = |c: u8| match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    _ => None,
                };
                digit(hex[0])
                    .zip(digit(hex[1]))
                    .is_some_and(|(high, low)| high * 16 + low == byte)
            })
}

fn finite_number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite())
}

fn hex_digits(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Read one non-empty string member, or state which one is unusable.
fn text<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| format!("missing or invalid {name}"))
}

/// Read a positive integer member (`u64`, never a float or a string).
fn positive(value: &Value, name: &str) -> Result<u64, String> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .filter(|number| *number > 0)
        .ok_or_else(|| format!("missing or invalid {name}"))
}

fn admit(
    input: &LongRunInput<'_>,
    candidate_commit: Option<&str>,
) -> Result<LongRunAdmission, String> {
    let bytes = read_bytes(input.path)?;
    if !digest_matches(&bytes, input.sha256) {
        return Err(format!("report SHA-256 mismatch: {}", input.path.display()));
    }
    let report: Value =
        serde_json::from_slice(&bytes).map_err(|error| format!("invalid report JSON: {error}"))?;
    if report.get("kind").and_then(Value::as_str) != Some(REPORT_KIND) {
        return Err(format!("not a {REPORT_KIND} document"));
    }
    let schema = report
        .get("schema")
        .and_then(Value::as_u64)
        .ok_or("report has no numeric schema")?;
    if schema != REPORT_SCHEMA {
        return Err(format!(
            "unsupported long-run report schema {schema}; only a sealed schema \
             {REPORT_SCHEMA} report can discharge the duration obligation"
        ));
    }
    // The report's candidate block is the sampler's own manifest declaration
    // plus the binary it measured at `init`; a duration claim that names no
    // artifact is not attributable to anything.
    let candidate = report
        .get("candidate")
        .filter(|value| value.is_object())
        .ok_or("report has no candidate block")?;
    let report_candidate = text(candidate, "commit")?.to_string();
    if !hex_digits(&report_candidate, 40) {
        return Err("candidate commit is not a 40-hex git commit".to_string());
    }
    let profile = text(candidate, "profile")?.to_string();
    let binary_sha256 = text(candidate, "binary_sha256")?.to_string();
    if !hex_digits(&binary_sha256, 64) {
        return Err("candidate binary_sha256 is not a SHA-256 digest".to_string());
    }
    let measured = candidate
        .get("binary_measured")
        .filter(|value| value.is_object())
        .ok_or("report has no measured candidate binary")?;
    if text(measured, "sha256")? != binary_sha256.as_str() {
        return Err("measured candidate binary does not match the declaration".to_string());
    }
    match candidate_commit.filter(|commit| !commit.is_empty()) {
        None => return Err("candidate identity is missing".to_string()),
        Some(commit) if commit != report_candidate.as_str() => {
            return Err("report belongs to another candidate".to_string());
        }
        Some(_) => {}
    }
    let evidence_class = text(&report, "evidence_class")?.to_string();
    if evidence_class != REAL_OBSERVATION {
        return Err(format!(
            "report class {evidence_class} cannot discharge a product observation"
        ));
    }
    let integrity = report
        .get("integrity")
        .filter(|value| value.is_object())
        .ok_or("report has no integrity section")?;
    if integrity.get("intact").and_then(Value::as_bool) != Some(true) {
        return Err("report integrity is not intact".to_string());
    }
    if text(integrity, "candidate_binary_binding")? != "proc_exe_sha256" {
        return Err("report has no executed-file SHA-256 binding".to_string());
    }
    let recorded_samples = positive(&report, "samples")?;
    let alive_samples = positive(&report, "subject_alive_samples")?;
    let verified_samples = integrity
        .get("candidate_binary_verified_sample_seqs")
        .and_then(Value::as_array)
        .ok_or("report has no verified executed-file sample list")?;
    if alive_samples > recorded_samples || verified_samples.len() as u64 != alive_samples {
        return Err("executed-file verification does not cover every live sample".to_string());
    }
    let mut previous_sample = 0;
    for sample in verified_samples {
        let sequence = sample.as_u64().ok_or("invalid verified sample sequence")?;
        if sequence <= previous_sample || sequence > recorded_samples {
            return Err("invalid or repeated verified executed-file sample".to_string());
        }
        previous_sample = sequence;
    }
    for name in [
        "candidate_binary_unverified_sample_seqs",
        "candidate_binary_drift_sample_seqs",
    ] {
        if !integrity
            .get(name)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("report has no {name}"))?
            .is_empty()
        {
            return Err("report contains unverified or mismatched executed files".to_string());
        }
    }
    // The report's own eligibility/verdict are summaries of *all* declared
    // windows. Admission is per window instead: a run that declared 7d+30d
    // and was stopped after 7 observed days is genuine 7-day evidence while
    // its 30-day window stays unobserved, and hiding that would lose a real
    // observation. An unknown verdict is still refused rather than read as
    // either state.
    let verdict = text(&report, "verdict")?;
    if verdict != OBSERVED_VERDICT && verdict != INCOMPLETE_VERDICT {
        return Err(format!("unknown report verdict {verdict}"));
    }
    // A run that was never stopped, or was stopped for any reason other than
    // completion, is not a finished window: it can still grow a gap, be
    // superseded or be abandoned, so its span cannot be read as final.
    let lifecycle = report
        .get("lifecycle")
        .filter(|value| value.is_object())
        .ok_or("report has no sealed run lifecycle; the run was never stopped")?;
    if lifecycle.get("sealed").and_then(Value::as_bool) != Some(true) {
        return Err("report run was not sealed".to_string());
    }
    let stop_reason = text(lifecycle, "stop_reason")?.to_string();
    if stop_reason != COMPLETED {
        return Err(format!(
            "report run stopped for {stop_reason}, not {COMPLETED}"
        ));
    }
    let stop_sample_seq = positive(lifecycle, "stop_sample_seq")?;
    let stop_epoch = finite_number(lifecycle.get("stop_epoch"))
        .ok_or("sealed run has no stop epoch")?;
    if let Some(last_sample_epoch) = finite_number(integrity.get("last_sample_epoch"))
        && stop_epoch < last_sample_epoch
    {
        return Err("run stopped before its last recorded sample".to_string());
    }
    // Terminal completion names a sample the report actually recorded. A
    // sealed run whose end claims a later sample than it wrote is not a
    // finished observation of these bytes, whatever its stop reason says.
    if stop_sample_seq > recorded_samples {
        return Err(format!(
            "sealed run stopped at sample {stop_sample_seq}, past its {recorded_samples} recorded samples"
        ));
    }
    let checkpoints = lifecycle
        .get("checkpoints")
        .and_then(Value::as_array)
        .ok_or("sealed run declares no checkpoint list")?;
    let mut admitted_checkpoints = Vec::new();
    for checkpoint in checkpoints {
        admitted_checkpoints.push(LongRunCheckpoint {
            checkpoint_id: text(checkpoint, "checkpoint_id")?.to_string(),
            sample_seq: positive(checkpoint, "sample_seq")?,
            epoch: finite_number(checkpoint.get("epoch"))
                .ok_or("checkpoint has no epoch")?,
        });
    }
    let rows = report
        .get("declared_windows")
        .and_then(Value::as_array)
        .filter(|rows| !rows.is_empty())
        .ok_or("report declares no windows")?;
    let mut windows = Vec::new();
    for row in rows {
        let name = text(row, "window")?.to_string();
        let required_seconds =
            finite_number(row.get("required_seconds")).ok_or("window has no required duration")?;
        if required_seconds <= 0.0 {
            return Err(format!("declared window {name} has no positive duration"));
        }
        if let Some((_, minimum)) = SPEC_WINDOWS
            .iter()
            .find(|(spec, _)| *spec == name.as_str())
            && required_seconds < *minimum
        {
            return Err(format!(
                "declared window {name} requires {required_seconds} seconds, less than the \
                 {minimum} second SPEC window of that name"
            ));
        }
        let qualifying_seconds = finite_number(row.get("qualifying_span_seconds")).unwrap_or(0.0);
        let window = LongRunWindow {
            name,
            required_seconds,
            qualifying_seconds,
        };
        match text(row, "status")? {
            "observed" if window.observed() => windows.push(window),
            "observed" => {
                return Err(format!(
                    "declared window {} is marked observed with {} qualifying seconds of {} required",
                    window.name, window.qualifying_seconds, window.required_seconds
                ));
            }
            // Declared but not covered by this run: it stays visible in the
            // admission with `observed() == false`, so the summary reports it
            // as unobserved instead of dropping it from the denominator.
            "unobserved" => windows.push(window),
            other => {
                return Err(format!(
                    "declared window {} has an unknown status {other}",
                    window.name
                ));
            }
        }
    }
    if !windows.iter().any(LongRunWindow::observed) {
        return Err("no declared window is observed".to_string());
    }
    Ok(LongRunAdmission {
        path: input.path.to_path_buf(),
        sha256: input.sha256.to_string(),
        run_id: text(&report, "run_id")?.to_string(),
        candidate_commit: report_candidate,
        profile,
        binary_sha256,
        evidence_class,
        stop_reason,
        stop_sample_seq,
        stop_epoch,
        checkpoints: admitted_checkpoints,
        windows,
    })
}
