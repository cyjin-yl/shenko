//! Coverage and obligation mapping — SPEC §23/§29/§40/§46, C19.
//!
//! This module turns the parsed SPEC index into a machine-readable statement
//! of what is still owed. It never decides that an obligation is met by
//! looking at words: a requirement is only *covered* when a named artifact
//! or a named native/model receipt is bound to it, and every obligation that
//! lacks such a binding is reported as unmet with its reason.
//!
//! The four verification scopes are the ones the SPEC's acceptance tables
//! actually distinguish:
//!
//! * `failure` — the case's pass condition is about a fault, crash, kill,
//!   timeout or otherwise broken state (SPEC §41.16 fault matrix, §36
//!   recovery boundaries, A-cases about crash/recovery).
//! * `recovery` — the case's pass condition is about returning to a working
//!   state after such a fault.
//! * `model` — the pass condition needs a real served model to judge
//!   something (semantic discovery, choice, intent).
//! * `native` — the pass condition needs a real native/host execution
//!   surface (WASM ABI, SSH, browser, desktop, provider wire).
//!
//! Scope requirements are explicit declarations, not prose classifiers.
//! Admission checks retained original receipts and their bound bytes; it
//! does not independently judge the truth of a producer's observations.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use crate::receipt::{AdmittedReceipt, admit_receipt};
use crate::scopes::{ScopeDeclaration, classify};
use crate::spec_index::SpecIndex;

/// A verification scope an acceptance case needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Failure,
    Recovery,
    Model,
    Native,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Failure => "failure",
            Scope::Recovery => "recovery",
            Scope::Model => "model",
            Scope::Native => "native",
        }
    }

    pub fn parse(value: &str) -> Option<Scope> {
        match value {
            "failure" => Some(Scope::Failure),
            "recovery" => Some(Scope::Recovery),
            "model" => Some(Scope::Model),
            "native" => Some(Scope::Native),
            _ => None,
        }
    }
}

/// Why an obligation is still owed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnmetReason {
    /// No evidence names this requirement or case at all.
    NoEvidence,
    /// Evidence names it but only as a reference, not as a passed case.
    ReferenceOnly,
    /// Evidence exists but is bound to an older candidate.
    StaleCandidate,
    UnboundCandidate,
    StatusNotPassed,
    ReceiptUnproven,
    UnmappedRequirement,
    UnknownReference,
    UnclassifiedScope,
    InventoryIncomplete,
    /// A required verification scope has no passing evidence yet.
    ScopeUnproven(Scope),
}

impl UnmetReason {
    pub fn as_str(self) -> String {
        match self {
            UnmetReason::NoEvidence => "no_evidence".to_string(),
            UnmetReason::ReferenceOnly => "reference_only".to_string(),
            UnmetReason::StaleCandidate => "stale_candidate".to_string(),
            UnmetReason::UnboundCandidate => "unbound_candidate".to_string(),
            UnmetReason::StatusNotPassed => "status_not_passed".to_string(),
            UnmetReason::ReceiptUnproven => "receipt_unproven".to_string(),
            UnmetReason::UnmappedRequirement => "unmapped_requirement".to_string(),
            UnmetReason::UnknownReference => "unknown_reference".to_string(),
            UnmetReason::UnclassifiedScope => "unclassified_scope".to_string(),
            UnmetReason::InventoryIncomplete => "inventory_incomplete".to_string(),
            UnmetReason::ScopeUnproven(scope) => {
                format!("scope_unproven:{}", scope.as_str())
            }
        }
    }
}

/// A recorded piece of evidence, as declared by an evidence record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRecord {
    pub evidence_id: String,
    /// The candidate commit the evidence was produced against.
    pub candidate_commit: Option<String>,
    /// Requirement IDs the evidence names.
    pub requirements: Vec<String>,
    /// Case IDs the evidence names as fully passed.
    pub full_cases_passed: Vec<String>,
    /// Case IDs the evidence names only as references.
    pub cases_reference_only: Vec<String>,
    /// The evidence's own status string, kept verbatim.
    pub status: Option<String>,
    /// Path to the native receipt, when the evidence has one.
    pub native_receipt: Option<String>,
    /// The evidence file this record was read from.
    pub source_path: String,
}

impl EvidenceRecord {
    /// Parse one evidence record. The records predate a single schema, so
    /// every field is read defensively and a missing field stays missing
    /// instead of becoming a claim.
    pub fn from_json(value: &Value, source_path: &str) -> Option<EvidenceRecord> {
        let object = value.as_object()?;
        if !looks_like_evidence(value) {
            return None;
        }
        let evidence_id = optional_string(object.get("evidence_id"))?.or_else(|| {
            std::path::Path::new(source_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })?;
        let mut candidate_commit = None;
        for key in [
            "candidate_commit",
            "product_commit",
            "implementation_commit",
        ] {
            if let Some(commit) = optional_string(object.get(key))? {
                if candidate_commit
                    .as_ref()
                    .is_some_and(|previous| previous != &commit)
                {
                    return None;
                }
                candidate_commit = Some(commit);
            }
        }
        let requirements = string_list(object.get("requirements"))?;
        let full_cases_passed = string_list(object.get("full_cases_passed"))?;
        let cases_reference_only = string_list(object.get("cases_reference_only"))?;
        let status = optional_string(object.get("status"))?;
        let native_receipt = optional_string(object.get("native_receipt"))?;
        Some(EvidenceRecord {
            evidence_id,
            candidate_commit,
            requirements,
            full_cases_passed,
            cases_reference_only,
            status,
            native_receipt,
            source_path: source_path.to_string(),
        })
    }
}

pub(crate) fn looks_like_evidence(value: &Value) -> bool {
    [
        "evidence_id",
        "full_cases_passed",
        "cases_reference_only",
        "native_receipt",
        "candidate_commit",
        "product_commit",
        "implementation_commit",
    ]
    .iter()
    .any(|key| value.get(*key).is_some())
        || value.get("requirements").is_some_and(Value::is_array)
}

fn optional_string(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(text)) if !text.trim().is_empty() => Some(Some(text.clone())),
        _ => None,
    }
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
            })
            .collect(),
        None => Some(Vec::new()),
        _ => None,
    }
}

/// One obligation derived from a requirement and its acceptance cases.
#[derive(Debug, Clone)]
pub struct RequirementObligation {
    pub requirement_id: String,
    pub summary: String,
    pub spec_locations: String,
    pub behaviors: Vec<String>,
    /// Acceptance cases that apply to this requirement.
    pub acceptance_cases: Vec<String>,
    /// Unknown behavior IDs, unresolved ranges, or undefined case IDs.
    pub unresolved_references: Vec<String>,
    /// The verification scopes this requirement's cases require.
    pub required_scopes: Vec<Scope>,
    /// Evidence naming this requirement.
    pub evidence_ids: Vec<String>,
    /// Cases admitted from original, current receipts covering every scope.
    pub passed_cases: Vec<String>,
    /// Cases named only as references.
    pub referenced_cases: Vec<String>,
    /// Whether all applicable cases have admitted current-candidate proof.
    pub bound_to_current_candidate: bool,
    /// Why the obligation is unmet, when it is.
    pub unmet_reasons: Vec<UnmetReason>,
}

impl RequirementObligation {
    pub fn is_met(&self) -> bool {
        self.unmet_reasons.is_empty()
    }
}

/// The complete obligation map for a candidate.
#[derive(Debug, Clone)]
pub struct CoverageReport {
    /// The candidate commit the report is bound to.
    pub candidate_commit: Option<String>,
    pub requirements: Vec<RequirementObligation>,
    /// Per-case scope requirements, keyed by case ID.
    pub case_scopes: BTreeMap<String, Vec<Scope>>,
    /// Cases the SPEC defines but no behavior names.
    pub unreferenced_cases: Vec<String>,
    /// Requirements the SPEC defines.
    pub requirement_count: usize,
    pub acceptance_case_count: usize,
    /// Rejected full-pass declarations, including their original failure.
    pub evidence_rejections: BTreeMap<String, String>,
    pub admitted_receipts: BTreeMap<String, Value>,
}

impl CoverageReport {
    /// Requirements that are not met.
    pub fn unmet(&self) -> Vec<&RequirementObligation> {
        self.requirements
            .iter()
            .filter(|obligation| !obligation.is_met())
            .collect()
    }

    /// Machine-readable JSON for the report.
    pub fn to_json(&self) -> Value {
        let requirements: Vec<Value> = self
            .requirements
            .iter()
            .map(|obligation| {
                serde_json::json!({
                    "requirement_id": obligation.requirement_id,
                    "summary": obligation.summary,
                    "spec_locations": obligation.spec_locations,
                    "behaviors": obligation.behaviors,
                    "acceptance_cases": obligation.acceptance_cases,
                    "unresolved_references": obligation.unresolved_references,
                    "required_scopes": obligation
                        .required_scopes
                        .iter()
                        .map(|scope| scope.as_str())
                        .collect::<Vec<_>>(),
                    "evidence_ids": obligation.evidence_ids,
                    "passed_cases": obligation.passed_cases,
                    "referenced_cases": obligation.referenced_cases,
                    "bound_to_current_candidate": obligation.bound_to_current_candidate,
                    "met": obligation.is_met(),
                    "unmet_reasons": obligation
                        .unmet_reasons
                        .iter()
                        .map(|reason| reason.as_str())
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        let case_scopes: serde_json::Map<String, Value> = self
            .case_scopes
            .iter()
            .map(|(case_id, scopes)| {
                (
                    case_id.clone(),
                    Value::Array(
                        scopes
                            .iter()
                            .map(|scope| Value::String(scope.as_str().to_string()))
                            .collect(),
                    ),
                )
            })
            .collect();
        serde_json::json!({
            "kind": "shenko_spec_obligation_report",
            "schema_version": 2,
            "verification_boundary": "mechanical receipt admission; not independent product or model judgment",
            "candidate_commit": self.candidate_commit,
            "denominator": {
                "requirements": self.requirement_count,
                "acceptance_cases": self.acceptance_case_count,
            },
            "unmet_requirement_count": self.unmet().len(),
            "requirements": requirements,
            "case_scopes": case_scopes,
            "unreferenced_acceptance_cases": self.unreferenced_cases,
            "evidence_rejections": self.evidence_rejections,
            "admitted_receipts": self.admitted_receipts,
        })
    }
}

/// Build the obligation report.
///
/// `candidate_commit` is the commit the report is bound to. Evidence bound to
/// a different commit is reported as stale rather than silently counted.
pub fn build_report(
    index: &SpecIndex,
    evidence: &[EvidenceRecord],
    candidate_commit: Option<&str>,
) -> CoverageReport {
    build_report_from_records(index, evidence.iter(), candidate_commit)
}

pub(crate) fn build_report_from_records<'a>(
    index: &SpecIndex,
    evidence: impl IntoIterator<Item = &'a EvidenceRecord>,
    candidate_commit: Option<&str>,
) -> CoverageReport {
    let case_scopes = classify_case_scopes(index);
    let referenced = index.referenced_acceptance_cases();
    let unreferenced_cases: Vec<String> = index
        .acceptance_cases
        .keys()
        .filter(|case_id| !referenced.contains(*case_id))
        .cloned()
        .collect();

    // Read each original only once, not once per requirement that names it.
    let admissions: Vec<_> = evidence
        .into_iter()
        .map(|record| {
            let unknown = record
                .requirements
                .iter()
                .find(|id| !index.requirements.contains_key(*id))
                .or_else(|| {
                    record
                        .full_cases_passed
                        .iter()
                        .chain(&record.cases_reference_only)
                        .find(|id| !index.acceptance_cases.contains_key(*id))
                });
            let admitted = if let Some(id) = unknown {
                Err((
                    UnmetReason::UnknownReference,
                    format!("record names an undefined obligation: {id}"),
                ))
            } else if record.full_cases_passed.is_empty() {
                Ok(AdmittedReceipt {
                    cases: BTreeMap::new(),
                    binding: Value::Null,
                })
            } else if candidate_commit.is_none_or(str::is_empty)
                || record.candidate_commit.as_deref().is_none_or(str::is_empty)
            {
                Err((
                    UnmetReason::UnboundCandidate,
                    "candidate identity is missing".to_string(),
                ))
            } else if candidate_commit != record.candidate_commit.as_deref() {
                Err((
                    UnmetReason::StaleCandidate,
                    "record belongs to another candidate".to_string(),
                ))
            } else if record.status.as_deref() != Some("passed") {
                Err((
                    UnmetReason::StatusNotPassed,
                    "record status is not passed".to_string(),
                ))
            } else {
                admit_receipt(record).map_err(|error| (UnmetReason::ReceiptUnproven, error))
            };
            (record, admitted)
        })
        .collect();
    let evidence_rejections = admissions
        .iter()
        .filter_map(|(record, admitted)| {
            admitted
                .as_ref()
                .err()
                .map(|(_, error)| (record.source_path.clone(), error.clone()))
        })
        .collect();
    let admitted_receipts = admissions
        .iter()
        .filter_map(|(record, admitted)| {
            admitted
                .as_ref()
                .ok()
                .filter(|receipt| !receipt.binding.is_null())
                .map(|receipt| (record.source_path.clone(), receipt.binding.clone()))
        })
        .collect();

    let mut requirements = Vec::new();
    for requirement in index.requirements.values() {
        let mut applicable = BTreeSet::new();
        let mut unresolved = BTreeSet::new();
        let mut unmapped = requirement.behaviors.is_empty();
        for behavior_id in &requirement.behaviors {
            if let Some(behavior) = index.behaviors.get(behavior_id) {
                unmapped |= behavior.acceptance_cases.is_empty();
                unresolved.extend(behavior.unresolved_case_refs.iter().cloned());
                for case_id in &behavior.acceptance_cases {
                    if index.acceptance_cases.contains_key(case_id) {
                        applicable.insert(case_id.clone());
                    } else {
                        unresolved.insert(case_id.clone());
                    }
                }
            } else {
                unresolved.insert(behavior_id.clone());
                unmapped = true;
            }
        }
        let mut evidence_ids = BTreeSet::new();
        let mut passed_cases = BTreeSet::new();
        let mut referenced_cases = BTreeSet::new();
        let mut rejection_reasons = BTreeSet::new();
        for (record, admission) in &admissions {
            let names_requirement = record.requirements.iter().any(|id| id == &requirement.id);
            let names_case = record
                .full_cases_passed
                .iter()
                .chain(record.cases_reference_only.iter())
                .any(|case_id| applicable.contains(case_id));
            if !names_requirement && !names_case {
                continue;
            }
            evidence_ids.insert(record.evidence_id.clone());
            if let Err((reason, _)) = admission {
                rejection_reasons.insert(*reason);
            }
            for case_id in &record.full_cases_passed {
                let full_scope = admission
                    .as_ref()
                    .ok()
                    .and_then(|receipt| receipt.cases.get(case_id))
                    .zip(case_scopes.get(case_id))
                    .is_some_and(|(observed, required)| {
                        !required.is_empty()
                            && required.iter().all(|scope| observed.contains(scope))
                    });
                if applicable.contains(case_id) && full_scope {
                    passed_cases.insert(case_id.clone());
                }
            }
            for case_id in &record.cases_reference_only {
                if applicable.contains(case_id) {
                    referenced_cases.insert(case_id.clone());
                }
            }
        }

        let required_scopes = merge_scopes(
            applicable
                .iter()
                .filter_map(|case_id| case_scopes.get(case_id))
                .flat_map(|scopes| scopes.iter().copied()),
        );

        let mut unmet_reasons = BTreeSet::new();
        if unmapped || applicable.is_empty() {
            unmet_reasons.insert(UnmetReason::UnmappedRequirement);
        }
        if !unresolved.is_empty() {
            unmet_reasons.insert(UnmetReason::UnknownReference);
        }
        if evidence_ids.is_empty() {
            unmet_reasons.insert(UnmetReason::NoEvidence);
        }
        if passed_cases.len() < applicable.len() {
            unmet_reasons.extend(rejection_reasons);
            if referenced_cases.is_empty() {
                unmet_reasons.insert(UnmetReason::NoEvidence);
            } else {
                unmet_reasons.insert(UnmetReason::ReferenceOnly);
            }
        }
        for case_id in applicable.difference(&passed_cases) {
            if let Some(scopes) = case_scopes.get(case_id).filter(|scopes| !scopes.is_empty()) {
                for scope in scopes {
                    unmet_reasons.insert(UnmetReason::ScopeUnproven(*scope));
                }
            } else {
                unmet_reasons.insert(UnmetReason::UnclassifiedScope);
            }
        }

        let all_current = !applicable.is_empty() && passed_cases.len() == applicable.len();
        requirements.push(RequirementObligation {
            requirement_id: requirement.id.clone(),
            summary: requirement.summary.clone(),
            spec_locations: requirement.spec_locations.clone(),
            behaviors: requirement.behaviors.clone(),
            acceptance_cases: applicable.into_iter().collect(),
            unresolved_references: unresolved.into_iter().collect(),
            required_scopes,
            evidence_ids: evidence_ids.into_iter().collect(),
            passed_cases: passed_cases.into_iter().collect(),
            referenced_cases: referenced_cases.into_iter().collect(),
            bound_to_current_candidate: all_current,
            unmet_reasons: unmet_reasons.into_iter().collect(),
        });
    }

    CoverageReport {
        candidate_commit: candidate_commit.map(str::to_string),
        requirements,
        case_scopes,
        unreferenced_cases,
        requirement_count: index.requirements.len(),
        acceptance_case_count: index.acceptance_cases.len(),
        evidence_rejections,
        admitted_receipts,
    }
}

fn merge_scopes<I: Iterator<Item = Scope>>(scopes: I) -> Vec<Scope> {
    let mut set = BTreeSet::new();
    set.extend(scopes);
    set.into_iter().collect()
}

/// Classify the verification scopes each acceptance case requires.
///
/// The classification is declared per case in [`crate::scopes`], so it is a
/// reviewable decision rather than a runtime heuristic. It says *what kind
/// of proof a case needs*; it never decides whether the proof exists.
pub fn classify_case_scopes(index: &SpecIndex) -> BTreeMap<String, Vec<Scope>> {
    classify(index)
        .into_iter()
        .map(|(case_id, declaration)| (case_id, declaration.scopes))
        .collect()
}

/// The declared scope set of every case, with the reason for each.
pub fn scope_declarations(index: &SpecIndex) -> BTreeMap<String, ScopeDeclaration> {
    classify(index)
}
