//! Producing a retained correction/withdrawal record.
//!
//! The report already reads `withdraws`/`withdrawn` declarations, but until
//! now nothing wrote them: a reviewer who found an admitted claim wrong could
//! only hand-edit JSON, and hand-editing is exactly what "retained bytes are
//! never rewritten" forbids. This module is the producer for that existing
//! vocabulary.
//!
//! A correction is a new record, never an edit:
//!
//! * it names the original by `evidence_id` (`withdraws`) and by the SHA-256
//!   of the exact retained bytes the reviewer read (`withdraws_sha256`);
//! * it states why in `withdrawn`, and may name a real replacement record in
//!   `superseded_by` — the same keys [`crate::coverage::EvidenceRecord`] already
//!   parses, so there is one schema, not a parallel one;
//! * it contains no requirement, case, status or receipt claim of its own, so
//!   it can never be mistaken for the product evidence it withdraws;
//! * it is written with `create_new`, so an existing correction is never
//!   overwritten, and the original file is only ever opened for reading.
//!
//! Missing targets, ambiguous duplicate ids, a digest that no longer matches
//! the retained bytes, an already-withdrawn record, a correction of another
//! declaration, or a `superseded_by` naming nothing are all refusals: the
//! producer never invents the fact it was asked to reference.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::coverage::{EvidenceRecord, looks_like_evidence};

/// The `kind` written into a produced correction record.
pub const CORRECTION_KIND: &str = "shenko_evidence_correction";

/// Same bound the admission reader applies to JSON inputs.
const MAX_RECORD_BYTES: u64 = 8 * 1024 * 1024;

/// A request to withdraw one retained evidence record.
#[derive(Debug, Clone)]
pub struct CorrectionRequest<'a> {
    /// Directory holding the retained evidence records.
    pub evidence_dir: &'a Path,
    /// The `evidence_id` of the record being corrected.
    pub evidence_id: &'a str,
    /// Why the record can no longer be trusted, kept verbatim.
    pub reason: &'a str,
    /// SHA-256 of the original record's bytes as the reviewer read them.
    pub source_sha256: &'a str,
    /// Retained record that replaces the original, when this is a
    /// supersession rather than a bare withdrawal.
    pub superseded_by: Option<&'a str>,
}

/// What the producer actually wrote.
#[derive(Debug, Clone)]
pub struct CorrectionOutcome {
    pub correction_id: String,
    pub correction_path: PathBuf,
    pub source_path: PathBuf,
    pub withdraws: String,
    pub withdraws_sha256: String,
    pub superseded_by: Option<String>,
    pub bytes_written: usize,
}

impl CorrectionOutcome {
    /// Machine-readable outcome for a caller that wants to retain it.
    ///
    /// Deliberately carries no evidence keys (`evidence_id`, `requirements`,
    /// `full_cases_passed`, `status`, `native_receipt`), so writing it next to
    /// the evidence never invents another record.
    pub fn to_json(&self) -> Value {
        json!({
            "kind": "shenko_evidence_correction_outcome",
            "schema_version": 1,
            "correction_id": self.correction_id,
            "correction_path": self.correction_path.to_string_lossy(),
            "withdraws": self.withdraws,
            "withdraws_sha256": self.withdraws_sha256,
            "source_path": self.source_path.to_string_lossy(),
            "superseded_by": self.superseded_by,
            "bytes_written": self.bytes_written,
        })
    }
}

/// The id a correction for `evidence_id` receives.
///
/// Deterministic so a second attempt collides instead of silently forking the
/// correction history.
pub fn correction_id(evidence_id: &str) -> String {
    format!("{evidence_id}-withdrawal")
}

struct Retained {
    path: PathBuf,
    record: EvidenceRecord,
    bytes: Vec<u8>,
}

/// Write a new correction record that withdraws `evidence_id`.
///
/// The original file is never opened for writing, and no output is produced
/// unless every reference and the reviewed digest are verified first.
pub fn write_correction(
    request: &CorrectionRequest<'_>,
) -> Result<CorrectionOutcome, String> {
    let target_id = request.evidence_id.trim();
    if target_id.is_empty() {
        return Err("a correction needs the original evidence_id".to_string());
    }
    if !is_safe_component(target_id) {
        return Err(format!(
            "evidence_id {target_id:?} is not a safe file component; refuse to derive a correction name"
        ));
    }
    let reason = request.reason.trim();
    if reason.is_empty() {
        return Err("a correction needs an explicit reason (withdrawn)".to_string());
    }
    let expected = normalize_sha256(request.source_sha256).ok_or_else(|| {
        format!(
            "source digest {:?} is not a 64-character SHA-256",
            request.source_sha256
        )
    })?;
    let superseded_by = request
        .superseded_by
        .map(str::trim)
        .filter(|id| !id.is_empty());

    let directory = std::fs::canonicalize(request.evidence_dir).map_err(|error| {
        format!(
            "cannot read evidence directory {}: {error}",
            request.evidence_dir.display()
        )
    })?;
    if !std::fs::metadata(&directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
        .is_dir()
    {
        return Err(format!("not a directory: {}", directory.display()));
    }

    let (retained, target_failure) = scan(&directory, target_id)?;
    let candidates: Vec<&Retained> = retained
        .iter()
        .filter(|retained| retained.record.evidence_id == target_id)
        .collect();
    let target = match candidates.as_slice() {
        [target] => *target,
        [] => {
            return Err(match target_failure {
                Some(detail) => {
                    format!("the record named {target_id} cannot be read as evidence: {detail}")
                }
                None => format!("no retained evidence record has evidence_id {target_id}"),
            });
        }
        _ => {
            return Err(format!(
                "{target_id} names {} retained records; a correction needs one exact record",
                candidates.len()
            ));
        }
    };
    if target.record.withdraws.is_some() {
        return Err(format!(
            "{target_id} is itself a withdrawal declaration, not evidence that can be corrected"
        ));
    }
    if let Some(reason) = &target.record.withdrawn {
        return Err(format!(
            "{target_id} is already withdrawn ({reason}); refusing a second correction"
        ));
    }
    let actual = sha256(&target.bytes);

    // Only an effective correction blocks another one. A correction whose
    // digest no longer matches the target's current bytes is ignored by the
    // report, so it is not a fact this producer can build on either. A legacy
    // correction without a digest still declares the withdrawal.
    let existing = retained.iter().find(|other| {
        other.record.withdraws.as_deref() == Some(target_id)
            && other
                .record
                .withdraws_sha256
                .as_deref()
                .is_none_or(|expected| expected.eq_ignore_ascii_case(&actual))
    });
    if let Some(existing) = existing {
        return Err(format!(
            "{target_id} is already withdrawn by {}; refusing a second correction",
            existing.record.evidence_id
        ));
    }

    if actual != expected {
        return Err(format!(
            "source digest mismatch for {target_id}: reviewed {expected}, retained bytes are {actual}"
        ));
    }

    if let Some(replacement_id) = superseded_by {
        if replacement_id == target_id {
            return Err(format!(
                "superseded_by {replacement_id} names the record being withdrawn"
            ));
        }
        let replacements: Vec<&Retained> = retained
            .iter()
            .filter(|retained| retained.record.evidence_id == replacement_id)
            .collect();
        let replacement = match replacements.as_slice() {
            [replacement] => *replacement,
            [] => {
                return Err(format!(
                    "superseded_by names no retained evidence record: {replacement_id}"
                ));
            }
            _ => {
                return Err(format!(
                    "superseded_by names {} retained records: {replacement_id}",
                    replacements.len()
                ));
            }
        };
        if replacement.record.withdraws.is_some() {
            return Err(format!(
                "superseded_by names a withdrawal declaration, not a replacement claim: {replacement_id}"
            ));
        }
        let replacement_digest = sha256(&replacement.bytes);
        let replacement_withdrawn = replacement.record.withdrawn.is_some()
            || retained.iter().any(|other| {
                other.record.withdraws.as_deref() == Some(replacement_id)
                    && other
                        .record
                        .withdraws_sha256
                        .as_deref()
                        .is_none_or(|expected| {
                            expected.eq_ignore_ascii_case(&replacement_digest)
                        })
            });
        if replacement_withdrawn {
            return Err(format!(
                "superseded_by {replacement_id} is itself withdrawn and cannot replace the original"
            ));
        }
    }

    let new_id = correction_id(target_id);
    let correction_path = directory.join(format!("{new_id}.json"));
    if retained
        .iter()
        .any(|retained| retained.record.evidence_id == new_id)
    {
        return Err(format!(
            "a retained record already has evidence_id {new_id}; refusing to overwrite"
        ));
    }
    if correction_path.exists() {
        return Err(format!(
            "refusing to overwrite existing {}",
            correction_path.display()
        ));
    }

    let record = correction_json(&new_id, target_id, reason, &actual, superseded_by);
    // The producer must emit exactly what the existing reader understands.
    let round_trip = EvidenceRecord::from_json(&record, &correction_path.to_string_lossy())
        .ok_or_else(|| "produced correction is not a valid evidence declaration".to_string())?;
    if round_trip.evidence_id != new_id
        || round_trip.withdraws.as_deref() != Some(target_id)
        || round_trip.withdrawn.as_deref() != Some(reason)
        || round_trip.withdraws_sha256.as_deref() != Some(actual.as_str())
    {
        return Err("produced correction does not round-trip through the evidence parser".to_string());
    }
    let mut bytes =
        serde_json::to_vec_pretty(&record).map_err(|error| format!("cannot encode: {error}"))?;
    bytes.push(b'\n');

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&correction_path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "refusing to overwrite existing {}",
                    correction_path.display()
                )
            } else {
                format!("cannot create {}: {error}", correction_path.display())
            }
        })?;
    file.write_all(&bytes)
        .map_err(|error| format!("cannot write {}: {error}", correction_path.display()))?;
    file.sync_all()
        .map_err(|error| format!("cannot sync {}: {error}", correction_path.display()))?;

    // The already-reviewed bytes must still be the retained bytes. A record
    // that changed while the correction was being written leaves the
    // correction pointing at a different fact, so it is removed again.
    let after = read_record(&target.path).map(|bytes| sha256(&bytes));
    if matches!(&after, Ok(after) if after == &actual) {
        return Ok(CorrectionOutcome {
            correction_id: new_id,
            correction_path,
            source_path: target.path.clone(),
            withdraws: target_id.to_string(),
            withdraws_sha256: actual,
            superseded_by: superseded_by.map(str::to_string),
            bytes_written: bytes.len(),
        });
    }
    let removed = std::fs::remove_file(&correction_path);
    Err(format!(
        "the retained bytes of {target_id} could not be re-verified after writing the correction \
         ({after:?}); removed the unbound correction (remove result: {removed:?})"
    ))
}

/// The correction record itself, using only keys the report already parses.
fn correction_json<'a>(
    correction_id: &str,
    target_id: &str,
    reason: &str,
    digest: &str,
    superseded_by: Option<&'a str>,
) -> Value {
    let mut object = Map::new();
    object.insert("schema_version".to_string(), Value::from(1));
    object.insert("kind".to_string(), Value::from(CORRECTION_KIND));
    object.insert("evidence_id".to_string(), Value::from(correction_id));
    object.insert("withdraws".to_string(), Value::from(target_id));
    object.insert("withdraws_sha256".to_string(), Value::from(digest));
    object.insert("withdrawn".to_string(), Value::from(reason));
    if let Some(replacement) = superseded_by {
        object.insert("superseded_by".to_string(), Value::from(replacement));
    }
    Value::Object(object)
}

/// Read every `*.json` record, keeping the bytes that were parsed.
///
/// A file that fails to read or parse but is named like the target makes the
/// reference ambiguous instead of silently absent.
fn scan(directory: &Path, target_id: &str) -> Result<(Vec<Retained>, Option<String>), String> {
    let dir = std::fs::read_dir(directory)
        .map_err(|error| format!("cannot list {}: {error}", directory.display()))?;
    let mut paths = Vec::new();
    for entry in dir {
        let entry = entry.map_err(|error| format!("directory entry error: {error}"))?;
        if entry.path().extension().is_some_and(|ext| ext == "json") {
            paths.push(entry.path());
        }
    }
    paths.sort();
    let mut retained = Vec::new();
    let mut target_failure = None;
    for path in paths {
        let bytes = match read_record(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                if path.file_stem().and_then(|stem| stem.to_str()) == Some(target_id) {
                    target_failure = Some(error);
                }
                continue;
            }
        };
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(error) => {
                if path.file_stem().and_then(|stem| stem.to_str()) == Some(target_id) {
                    target_failure = Some(format!("{}: {error}", path.display()));
                }
                continue;
            }
        };
        if !looks_like_evidence(&value) {
            continue;
        }
        let Some(record) = EvidenceRecord::from_json(&value, &path.to_string_lossy()) else {
            let names_target = value
                .get("evidence_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id == target_id);
            if names_target || path.file_stem().and_then(|stem| stem.to_str()) == Some(target_id) {
                target_failure = Some(format!(
                    "{}: malformed evidence fields",
                    path.display()
                ));
            }
            continue;
        };
        retained.push(Retained {
            path,
            record,
            bytes,
        });
    }
    Ok((retained, target_failure))
}

fn read_record(path: &Path) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    if !file
        .metadata()
        .map_err(|error| format!("cannot stat {}: {error}", path.display()))?
        .is_file()
    {
        return Err(format!("not a regular file: {}", path.display()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(format!(
            "record exceeds {MAX_RECORD_BYTES} bytes: {}",
            path.display()
        ));
    }
    Ok(bytes)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn normalize_sha256(value: &str) -> Option<String> {
    let lower = value.trim().to_ascii_lowercase();
    (lower.len() == 64 && lower.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(lower)
}

fn is_safe_component(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && !id.ends_with('.')
        && !id.ends_with(' ')
        && !id.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
        })
}
