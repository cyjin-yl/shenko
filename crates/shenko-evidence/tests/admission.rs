use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shenko_evidence::{EvidenceRecord, Scope, SpecIndex, UnmetReason, build_report};

const CANDIDATE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn bound_file(root: &Path, name: &str, contents: &[u8]) -> Value {
    fs::write(root.join(name), contents).unwrap();
    json!({"path": name, "sha256": digest(contents)})
}

fn spec(case_id: &str) -> SpecIndex {
    SpecIndex::parse(&format!(
        "| R001 | fixture | §23 | U01;B01 |\n| B01 fixture | R001 | contract | {case_id} |\n| {case_id} | scenario | condition |\n"
    )).unwrap()
}

fn fixture(root: &Path, case_id: &str) -> Value {
    let source = bound_file(
        root,
        "source.txt",
        b"synthetic source binding, not product acceptance",
    );
    let artifact = bound_file(root, "artifact.bin", b"synthetic artifact binding");
    let output = bound_file(root, "observed.txt", b"retained observation");
    json!({
        "kind": "shenko_native_case_receipt", "schema_version": 1,
        "evidence_id": "fixture", "candidate_commit": CANDIDATE, "status": "passed",
        "source": source, "artifact": artifact,
        "build": {"profile": "fixture"}, "configuration": {"mode": "fixture"},
        "environment": {"surface": "fixture"}, "boundaries": ["not product proof"],
        "run": {"started_unix_ms": 1, "finished_unix_ms": 2, "exit_code": 0},
        "cases": [{"case_id": case_id, "status": "passed", "observations": {
            "native": {"status": "passed", "checks": {"condition": true}, "output": output}
        }}]
    })
}

fn publish(root: &Path, receipt: &Value) -> EvidenceRecord {
    let bytes = serde_json::to_vec(receipt).unwrap();
    let receipt_path = root.join("receipt.json");
    fs::write(&receipt_path, &bytes).unwrap();
    let descriptor = json!({
        "evidence_id": "fixture", "candidate_commit": CANDIDATE, "status": "passed",
        "requirements": ["R001"], "full_cases_passed": [receipt["cases"][0]["case_id"]],
        "native_receipt": receipt_path, "native_receipt_sha256": digest(&bytes)
    });
    let path = root.join("record.json");
    fs::write(&path, serde_json::to_vec(&descriptor).unwrap()).unwrap();
    EvidenceRecord::from_json(&descriptor, path.to_str().unwrap()).unwrap()
}

#[test]
fn range_holes_remain_unresolved_instead_of_shrinking_the_obligation() {
    let index = SpecIndex::parse("| R001 | range | §45 | U01;B01 |\n| B01 range | R001 | contract | AW01–03 |\n| AW01 | first | condition |\n| AW03 | last | condition |\n").unwrap();
    assert_eq!(index.behaviors["B01"].acceptance_cases, ["AW01", "AW03"]);
    assert_eq!(index.behaviors["B01"].unresolved_case_refs, ["AW01–03"]);
    let report = build_report(&index, &[], Some(CANDIDATE));
    assert!(
        report.requirements[0]
            .unmet_reasons
            .contains(&UnmetReason::UnknownReference)
    );
}

#[test]
fn qualified_ids_cannot_replace_parent_definitions() {
    let index = SpecIndex::parse("| B15 memory | R001 | contract | A01 |\n| B15.Q1 detail | upstream | comparison |\n| A01 | parent scenario | parent condition |\n| A01.detail | child scenario | child condition |\n").unwrap();
    assert_eq!(index.behaviors["B15"].acceptance_cases, ["A01"]);
    assert_eq!(
        index.acceptance_cases["A01"].pass_condition,
        "parent condition"
    );
}

#[test]
fn unknown_case_family_stays_visible_as_unclassified() {
    let index = SpecIndex::parse("| R001 | unknown family | §23 | B01 |\n| B01 extension | R001 | contract | ZZ01 |\n\n| Case | scenario | result |\n| --- | --- | --- |\n| ZZ01 | new family | condition |\n").unwrap();
    let report = build_report(&index, &[], Some(CANDIDATE));
    assert_eq!(report.requirements[0].acceptance_cases, ["ZZ01"]);
    assert!(
        report.requirements[0]
            .unmet_reasons
            .contains(&UnmetReason::UnclassifiedScope)
    );
}

#[test]
fn modifying_observed_bytes_withdraws_previously_admitted_coverage() {
    let root = tempfile::tempdir().unwrap();
    let receipt = fixture(root.path(), "A01");
    let record = publish(root.path(), &receipt);
    let index = spec("A01");
    assert!(
        build_report(&index, std::slice::from_ref(&record), Some(CANDIDATE)).requirements[0]
            .is_met()
    );
    fs::write(root.path().join("observed.txt"), b"different result").unwrap();
    let report = build_report(&index, &[record], Some(CANDIDATE));
    assert!(!report.requirements[0].is_met());
    assert!(
        report.requirements[0]
            .unmet_reasons
            .contains(&UnmetReason::ReceiptUnproven)
    );
}

#[test]
fn original_outcome_contradiction_cannot_be_overridden_by_the_record() {
    let root = tempfile::tempdir().unwrap();
    let mut receipt = fixture(root.path(), "A01");
    receipt["passed"] = json!(false);
    let record = publish(root.path(), &receipt);
    let report = build_report(&spec("A01"), &[record], Some(CANDIDATE));
    assert!(!report.requirements[0].is_met());
    assert!(
        report.requirements[0]
            .unmet_reasons
            .contains(&UnmetReason::ReceiptUnproven)
    );
}

#[test]
fn required_model_scope_is_not_borrowed_from_native_or_stale_evidence() {
    let root = tempfile::tempdir().unwrap();
    let mut receipt = fixture(root.path(), "A06");
    let record = publish(root.path(), &receipt);
    let index = spec("A06");
    let partial = build_report(&index, &[record], Some(CANDIDATE));
    assert!(!partial.requirements[0].is_met());
    assert!(
        partial.requirements[0]
            .unmet_reasons
            .contains(&UnmetReason::ScopeUnproven(Scope::Model))
    );

    // An explicit second scope is fixture data for the admission contract,
    // not a model execution or a claim that SPEC A06 has passed.
    receipt["cases"][0]["observations"]["model"] = json!({
        "status": "passed", "checks": {"model_condition": true},
        "output": bound_file(root.path(), "model.txt", b"synthetic model observation")
    });
    let current = publish(root.path(), &receipt);
    let mut historical = current.clone();
    historical.evidence_id = "historical".to_string();
    historical.source_path = root
        .path()
        .join("historical.json")
        .to_string_lossy()
        .into_owned();
    historical.candidate_commit = Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string());
    let report = build_report(&index, &[historical, current], Some(CANDIDATE));
    assert!(report.requirements[0].is_met());
    assert!(report.requirements[0].bound_to_current_candidate);
}
