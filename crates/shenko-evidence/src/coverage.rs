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
//! A case can require several scopes; the mapping is derived from the SPEC
//! row's own structure — its section, its case family and the words the SPEC
//! uses in the pass condition — and every classification is emitted with the
//! SPEC line it came from so a reviewer can check it.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use crate::scopes::{classify, ScopeDeclaration};
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
    /// A required verification scope has no passing evidence yet.
    ScopeUnproven(Scope),
}

impl UnmetReason {
    pub fn as_str(self) -> String {
        match self {
            UnmetReason::NoEvidence => "no_evidence".to_string(),
            UnmetReason::ReferenceOnly => "reference_only".to_string(),
            UnmetReason::StaleCandidate => "stale_candidate".to_string(),
            UnmetReason::ScopeUnproven(scope) => {
                format!("scope_unproven:{}", scope.as_str())
            }
        }
    }
}

/// A recorded piece of evidence, as declared by an evidence record.
#[derive(Debug, Clone)]
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
        let evidence_id = object
            .get("evidence_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                std::path::Path::new(source_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
            })?;
        let candidate_commit = ["candidate_commit", "product_commit", "implementation_commit"]
            .iter()
            .find_map(|key| object.get(*key).and_then(Value::as_str))
            .map(str::to_string);
        let requirements = string_list(object.get("requirements"));
        let full_cases_passed = string_list(object.get("full_cases_passed"));
        let cases_reference_only = string_list(object.get("cases_reference_only"));
        let status = object.get("status").and_then(Value::as_str).map(str::to_string);
        let native_receipt = object
            .get("native_receipt")
            .and_then(Value::as_str)
            .map(str::to_string);
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

fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
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
    /// The verification scopes this requirement's cases require.
    pub required_scopes: Vec<Scope>,
    /// Evidence naming this requirement.
    pub evidence_ids: Vec<String>,
    /// Cases with a fully-passed evidence claim.
    pub passed_cases: Vec<String>,
    /// Cases named only as references.
    pub referenced_cases: Vec<String>,
    /// Whether every referenced evidence record is bound to the current
    /// candidate.
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
            "schema_version": 1,
            "candidate_commit": self.candidate_commit,
            "denominator": {
                "requirements": self.requirement_count,
                "acceptance_cases": self.acceptance_case_count,
            },
            "unmet_requirement_count": self.unmet().len(),
            "requirements": requirements,
            "case_scopes": case_scopes,
            "unreferenced_acceptance_cases": self.unreferenced_cases,
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
    let case_scopes = classify_case_scopes(index);
    let referenced = index.referenced_acceptance_cases();
    let unreferenced_cases: Vec<String> = index
        .acceptance_cases
        .keys()
        .filter(|case_id| !referenced.contains(*case_id))
        .cloned()
        .collect();

    let mut requirements = Vec::new();
    for requirement in index.requirements.values() {
        let applicable: Vec<String> = requirement
            .behaviors
            .iter()
            .filter_map(|behavior_id| index.behaviors.get(behavior_id))
            .flat_map(|behavior| behavior.acceptance_cases.clone())
            .collect();
        let applicable: Vec<String> = applicable
            .into_iter()
            .collect::<BTreeSet<String>>()
            .into_iter()
            .collect();

        let mut evidence_ids = BTreeSet::new();
        let mut passed_cases = BTreeSet::new();
        let mut referenced_cases = BTreeSet::new();
        let mut all_current = true;
        for record in evidence {
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
            for case_id in &record.full_cases_passed {
                if applicable.contains(case_id) {
                    passed_cases.insert(case_id.clone());
                }
            }
            for case_id in &record.cases_reference_only {
                if applicable.contains(case_id) {
                    referenced_cases.insert(case_id.clone());
                }
            }
            if let (Some(candidate), Some(record_commit)) =
                (candidate_commit, record.candidate_commit.as_deref())
            {
                if candidate != record_commit {
                    all_current = false;
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
        if evidence_ids.is_empty() {
            unmet_reasons.insert(UnmetReason::NoEvidence);
        } else {
            if !all_current {
                unmet_reasons.insert(UnmetReason::StaleCandidate);
            }
            if passed_cases.len() < applicable.len() {
                if referenced_cases.is_empty() {
                    unmet_reasons.insert(UnmetReason::NoEvidence);
                } else {
                    unmet_reasons.insert(UnmetReason::ReferenceOnly);
                }
            }
        }
        // A required scope stays unproven until an evidence record bound to
        // the current candidate declares one of the requirement's cases as
        // fully passed. The scope says which kind of proof is still missing.
        if passed_cases.is_empty() {
            for scope in &required_scopes {
                unmet_reasons.insert(UnmetReason::ScopeUnproven(*scope));
            }
        }

        requirements.push(RequirementObligation {
            requirement_id: requirement.id.clone(),
            summary: requirement.summary.clone(),
            spec_locations: requirement.spec_locations.clone(),
            behaviors: requirement.behaviors.clone(),
            acceptance_cases: applicable,
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
