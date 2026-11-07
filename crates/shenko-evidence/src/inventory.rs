//! Retained-evidence inventory — SPEC §46.6 (G08 证据与候选绑定).
//!
//! The repository keeps evidence records from earlier slices. Those records
//! are real, but each is bound to the candidate it was produced against.
//! Candidate equality is a metadata classification, not receipt admission.
//! Missing identities remain unbound; read failures stay visible.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

use crate::coverage::{EvidenceRecord, looks_like_evidence};
use crate::receipt::{read_json, receipt_path};

/// How a retained evidence record relates to the current candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Binding {
    /// Produced against the candidate under review.
    CurrentCandidate,
    /// Produced against a different candidate; history, not proof for the
    /// current one.
    OldCandidate,
    /// Names no candidate commit, so it cannot be bound either way.
    Unbound,
}

impl Binding {
    pub fn as_str(self) -> &'static str {
        match self {
            Binding::CurrentCandidate => "current_candidate",
            Binding::OldCandidate => "old_candidate",
            Binding::Unbound => "unbound",
        }
    }
}

/// One inventoried evidence record.
#[derive(Debug, Clone)]
pub struct InventoryEntry {
    pub record: EvidenceRecord,
    pub binding: Binding,
    /// Whether the record's native receipt file exists on disk.
    pub receipt_present: bool,
}

/// The full retained-evidence inventory.
#[derive(Debug, Clone, Default)]
pub struct EvidenceInventory {
    pub entries: Vec<InventoryEntry>,
    pub issues: Vec<InventoryIssue>,
    pub non_evidence_files: Vec<String>,
}

/// An input that could not be inventoried; never silently treated as absent.
#[derive(Debug, Clone)]
pub struct InventoryIssue {
    pub source_path: String,
    pub kind: &'static str,
    pub detail: String,
}

impl EvidenceInventory {
    /// Read every `*.json` in `evidence_dir` and inventory the records.
    ///
    /// Non-evidence JSON is listed separately; unreadable or malformed inputs
    /// make the inventory explicitly incomplete.
    pub fn read_dir(evidence_dir: &Path, candidate_commit: Option<&str>) -> EvidenceInventory {
        let mut inventory = EvidenceInventory::default();
        let directory = match std::fs::canonicalize(evidence_dir) {
            Ok(path) => path,
            Err(error) => {
                inventory.issue(evidence_dir, "directory_io", error.to_string());
                return inventory;
            }
        };
        let dir = match std::fs::read_dir(&directory) {
            Ok(dir) => dir,
            Err(error) => {
                inventory.issue(&directory, "directory_io", error.to_string());
                return inventory;
            }
        };
        let mut paths = Vec::new();
        for entry in dir {
            match entry {
                Ok(entry) if entry.path().extension().is_some_and(|ext| ext == "json") => {
                    paths.push(entry.path())
                }
                Ok(_) => {}
                Err(error) => inventory.issue(&directory, "directory_entry_io", error.to_string()),
            }
        }
        paths.sort();
        for path in paths {
            let value = match read_json(&path) {
                Ok(value) => value,
                Err(error) => {
                    inventory.issue(&path, "unreadable_or_malformed_json", error);
                    continue;
                }
            };
            if !looks_like_evidence(&value) {
                inventory
                    .non_evidence_files
                    .push(path.to_string_lossy().into_owned());
                continue;
            }
            let Some(record) = EvidenceRecord::from_json(&value, &path.to_string_lossy()) else {
                inventory.issue(
                    &path,
                    "malformed_record",
                    "invalid evidence fields or conflicting candidate aliases".to_string(),
                );
                continue;
            };
            let binding = match (candidate_commit, record.candidate_commit.as_deref()) {
                (Some(candidate), Some(record_commit))
                    if !candidate.is_empty() && !record_commit.is_empty() =>
                {
                    if candidate == record_commit {
                        Binding::CurrentCandidate
                    } else {
                        Binding::OldCandidate
                    }
                }
                _ => Binding::Unbound,
            };
            let receipt_present = receipt_path(&record).is_some_and(|path| path.is_file());
            inventory.entries.push(InventoryEntry {
                record,
                binding,
                receipt_present,
            });
        }
        inventory
    }

    fn issue(&mut self, path: &Path, kind: &'static str, detail: String) {
        self.issues.push(InventoryIssue {
            source_path: path.to_string_lossy().into_owned(),
            kind,
            detail,
        });
    }

    /// Counts per binding, for the machine-readable report.
    pub fn binding_counts(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for entry in &self.entries {
            *counts.entry(entry.binding.as_str()).or_insert(0) += 1;
        }
        counts
    }

    /// Machine-readable JSON for the inventory.
    pub fn to_json(&self) -> Value {
        let entries: Vec<Value> = self
            .entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "evidence_id": entry.record.evidence_id,
                    "source_path": entry.record.source_path,
                    "candidate_commit": entry.record.candidate_commit,
                    "binding": entry.binding.as_str(),
                    "receipt_present": entry.receipt_present,
                    "status": entry.record.status,
                    "requirements": entry.record.requirements,
                    "full_cases_passed": entry.record.full_cases_passed,
                    "cases_reference_only": entry.record.cases_reference_only,
                    "native_receipt": entry.record.native_receipt,
                    "withdrawn": entry.record.withdrawn,
                    "withdraws": entry.record.withdraws,
                    "withdraws_sha256": entry.record.withdraws_sha256,
                })
            })
            .collect();
        let counts: serde_json::Map<String, Value> = self
            .binding_counts()
            .into_iter()
            .map(|(key, value)| (key.to_string(), Value::from(value)))
            .collect();
        serde_json::json!({
            "kind": "shenko_evidence_inventory",
            "schema_version": 2,
            "complete": self.issues.is_empty(),
            "counts": counts,
            "entries": entries,
            "non_evidence_files": self.non_evidence_files,
            "issues": self.issues.iter().map(|issue| serde_json::json!({
                "source_path": issue.source_path,
                "kind": issue.kind,
                "detail": issue.detail,
            })).collect::<Vec<_>>(),
        })
    }
}
