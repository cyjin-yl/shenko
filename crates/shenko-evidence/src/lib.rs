//! SPEC obligation, acceptance-case and evidence mapping — docs/specs/verification.md#spec-23, docs/specs/verification.md#spec-29,
//! docs/governance/development-contract.md#spec-40 and docs/governance/COMPLETION.md#spec-46; C19.
//!
//! This crate answers one question honestly: *what is still owed, and what
//! evidence would discharge it?* It is deliberately not a judge.
//!
//! * [`spec_index`] parses the SPEC's own requirement (docs/requirements/traceability.md#spec-32), behavior (docs/requirements/design-provenance.md#spec-39)
//!   and acceptance tables. The denominator therefore comes from the SPEC,
//!   not from the hand-maintained coverage ledger, whose IDs may be stale.
//! * [`coverage`] turns that index plus the retained evidence into a
//!   machine-readable list of unmet obligations, each with the verification
//!   scopes (failure / recovery / model / native) its acceptance cases need.
//! * [`inventory`] inventories the retained evidence records and states for
//!   each whether it is bound to the current candidate or is history from an
//!   older one.
//! * [`longrun`] admits a retained 7/30-day sampler report only when it is a
//!   digest-matched, intact, terminally stopped observation of the same
//!   candidate; the honest default for every candidate is `unobserved`.
//! * [`correction`] writes the missing producer for the report's
//!   `withdraws`/`withdrawn` vocabulary: a new, digest-bound record that
//!   withdraws one retained record without rewriting its bytes, adding no
//!   product claim of its own.
//!
//! Nothing here decides that a requirement is satisfied because words match.
//! Mechanical coverage requires original hash-bound receipts for every case
//! and scope, a current candidate, and a complete specification mapping and
//! inventory. This is not independent validation of an observer's judgment.

pub mod contract_input;
pub mod correction;
pub mod coverage;
pub mod inventory;
pub mod longrun;
mod receipt;
pub mod scopes;
pub mod spec_index;

pub use correction::{
    CORRECTION_KIND, CorrectionOutcome, CorrectionRequest, correction_id, write_correction,
};
pub use coverage::{
    CaseAdmission, CoverageReport, EvidenceRecord, RequirementObligation, Scope, UnmetReason,
    build_report, classify_case_scopes, scope_declarations,
};
pub use inventory::{Binding, EvidenceInventory, InventoryEntry, InventoryIssue};
pub use longrun::{
    LongRunAdmission, LongRunCheckpoint, LongRunInput, LongRunSummary, LongRunWindow, summarize,
};
pub use scopes::{ScopeDeclaration, scopes_for_case};
pub use spec_index::{AcceptanceCase, Behavior, IndexCounts, IndexError, Requirement, SpecIndex};

use std::path::Path;

/// Everything the report needs to know about where its inputs live.
#[derive(Debug, Clone)]
pub struct ReportInputs<'a> {
    /// Current contract text (assembled from docs when using a manifest).
    pub spec_text: &'a str,
    /// The directory holding the retained evidence records.
    pub evidence_dir: &'a Path,
    /// The candidate commit the report is bound to.
    pub candidate_commit: Option<&'a str>,
    /// Retained long-run sampler reports offered as duration evidence.
    ///
    /// Empty is the normal state: no candidate has a completed 7/30-day
    /// observation, so the report states `duration_status: unobserved`
    /// instead of inferring durability from the tooling that could observe it.
    pub long_run: &'a [LongRunInput<'a>],
}

/// Parse the SPEC, inventory the evidence, and build the obligation report.
pub fn analyze(inputs: &ReportInputs<'_>) -> Result<Analysis, IndexError> {
    let index = SpecIndex::parse(inputs.spec_text)?;
    let inventory = EvidenceInventory::read_dir(inputs.evidence_dir, inputs.candidate_commit);
    let mut report = coverage::build_report_from_records(
        &index,
        inventory.entries.iter().map(|entry| &entry.record),
        inputs.candidate_commit,
    );
    if !inventory.issues.is_empty() {
        for obligation in &mut report.requirements {
            obligation
                .unmet_reasons
                .push(UnmetReason::InventoryIncomplete);
        }
    }
    let long_run = longrun::summarize(inputs.long_run, inputs.candidate_commit);
    Ok(Analysis {
        index,
        inventory,
        report,
        long_run,
    })
}

/// The complete analysis of one candidate.
#[derive(Debug, Clone)]
pub struct Analysis {
    pub index: SpecIndex,
    pub inventory: EvidenceInventory,
    pub report: CoverageReport,
    pub long_run: LongRunSummary,
}

impl Analysis {
    /// The machine-readable report: obligations, scopes and the retained
    /// evidence inventory in one document.
    pub fn to_json(&self) -> serde_json::Value {
        let mut value = self.report.to_json();
        if let Some(object) = value.as_object_mut() {
            object.insert("evidence_inventory".to_string(), self.inventory.to_json());
            object.insert(
                "long_run_observation".to_string(),
                self.long_run.to_json(),
            );
            object.insert(
                "spec_index_counts".to_string(),
                serde_json::json!({
                    "requirements": self.index.counts().requirements,
                    "behaviors": self.index.counts().behaviors,
                    "acceptance_cases": self.index.counts().acceptance_cases,
                }),
            );
            object.insert(
                "spec_index".to_string(),
                spec_index::index_to_json(&self.index),
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
        lines.push(format!(
            "inventory errors: {} ({} non-evidence JSON files)",
            self.inventory.issues.len(),
            self.inventory.non_evidence_files.len()
        ));
        lines.push(format!(
            "long-run duration: {} (admitted reports {}, rejected reports {}, unobserved windows: {})",
            self.long_run.duration_status(),
            self.long_run.admitted.len(),
            self.long_run.rejections.len(),
            self.long_run.unobserved_windows().join(",")
        ));
        for conflict in self.long_run.identity_conflicts() {
            lines.push(format!("  long-run identity conflict: {conflict}"));
        }
        for obligation in unmet.iter().take(20) {
            let reasons: Vec<String> = obligation
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
