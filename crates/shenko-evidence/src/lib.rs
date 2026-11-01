//! SPEC obligation, acceptance-case and evidence mapping — SPEC §23, §29,
//! §40 and §46; C19.
//!
//! This crate answers one question honestly: *what is still owed, and what
//! evidence would discharge it?* It is deliberately not a judge.
//!
//! * [`spec_index`] parses the SPEC's own requirement (§32), behavior (§39)
//!   and acceptance tables. The denominator therefore comes from the SPEC,
//!   not from the hand-maintained coverage ledger, whose IDs may be stale.
//! * [`coverage`] turns that index plus the retained evidence into a
//!   machine-readable list of unmet obligations, each with the verification
//!   scopes (failure / recovery / model / native) its acceptance cases need.
//! * [`inventory`] inventories the retained evidence records and states for
//!   each whether it is bound to the current candidate or is history from an
//!   older one.
//!
//! Nothing here decides that a requirement is satisfied because words match.
//! A requirement is met only when an evidence record that names it also
//! declares the relevant acceptance cases as fully passed, and that record
//! is bound to the candidate under review. Everything else is reported as
//! unmet, with its reason.

pub mod coverage;
pub mod inventory;
pub mod scopes;
pub mod spec_index;

pub use coverage::{
    build_report, classify_case_scopes, scope_declarations, CoverageReport, EvidenceRecord,
    RequirementObligation, Scope, UnmetReason,
};
pub use inventory::{Binding, EvidenceInventory, InventoryEntry};
pub use scopes::{scopes_for_case, ScopeDeclaration};
pub use spec_index::{AcceptanceCase, Behavior, IndexCounts, IndexError, Requirement, SpecIndex};

use std::path::Path;

/// Everything the report needs to know about where its inputs live.
#[derive(Debug, Clone)]
pub struct ReportInputs<'a> {
    /// The SPEC text.
    pub spec_text: &'a str,
    /// The directory holding the retained evidence records.
    pub evidence_dir: &'a Path,
    /// The candidate commit the report is bound to.
    pub candidate_commit: Option<&'a str>,
}

/// Parse the SPEC, inventory the evidence, and build the obligation report.
pub fn analyze(inputs: &ReportInputs<'_>) -> Result<Analysis, IndexError> {
    let index = SpecIndex::parse(inputs.spec_text)?;
    let inventory = EvidenceInventory::read_dir(inputs.evidence_dir, inputs.candidate_commit);
    let report = build_report(&index, &inventory.records(), inputs.candidate_commit);
    Ok(Analysis {
        index,
        inventory,
        report,
    })
}

/// The complete analysis of one candidate.
#[derive(Debug, Clone)]
pub struct Analysis {
    pub index: SpecIndex,
    pub inventory: EvidenceInventory,
    pub report: CoverageReport,
}

impl Analysis {
    /// The machine-readable report: obligations, scopes and the retained
    /// evidence inventory in one document.
    pub fn to_json(&self) -> serde_json::Value {
        let mut value = self.report.to_json();
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "evidence_inventory".to_string(),
                self.inventory.to_json(),
            );
            object.insert(
                "spec_index_counts".to_string(),
                serde_json::json!({
                    "requirements": self.index.counts().requirements,
                    "behaviors": self.index.counts().behaviors,
                    "acceptance_cases": self.index.counts().acceptance_cases,
                }),
            );
        }
        value
    }

    /// A short human-readable summary.
    pub fn summary(&self) -> String {
        let unmet = self.report.unmet();
        let counts = self.inventory.binding_counts();
        let mut lines = Vec::new();
        lines.push(format!(
            "requirements: {} ({} unmet)",
            self.report.requirement_count,
            unmet.len()
        ));
        lines.push(format!(
            "acceptance cases: {} ({} not named by any behavior)",
            self.report.acceptance_case_count,
            self.report.unreferenced_cases.len()
        ));
        for (binding, count) in counts {
            lines.push(format!("evidence {binding}: {count}"));
        }
        for obligation in unmet.iter().take(20) {
            let reasons: Vec<&str> = obligation
                .unmet_reasons
                .iter()
                .map(|reason| reason.as_str())
                .collect();
            lines.push(format!(
                "  {} {} [{}] scopes={:?}",
                obligation.requirement_id,
                obligation.summary.chars().take(40).collect::<String>(),
                reasons.join(","),
                obligation
                    .required_scopes
                    .iter()
                    .map(|scope| scope.as_str())
                    .collect::<Vec<_>>()
            ));
        }
        if unmet.len() > 20 {
            lines.push(format!("  … and {} more", unmet.len() - 20));
        }
        lines.join("\n")
    }
}
