//! Retained-evidence inventory — SPEC §46.6 (G08 证据与候选绑定).
//!
//! The repository keeps evidence records from earlier slices. Those records
//! are real, but each is bound to the candidate it was produced against.
//! This module inventories them and states, per record, whether it can be
//! re-bound to the current candidate or whether it is history that a new
//! candidate has to reproduce.
//!
//! "Re-bindable" is a fact about the record, not an opinion about the
//! product: a record whose `product_commit` equals the current candidate
//! commit, or which declares no commit at all but names a receipt, can be
//! carried forward; a record bound to a different commit cannot.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::coverage::EvidenceRecord;

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
}

impl EvidenceInventory {
    /// Read every `*.json` in `evidence_dir` and inventory the records.
    ///
    /// Files that are not evidence records (documentation-integrity reports
    /// and similar) are kept in the inventory as unbound entries rather than
    /// dropped, so the denominator of "what evidence exists" stays visible.
    pub fn read_dir(evidence_dir: &Path, candidate_commit: Option<&str>) -> EvidenceInventory {
        let mut entries = Vec::new();
        let Ok(dir) = std::fs::read_dir(evidence_dir) else {
            return EvidenceInventory { entries };
        };
        let mut paths: Vec<PathBuf> = dir
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        for path in paths {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(record) = EvidenceRecord::from_json(&value, &path.to_string_lossy()) else {
                continue;
            };
            let binding = match (candidate_commit, record.candidate_commit.as_deref()) {
                (Some(candidate), Some(record_commit)) => {
                    if candidate == record_commit {
                        Binding::CurrentCandidate
                    } else {
                        Binding::OldCandidate
                    }
                }
                (None, Some(_)) => Binding::OldCandidate,
                _ => Binding::Unbound,
            };
            let receipt_present = record
                .native_receipt
                .as_deref()
                .map(|receipt| {
                    let candidate = Path::new(receipt);
                    candidate.exists()
                        || evidence_dir
                            .parent()
                            .map(|root| root.join(receipt).exists())
                            .unwrap_or(false)
                })
                .unwrap_or(false);
            entries.push(InventoryEntry {
                record,
                binding,
                receipt_present,
            });
        }
        EvidenceInventory { entries }
    }

    /// The records only, for obligation building.
    pub fn records(&self) -> Vec<EvidenceRecord> {
        self.entries
            .iter()
            .map(|entry| entry.record.clone())
            .collect()
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
            "schema_version": 1,
            "counts": counts,
            "entries": entries,
        })
    }
}
